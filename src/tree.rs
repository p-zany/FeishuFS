use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use fsk::{FileType, ROOT_INODE};

use crate::{
    link::KnowledgeLink,
    markdown::rewrite_document_links,
    remote::{ContentRequest, RemoteNode, RemoteTree},
};

const MAX_COMPONENT_BYTES: usize = 180;

#[derive(Clone, Debug)]
pub(crate) struct DirectoryEntry {
    pub name: Vec<u8>,
    pub inode: u64,
    pub kind: FileType,
}

#[derive(Clone, Debug)]
pub(crate) enum VirtualNodeKind {
    Directory { entries: Vec<DirectoryEntry> },
    File { request: ContentRequest },
}

#[derive(Clone, Debug)]
pub(crate) struct VirtualNode {
    pub inode: u64,
    pub parent: u64,
    pub path: String,
    pub generation: u64,
    pub created_ns: u64,
    pub kind: VirtualNodeKind,
}

impl VirtualNode {
    #[must_use]
    pub fn file_type(&self) -> FileType {
        match self.kind {
            VirtualNodeKind::Directory { .. } => FileType::Directory,
            VirtualNodeKind::File { .. } => FileType::File,
        }
    }
}

pub(crate) struct FsState {
    pub nodes: HashMap<u64, VirtualNode>,
    inode_by_key: HashMap<String, u64>,
    name_by_edge: HashMap<(String, String), String>,
    base_name_by_edge: HashMap<(String, String), String>,
    content_path_by_token: HashMap<String, String>,
    next_inode: u64,
    revision: u64,
    created_ns: u64,
    pub root_title: String,
}

impl FsState {
    pub fn from_remote(tree: &RemoteTree, created_ns: u64) -> Result<Self> {
        let mut state = Self {
            nodes: HashMap::new(),
            inode_by_key: HashMap::new(),
            name_by_edge: HashMap::new(),
            base_name_by_edge: HashMap::new(),
            content_path_by_token: HashMap::new(),
            next_inode: ROOT_INODE + 1,
            revision: 0,
            created_ns,
            root_title: String::new(),
        };
        state.replace(tree)?;
        Ok(state)
    }

    pub fn replace(&mut self, tree: &RemoteTree) -> Result<Vec<String>> {
        self.revision = self.revision.saturating_add(1);
        self.root_title.clone_from(&tree.root.title);
        self.inode_by_key
            .insert(directory_key(&tree.root.node_token), ROOT_INODE);

        let old_paths = self
            .nodes
            .values()
            .filter(|node| !node.path.is_empty())
            .map(|node| node.path.clone())
            .collect();
        let mut building = Building {
            nodes: HashMap::with_capacity(tree.node_count().saturating_mul(2)),
            content_path_by_token: HashMap::with_capacity(tree.node_count().saturating_mul(2)),
        };
        self.add_directory(
            tree,
            &tree.root,
            ROOT_INODE,
            ROOT_INODE,
            String::new(),
            &mut building,
        )?;
        self.nodes = building.nodes;
        self.content_path_by_token = building.content_path_by_token;
        Ok(old_paths)
    }

    fn add_directory(
        &mut self,
        tree: &RemoteTree,
        remote: &RemoteNode,
        inode: u64,
        parent: u64,
        path: String,
        building: &mut Building,
    ) -> Result<()> {
        let content_path_by_token = &mut building.content_path_by_token;
        let (content_name, content_request) = content_mapping(remote);
        let content_inode = self.inode_for(&content_key(&remote.node_token))?;
        let content_path = join_path(&path, &content_name);
        register_link_target(content_path_by_token, &remote.node_token, &content_path);
        if let Some(object_token) = remote.object_token.as_deref() {
            register_link_target(content_path_by_token, object_token, &content_path);
        }
        if let Some(link_token) = remote
            .url
            .as_deref()
            .and_then(|url| url.parse::<KnowledgeLink>().ok())
            .map(|link| link.token)
        {
            register_link_target(content_path_by_token, &link_token, &content_path);
        }
        building.nodes.insert(
            content_inode,
            VirtualNode {
                inode: content_inode,
                parent: inode,
                path: content_path.clone(),
                generation: self.revision,
                created_ns: self.created_ns,
                kind: VirtualNodeKind::File {
                    request: content_request,
                },
            },
        );

        let children = tree
            .children
            .get(&remote.node_token)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let assigned_names = self.assign_child_names(remote, children, &content_name);
        let mut entries = Vec::with_capacity(children.len() + 1);
        entries.push(DirectoryEntry {
            name: content_name.as_bytes().to_vec(),
            inode: content_inode,
            kind: FileType::File,
        });

        for (child, child_name) in children.iter().zip(assigned_names) {
            let child_inode = self.inode_for(&directory_key(&child.node_token))?;
            let child_path = join_path(&path, &child_name);
            entries.push(DirectoryEntry {
                name: child_name.as_bytes().to_vec(),
                inode: child_inode,
                kind: FileType::Directory,
            });
            self.add_directory(tree, child, child_inode, inode, child_path, building)?;
        }

        entries.sort_by(|left, right| {
            fold_name_bytes(&left.name)
                .cmp(&fold_name_bytes(&right.name))
                .then_with(|| left.name.cmp(&right.name))
        });
        building.nodes.insert(
            inode,
            VirtualNode {
                inode,
                parent,
                path,
                generation: self.revision,
                created_ns: self.created_ns,
                kind: VirtualNodeKind::Directory { entries },
            },
        );
        Ok(())
    }

    fn assign_child_names(
        &mut self,
        parent: &RemoteNode,
        children: &[RemoteNode],
        content_name: &str,
    ) -> Vec<String> {
        let mut used = HashSet::from([fold_name(content_name)]);
        let mut names = Vec::with_capacity(children.len());
        for child in children {
            let edge = (parent.node_token.clone(), child.node_token.clone());
            let base = sanitize_component(&child.title);
            let title_is_unchanged = self
                .base_name_by_edge
                .get(&edge)
                .is_some_and(|existing| existing == &base);
            if title_is_unchanged
                && let Some(existing) = self.name_by_edge.get(&edge)
                && used.insert(fold_name(existing))
            {
                names.push(existing.clone());
                continue;
            }

            let name = unique_component(&base, &child.node_token, &mut used);
            self.base_name_by_edge.insert(edge.clone(), base);
            self.name_by_edge.insert(edge, name.clone());
            names.push(name);
        }
        names
    }

    fn inode_for(&mut self, key: &str) -> Result<u64> {
        if let Some(inode) = self.inode_by_key.get(key) {
            return Ok(*inode);
        }
        let inode = self.next_inode;
        if inode == u64::MAX {
            bail!("inode 空间已耗尽");
        }
        self.next_inode += 1;
        self.inode_by_key.insert(key.to_owned(), inode);
        Ok(inode)
    }

    #[must_use]
    pub fn file_inodes(&self) -> HashSet<u64> {
        self.nodes
            .iter()
            .filter_map(|(inode, node)| {
                matches!(node.kind, VirtualNodeKind::File { .. }).then_some(*inode)
            })
            .collect()
    }

    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.nodes
            .values()
            .filter(|node| !node.path.is_empty())
            .map(|node| node.path.clone())
            .collect()
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn render_markdown(&self, inode: u64, request: &ContentRequest, source: &[u8]) -> Vec<u8> {
        let Some(node) = self.nodes.get(&inode) else {
            return source.to_vec();
        };
        let VirtualNodeKind::File {
            request: current_request,
        } = &node.kind
        else {
            return source.to_vec();
        };
        if current_request != request {
            return source.to_vec();
        }
        let Ok(markdown) = std::str::from_utf8(source) else {
            return source.to_vec();
        };
        rewrite_document_links(markdown, &node.path, &self.content_path_by_token).into_bytes()
    }
}

struct Building {
    nodes: HashMap<u64, VirtualNode>,
    content_path_by_token: HashMap<String, String>,
}

fn register_link_target(targets: &mut HashMap<String, String>, token: &str, path: &str) {
    if !token.is_empty() {
        targets
            .entry(token.to_owned())
            .or_insert_with(|| path.to_owned());
    }
}

fn directory_key(node_token: &str) -> String {
    format!("directory:{node_token}")
}

fn content_key(node_token: &str) -> String {
    format!("content:{node_token}")
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

#[must_use]
pub fn content_mapping(node: &RemoteNode) -> (String, ContentRequest) {
    let Some(token) = node.object_token.as_ref().filter(|token| !token.is_empty()) else {
        return unsupported_content(node, "节点没有可读取的 obj_token");
    };
    match node.object_type.as_deref().map(str::to_ascii_lowercase) {
        Some(kind) if kind == "docx" => (
            "index.md".into(),
            ContentRequest::Markdown {
                token: token.clone(),
                document_type: kind,
            },
        ),
        Some(kind) if kind == "doc" => (
            "index.docx".into(),
            ContentRequest::Export {
                token: token.clone(),
                document_type: kind,
                extension: "docx".into(),
            },
        ),
        Some(kind) if kind == "sheet" || kind == "bitable" => (
            "index.xlsx".into(),
            ContentRequest::Export {
                token: token.clone(),
                document_type: kind,
                extension: "xlsx".into(),
            },
        ),
        Some(kind) if kind == "file" => (
            format!("index.{}", safe_extension(&node.title)),
            ContentRequest::Download {
                token: token.clone(),
            },
        ),
        Some(kind) => unsupported_content(node, &format!("暂不支持对象类型 {kind}")),
        None => unsupported_content(node, "节点没有 obj_type"),
    }
}

fn unsupported_content(node: &RemoteNode, reason: &str) -> (String, ContentRequest) {
    let object_type = node.object_type.as_deref().unwrap_or("unknown");
    let url = node.url.as_deref().unwrap_or("（飞书未返回链接）");
    let markdown = format!(
        "# {}\n\n> FeishuFS 未能导出此节点。\n\n- 原因：{}\n- 对象类型：`{}`\n- 云端链接：{}\n",
        node.title, reason, object_type, url
    );
    (
        "index.md".into(),
        ContentRequest::Inline {
            bytes: markdown.into_bytes(),
        },
    )
}

fn safe_extension(title: &str) -> String {
    let extension = title
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or("")
        .to_ascii_lowercase();
    if (1..=10).contains(&extension.len())
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        extension
    } else {
        "bin".into()
    }
}

#[must_use]
pub fn sanitize_component(title: &str) -> String {
    let mut output = String::with_capacity(title.len());
    for character in title.chars() {
        if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
        {
            output.push('_');
        } else {
            output.push(character);
        }
    }
    let trimmed = output.trim().trim_end_matches([' ', '.']);
    let mut output = if trimmed.is_empty() {
        "未命名".to_owned()
    } else {
        trimmed.to_owned()
    };

    let stem_end = output.find('.').unwrap_or(output.len());
    if is_windows_reserved(&output[..stem_end]) {
        output.insert(stem_end, '_');
    }
    truncate_utf8(&mut output, MAX_COMPONENT_BYTES);
    if output.is_empty() {
        "未命名".to_owned()
    } else {
        output
    }
}

fn unique_component(base: &str, token: &str, used: &mut HashSet<String>) -> String {
    if used.insert(fold_name(base)) {
        return base.to_owned();
    }

    let short_token: String = token
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let short_token = if short_token.is_empty() {
        "node"
    } else {
        &short_token
    };
    for index in 1_u32.. {
        let suffix = if index == 1 {
            format!("~{short_token}")
        } else {
            format!("~{short_token}-{index}")
        };
        let max_base = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
        let mut candidate_base = base.to_owned();
        truncate_utf8(&mut candidate_base, max_base);
        let candidate = format!("{candidate_base}{suffix}");
        if used.insert(fold_name(&candidate)) {
            return candidate;
        }
    }
    unreachable!("u32 name suffix space exhausted")
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn fold_name(value: &str) -> String {
    value.to_lowercase()
}

fn fold_name_bytes(value: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(value).to_lowercase().into_bytes()
}

fn is_windows_reserved(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|number| number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9'))
}

pub(crate) fn file_request(node: &VirtualNode) -> Result<&ContentRequest> {
    match &node.kind {
        VirtualNodeKind::File { request } => Ok(request),
        VirtualNodeKind::Directory { .. } => {
            anyhow::bail!("inode {} 是目录而不是文件", node.inode)
        }
    }
}

pub(crate) fn directory_entries(node: &VirtualNode) -> Result<&[DirectoryEntry]> {
    match &node.kind {
        VirtualNodeKind::Directory { entries } => Ok(entries),
        VirtualNodeKind::File { .. } => {
            anyhow::bail!("inode {} 是文件而不是目录", node.inode)
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use crate::remote::test_support::node;

    use super::*;

    #[test]
    fn sanitizes_cross_platform_names() {
        assert_eq!(sanitize_component(" A<B>:C. "), "A_B__C");
        assert_eq!(sanitize_component("CON"), "CON_");
        assert_eq!(sanitize_component("lpt9.txt"), "lpt9_.txt");
        assert_eq!(sanitize_component("CON.md"), "CON_.md");
        assert_eq!(sanitize_component("..."), "未命名");
    }

    #[test]
    fn truncation_keeps_utf8_valid() {
        let title = "文".repeat(100);
        let result = sanitize_component(&title);
        assert!(result.len() <= MAX_COMPONENT_BYTES);
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn maps_supported_content_types() {
        let doc = node("doc", "Doc", "docx");
        assert!(matches!(
            content_mapping(&doc),
            (name, ContentRequest::Markdown { .. }) if name == "index.md"
        ));
        let file = node("file", "Manual.PDF", "file");
        assert!(matches!(
            content_mapping(&file),
            (name, ContentRequest::Download { .. }) if name == "index.pdf"
        ));
    }

    #[test]
    fn keeps_existing_name_when_a_case_insensitive_duplicate_appears() {
        let root = node("root", "Root", "docx");
        let first = node("first", "Guide", "docx");
        let mut tree = RemoteTree {
            root: root.clone(),
            children: HashMap::from([
                ("root".into(), vec![first.clone()]),
                ("first".into(), vec![]),
            ]),
        };
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        let original = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .find(|entry| entry.kind == FileType::Directory)
            .unwrap()
            .name
            .clone();

        let duplicate = node("second", "guide", "docx");
        tree.children.insert("root".into(), vec![first, duplicate]);
        tree.children.insert("second".into(), vec![]);
        state.replace(&tree).unwrap();
        let directory_names: Vec<_> = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .filter(|entry| entry.kind == FileType::Directory)
            .map(|entry| String::from_utf8(entry.name.clone()).unwrap())
            .collect();
        assert!(directory_names.contains(&String::from_utf8(original).unwrap()));
        assert!(
            directory_names
                .iter()
                .any(|name| name.starts_with("guide~"))
        );
    }

    #[test]
    fn cloud_rename_updates_name_without_changing_inode() {
        let root = node("root", "Root", "docx");
        let child = node("child", "Old name", "docx");
        let mut tree = RemoteTree {
            root: root.clone(),
            children: HashMap::from([
                ("root".into(), vec![child.clone()]),
                ("child".into(), vec![]),
            ]),
        };
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        let initial = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .find(|entry| entry.kind == FileType::Directory)
            .unwrap()
            .clone();

        let mut renamed = child;
        renamed.title = "New name".into();
        tree.children.insert("root".into(), vec![renamed]);
        state.replace(&tree).unwrap();
        let refreshed = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .find(|entry| entry.kind == FileType::Directory)
            .unwrap();

        assert_eq!(refreshed.name, b"New name");
        assert_eq!(refreshed.inode, initial.inode);
    }

    #[test]
    fn inodes_survive_refresh_and_are_never_reused() {
        let root = node("root", "Root", "docx");
        let child = node("child", "Child", "docx");
        let mut tree = RemoteTree {
            root: root.clone(),
            children: HashMap::from([
                ("root".into(), vec![child.clone()]),
                ("child".into(), vec![]),
            ]),
        };
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        let child_inode = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .find(|entry| entry.kind == FileType::Directory)
            .unwrap()
            .inode;

        tree.children.insert("root".into(), vec![]);
        state.replace(&tree).unwrap();
        tree.children.insert("root".into(), vec![child]);
        state.replace(&tree).unwrap();
        let refreshed_inode = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .find(|entry| entry.kind == FileType::Directory)
            .unwrap()
            .inode;
        assert_eq!(child_inode, refreshed_inode);
    }
}

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use fsk::{FileType, ROOT_INODE};

use crate::{
    link::KnowledgeLink,
    markdown::rewrite_document_links_with_assets,
    remote::{ContentRequest, RemoteNode, RemoteTree},
};

const MAX_COMPONENT_BYTES: usize = 180;
pub(crate) const ASSETS_DIRECTORY: &str = ".feishufs-assets";

#[derive(Clone, Debug, Eq, PartialEq)]
struct AssetSpec {
    name: String,
    request: ContentRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AssetManifest {
    request: ContentRequest,
    assets: Vec<AssetSpec>,
}

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
    assets: HashMap<u64, AssetManifest>,
    asset_inodes: HashSet<u64>,
    next_inode: u64,
    revision: u64,
    remote_tree: Option<RemoteTree>,
    created_ns: u64,
    pub root_title: String,
    /// Number of remote documents in the current snapshot.
    pub document_count: usize,
}

impl FsState {
    pub fn from_remote(tree: &RemoteTree, created_ns: u64) -> Result<Self> {
        let mut state = Self {
            nodes: HashMap::new(),
            inode_by_key: HashMap::new(),
            name_by_edge: HashMap::new(),
            base_name_by_edge: HashMap::new(),
            content_path_by_token: HashMap::new(),
            assets: HashMap::new(),
            asset_inodes: HashSet::new(),
            next_inode: ROOT_INODE + 1,
            revision: 0,
            remote_tree: None,
            created_ns,
            root_title: String::new(),
            document_count: 0,
        };
        state.replace(tree)?;
        Ok(state)
    }

    pub fn replace(&mut self, tree: &RemoteTree) -> Result<Option<Vec<String>>> {
        if self.remote_tree.as_ref() == Some(tree) {
            return Ok(None);
        }
        self.revision = self.revision.saturating_add(1);
        self.root_title.clone_from(&tree.title);
        self.document_count = tree.node_count();

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
        // The mount root is the space itself: a directory without content.
        let root_entries = self.place_children(
            tree,
            &tree.space_id,
            ROOT_INODE,
            "",
            Some(ASSETS_DIRECTORY),
            &mut building,
        )?;
        building.nodes.insert(
            ROOT_INODE,
            VirtualNode {
                inode: ROOT_INODE,
                parent: ROOT_INODE,
                path: String::new(),
                generation: self.revision,
                created_ns: self.created_ns,
                kind: VirtualNodeKind::Directory {
                    entries: root_entries,
                },
            },
        );
        self.nodes = building.nodes;
        self.content_path_by_token = building.content_path_by_token;
        self.asset_inodes.clear();
        self.assets.retain(|inode, manifest| {
            self.nodes
                .get(inode)
                .and_then(|node| file_request(node).ok())
                == Some(&manifest.request)
        });
        self.rebuild_assets()?;
        self.remote_tree = Some(tree.clone());
        Ok(Some(old_paths))
    }

    /// Create the entries for every child of `parent_token` inside `parent_path`.
    ///
    /// `reserved` holds names already taken in that directory (the parent's own
    /// content file), so children never shadow it.
    fn place_children(
        &mut self,
        tree: &RemoteTree,
        parent_token: &str,
        parent_inode: u64,
        parent_path: &str,
        reserved: Option<&str>,
        building: &mut Building,
    ) -> Result<Vec<DirectoryEntry>> {
        let children = tree.children_of(parent_token);
        let mut names = self.assign_child_names(tree, parent_token, children, reserved);
        let mut entries = Vec::with_capacity(children.len());
        for (child, name) in children.iter().zip(names.drain(..)) {
            let path = join_path(parent_path, &name);
            let entry = if tree.has_children(&child.node_token) {
                self.place_directory(tree, child, parent_inode, path, name, building)?
            } else {
                self.place_content_file(child, parent_inode, parent_path, &name, building)?
            };
            entries.push(entry);
        }

        entries.sort_by(|left, right| {
            fold_name_bytes(&left.name)
                .cmp(&fold_name_bytes(&right.name))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(entries)
    }

    /// A node with child pages becomes a directory holding `index.*` plus them.
    fn place_directory(
        &mut self,
        tree: &RemoteTree,
        remote: &RemoteNode,
        parent_inode: u64,
        path: String,
        name: String,
        building: &mut Building,
    ) -> Result<DirectoryEntry> {
        let inode = self.inode_for(&directory_key(&remote.node_token))?;
        let content_name = content_spec(remote).map(|spec| index_file_name(&spec));
        let mut entries = Vec::new();
        if let Some(content_name) = content_name.as_deref() {
            entries.push(self.place_content_file(remote, inode, &path, content_name, building)?);
        }
        entries.extend(self.place_children(
            tree,
            &remote.node_token,
            inode,
            &path,
            content_name.as_deref(),
            building,
        )?);
        entries.sort_by(|left, right| {
            fold_name_bytes(&left.name)
                .cmp(&fold_name_bytes(&right.name))
                .then_with(|| left.name.cmp(&right.name))
        });
        building.nodes.insert(
            inode,
            VirtualNode {
                inode,
                parent: parent_inode,
                path,
                generation: self.revision,
                created_ns: self.created_ns,
                kind: VirtualNodeKind::Directory { entries },
            },
        );
        Ok(DirectoryEntry {
            name: name.as_bytes().to_vec(),
            inode,
            kind: FileType::Directory,
        })
    }

    /// Materialise a node's own content as a file at `path` and index its tokens.
    fn place_content_file(
        &mut self,
        remote: &RemoteNode,
        parent_inode: u64,
        directory: &str,
        file_name: &str,
        building: &mut Building,
    ) -> Result<DirectoryEntry> {
        let spec = content_spec(remote);
        let request = spec
            .map(|spec| spec.request)
            .unwrap_or_else(|| unsupported_request(remote, "The node has no readable obj_token"));
        let inode = self.inode_for(&content_key(&remote.node_token))?;
        let path = join_path(directory, file_name);
        self.register_link_targets(building, remote, &path);
        building.nodes.insert(
            inode,
            VirtualNode {
                inode,
                parent: parent_inode,
                path,
                generation: self.revision,
                created_ns: self.created_ns,
                kind: VirtualNodeKind::File { request },
            },
        );
        Ok(DirectoryEntry {
            name: file_name.as_bytes().to_vec(),
            inode,
            kind: FileType::File,
        })
    }

    fn register_link_targets(&self, building: &mut Building, remote: &RemoteNode, path: &str) {
        let targets = &mut building.content_path_by_token;
        register_link_target(targets, &remote.node_token, path);
        if let Some(object_token) = remote.object_token.as_deref() {
            register_link_target(targets, object_token, path);
        }
        if let Some(link_token) = remote
            .url
            .as_deref()
            .and_then(|url| url.parse::<KnowledgeLink>().ok())
            .map(|link| link.token)
        {
            register_link_target(targets, &link_token, path);
        }
    }

    fn assign_child_names(
        &mut self,
        tree: &RemoteTree,
        parent_token: &str,
        children: &[RemoteNode],
        reserved: Option<&str>,
    ) -> Vec<String> {
        let mut used: HashSet<String> = reserved.into_iter().map(fold_name).collect();
        let mut names = Vec::with_capacity(children.len());
        for child in children {
            let edge = (parent_token.to_owned(), child.node_token.clone());
            let has_children = tree.has_children(&child.node_token);
            let base = if has_children {
                sanitize_component(&child.title)
            } else {
                leaf_file_name(child)
            };
            let extension = if has_children {
                None
            } else {
                content_spec(child).map(|spec| spec.extension)
            };
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

            let name = unique_component(&base, &child.node_token, &mut used, extension.as_deref());
            self.base_name_by_edge.insert(edge.clone(), base);
            self.name_by_edge.insert(edge, name.clone());
            names.push(name);
        }
        names
    }

    /// Install the assets of a successfully fetched document. Failure/recovery
    /// semantics are deliberately not defined by this happy-path prototype.
    pub fn set_assets(
        &mut self,
        owner: u64,
        request: &ContentRequest,
        specs: Vec<(String, ContentRequest)>,
    ) -> Result<bool> {
        if !request.is_markdown() {
            if specs.is_empty() {
                return Ok(false);
            }
            bail!("Only Markdown files can publish associated assets");
        }
        let mut used = HashSet::new();
        let mut used_tokens = HashSet::new();
        for (name, asset_request) in &specs {
            let valid_name = name.rsplit_once('.').is_some_and(|(stem, extension)| {
                !stem.is_empty()
                    && stem
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                    && matches!(extension, "jpg" | "png" | "webp" | "gif")
            });
            if !valid_name
                || name.len() > MAX_COMPONENT_BYTES
                || !used.insert(fold_name(name))
                || !matches!(asset_request, ContentRequest::Whiteboard { token } if crate::docx::valid_token(token) && used_tokens.insert(token.clone()) && name.split('.').next() == Some(token.bytes().map(|b| format!("{b:02x}")).collect::<String>().as_str()))
            {
                bail!("Invalid or duplicate asset name: {name}");
            }
        }
        let current = self
            .nodes
            .get(&owner)
            .and_then(|node| file_request(node).ok());
        if current != Some(request) {
            bail!("Document identity changed; stale assets cannot be published");
        }
        let manifest = AssetManifest {
            request: request.clone(),
            assets: specs
                .into_iter()
                .map(|(name, request)| AssetSpec { name, request })
                .collect(),
        };
        if self.assets.get(&owner) == Some(&manifest)
            || (manifest.assets.is_empty() && !self.assets.contains_key(&owner))
        {
            return Ok(false);
        }
        if manifest.assets.is_empty() {
            self.assets.remove(&owner);
        } else {
            self.assets.insert(owner, manifest);
        }
        self.revision = self.revision.saturating_add(1);
        self.rebuild_assets()?;
        Ok(true)
    }

    fn rebuild_assets(&mut self) -> Result<()> {
        self.nodes
            .retain(|inode, _| !self.asset_inodes.contains(inode));
        self.asset_inodes.clear();
        if let Some(root) = self.nodes.get_mut(&ROOT_INODE)
            && let VirtualNodeKind::Directory { entries } = &mut root.kind
        {
            entries.retain(|entry| entry.name != ASSETS_DIRECTORY.as_bytes());
            root.generation = self.revision;
        }
        if self.assets.is_empty() {
            return Ok(());
        }
        let root = self.inode_for("assets:root")?;
        let mut owners: Vec<_> = self.assets.keys().copied().collect();
        owners.sort_unstable();
        let mut root_entries = Vec::new();
        for owner in owners {
            let directory = self.inode_for(&format!("assets:directory:{owner}"))?;
            let path = format!("{ASSETS_DIRECTORY}/{owner}");
            let mut entries = Vec::new();
            for spec in self.assets[&owner].assets.clone() {
                let ContentRequest::Whiteboard { token } = &spec.request else {
                    unreachable!("validated asset request");
                };
                let inode = self.inode_for(&format!("assets:file:{owner}:{token}"))?;
                entries.push(DirectoryEntry {
                    name: spec.name.as_bytes().to_vec(),
                    inode,
                    kind: FileType::File,
                });
                self.nodes.insert(
                    inode,
                    VirtualNode {
                        inode,
                        parent: directory,
                        path: join_path(&path, &spec.name),
                        generation: self.revision,
                        created_ns: self.created_ns,
                        kind: VirtualNodeKind::File {
                            request: spec.request,
                        },
                    },
                );
                self.asset_inodes.insert(inode);
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            self.nodes.insert(
                directory,
                VirtualNode {
                    inode: directory,
                    parent: root,
                    path,
                    generation: self.revision,
                    created_ns: self.created_ns,
                    kind: VirtualNodeKind::Directory { entries },
                },
            );
            self.asset_inodes.insert(directory);
            root_entries.push(DirectoryEntry {
                name: owner.to_string().into_bytes(),
                inode: directory,
                kind: FileType::Directory,
            });
        }
        root_entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.nodes.insert(
            root,
            VirtualNode {
                inode: root,
                parent: ROOT_INODE,
                path: ASSETS_DIRECTORY.into(),
                generation: self.revision,
                created_ns: self.created_ns,
                kind: VirtualNodeKind::Directory {
                    entries: root_entries,
                },
            },
        );
        self.asset_inodes.insert(root);
        if let VirtualNodeKind::Directory { entries } =
            &mut self.nodes.get_mut(&ROOT_INODE).expect("root").kind
        {
            entries.push(DirectoryEntry {
                name: ASSETS_DIRECTORY.as_bytes().to_vec(),
                inode: root,
                kind: FileType::Directory,
            });
            entries.sort_by(|a, b| fold_name_bytes(&a.name).cmp(&fold_name_bytes(&b.name)));
        }
        Ok(())
    }

    pub fn asset_paths(&self) -> Vec<String> {
        self.asset_inodes
            .iter()
            .filter_map(|inode| self.nodes.get(inode).map(|node| node.path.clone()))
            .collect()
    }

    pub fn asset_files(&self, owner: u64) -> HashMap<String, u64> {
        self.assets
            .get(&owner)
            .map(|manifest| {
                manifest
                    .assets
                    .iter()
                    .filter_map(|spec| {
                        let ContentRequest::Whiteboard { token } = &spec.request else {
                            return None;
                        };
                        self.inode_by_key
                            .get(&format!("assets:file:{owner}:{token}"))
                            .map(|inode| (spec.name.clone(), *inode))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn inode_for(&mut self, key: &str) -> Result<u64> {
        if let Some(inode) = self.inode_by_key.get(key) {
            return Ok(*inode);
        }
        let inode = self.next_inode;
        if inode == u64::MAX {
            bail!("Inode space exhausted");
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
        let asset_paths = self
            .asset_files(inode)
            .into_iter()
            .filter_map(|(name, inode)| {
                self.nodes.get(&inode).map(|node| (name, node.path.clone()))
            })
            .collect();
        rewrite_document_links_with_assets(
            markdown,
            &node.path,
            &self.content_path_by_token,
            &asset_paths,
        )
        .into_bytes()
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

/// Extension and fetch request for a node's own content file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentSpec {
    /// Extension without the dot.
    pub extension: String,
    pub request: ContentRequest,
}

/// Content of a node, or `None` for nodes that have no document at all
/// (currently only the space root).
#[must_use]
pub fn content_spec(node: &RemoteNode) -> Option<ContentSpec> {
    if node.object_type.as_deref() == Some(crate::remote::SPACE_OBJECT_TYPE) {
        return None;
    }
    let Some(token) = node.object_token.as_ref().filter(|token| !token.is_empty()) else {
        return Some(unsupported_spec(node, "The node has no readable obj_token"));
    };
    let kind = node.object_type.as_deref().map(str::to_ascii_lowercase);
    match kind.as_deref() {
        Some("docx") => Some(ContentSpec {
            extension: "md".into(),
            request: ContentRequest::Markdown {
                token: token.clone(),
                document_type: "docx".into(),
                url: node.url.clone(),
            },
        }),
        Some("doc") => Some(ContentSpec {
            extension: "docx".into(),
            request: ContentRequest::Export {
                token: token.clone(),
                document_type: "doc".into(),
                extension: "docx".into(),
            },
        }),
        Some(kind @ ("sheet" | "bitable")) => Some(ContentSpec {
            extension: "xlsx".into(),
            request: ContentRequest::Export {
                token: token.clone(),
                document_type: kind.to_owned(),
                extension: "xlsx".into(),
            },
        }),
        Some("file") => Some(ContentSpec {
            extension: safe_extension(&node.title),
            request: ContentRequest::Download {
                token: token.clone(),
            },
        }),
        Some(kind) => Some(unsupported_spec(
            node,
            &format!("Unsupported object type: {kind}"),
        )),
        None => Some(unsupported_spec(node, "The node has no obj_type")),
    }
}

/// File name for the content of a node that owns child pages.
fn index_file_name(spec: &ContentSpec) -> String {
    format!("index.{}", spec.extension)
}

/// File name for a node without child pages: the title itself carries the name.
fn leaf_file_name(node: &RemoteNode) -> String {
    let Some(spec) = content_spec(node) else {
        return sanitize_component(&node.title);
    };
    let title = node.title.trim();
    let suffix = format!(".{}", spec.extension.to_ascii_lowercase());
    let named = if title.to_ascii_lowercase().ends_with(&suffix) {
        title.to_owned()
    } else {
        format!("{title}.{}", spec.extension)
    };
    sanitize_component(&named)
}

fn unsupported_spec(node: &RemoteNode, reason: &str) -> ContentSpec {
    ContentSpec {
        extension: "md".into(),
        request: unsupported_request(node, reason),
    }
}

fn unsupported_request(node: &RemoteNode, reason: &str) -> ContentRequest {
    let object_type = node.object_type.as_deref().unwrap_or("unknown");
    let url = node.url.as_deref().unwrap_or("(Feishu returned no URL)");
    let markdown = format!(
        "# {}\n\n> FeishuFS could not export this node.\n\n- Reason: {}\n- Object type: `{}`\n- Cloud URL: {}\n",
        node.title, reason, object_type, url
    );
    ContentRequest::Inline {
        bytes: markdown.into_bytes(),
    }
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
        "Untitled".to_owned()
    } else {
        trimmed.to_owned()
    };

    let stem_end = output.find('.').unwrap_or(output.len());
    if is_windows_reserved(&output[..stem_end]) {
        output.insert(stem_end, '_');
    }
    truncate_utf8(&mut output, MAX_COMPONENT_BYTES);
    if output.is_empty() {
        "Untitled".to_owned()
    } else {
        output
    }
}

/// Disambiguate a name already taken in the same directory.
///
/// When `extension` is given the suffix is inserted before it, so a renamed
/// collision keeps a usable file extension (`guide~two.md`, not `guide.md~two`).
fn unique_component(
    base: &str,
    token: &str,
    used: &mut HashSet<String>,
    extension: Option<&str>,
) -> String {
    if used.insert(fold_name(base)) {
        return base.to_owned();
    }

    let (stem, extension) = match extension {
        Some(extension)
            if base.len() > extension.len() + 1
                && base.as_bytes()[base.len() - extension.len() - 1] == b'.'
                && base[base.len() - extension.len()..].eq_ignore_ascii_case(extension) =>
        {
            (
                &base[..base.len() - extension.len() - 1],
                &base[base.len() - extension.len() - 1..],
            )
        }
        _ => (base, ""),
    };

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
        let max_stem = MAX_COMPONENT_BYTES.saturating_sub(suffix.len() + extension.len());
        let mut candidate_stem = stem.to_owned();
        truncate_utf8(&mut candidate_stem, max_stem);
        let candidate = format!("{candidate_stem}{suffix}{extension}");
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
            anyhow::bail!("Inode {} is a directory, not a file", node.inode)
        }
    }
}

pub(crate) fn directory_entries(node: &VirtualNode) -> Result<&[DirectoryEntry]> {
    match &node.kind {
        VirtualNodeKind::Directory { entries } => Ok(entries),
        VirtualNodeKind::File { .. } => {
            anyhow::bail!("Inode {} is a file, not a directory", node.inode)
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use crate::remote::test_support::{leaf, node};

    use super::*;

    /// Build a space whose top-level nodes are `root_nodes`.
    fn wiki_tree(
        root_nodes: Vec<RemoteNode>,
        children: HashMap<String, Vec<RemoteNode>>,
    ) -> RemoteTree {
        RemoteTree {
            space_id: "space-1".into(),
            title: "Team Wiki".into(),
            root_nodes,
            children,
        }
    }

    fn names_of(state: &FsState, inode: u64) -> Vec<String> {
        directory_entries(&state.nodes[&inode])
            .unwrap()
            .iter()
            .map(|entry| String::from_utf8(entry.name.clone()).unwrap())
            .collect()
    }

    #[test]
    fn sanitizes_cross_platform_names() {
        assert_eq!(sanitize_component(" A<B>:C. "), "A_B__C");
        assert_eq!(sanitize_component("CON"), "CON_");
        assert_eq!(sanitize_component("lpt9.txt"), "lpt9_.txt");
        assert_eq!(sanitize_component("CON.md"), "CON_.md");
        assert_eq!(sanitize_component("..."), "Untitled");
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
        assert_eq!(
            content_spec(&doc),
            Some(ContentSpec {
                extension: "md".into(),
                request: ContentRequest::Markdown {
                    token: "obj-doc".into(),
                    document_type: "docx".into(),
                    url: doc.url.clone(),
                },
            })
        );
        assert_eq!(index_file_name(&content_spec(&doc).unwrap()), "index.md");
        assert_eq!(leaf_file_name(&doc), "Doc.md");

        let file = node("file", "Manual.PDF", "file");
        assert_eq!(leaf_file_name(&file), "Manual.PDF");
        assert_eq!(index_file_name(&content_spec(&file).unwrap()), "index.pdf");

        let sheet = node("sheet", "2024 报表", "sheet");
        assert_eq!(leaf_file_name(&sheet), "2024 报表.xlsx");

        let space = crate::remote::test_support::space_node();
        assert_eq!(content_spec(&space), None);
    }

    #[test]
    fn nodes_without_child_pages_become_sibling_files() {
        let tree = wiki_tree(
            vec![leaf("a", "Alpha", "docx"), leaf("b", "Beta", "file")],
            HashMap::new(),
        );
        let state = FsState::from_remote(&tree, 1).unwrap();

        let entries = directory_entries(&state.nodes[&ROOT_INODE]).unwrap();
        assert_eq!(names_of(&state, ROOT_INODE), ["Alpha.md", "Beta.bin"]);
        assert!(entries.iter().all(|entry| entry.kind == FileType::File));
    }

    #[test]
    fn nodes_with_child_pages_become_directories_with_index() {
        let mut tree = wiki_tree(vec![node("a", "Alpha", "docx")], HashMap::new());
        tree.children
            .insert("a".into(), vec![leaf("child", "Child", "docx")]);
        let state = FsState::from_remote(&tree, 1).unwrap();

        let alpha = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .find(|entry| entry.name == b"Alpha")
            .expect("Alpha should be a directory");
        assert_eq!(alpha.kind, FileType::Directory);

        let entries = directory_entries(&state.nodes[&alpha.inode]).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| String::from_utf8(entry.name.clone()).unwrap())
                .collect::<Vec<_>>(),
            vec!["Child.md".to_owned(), "index.md".to_owned()]
        );
    }

    #[test]
    fn turning_a_leaf_into_a_branch_keeps_the_content_inode() {
        let mut tree = wiki_tree(vec![leaf("a", "Alpha", "docx")], HashMap::new());
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        let file_inode = directory_entries(&state.nodes[&ROOT_INODE]).unwrap()[0].inode;

        tree.root_nodes = vec![node("a", "Alpha", "docx")];
        tree.children
            .insert("a".into(), vec![leaf("child", "Child", "docx")]);
        state.replace(&tree).unwrap();

        let directory = directory_entries(&state.nodes[&ROOT_INODE]).unwrap()[0].clone();
        assert_eq!(directory.kind, FileType::Directory);
        let index = directory_entries(&state.nodes[&directory.inode])
            .unwrap()
            .iter()
            .find(|entry| entry.name == b"index.md")
            .expect("index.md should exist");
        assert_eq!(index.inode, file_inode);
    }

    #[test]
    fn keeps_existing_name_when_a_case_insensitive_duplicate_appears() {
        let first = node("first", "Guide", "docx");
        let mut tree = wiki_tree(
            vec![first.clone()],
            HashMap::from([("first".into(), vec![leaf("kid", "Kid", "docx")])]),
        );
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        let original = directory_entries(&state.nodes[&ROOT_INODE]).unwrap()[0]
            .name
            .clone();
        assert_eq!(original, b"Guide");

        let duplicate = node("second", "guide", "docx");
        tree.root_nodes = vec![first, duplicate];
        tree.children
            .insert("second".into(), vec![leaf("kid2", "Kid", "docx")]);
        state.replace(&tree).unwrap();

        let names = names_of(&state, ROOT_INODE);
        assert!(names.contains(&"Guide".to_owned()));
        assert!(names.iter().any(|name| name.starts_with("guide~")));
    }

    #[test]
    fn cloud_rename_updates_name_without_changing_inode() {
        let child = node("child", "Old name", "docx");
        let mut tree = wiki_tree(
            vec![child.clone()],
            HashMap::from([("child".into(), vec![leaf("kid", "Kid", "docx")])]),
        );
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        let initial = directory_entries(&state.nodes[&ROOT_INODE]).unwrap()[0].clone();
        assert_eq!(initial.name, b"Old name");

        let mut renamed = child;
        renamed.title = "New name".into();
        tree.root_nodes = vec![renamed];
        state.replace(&tree).unwrap();
        let refreshed = directory_entries(&state.nodes[&ROOT_INODE]).unwrap()[0].clone();

        assert_eq!(refreshed.name, b"New name");
        assert_eq!(refreshed.inode, initial.inode);
    }

    #[test]
    fn inodes_survive_refresh_and_are_never_reused() {
        let child = node("child", "Child", "docx");
        let mut tree = wiki_tree(
            vec![child.clone()],
            HashMap::from([("child".into(), vec![leaf("kid", "Kid", "docx")])]),
        );
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        let child_inode = directory_entries(&state.nodes[&ROOT_INODE]).unwrap()[0].inode;

        tree.root_nodes = vec![];
        state.replace(&tree).unwrap();
        assert!(
            directory_entries(&state.nodes[&ROOT_INODE])
                .unwrap()
                .is_empty()
        );

        tree.root_nodes = vec![child];
        state.replace(&tree).unwrap();
        let refreshed_inode = directory_entries(&state.nodes[&ROOT_INODE]).unwrap()[0].inode;
        assert_eq!(child_inode, refreshed_inode);
    }

    #[test]
    fn asset_namespace_is_reserved_without_shadowing_a_remote_directory() {
        let tree = wiki_tree(
            vec![
                leaf("doc", "Doc", "docx"),
                node("reserved", ASSETS_DIRECTORY, "docx"),
            ],
            HashMap::from([("reserved".into(), vec![leaf("kid", "Kid", "docx")])]),
        );
        let mut state = FsState::from_remote(&tree, 1).unwrap();
        assert!(names_of(&state, ROOT_INODE).contains(&format!("{ASSETS_DIRECTORY}~reserved")));
        let owner = state
            .nodes
            .values()
            .find(|node| node.path == "Doc.md")
            .unwrap()
            .inode;
        let request = file_request(&state.nodes[&owner]).unwrap().clone();
        let specs = vec![(
            "62.jpg".into(),
            ContentRequest::Whiteboard { token: "b".into() },
        )];
        state.set_assets(owner, &request, specs).unwrap();
        let names = names_of(&state, ROOT_INODE);
        assert!(names.contains(&ASSETS_DIRECTORY.to_owned()));
        assert!(names.contains(&format!("{ASSETS_DIRECTORY}~reserved")));
        let before = state.paths();
        assert!(
            state
                .set_assets(
                    owner,
                    &request,
                    vec![(
                        "../escape.jpg".into(),
                        ContentRequest::Whiteboard { token: "b".into() }
                    )]
                )
                .is_err()
        );
        assert_eq!(before, state.paths());
        assert!(
            state
                .set_assets(
                    owner,
                    &request,
                    vec![
                        (
                            "62.jpg".into(),
                            ContentRequest::Whiteboard { token: "b".into() }
                        ),
                        (
                            "62.png".into(),
                            ContentRequest::Whiteboard { token: "b".into() }
                        )
                    ]
                )
                .is_err()
        );
    }

    #[test]
    fn links_are_rewritten_to_leaf_and_index_paths() {
        let branch = node("branch", "指南", "docx");
        let tree = wiki_tree(
            vec![branch, leaf("leaf", "API 说明", "docx")],
            HashMap::from([("branch".into(), vec![leaf("kid", "子页", "docx")])]),
        );
        let state = FsState::from_remote(&tree, 1).unwrap();
        let index = directory_entries(&state.nodes[&ROOT_INODE])
            .unwrap()
            .iter()
            .find(|entry| entry.name == "指南".as_bytes())
            .and_then(|entry| {
                directory_entries(&state.nodes[&entry.inode])
                    .unwrap()
                    .iter()
                    .find(|entry| entry.name == b"index.md")
            })
            .map(|entry| entry.inode)
            .expect("The branch index.md should exist");

        let branch_request = file_request(&state.nodes[&index]).unwrap().clone();
        let source = concat!(
            "[说明](https://example.feishu.cn/wiki/leaf)\n",
            "[子页](https://example.feishu.cn/wiki/kid)\n",
        );
        let rendered =
            String::from_utf8(state.render_markdown(index, &branch_request, source.as_bytes()))
                .unwrap();
        assert_eq!(
            rendered,
            concat!("[说明](<../API 说明.md>)\n", "[子页](<子页.md>)\n",)
        );
    }
}

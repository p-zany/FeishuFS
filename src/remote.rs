use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::link::KnowledgeLink;

/// `object_type` used for the synthetic space root.
pub(crate) const SPACE_OBJECT_TYPE: &str = "space";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteNode {
    pub space_id: String,
    pub node_token: String,
    pub object_token: Option<String>,
    pub object_type: Option<String>,
    pub title: String,
    pub url: Option<String>,
    /// Whether the node has child nodes. Used to skip listing leaves.
    pub has_child: bool,
}

/// The space that a document link belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteSpace {
    pub space_id: String,
    pub name: String,
}

/// One worksheet in a sheet or one data table in a bitable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteSubtable {
    pub id: String,
    pub name: String,
}

impl RemoteNode {
    #[must_use]
    pub fn is_table(&self) -> bool {
        self.object_type.as_deref().is_some_and(|kind| {
            kind.eq_ignore_ascii_case("sheet") || kind.eq_ignore_ascii_case("bitable")
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentRequest {
    Markdown {
        token: String,
        document_type: String,
        /// Original tenant URL used when turning structured references into links.
        url: Option<String>,
    },
    Download {
        token: String,
    },
    Whiteboard {
        token: String,
    },
    Export {
        token: String,
        document_type: String,
        extension: String,
    },
    /// CSV always identifies exactly one worksheet or data table.
    TableCsv {
        token: String,
        document_type: String,
        sub_id: String,
    },
    Inline {
        bytes: Vec<u8>,
    },
}

impl ContentRequest {
    #[must_use]
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline { .. })
    }

    #[must_use]
    pub fn is_markdown(&self) -> bool {
        matches!(self, Self::Markdown { .. })
    }
}

/// A successful content fetch and its associated resources. This is not a
/// transaction/recovery contract; failure semantics remain a design placeholder.
#[derive(Clone, Debug, Default)]
pub struct ContentBundle {
    pub bytes: Vec<u8>,
    pub assets: Vec<ContentAsset>,
}

impl From<Vec<u8>> for ContentBundle {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            assets: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContentAsset {
    pub name: String,
    pub request: ContentRequest,
    pub bytes: Vec<u8>,
}

pub trait RemoteSource: Send + Sync + 'static {
    /// Resolve the space that owns the document behind `link`.
    fn resolve_space(&self, link: &KnowledgeLink) -> Result<RemoteSpace>;
    /// Refresh metadata for an already selected space, without resolving an entry link.
    fn refresh_space(&self, space: &RemoteSpace) -> Result<RemoteSpace> {
        Ok(space.clone())
    }
    /// List the direct children of `parent`; implementations may skip the
    /// request when `parent.has_child` is false.
    fn list_children(&self, space_id: &str, parent: &RemoteNode) -> Result<Vec<RemoteNode>>;
    /// Read only subtable metadata, never export content during traversal.
    fn list_subtables(&self, _node: &RemoteNode) -> Result<Vec<RemoteSubtable>> {
        bail!("Subtable listing is not supported by this remote source");
    }
    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>>;
    fn fetch_bundle(&self, request: &ContentRequest) -> Result<ContentBundle> {
        self.fetch_content(request).map(ContentBundle::from)
    }
}

/// A whole space, mounted as the filesystem root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteTree {
    pub space_id: String,
    pub title: String,
    /// Top-level nodes of the space (the children of the space root).
    pub root_nodes: Vec<RemoteNode>,
    /// Children keyed by parent `node_token`.
    pub children: HashMap<String, Vec<RemoteNode>>,
    /// Complete subtable manifests keyed by the owning wiki node token.
    pub subtables: HashMap<String, Vec<RemoteSubtable>>,
}

impl RemoteTree {
    pub fn load(source: &dyn RemoteSource, link: &KnowledgeLink, max_nodes: usize) -> Result<Self> {
        let space = source
            .resolve_space(link)
            .with_context(|| format!("Failed to resolve the space for {}", link.url))?;
        Self::load_space(source, &space, link, max_nodes)
    }

    /// Reload the selected space. The entry URL only supplies the tenant origin.
    pub fn load_space(
        source: &dyn RemoteSource,
        selected: &RemoteSpace,
        link: &KnowledgeLink,
        max_nodes: usize,
    ) -> Result<Self> {
        if max_nodes == 0 {
            bail!("max_nodes must be greater than zero");
        }
        if selected.space_id.trim().is_empty() {
            bail!("Space response is missing space_id");
        }
        let space = source.refresh_space(selected)?;
        if space.space_id != selected.space_id {
            bail!("Space metadata must not change the selected space_id");
        }
        let mut tree = Self {
            space_id: space.space_id.clone(),
            title: space.name,
            root_nodes: Vec::new(),
            children: HashMap::new(),
            subtables: HashMap::new(),
        };

        let root = RemoteNode {
            space_id: space.space_id.clone(),
            node_token: space.space_id.clone(),
            object_token: None,
            object_type: Some(SPACE_OBJECT_TYPE.to_owned()),
            title: tree.title.clone(),
            url: None,
            has_child: true,
        };

        let mut visited = HashSet::new();
        let mut queue = VecDeque::from([root]);
        while let Some(parent) = queue.pop_front() {
            let mut page = if parent.has_child {
                source
                    .list_children(&tree.space_id, &parent)
                    .with_context(|| {
                        format!("Failed to list children of node {}", parent.node_token)
                    })?
            } else {
                Vec::new()
            };
            page.sort_by(|left, right| {
                left.title
                    .to_lowercase()
                    .cmp(&right.title.to_lowercase())
                    .then_with(|| left.node_token.cmp(&right.node_token))
            });

            let mut unique_children = Vec::with_capacity(page.len());
            for mut child in page {
                if let Err(error) = validate_node(&child) {
                    warn!(
                        parent = %parent.node_token,
                        node = %child.node_token,
                        %error,
                        "Ignoring an incomplete space node"
                    );
                    continue;
                }
                if child.space_id != tree.space_id {
                    warn!(
                        expected_space = %tree.space_id,
                        actual_space = %child.space_id,
                        node = %child.node_token,
                        "Ignoring a node from another space"
                    );
                    continue;
                }
                if !visited.insert(child.node_token.clone()) {
                    warn!(
                        parent = %parent.node_token,
                        node = %child.node_token,
                        "Ignoring a duplicate or cyclic space node"
                    );
                    continue;
                }
                if visited.len() > max_nodes {
                    bail!("Space node count exceeds limit {max_nodes}; increase --max-nodes");
                }
                // Node lists often omit URLs. Preserve the entry link's tenant
                // for structured references to documents outside this mount too.
                if child.url.as_deref().is_none_or(|url| url.trim().is_empty()) {
                    child.url = Some(format!(
                        "{}/wiki/{}",
                        link.url.origin().ascii_serialization(),
                        child.node_token
                    ));
                }
                if child.is_table()
                    && child
                        .object_token
                        .as_deref()
                        .is_some_and(|token| !token.is_empty())
                {
                    let mut subtables = source.list_subtables(&child).with_context(|| {
                        format!("Failed to list subtables of node {}", child.node_token)
                    })?;
                    let mut ids = HashSet::new();
                    for table in &mut subtables {
                        if !crate::docx::valid_token(&table.id) || !ids.insert(table.id.clone()) {
                            bail!(
                                "Invalid or duplicate subtable ID for node {}",
                                child.node_token
                            );
                        }
                        if table.name.trim().is_empty() {
                            table.name = format!("Untitled-{}", table.id);
                        }
                    }
                    // API order is not part of the snapshot identity. IDs also
                    // provide a stable default target for links without a selector.
                    subtables.sort_by(|a, b| a.id.cmp(&b.id));
                    tree.subtables.insert(child.node_token.clone(), subtables);
                }
                if child.has_child {
                    queue.push_back(child.clone());
                }
                unique_children.push(child);
            }

            if parent.object_type.as_deref() == Some(SPACE_OBJECT_TYPE) {
                tree.root_nodes = unique_children;
            } else {
                tree.children.insert(parent.node_token, unique_children);
            }
        }

        Ok(tree)
    }

    /// Direct children of a node; the space root returns its top-level nodes.
    #[must_use]
    pub fn children_of(&self, node_token: &str) -> &[RemoteNode] {
        if node_token == self.space_id {
            return &self.root_nodes;
        }
        self.children
            .get(node_token)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// A node is mounted as a directory exactly when it has child pages.
    #[must_use]
    pub fn has_children(&self, node_token: &str) -> bool {
        !self.children_of(node_token).is_empty()
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.children.values().map(Vec::len).sum::<usize>() + self.root_nodes.len()
    }
}

fn validate_node(node: &RemoteNode) -> Result<()> {
    if node.space_id.trim().is_empty() {
        bail!("space_id is empty");
    }
    if node.node_token.trim().is_empty() {
        bail!("node_token is empty");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    type ContentResult = Result<Vec<u8>, String>;
    type ContentMap = HashMap<String, ContentResult>;
    type SubtableMap = HashMap<String, Result<Vec<RemoteSubtable>, String>>;

    #[derive(Clone, Default)]
    pub struct FakeSource {
        pub root: Arc<Mutex<Option<RemoteNode>>>,
        pub children: Arc<Mutex<HashMap<String, Vec<RemoteNode>>>>,
        pub content: Arc<Mutex<ContentMap>>,
        pub fetches: Arc<Mutex<Vec<String>>>,
        pub bundles: Arc<Mutex<HashMap<String, ContentBundle>>>,
        pub subtables: Arc<Mutex<SubtableMap>>,
        pub subtable_lists: Arc<Mutex<Vec<String>>>,
    }

    impl FakeSource {
        pub fn with_root(root: RemoteNode) -> Self {
            Self {
                root: Arc::new(Mutex::new(Some(root))),
                ..Self::default()
            }
        }
    }

    impl RemoteSource for FakeSource {
        fn resolve_space(&self, _link: &KnowledgeLink) -> Result<RemoteSpace> {
            self.root
                .lock()
                .clone()
                .map(|root| RemoteSpace {
                    space_id: root.space_id,
                    name: root.title,
                })
                .context("fake space is not configured")
        }

        fn list_children(&self, _space_id: &str, parent: &RemoteNode) -> Result<Vec<RemoteNode>> {
            Ok(self
                .children
                .lock()
                .get(&parent.node_token)
                .cloned()
                .unwrap_or_default())
        }

        fn list_subtables(&self, node: &RemoteNode) -> Result<Vec<RemoteSubtable>> {
            let token = node.object_token.as_ref().context("missing table token")?;
            self.subtable_lists.lock().push(token.clone());
            match self.subtables.lock().get(token).cloned() {
                Some(Ok(tables)) => Ok(tables),
                Some(Err(message)) => bail!(message),
                None => bail!("fake subtables not configured for {token}"),
            }
        }

        fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
            let key = match request {
                ContentRequest::TableCsv { token, sub_id, .. } => format!("{token}/{sub_id}"),
                ContentRequest::Markdown { token, .. }
                | ContentRequest::Download { token }
                | ContentRequest::Whiteboard { token }
                | ContentRequest::Export { token, .. } => token.clone(),
                ContentRequest::Inline { bytes } => return Ok(bytes.clone()),
            };
            self.fetches.lock().push(key.clone());
            match self.content.lock().get(&key).cloned() {
                Some(Ok(bytes)) => Ok(bytes),
                Some(Err(message)) => bail!(message),
                None => bail!("fake content not configured for {key}"),
            }
        }

        fn fetch_bundle(&self, request: &ContentRequest) -> Result<ContentBundle> {
            if let ContentRequest::Markdown { token, .. } = request
                && let Some(bundle) = self.bundles.lock().get(token).cloned()
            {
                self.fetches.lock().push(token.clone());
                return Ok(bundle);
            }
            self.fetch_content(request).map(ContentBundle::from)
        }
    }

    /// A document that may have child pages, so it is listed for children.
    pub fn node(token: &str, title: &str, object_type: &str) -> RemoteNode {
        remote_node(token, title, object_type, true)
    }

    /// A document the API reports as childless; its children are never listed.
    pub fn leaf(token: &str, title: &str, object_type: &str) -> RemoteNode {
        remote_node(token, title, object_type, false)
    }

    /// The synthetic space root, which has no content of its own.
    pub fn space_node() -> RemoteNode {
        RemoteNode {
            space_id: "space-1".into(),
            node_token: "space-1".into(),
            object_token: None,
            object_type: Some(crate::remote::SPACE_OBJECT_TYPE.into()),
            title: "Team Wiki".into(),
            url: None,
            has_child: true,
        }
    }

    fn remote_node(token: &str, title: &str, object_type: &str, has_child: bool) -> RemoteNode {
        RemoteNode {
            space_id: "space-1".into(),
            node_token: token.into(),
            object_token: Some(format!("obj-{token}")),
            object_type: Some(object_type.into()),
            title: title.into(),
            url: Some(format!("https://example.feishu.cn/wiki/{token}")),
            has_child,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeSource, leaf, node};
    use super::*;

    fn link() -> KnowledgeLink {
        "https://example.feishu.cn/wiki/root".parse().unwrap()
    }

    #[test]
    fn loads_the_whole_space_from_one_document_link() {
        let source = FakeSource::with_root(node("root", "Team Wiki", "docx"));
        source.children.lock().insert(
            "space-1".into(),
            vec![node("b", "Zulu", "docx"), node("a", "Alpha", "docx")],
        );
        source
            .children
            .lock()
            .insert("a".into(), vec![leaf("leaf", "Leaf", "file")]);

        let tree = RemoteTree::load(&source, &link(), 10).unwrap();
        assert_eq!(tree.space_id, "space-1");
        assert_eq!(tree.title, "Team Wiki");
        assert_eq!(tree.node_count(), 3);
        assert_eq!(tree.root_nodes[0].node_token, "a");
        assert_eq!(tree.root_nodes[1].node_token, "b");
        assert_eq!(tree.children["a"][0].node_token, "leaf");
        assert!(tree.has_children("a"));
        assert!(!tree.has_children("leaf"));
    }

    #[test]
    fn skips_listing_nodes_reported_as_childless() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        source
            .children
            .lock()
            .insert("space-1".into(), vec![leaf("leaf", "Leaf", "docx")]);
        // Even if the tree map holds children, a childless node must not be listed.
        source
            .children
            .lock()
            .insert("leaf".into(), vec![leaf("hidden", "Hidden", "docx")]);

        let tree = RemoteTree::load(&source, &link(), 10).unwrap();
        assert_eq!(tree.node_count(), 1);
        assert!(!tree.has_children("leaf"));
    }

    #[test]
    fn layout_follows_the_visible_snapshot_not_the_has_child_flag() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        // The API claims children, but permission filtering returns none.
        source
            .children
            .lock()
            .insert("space-1".into(), vec![node("a", "A", "docx")]);
        source.children.lock().insert("a".into(), vec![]);

        let tree = RemoteTree::load(&source, &link(), 10).unwrap();
        assert!(tree.root_nodes[0].has_child);
        assert!(!tree.has_children("a"));
    }

    #[test]
    fn missing_node_urls_use_the_entry_links_tenant() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        let mut child = leaf("a", "A", "docx");
        child.url = None;
        source.children.lock().insert("space-1".into(), vec![child]);
        let entry = "https://tenant.larksuite.com/wiki/root".parse().unwrap();
        let tree = RemoteTree::load(&source, &entry, 10).unwrap();
        assert_eq!(
            tree.root_nodes[0].url.as_deref(),
            Some("https://tenant.larksuite.com/wiki/a")
        );
    }

    #[test]
    fn invalid_or_duplicate_subtable_ids_reject_the_entire_snapshot() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        source
            .children
            .lock()
            .insert("space-1".into(), vec![leaf("table", "Table", "sheet")]);
        for ids in [vec![""], vec!["../escape"], vec!["a", "a"]] {
            source.subtables.lock().insert(
                "obj-table".into(),
                Ok(ids
                    .into_iter()
                    .map(|id| RemoteSubtable {
                        id: id.into(),
                        name: "Name".into(),
                    })
                    .collect()),
            );
            assert!(RemoteTree::load(&source, &link(), 10).is_err());
        }
    }

    #[test]
    fn unnamed_subtables_use_their_stable_id_without_exporting_content() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        source
            .children
            .lock()
            .insert("space-1".into(), vec![leaf("table", "Table", "bitable")]);
        source.subtables.lock().insert(
            "obj-table".into(),
            Ok(vec![RemoteSubtable {
                id: "tbl1".into(),
                name: "  ".into(),
            }]),
        );
        let tree = RemoteTree::load(&source, &link(), 10).unwrap();
        assert_eq!(tree.subtables["table"][0].name, "Untitled-tbl1");
        assert!(source.fetches.lock().is_empty());
    }

    #[test]
    fn rejects_trees_over_limit() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        source.children.lock().insert(
            "space-1".into(),
            vec![node("a", "A", "docx"), node("b", "B", "docx")],
        );

        let error = RemoteTree::load(&source, &link(), 1).unwrap_err();
        assert!(error.to_string().contains("exceeds limit"));
    }
}

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::link::KnowledgeLink;

/// `object_type` used for the synthetic knowledge-space root.
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

/// The knowledge space that a document link belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteSpace {
    pub space_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentRequest {
    Markdown {
        token: String,
        document_type: String,
    },
    Download {
        token: String,
    },
    Export {
        token: String,
        document_type: String,
        extension: String,
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

pub trait RemoteSource: Send + Sync + 'static {
    /// Resolve the knowledge space that owns the document behind `link`.
    fn resolve_space(&self, link: &KnowledgeLink) -> Result<RemoteSpace>;
    /// List the direct children of `parent`; implementations may skip the
    /// request when `parent.has_child` is false.
    fn list_children(&self, space_id: &str, parent: &RemoteNode) -> Result<Vec<RemoteNode>>;
    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>>;
}

/// A whole knowledge space, mounted as the filesystem root.
#[derive(Clone, Debug)]
pub struct RemoteTree {
    pub space_id: String,
    pub title: String,
    /// Top-level nodes of the space (the children of the space root).
    pub root_nodes: Vec<RemoteNode>,
    /// Children keyed by parent `node_token`.
    pub children: HashMap<String, Vec<RemoteNode>>,
}

impl RemoteTree {
    pub fn load(source: &dyn RemoteSource, link: &KnowledgeLink, max_nodes: usize) -> Result<Self> {
        if max_nodes == 0 {
            bail!("max_nodes 必须大于 0");
        }

        let space = source
            .resolve_space(link)
            .with_context(|| format!("无法定位文档 {} 所属的知识库空间", link.url))?;
        if space.space_id.trim().is_empty() {
            bail!("知识库空间响应缺少 space_id");
        }

        let mut tree = Self {
            space_id: space.space_id.clone(),
            title: space.name,
            root_nodes: Vec::new(),
            children: HashMap::new(),
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
                    .with_context(|| format!("无法读取节点 {} 的子节点", parent.node_token))?
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
            for child in page {
                if let Err(error) = validate_node(&child) {
                    warn!(
                        parent = %parent.node_token,
                        node = %child.node_token,
                        %error,
                        "忽略字段不完整的知识库节点"
                    );
                    continue;
                }
                if child.space_id != tree.space_id {
                    warn!(
                        expected_space = %tree.space_id,
                        actual_space = %child.space_id,
                        node = %child.node_token,
                        "忽略跨知识空间节点"
                    );
                    continue;
                }
                if !visited.insert(child.node_token.clone()) {
                    warn!(
                        parent = %parent.node_token,
                        node = %child.node_token,
                        "忽略重复或循环知识库节点"
                    );
                    continue;
                }
                if visited.len() > max_nodes {
                    bail!("知识库节点数超过上限 {max_nodes}；可通过 --max-nodes 调高限制");
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
        bail!("space_id 为空");
    }
    if node.node_token.trim().is_empty() {
        bail!("node_token 为空");
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

    #[derive(Clone, Default)]
    pub struct FakeSource {
        pub root: Arc<Mutex<Option<RemoteNode>>>,
        pub children: Arc<Mutex<HashMap<String, Vec<RemoteNode>>>>,
        pub content: Arc<Mutex<ContentMap>>,
        pub fetches: Arc<Mutex<Vec<String>>>,
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

        fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
            let key = match request {
                ContentRequest::Markdown { token, .. }
                | ContentRequest::Download { token }
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
    }

    /// A document that may have child pages, so it is listed for children.
    pub fn node(token: &str, title: &str, object_type: &str) -> RemoteNode {
        remote_node(token, title, object_type, true)
    }

    /// A document the API reports as childless; its children are never listed.
    pub fn leaf(token: &str, title: &str, object_type: &str) -> RemoteNode {
        remote_node(token, title, object_type, false)
    }

    /// The synthetic knowledge-space root, which has no content of its own.
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
    fn rejects_trees_over_limit() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        source.children.lock().insert(
            "space-1".into(),
            vec![node("a", "A", "docx"), node("b", "B", "docx")],
        );

        let error = RemoteTree::load(&source, &link(), 1).unwrap_err();
        assert!(error.to_string().contains("超过上限"));
    }
}

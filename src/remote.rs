use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::link::KnowledgeLink;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteNode {
    pub space_id: String,
    pub node_token: String,
    pub object_token: Option<String>,
    pub object_type: Option<String>,
    pub title: String,
    pub url: Option<String>,
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
    fn resolve_root(&self, link: &KnowledgeLink) -> Result<RemoteNode>;
    fn list_children(&self, space_id: &str, parent_node_token: &str) -> Result<Vec<RemoteNode>>;
    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>>;
}

#[derive(Clone, Debug)]
pub struct RemoteTree {
    pub root: RemoteNode,
    pub children: HashMap<String, Vec<RemoteNode>>,
}

impl RemoteTree {
    pub fn load(source: &dyn RemoteSource, link: &KnowledgeLink, max_nodes: usize) -> Result<Self> {
        if max_nodes == 0 {
            bail!("max_nodes 必须大于 0");
        }

        let root = source
            .resolve_root(link)
            .with_context(|| format!("无法解析知识库根节点 {}", link.url))?;
        validate_node(&root).context("根节点响应不完整")?;

        let mut children = HashMap::new();
        let mut visited = HashSet::from([root.node_token.clone()]);
        let mut queue = VecDeque::from([root.node_token.clone()]);

        while let Some(parent_token) = queue.pop_front() {
            let mut page = source
                .list_children(&root.space_id, &parent_token)
                .with_context(|| format!("无法读取节点 {parent_token} 的子节点"))?;
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
                        parent = %parent_token,
                        node = %child.node_token,
                        %error,
                        "忽略字段不完整的知识库节点"
                    );
                    continue;
                }
                if child.space_id != root.space_id {
                    warn!(
                        expected_space = %root.space_id,
                        actual_space = %child.space_id,
                        node = %child.node_token,
                        "忽略跨知识空间节点"
                    );
                    continue;
                }
                if !visited.insert(child.node_token.clone()) {
                    warn!(
                        parent = %parent_token,
                        node = %child.node_token,
                        "忽略重复或循环知识库节点"
                    );
                    continue;
                }
                if visited.len() > max_nodes {
                    bail!("知识库节点数超过上限 {max_nodes}；可通过 --max-nodes 调高限制");
                }
                queue.push_back(child.node_token.clone());
                unique_children.push(child);
            }
            children.insert(parent_token, unique_children);
        }

        Ok(Self { root, children })
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        1 + self.children.values().map(Vec::len).sum::<usize>()
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
        fn resolve_root(&self, _link: &KnowledgeLink) -> Result<RemoteNode> {
            self.root
                .lock()
                .clone()
                .context("fake root is not configured")
        }

        fn list_children(
            &self,
            _space_id: &str,
            parent_node_token: &str,
        ) -> Result<Vec<RemoteNode>> {
            Ok(self
                .children
                .lock()
                .get(parent_node_token)
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

    pub fn node(token: &str, title: &str, object_type: &str) -> RemoteNode {
        RemoteNode {
            space_id: "space-1".into(),
            node_token: token.into(),
            object_token: Some(format!("obj-{token}")),
            object_type: Some(object_type.into()),
            title: title.into(),
            url: Some(format!("https://example.feishu.cn/wiki/{token}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeSource, node};
    use super::*;

    fn link() -> KnowledgeLink {
        "https://example.feishu.cn/wiki/root".parse().unwrap()
    }

    #[test]
    fn recursively_loads_and_sorts_tree() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        source.children.lock().insert(
            "root".into(),
            vec![node("b", "Zulu", "docx"), node("a", "Alpha", "docx")],
        );
        source
            .children
            .lock()
            .insert("a".into(), vec![node("leaf", "Leaf", "file")]);

        let tree = RemoteTree::load(&source, &link(), 10).unwrap();
        assert_eq!(tree.node_count(), 4);
        assert_eq!(tree.children["root"][0].node_token, "a");
        assert_eq!(tree.children["a"][0].node_token, "leaf");
    }

    #[test]
    fn rejects_trees_over_limit() {
        let source = FakeSource::with_root(node("root", "Root", "docx"));
        source.children.lock().insert(
            "root".into(),
            vec![node("a", "A", "docx"), node("b", "B", "docx")],
        );

        let error = RemoteTree::load(&source, &link(), 2).unwrap_err();
        assert!(error.to_string().contains("超过上限"));
    }
}

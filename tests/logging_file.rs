//! The log file must carry the full error chain; the console stays short.
//!
//! This is its own integration binary because `logging::init` installs a global
//! subscriber, which can only happen once per process.

use fsk::Filesystem as _;
use std::{env, fs, sync::Arc};

use anyhow::{Result, bail};
use feishufs::{
    ContentRequest, FeishuFs, FsOptions, KnowledgeLink, RemoteNode, RemoteSource, RemoteSpace,
    logging,
};

struct FailingSource;

impl RemoteSource for FailingSource {
    fn resolve_space(&self, _link: &KnowledgeLink) -> Result<RemoteSpace> {
        Ok(RemoteSpace {
            space_id: "space-1".into(),
            name: "Team Wiki".into(),
        })
    }

    fn list_children(&self, _space_id: &str, _parent: &RemoteNode) -> Result<Vec<RemoteNode>> {
        Ok(vec![RemoteNode {
            space_id: "space-1".into(),
            node_token: "doc".into(),
            object_token: Some("obj-doc".into()),
            object_type: Some("docx".into()),
            title: "Doc".into(),
            url: Some("https://example.feishu.cn/wiki/doc".into()),
            has_child: false,
        }])
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        match request {
            ContentRequest::Inline { bytes } => Ok(bytes.clone()),
            _ => bail!("API错误 131006 response: permission denied, tenant needs read permission."),
        }
    }
}

#[test]
fn log_file_records_the_feishu_error_chain() {
    // SAFETY: this is the only test in this binary, so nothing else is running
    // yet; removing RUST_LOG keeps the level assertions below deterministic.
    unsafe { env::remove_var("RUST_LOG") };

    let temporary = tempfile::tempdir().unwrap();
    let log_path = temporary.path().join("feishufs.log");
    let active = logging::init(0, Some(&log_path)).expect("log file should open");
    assert_eq!(
        active,
        fs::canonicalize(temporary.path())
            .unwrap()
            .join("feishufs.log")
    );

    let filesystem = FeishuFs::connect(
        Arc::new(FailingSource),
        "https://example.feishu.cn/wiki/doc".parse().unwrap(),
        FsOptions {
            cache_ttl: std::time::Duration::from_secs(60),
            max_nodes: 10,
        },
    )
    .unwrap();

    // Reading metadata forces a content fetch, which fails.
    let (inode, _) = filesystem.lookup(fsk::ROOT_INODE, b"Doc.md").unwrap();
    assert!(filesystem.metadata(inode).is_err());

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("内容首次获取失败"),
        "the concise line must be present:\n{log}"
    );
    assert!(
        log.contains("131006"),
        "the log file must contain the Feishu error code:\n{log}"
    );
    assert!(
        log.contains("permission denied"),
        "the log file must contain the Feishu response body:\n{log}"
    );
    assert!(
        log.contains("DEBUG"),
        "the log file must keep the detailed line:\n{log}"
    );
}

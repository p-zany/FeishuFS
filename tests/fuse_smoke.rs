#![cfg(target_os = "linux")]

use std::{fs, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use feishufs::{ContentRequest, FeishuFs, FsOptions, KnowledgeLink, RemoteNode, RemoteSource};
use fsk::{MountOptions, MountSession};

struct SmokeSource;

impl RemoteSource for SmokeSource {
    fn resolve_root(&self, _link: &KnowledgeLink) -> Result<RemoteNode> {
        Ok(node("root", "Root"))
    }

    fn list_children(&self, _space_id: &str, parent_node_token: &str) -> Result<Vec<RemoteNode>> {
        Ok(match parent_node_token {
            "root" => vec![node("child", "Child")],
            _ => Vec::new(),
        })
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        match request {
            ContentRequest::Markdown { token, .. } if token == "obj-root" => {
                Ok(b"# Root\n\nSee [child](https://smoke.feishu.cn/wiki/child).\n".to_vec())
            }
            ContentRequest::Markdown { token, .. } if token == "obj-child" => {
                Ok(b"# Child\n".to_vec())
            }
            ContentRequest::Inline { bytes } => Ok(bytes.clone()),
            _ => bail!("unexpected smoke-test content request: {request:?}"),
        }
    }
}

fn node(token: &str, title: &str) -> RemoteNode {
    RemoteNode {
        space_id: "space-smoke".into(),
        node_token: token.into(),
        object_token: Some(format!("obj-{token}")),
        object_type: Some("docx".into()),
        title: title.into(),
        url: Some(format!("https://smoke.feishu.cn/wiki/{token}")),
    }
}

#[test]
#[ignore = "requires a usable /dev/fuse device"]
fn mounts_reads_and_rejects_writes() {
    assert!(
        std::path::Path::new("/dev/fuse").exists(),
        "/dev/fuse is unavailable"
    );

    let link = "https://smoke.feishu.cn/wiki/root".parse().unwrap();
    let filesystem = FeishuFs::connect(
        Arc::new(SmokeSource),
        link,
        FsOptions {
            cache_ttl: Duration::from_secs(60),
            max_nodes: 10,
        },
    )
    .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let mountpoint = temporary.path().join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let options = MountOptions {
        read_only: true,
        filesystem_name: "feishufs-smoke".into(),
        metadata_ttl: Duration::from_millis(10),
        ..MountOptions::default()
    };
    let session = MountSession::mount_network_with_options(filesystem, &mountpoint, &options)
        .expect("failed to establish smoke-test FUSE mount");

    assert_eq!(
        fs::read_to_string(mountpoint.join("index.md")).unwrap(),
        "# Root\n\nSee [child](<Child/index.md>).\n"
    );
    assert_eq!(
        fs::read_to_string(mountpoint.join("Child/index.md")).unwrap(),
        "# Child\n"
    );
    assert!(fs::write(mountpoint.join("index.md"), "changed").is_err());
    assert!(fs::write(mountpoint.join("new.md"), "new").is_err());

    session.unmount();
}

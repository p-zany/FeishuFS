#![cfg(target_os = "linux")]

use std::{fs, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use feishufs::{
    ContentRequest, FeishuFs, FsOptions, KnowledgeLink, RemoteNode, RemoteSource, RemoteSpace,
};
use fsk::{MountOptions, MountSession};

/// A fake knowledge space reached through a document link.
struct SmokeSource;

impl RemoteSource for SmokeSource {
    fn resolve_space(&self, _link: &KnowledgeLink) -> Result<RemoteSpace> {
        Ok(RemoteSpace {
            space_id: "space-smoke".into(),
            name: "Root".into(),
        })
    }

    fn list_children(&self, _space_id: &str, parent: &RemoteNode) -> Result<Vec<RemoteNode>> {
        if !parent.has_child {
            return Ok(Vec::new());
        }
        Ok(match parent.node_token.as_str() {
            // A document with child pages becomes a directory with index.md.
            "space-smoke" => vec![node("root", "Root")],
            "root" => vec![node("child", "Child")],
            "child" => vec![leaf("leaf", "Leaf")],
            _ => Vec::new(),
        })
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        match request {
            ContentRequest::Markdown { token, .. } if token == "obj-root" => {
                Ok(b"# Root\n\nSee [leaf](https://smoke.feishu.cn/wiki/leaf).\n".to_vec())
            }
            ContentRequest::Markdown { token, .. } if token == "obj-child" => {
                Ok(b"# Child\n".to_vec())
            }
            ContentRequest::Markdown { token, .. } if token == "obj-leaf" => {
                Ok(b"# Leaf\n".to_vec())
            }
            ContentRequest::Inline { bytes } => Ok(bytes.clone()),
            _ => bail!("unexpected smoke-test content request: {request:?}"),
        }
    }
}

fn node(token: &str, title: &str) -> RemoteNode {
    remote_node(token, title, true)
}

fn leaf(token: &str, title: &str) -> RemoteNode {
    remote_node(token, title, false)
}

fn remote_node(token: &str, title: &str, has_child: bool) -> RemoteNode {
    RemoteNode {
        space_id: "space-smoke".into(),
        node_token: token.into(),
        object_token: Some(format!("obj-{token}")),
        object_type: Some("docx".into()),
        title: title.into(),
        url: Some(format!("https://smoke.feishu.cn/wiki/{token}")),
        has_child,
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

    // The mount root is the space: no index of its own, child pages as trees.
    assert_eq!(
        fs::read_to_string(mountpoint.join("Root/index.md")).unwrap(),
        "# Root\n\nSee [leaf](<Child/Leaf.md>).\n"
    );
    assert_eq!(
        fs::read_to_string(mountpoint.join("Root/Child/index.md")).unwrap(),
        "# Child\n"
    );
    assert_eq!(
        fs::read_to_string(mountpoint.join("Root/Child/Leaf.md")).unwrap(),
        "# Leaf\n"
    );
    assert!(fs::write(mountpoint.join("Root/index.md"), "changed").is_err());
    assert!(fs::write(mountpoint.join("new.md"), "new").is_err());

    session.unmount();
}

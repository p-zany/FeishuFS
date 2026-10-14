#![cfg(target_os = "linux")]

use std::{fs, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use feishufs::{
    ContentAsset, ContentBundle, ContentRequest, FeishuFs, FsOptions, KnowledgeLink, RemoteNode,
    RemoteSource, RemoteSpace, RemoteSubtable,
};
use fsk::{Filesystem, MountOptions, MountSession};

/// A fake space reached through a document link.
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
            "space-smoke" => vec![
                node("root", "Root"),
                table_node("manual", "Manual.pdf", "file", false),
                table_node("sheet", "报表", "sheet", false),
                table_node("base", "数据", "bitable", true),
            ],
            "root" => vec![node("child", "Child")],
            "child" => vec![leaf("leaf", "Leaf")],
            "base" => vec![leaf("note", "说明")],
            _ => Vec::new(),
        })
    }

    fn list_subtables(&self, _node: &RemoteNode) -> Result<Vec<RemoteSubtable>> {
        Ok(vec![
            RemoteSubtable {
                id: "a".into(),
                name: "收入".into(),
            },
            RemoteSubtable {
                id: "b".into(),
                name: "支出".into(),
            },
        ])
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        match request {
            ContentRequest::Markdown { token, .. } if token == "obj-root" => {
                Ok(b"# Root\n\nSee [leaf](https://smoke.feishu.cn/wiki/leaf).\n[Manual](https://smoke.feishu.cn/file/obj-manual).\n".to_vec())
            }
            ContentRequest::Markdown { token, .. } if token == "obj-child" => {
                Ok(b"# Child\n".to_vec())
            }
            ContentRequest::Markdown { token, .. } if token == "obj-leaf" => {
                Ok(b"# Leaf\n".to_vec())
            }
            ContentRequest::Markdown { token, .. } if token == "obj-note" => {
                Ok(b"# Note\n".to_vec())
            }
            ContentRequest::TableCsv { sub_id, .. } => {
                Ok(format!("name,value,PDF\r\n{sub_id},10,https://smoke.feishu.cn/file/obj-manual\r\n").into_bytes())
            }
            ContentRequest::Download { token } if token == "obj-manual" => {
                Ok(b"%PDF-1.7\n\0manual".to_vec())
            }
            ContentRequest::Whiteboard { .. } => Ok(b"\xff\xd8\xffimage".to_vec()),
            ContentRequest::Inline { bytes } => Ok(bytes.clone()),
            _ => bail!("unexpected smoke-test content request: {request:?}"),
        }
    }

    fn fetch_bundle(&self, request: &ContentRequest) -> Result<ContentBundle> {
        let mut bundle = ContentBundle::from(self.fetch_content(request)?);
        if matches!(request, ContentRequest::Markdown { token, .. } if token == "obj-root") {
            bundle.bytes.extend_from_slice(
                b"\n```mermaid\nA --> B\n```\n\n![Board](feishufs-asset:62.jpg)\n",
            );
            bundle.assets.push(ContentAsset {
                name: "62.jpg".into(),
                request: ContentRequest::Whiteboard { token: "b".into() },
                bytes: b"\xff\xd8\xffimage".to_vec(),
            });
        }
        Ok(bundle)
    }
}

fn table_node(token: &str, title: &str, kind: &str, has_child: bool) -> RemoteNode {
    let mut node = remote_node(token, title, has_child);
    node.object_type = Some(kind.into());
    node
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

    let temporary = tempfile::tempdir().unwrap();
    let link = "https://smoke.feishu.cn/wiki/root".parse().unwrap();
    let filesystem = FeishuFs::connect(
        Arc::new(SmokeSource),
        link,
        FsOptions {
            asset_directory: Some(temporary.path().join("cache")),
            cache_ttl: Duration::from_secs(60),
            max_nodes: 10,
            ..FsOptions::default()
        },
    )
    .unwrap();
    let root_directory = filesystem.lookup(fsk::ROOT_INODE, b"Root").unwrap().0;
    let root_content = filesystem.lookup(root_directory, b"index.md").unwrap().0;
    let manual = filesystem.lookup(fsk::ROOT_INODE, b"Manual.pdf").unwrap().0;
    let disk_directory = filesystem.asset_directory().unwrap().to_path_buf();
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
        format!(
            "# Root\n\nSee [leaf](<Child/Leaf.md>).\n[Manual](<../.feishufs-assets/{manual}/Manual.pdf>).\n\n```mermaid\nA --> B\n```\n\n![Board](<../.feishufs-assets/{root_content}/62.jpg>)\n"
        )
    );
    let image_path = mountpoint.join(format!(".feishufs-assets/{root_content}/62.jpg"));
    assert_eq!(fs::read(&image_path).unwrap(), b"\xff\xd8\xffimage");
    assert!(fs::write(&image_path, b"changed").is_err());
    let manual_path = mountpoint.join(format!(".feishufs-assets/{manual}/Manual.pdf"));
    assert_eq!(fs::read(&manual_path).unwrap(), b"%PDF-1.7\n\0manual");
    assert_eq!(
        fs::read(mountpoint.join("Manual.pdf")).unwrap(),
        b"%PDF-1.7\n\0manual"
    );
    assert!(fs::write(&manual_path, b"changed").is_err());
    assert_eq!(
        fs::read(disk_directory.join("files/6f626a2d6d616e75616c/Manual.pdf")).unwrap(),
        b"%PDF-1.7\n\0manual",
    );
    assert_eq!(
        fs::read(disk_directory.join("whiteboards/62/62.jpg")).unwrap(),
        b"\xff\xd8\xffimage",
    );
    assert_eq!(
        fs::read_to_string(mountpoint.join("Root/Child/index.md")).unwrap(),
        "# Child\n"
    );
    assert_eq!(
        fs::read_to_string(mountpoint.join("Root/Child/Leaf.md")).unwrap(),
        "# Leaf\n"
    );
    for (path, prefix) in [("报表.收入.csv", ""), ("数据/数据.收入.csv", "../")] {
        assert_eq!(
            fs::read(mountpoint.join(path)).unwrap(),
            format!("name,value,PDF\r\na,10,{prefix}.feishufs-assets/{manual}/Manual.pdf\r\n")
                .as_bytes()
        );
        assert!(fs::write(mountpoint.join(path), b"changed").is_err());
    }
    assert_eq!(
        fs::read(mountpoint.join("报表.支出.csv")).unwrap(),
        format!("name,value,PDF\r\nb,10,.feishufs-assets/{manual}/Manual.pdf\r\n").as_bytes()
    );
    assert_eq!(
        fs::read_to_string(mountpoint.join("数据/说明.md")).unwrap(),
        "# Note\n"
    );
    assert!(!temporary.path().join(".feishufs-assets").exists());
    assert!(!mountpoint.join("报表.xlsx").exists());
    assert!(!mountpoint.join("数据/index.csv").exists());
    assert!(fs::write(mountpoint.join("Root/index.md"), "changed").is_err());
    assert!(fs::write(mountpoint.join("new.md"), "new").is_err());

    session.unmount();
}

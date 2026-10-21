use std::{
    collections::HashMap,
    fs,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use feishufs::{
    ContentAsset, ContentBundle, ContentRequest, FsOptions, KnowledgeLink, RemoteNode,
    RemoteSource, RemoteSpace, RemoteSubtable,
};
use fsk::{Filesystem, ROOT_INODE};

use super::*;

#[derive(Default)]
struct Source {
    children: HashMap<String, Vec<RemoteNode>>,
    content: HashMap<String, ContentBundle>,
    fetches: Mutex<Vec<String>>,
}

impl RemoteSource for Source {
    fn resolve_space(&self, _: &KnowledgeLink) -> Result<RemoteSpace> {
        Ok(RemoteSpace {
            space_id: "space".into(),
            name: "Team".into(),
        })
    }

    fn list_children(&self, _: &str, parent: &RemoteNode) -> Result<Vec<RemoteNode>> {
        Ok(self
            .children
            .get(&parent.node_token)
            .cloned()
            .unwrap_or_default())
    }

    fn list_subtables(&self, _: &RemoteNode) -> Result<Vec<RemoteSubtable>> {
        Ok(vec![RemoteSubtable {
            id: "sheet1".into(),
            name: "Data".into(),
        }])
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        self.fetch_bundle(request).map(|bundle| bundle.bytes)
    }

    fn fetch_bundle(&self, request: &ContentRequest) -> Result<ContentBundle> {
        let token = match request {
            ContentRequest::Markdown { token, .. }
            | ContentRequest::Download { token }
            | ContentRequest::Export { token, .. }
            | ContentRequest::TableCsv { token, .. }
            | ContentRequest::Whiteboard { token } => token,
            ContentRequest::Inline { bytes } => return Ok(bytes.clone().into()),
        };
        self.fetches.lock().unwrap().push(token.clone());
        self.content
            .get(token)
            .cloned()
            .context("Content unavailable")
    }
}

fn node(token: &str, title: &str, kind: &str, has_child: bool) -> RemoteNode {
    RemoteNode {
        space_id: "space".into(),
        node_token: token.into(),
        object_token: Some(token.into()),
        object_type: Some(kind.into()),
        title: title.into(),
        url: None,
        has_child,
    }
}

fn source(body: Option<&[u8]>) -> Source {
    let mut source = Source::default();
    source
        .children
        .insert("space".into(), vec![node("guide", "Guide", "docx", false)]);
    if let Some(body) = body {
        source.content.insert("guide".into(), body.to_vec().into());
    }
    source
}

fn connect(source: Source) -> (FeishuFs, Arc<Source>) {
    let source = Arc::new(source);
    let filesystem = FeishuFs::connect(
        source.clone(),
        "https://example.feishu.cn/wiki/entry".parse().unwrap(),
        FsOptions {
            cache_ttl: std::time::Duration::from_nanos(1),
            ..FsOptions::default()
        },
    )
    .unwrap();
    (filesystem, source)
}

#[test]
fn exports_the_existing_file_mapping_links_and_binary_bytes() {
    let mut source = source(Some(b"[child](https://example.feishu.cn/wiki/child)\n"));
    source.children.get_mut("space").unwrap()[0].has_child = true;
    source
        .children
        .insert("guide".into(), vec![node("child", "Child", "docx", false)]);
    source.children.get_mut("space").unwrap().extend([
        node("file", "Manual.pdf", "file", false),
        node("sheet", "Report", "sheet", false),
        node("legacy", "Legacy", "doc", false),
    ]);
    let binary = vec![0x80; 150_000];
    for (token, bytes) in [
        ("child", b"# Child\n".to_vec()),
        ("file", binary.clone()),
        (
            "sheet",
            b"\xef\xbb\xbfLink,Value\r\nhttps://example.feishu.cn/wiki/child,01\r\n".to_vec(),
        ),
        ("legacy", b"PK\0\xffdocx".to_vec()),
    ] {
        source.content.insert(token.into(), bytes.into());
    }
    let (filesystem, source) = connect(source);
    let temporary = tempfile::tempdir().unwrap();
    let output = temporary.path().join("export");
    assert_eq!(
        write(&filesystem, &output, WriteMode::CreateNew).unwrap(),
        6
    );
    assert_eq!(
        fs::read(output.join("Guide/index.md")).unwrap(),
        b"[child](<Child.md>)\n"
    );
    assert_eq!(
        fs::read(output.join("Guide/Child.md")).unwrap(),
        b"# Child\n"
    );
    assert_eq!(
        fs::read(output.join("Report.Data.csv")).unwrap(),
        b"\xef\xbb\xbfLink,Value\r\nGuide/Child.md,01\r\n"
    );
    assert_eq!(
        fs::read(output.join("Legacy.docx")).unwrap(),
        b"PK\0\xffdocx"
    );
    let owner = filesystem.lookup(ROOT_INODE, b"Manual.pdf").unwrap().0;
    for path in [
        "Manual.pdf".into(),
        format!(".feishufs-assets/{owner}/Manual.pdf"),
    ] {
        assert_eq!(fs::read(output.join(path)).unwrap(), binary);
    }
    assert_eq!(
        source
            .fetches
            .lock()
            .unwrap()
            .iter()
            .filter(|token| *token == "file")
            .count(),
        1
    );
}

#[test]
fn includes_whiteboards_discovered_after_the_initial_directory_listing() {
    let mut source = source(None);
    let image = b"\xff\xd8\xffimage";
    source.content.insert(
        "guide".into(),
        ContentBundle {
            bytes: b"![Board](feishufs-asset:62.jpg)\n".to_vec(),
            assets: vec![ContentAsset {
                name: "62.jpg".into(),
                request: ContentRequest::Whiteboard { token: "b".into() },
                bytes: image.to_vec(),
            }],
        },
    );
    let (filesystem, _) = connect(source);
    assert!(filesystem.lookup(ROOT_INODE, b".feishufs-assets").is_err());
    let temporary = tempfile::tempdir().unwrap();
    let output = temporary.path().join("export");
    assert_eq!(
        write(&filesystem, &output, WriteMode::CreateNew).unwrap(),
        2
    );
    let owner = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
    let asset = format!(".feishufs-assets/{owner}/62.jpg");
    assert_eq!(
        fs::read_to_string(output.join("Guide.md")).unwrap(),
        format!("![Board](<{asset}>)\n")
    );
    assert_eq!(fs::read(output.join(asset)).unwrap(), image);
}

#[test]
fn export_rejects_an_existing_directory_before_fetching_content() {
    let (filesystem, source) = connect(source(Some(b"new")));
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("Guide.md"), b"old").unwrap();
    assert!(write(&filesystem, temporary.path(), WriteMode::CreateNew).is_err());
    assert!(source.fetches.lock().unwrap().is_empty());
    assert_eq!(fs::read(temporary.path().join("Guide.md")).unwrap(), b"old");
}

#[test]
fn sync_refreshes_content_adds_files_and_retains_unrelated_or_removed_files() {
    let temporary = tempfile::tempdir().unwrap();
    let output = temporary.path().join("nested/export");
    write(
        &connect(source(Some(b"first"))).0,
        &output,
        WriteMode::Update,
    )
    .unwrap();
    fs::write(output.join("notes.txt"), b"local").unwrap();
    let mut updated = source(Some(b"second"));
    updated
        .children
        .get_mut("space")
        .unwrap()
        .push(node("new", "New", "docx", false));
    updated.content.insert("new".into(), Vec::new().into());
    write(&connect(updated).0, &output, WriteMode::Update).unwrap();
    assert_eq!(fs::read(output.join("Guide.md")).unwrap(), b"second");
    assert_eq!(fs::read(output.join("New.md")).unwrap(), b"");
    write(&connect(Source::default()).0, &output, WriteMode::Update).unwrap();
    assert_eq!(fs::read(output.join("notes.txt")).unwrap(), b"local");
    assert_eq!(fs::read(output.join("Guide.md")).unwrap(), b"second");
}

#[test]
fn a_fetch_failure_keeps_the_previous_file_and_reports_its_path() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("Guide.md"), b"old").unwrap();
    let error = write(
        &connect(source(None)).0,
        temporary.path(),
        WriteMode::Update,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("Guide.md"));
    assert_eq!(fs::read(temporary.path().join("Guide.md")).unwrap(), b"old");
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
}

#[test]
fn sync_reports_file_directory_conflicts_without_removing_local_content() {
    let temporary = tempfile::tempdir().unwrap();
    let conflict = temporary.path().join("Guide.md");
    fs::create_dir(&conflict).unwrap();
    fs::write(conflict.join("notes.txt"), b"local").unwrap();
    let error = write(
        &connect(source(Some(b"new"))).0,
        temporary.path(),
        WriteMode::Update,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("Guide.md"));
    assert_eq!(fs::read(conflict.join("notes.txt")).unwrap(), b"local");

    let mut source = source(Some(b"parent"));
    source.children.get_mut("space").unwrap()[0].has_child = true;
    source
        .children
        .insert("guide".into(), vec![node("child", "Child", "docx", false)]);
    fs::write(temporary.path().join("Guide"), b"local file").unwrap();
    let error = write(&connect(source).0, temporary.path(), WriteMode::Update).unwrap_err();
    assert!(format!("{error:#}").contains("Guide"));
    assert_eq!(
        fs::read(temporary.path().join("Guide")).unwrap(),
        b"local file"
    );
}

#[cfg(unix)]
#[test]
fn sync_rejects_symlinks_at_the_root_directories_and_files() {
    use std::os::unix::fs::symlink;
    for relative in ["", "Guide", "Guide/index.md"] {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("export");
        let outside = temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("index.md"), b"untouched").unwrap();
        let link = if relative.is_empty() {
            output.clone()
        } else {
            output.join(relative)
        };
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(
            if relative.ends_with(".md") {
                outside.join("index.md")
            } else {
                outside.clone()
            },
            &link,
        )
        .unwrap();
        let mut source = source(Some(b"new"));
        source.children.get_mut("space").unwrap()[0].has_child = true;
        source
            .children
            .insert("guide".into(), vec![node("child", "Child", "docx", false)]);
        source
            .content
            .insert("child".into(), b"child".to_vec().into());
        let (filesystem, _) = connect(source);
        assert!(
            write(&filesystem, &output, WriteMode::Update).is_err(),
            "{relative}"
        );
        if relative.is_empty() {
            assert!(write(&filesystem, &output.join(""), WriteMode::Update).is_err());
        }
        assert_eq!(fs::read(outside.join("index.md")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(outside).unwrap().count(), 1);
    }
}

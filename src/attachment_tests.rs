use std::{fs, sync::Barrier};

use crate::remote::test_support::{FakeSource, leaf, node};

use super::*;

const PDF: &[u8] = b"%PDF-1.7\n\0binary attachment\n";

fn test_fs(ttl: Duration) -> (FeishuFs, FakeSource, tempfile::TempDir) {
    let temporary = tempfile::tempdir().unwrap();
    let source = FakeSource::with_root(node("entry", "Team Wiki", "docx"));
    source.children.lock().insert(
        "space-1".into(),
        vec![
            leaf("doc", "Guide", "docx"),
            leaf("file", "Manual.pdf", "file"),
        ],
    );
    source.content.lock().insert(
        "obj-doc".into(),
        Ok(b"[PDF](https://example.feishu.cn/wiki/file?from=copy#page=2)\n".to_vec()),
    );
    source
        .content
        .lock()
        .insert("obj-file".into(), Ok(PDF.to_vec()));
    let filesystem = FeishuFs::connect(
        Arc::new(source.clone()),
        "https://example.feishu.cn/wiki/entry".parse().unwrap(),
        FsOptions {
            asset_directory: Some(temporary.path().join(".feishufs-assets")),
            cache_ttl: ttl,
            ..FsOptions::default()
        },
    )
    .unwrap();
    (filesystem, source, temporary)
}

fn original(fs: &FeishuFs) -> u64 {
    fs.lookup(ROOT_INODE, b"Manual.pdf").unwrap().0
}

fn asset(fs: &FeishuFs, owner: u64, name: &str) -> u64 {
    let root = fs.lookup(ROOT_INODE, b".feishufs-assets").unwrap().0;
    let directory = fs.lookup(root, owner.to_string().as_bytes()).unwrap().0;
    fs.lookup(directory, name.as_bytes()).unwrap().0
}

fn read_all(fs: &FeishuFs, inode: u64) -> Result<Vec<u8>, FsError> {
    let mut bytes = vec![0; 4096];
    let count = fs.read(inode, 0, &mut bytes)?;
    bytes.truncate(count);
    Ok(bytes)
}

fn disk_path(fs: &FeishuFs, token: &str, name: &str) -> PathBuf {
    fs.inner
        .asset_store
        .as_ref()
        .unwrap()
        .path(
            &ContentRequest::Download {
                token: token.into(),
            },
            name,
        )
        .unwrap()
}

fn pdf_fetches(source: &FakeSource) -> usize {
    source
        .fetches
        .lock()
        .iter()
        .filter(|token| token.as_str() == "obj-file")
        .count()
}

#[test]
fn references_point_to_assets_without_prefetching_the_attachment() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    assert_ne!(owner, alias);
    let doc = fs.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
    assert_eq!(
        read_all(&fs, doc).unwrap(),
        format!("[PDF](<.feishufs-assets/{owner}/Manual.pdf#page=2>)\n").as_bytes(),
    );
    assert_eq!(pdf_fetches(&source), 0);
    assert!(!disk_path(&fs, "obj-file", "Manual.pdf").exists());
}

#[test]
fn wiki_file_and_reference_links_use_the_same_asset_target() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    source.content.lock().insert(
        "obj-doc".into(),
        Ok(concat!(
            "[wiki](https://example.feishu.cn/wiki/file)\n",
            "[file](https://example.feishu.cn/file/obj-file)\n",
            "[ref]: https://example.feishu.cn/file/obj-file \"attachment\"\n",
            "[outside](https://example.feishu.cn/file/unknown)\n",
            "`[example](https://example.feishu.cn/file/obj-file)`\n",
        )
        .as_bytes()
        .to_vec()),
    );
    let doc = fs.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
    assert_eq!(
        String::from_utf8(read_all(&fs, doc).unwrap()).unwrap(),
        format!(
            "[wiki](<.feishufs-assets/{owner}/Manual.pdf>)\n[file](<.feishufs-assets/{owner}/Manual.pdf>)\n[ref]: <.feishufs-assets/{owner}/Manual.pdf> \"attachment\"\n[outside](https://example.feishu.cn/file/unknown)\n`[example](https://example.feishu.cn/file/obj-file)`\n"
        )
    );
    assert_eq!(pdf_fetches(&source), 0);
}

#[test]
fn original_and_asset_entries_share_download_cache_and_disk_bytes() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    assert_eq!(fs.metadata(alias).unwrap().size, PDF.len() as u64);
    assert_eq!(read_all(&fs, owner).unwrap(), PDF);
    assert_eq!(read_all(&fs, alias).unwrap(), PDF);
    assert_eq!(
        fs::read(disk_path(&fs, "obj-file", "Manual.pdf")).unwrap(),
        PDF
    );
    assert_eq!(pdf_fetches(&source), 1);
    assert!(fs.inner.content.lock().contains_key(&owner));
    assert!(!fs.inner.content.lock().contains_key(&alias));
    assert_eq!(fs.metadata(alias).unwrap().mode, 0o444);
    assert!(fs.open_file(alias, true).is_err());
    assert!(fs.write(alias, 0, b"changed").is_err());
}

#[test]
fn concurrent_reads_through_both_paths_download_only_once() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|index| {
            let fs = fs.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                read_all(&fs, if index % 2 == 0 { owner } else { alias }).unwrap()
            })
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), PDF);
    }
    assert_eq!(pdf_fetches(&source), 1);
}

#[test]
fn refresh_failure_keeps_last_disk_copy_and_shared_memory_cache() {
    let (fs, source, _temporary) = test_fs(Duration::from_nanos(1));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    read_all(&fs, alias).unwrap();
    let fetched_at = fs.inner.content.lock()[&owner].fetched_at;
    let path = disk_path(&fs, "obj-file", "Manual.pdf");
    source
        .content
        .lock()
        .insert("obj-file".into(), Err("Permission denied".into()));
    fs.open_file(alias, false).unwrap();
    assert_eq!(read_all(&fs, owner).unwrap(), PDF);
    assert_eq!(fs::read(&path).unwrap(), PDF);
    assert_eq!(fs.inner.content.lock()[&owner].fetched_at, fetched_at);
    assert!(fs.inner.failures.lock().contains_key(&owner));
    assert!(!fs.inner.failures.lock().contains_key(&alias));
    source
        .content
        .lock()
        .insert("obj-file".into(), Ok(b"%PDF-updated\0".to_vec()));
    fs.inner.failures.lock().get_mut(&owner).unwrap().retry_at = Instant::now();
    fs.open_file(owner, false).unwrap();
    assert_eq!(read_all(&fs, alias).unwrap(), b"%PDF-updated\0");
    assert_eq!(fs::read(&path).unwrap(), b"%PDF-updated\0");
}

#[test]
fn disk_write_failure_does_not_publish_an_empty_or_memory_only_success() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    let blocked = fs.asset_directory().unwrap().join("files");
    fs::write(&blocked, b"not a directory").unwrap();
    assert_eq!(read_all(&fs, alias), Err(FsError::IO));
    assert_eq!(read_all(&fs, owner), Err(FsError::IO));
    assert!(!fs.inner.content.lock().contains_key(&owner));
    assert_eq!(pdf_fetches(&source), 1);
    assert_eq!(fs::read(&blocked).unwrap(), b"not a directory");
    fs::remove_file(blocked).unwrap();
    fs.inner.failures.lock().get_mut(&owner).unwrap().retry_at = Instant::now();
    assert_eq!(read_all(&fs, alias).unwrap(), PDF);
    assert!(disk_path(&fs, "obj-file", "Manual.pdf").is_file());
}

#[test]
fn move_rename_and_leaf_to_branch_rerender_attachment_links_without_redownload() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    let doc = fs.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
    read_all(&fs, owner).unwrap();
    read_all(&fs, doc).unwrap();
    source.children.lock().insert(
        "space-1".into(),
        vec![
            leaf("doc", "Guide", "docx"),
            node("parent", "Parent", "docx"),
        ],
    );
    source
        .children
        .lock()
        .insert("parent".into(), vec![node("file", "Renamed", "file")]);
    source
        .children
        .lock()
        .insert("file".into(), vec![leaf("child", "Child", "docx")]);
    // Keep the file extension when a file node is retitled.
    source.children.lock().get_mut("parent").unwrap()[0].title = "Renamed.pdf".into();
    fs.refresh_now().unwrap();
    let parent = fs.lookup(ROOT_INODE, b"Parent").unwrap().0;
    let directory = fs.lookup(parent, b"Renamed.pdf").unwrap().0;
    assert_eq!(fs.lookup(directory, b"index.pdf").unwrap().0, owner);
    assert_eq!(asset(&fs, owner, "index.pdf"), alias);
    assert_eq!(read_all(&fs, alias).unwrap(), PDF);
    assert_eq!(
        read_all(&fs, doc).unwrap(),
        format!("[PDF](<.feishufs-assets/{owner}/index.pdf#page=2>)\n").as_bytes(),
    );
    assert_eq!(pdf_fetches(&source), 1);
}

#[test]
fn removed_attachments_leave_no_virtual_alias_but_retain_disk_copies() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    let path = disk_path(&fs, "obj-file", "Manual.pdf");
    read_all(&fs, alias).unwrap();
    source
        .children
        .lock()
        .insert("space-1".into(), vec![leaf("doc", "Guide", "docx")]);
    fs.refresh_now().unwrap();
    assert!(fs.lookup(ROOT_INODE, b".feishufs-assets").is_err());
    assert_eq!(read_all(&fs, alias), Err(NOT_FOUND));
    assert!(!fs.inner.content.lock().contains_key(&owner));
    assert_eq!(fs::read(path).unwrap(), PDF);
}

#[test]
fn disk_copies_survive_filesystem_destruction() {
    let (fs, _source, _temporary) = test_fs(Duration::from_secs(60));
    read_all(&fs, original(&fs)).unwrap();
    let path = disk_path(&fs, "obj-file", "Manual.pdf");
    drop(fs);
    assert_eq!(fs::read(path).unwrap(), PDF);
}

#[test]
fn replacement_attachment_identity_does_not_reuse_old_bytes_or_disk_path() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    read_all(&fs, alias).unwrap();
    let old_path = disk_path(&fs, "obj-file", "Manual.pdf");
    source.children.lock().get_mut("space-1").unwrap()[1].object_token = Some("replacement".into());
    source
        .content
        .lock()
        .insert("replacement".into(), Ok(b"%PDF-replacement".to_vec()));
    fs.refresh_now().unwrap();
    assert_eq!(original(&fs), owner);
    assert_eq!(asset(&fs, owner, "Manual.pdf"), alias);
    assert_eq!(read_all(&fs, alias).unwrap(), b"%PDF-replacement");
    let new_path = disk_path(&fs, "replacement", "Manual.pdf");
    assert_ne!(old_path, new_path);
    assert_eq!(fs::read(old_path).unwrap(), PDF);
    assert_eq!(fs::read(new_path).unwrap(), b"%PDF-replacement");
}

#[test]
fn attachment_fetch_errors_leave_no_partial_disk_file_and_share_cooldown() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let owner = original(&fs);
    let alias = asset(&fs, owner, "Manual.pdf");
    source.content.lock().insert(
        "obj-file".into(),
        Err("API error 99991672: Access denied".into()),
    );
    assert_eq!(read_all(&fs, alias), Err(FsError::IO));
    assert_eq!(read_all(&fs, owner), Err(FsError::IO));
    assert!(!disk_path(&fs, "obj-file", "Manual.pdf").exists());
    assert_eq!(pdf_fetches(&source), 1);
}

#[test]
fn whiteboard_images_are_persisted_in_the_same_backing_namespace() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    let image = b"\xff\xd8\xffimage";
    let request = ContentRequest::Whiteboard { token: "b".into() };
    source.bundles.lock().insert(
        "obj-doc".into(),
        ContentBundle {
            bytes: b"![Board](feishufs-asset:62.jpg)\n".to_vec(),
            assets: vec![crate::ContentAsset {
                name: "62.jpg".into(),
                request: request.clone(),
                bytes: image.to_vec(),
            }],
        },
    );
    let doc = fs.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
    read_all(&fs, doc).unwrap();
    let path = fs
        .inner
        .asset_store
        .as_ref()
        .unwrap()
        .path(&request, "62.jpg")
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), image);
    assert_eq!(read_all(&fs, asset(&fs, doc, "62.jpg")).unwrap(), image);
}

#[test]
fn percent_hash_and_unicode_attachment_filenames_are_link_encoded() {
    let (fs, source, _temporary) = test_fs(Duration::from_secs(60));
    source.children.lock().get_mut("space-1").unwrap()[1].title = "报告 100% #1.pdf".into();
    fs.refresh_now().unwrap();
    let owner = fs
        .lookup(ROOT_INODE, "报告 100% #1.pdf".as_bytes())
        .unwrap()
        .0;
    let doc = fs.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
    assert_eq!(
        read_all(&fs, doc).unwrap(),
        format!("[PDF](<.feishufs-assets/{owner}/报告 100%25 %231.pdf#page=2>)\n").as_bytes(),
    );
    read_all(&fs, owner).unwrap();
    assert_eq!(
        fs::read(disk_path(&fs, "obj-file", "报告 100% #1.pdf")).unwrap(),
        PDF
    );
}

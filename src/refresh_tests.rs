use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};

use crate::remote::test_support::{FakeSource, leaf, node};

use super::*;

type Blocker = (mpsc::Sender<()>, mpsc::Receiver<()>);

struct ControlledSource {
    source: FakeSource,
    resolve_calls: AtomicUsize,
    fetch_calls: AtomicUsize,
    entry_available: AtomicBool,
    entry_space: Mutex<String>,
    title: Mutex<String>,
    listing_fails: AtomicBool,
    blockers: Mutex<HashMap<String, Blocker>>,
}

impl ControlledSource {
    fn new() -> Arc<Self> {
        let source = FakeSource::with_root(node("entry", "Team Wiki", "docx"));
        source.children.lock().insert(
            "space-1".into(),
            vec![
                leaf("one", "One", "docx"),
                leaf("two", "Two", "docx"),
                leaf("three", "Three", "docx"),
            ],
        );
        for token in ["obj-one", "obj-two", "obj-three"] {
            source
                .content
                .lock()
                .insert(token.into(), Ok(b"original".to_vec()));
        }
        Arc::new(Self {
            source,
            resolve_calls: AtomicUsize::new(0),
            fetch_calls: AtomicUsize::new(0),
            entry_available: AtomicBool::new(true),
            entry_space: Mutex::new("space-1".into()),
            title: Mutex::new("Team Wiki".into()),
            listing_fails: AtomicBool::new(false),
            blockers: Mutex::new(HashMap::new()),
        })
    }

    fn connect(self: &Arc<Self>, options: FsOptions) -> FeishuFs {
        FeishuFs::connect(
            self.clone(),
            "https://example.feishu.cn/wiki/entry".parse().unwrap(),
            options,
        )
        .unwrap()
    }

    fn block(&self, token: &str) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        self.blockers
            .lock()
            .insert(token.into(), (started_tx, resume_rx));
        (started_rx, resume_tx)
    }
}

impl RemoteSource for ControlledSource {
    fn resolve_space(&self, _: &KnowledgeLink) -> Result<RemoteSpace> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        if !self.entry_available.load(Ordering::SeqCst) {
            bail!("Entry document is unavailable");
        }
        Ok(RemoteSpace {
            space_id: self.entry_space.lock().clone(),
            name: self.title.lock().clone(),
        })
    }

    fn refresh_space(&self, space: &RemoteSpace) -> Result<RemoteSpace> {
        assert_eq!(space.space_id, "space-1");
        Ok(RemoteSpace {
            space_id: space.space_id.clone(),
            name: self.title.lock().clone(),
        })
    }

    fn list_children(
        &self,
        space: &str,
        parent: &crate::remote::RemoteNode,
    ) -> Result<Vec<crate::remote::RemoteNode>> {
        if self.listing_fails.load(Ordering::SeqCst) {
            bail!("Listing failed");
        }
        self.source.list_children(space, parent)
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        self.fetch_calls.fetch_add(1, Ordering::SeqCst);
        if let ContentRequest::Markdown { token, .. } = request {
            let blocker = self.blockers.lock().remove(token);
            if let Some((started, resume)) = blocker {
                started.send(()).unwrap();
                resume
                    .recv_timeout(Duration::from_secs(5))
                    .expect("Test must release the fetch");
            }
        }
        self.source.fetch_content(request)
    }
}

fn inode(fs: &FeishuFs, name: &[u8]) -> u64 {
    fs.lookup(fsk::ROOT_INODE, name).unwrap().0
}

fn read_all(fs: &FeishuFs, inode: u64) -> Result<Vec<u8>, FsError> {
    let mut bytes = vec![0; 4096];
    let count = fs.read(inode, 0, &mut bytes)?;
    bytes.truncate(count);
    Ok(bytes)
}

#[test]
fn entry_deletion_and_move_do_not_change_the_selected_space() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    source.entry_available.store(false, Ordering::SeqCst);
    fs.refresh_now().unwrap();
    *source.entry_space.lock() = "space-2".into();
    source.entry_available.store(true, Ordering::SeqCst);
    fs.refresh_now().unwrap();
    assert_eq!(source.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fs.root_title(), "Team Wiki");
    assert_eq!(fs.node_count(), 3);
    assert!(fs.lookup(fsk::ROOT_INODE, b"One.md").is_ok());
    *source.title.lock() = "Renamed Wiki".into();
    assert!(fs.refresh_now().unwrap().revision_changed);
    assert_eq!(fs.root_title(), "Renamed Wiki");
}

#[test]
fn unchanged_tree_preserves_revision_and_content_rendering() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    read_all(&fs, one).unwrap();
    let revision = fs.inner.state.read().revision();
    let original = fs.inner.content.lock()[&one].clone();
    assert!(!fs.refresh_now().unwrap().revision_changed);
    assert_eq!(fs.inner.state.read().revision(), revision);
    let current = fs.ensure_content(one, false).unwrap();
    assert!(Arc::ptr_eq(&original.bytes, &current.bytes));
    assert_eq!(original.modified_ns, current.modified_ns);
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn identity_changes_are_detected_even_when_paths_are_unchanged() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    assert_eq!(read_all(&fs, one).unwrap(), b"original");
    source.source.children.lock().get_mut("space-1").unwrap()[0].object_token =
        Some("replacement".into());
    source
        .source
        .content
        .lock()
        .insert("replacement".into(), Ok(b"replacement".to_vec()));
    assert!(fs.refresh_now().unwrap().revision_changed);
    assert_eq!(inode(&fs, b"One.md"), one);
    assert_eq!(read_all(&fs, one).unwrap(), b"replacement");
}

#[test]
fn failed_directory_refresh_keeps_the_previous_snapshot() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let revision = fs.inner.state.read().revision();
    let paths = fs.tree_lines();
    source.listing_fails.store(true, Ordering::SeqCst);
    assert!(fs.refresh_now().is_err());
    assert_eq!(fs.inner.state.read().revision(), revision);
    assert_eq!(fs.tree_lines(), paths);
}

#[test]
fn failed_content_refresh_cools_down_without_marking_stale_content_fresh() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions {
        cache_ttl: Duration::from_nanos(1),
        ..FsOptions::default()
    });
    let one = inode(&fs, b"One.md");
    fs.metadata(one).unwrap();
    let fetched_at = fs.inner.content.lock()[&one].fetched_at;
    source
        .source
        .content
        .lock()
        .insert("obj-one".into(), Err("Offline".into()));
    for _ in 0..3 {
        assert_eq!(fs.metadata(one).unwrap().size, 8);
    }
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fs.inner.content.lock()[&one].fetched_at, fetched_at);
    source
        .source
        .content
        .lock()
        .insert("obj-one".into(), Ok(b"recovered".to_vec()));
    fs.inner.failures.lock().get_mut(&one).unwrap().retry_at = Instant::now();
    assert_eq!(fs.metadata(one).unwrap().size, 9);
    assert!(!fs.inner.failures.lock().contains_key(&one));
}

#[test]
fn first_fetch_failure_is_cooled_down_but_never_returns_an_empty_file() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    source
        .source
        .content
        .lock()
        .insert("obj-one".into(), Err("Offline".into()));
    for _ in 0..3 {
        assert_eq!(fs.metadata(one), Err(FsError::IO));
        assert_eq!(read_all(&fs, one), Err(FsError::IO));
    }
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 1);
    source
        .source
        .content
        .lock()
        .insert("obj-one".into(), Ok(b"recovered".to_vec()));
    fs.inner.failures.lock().get_mut(&one).unwrap().retry_at = Instant::now();
    assert_eq!(read_all(&fs, one).unwrap(), b"recovered");
}

#[test]
fn slow_fetch_does_not_block_cached_reads_or_directory_publication() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    let two = inode(&fs, b"Two.md");
    read_all(&fs, two).unwrap();
    let (started, resume) = source.block("obj-one");
    let reader_fs = fs.clone();
    let reader = thread::spawn(move || read_all(&reader_fs, one));
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let other_fs = fs.clone();
    let other = thread::spawn(move || {
        let bytes = read_all(&other_fs, two).unwrap();
        let changed = other_fs.refresh_now().unwrap().revision_changed;
        done_tx.send((bytes, changed)).unwrap();
    });
    let result = done_rx.recv_timeout(Duration::from_secs(1));
    resume.send(()).unwrap();
    reader.join().unwrap().unwrap();
    other.join().unwrap();
    assert_eq!(result.unwrap(), (b"original".to_vec(), false));
}

#[test]
fn concurrent_first_fetches_of_the_same_inode_are_deduplicated() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    let (started, resume) = source.block("obj-one");
    let reader_fs = fs.clone();
    let first = thread::spawn(move || read_all(&reader_fs, one).unwrap());
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    let second = thread::spawn(move || read_all(&fs, one).unwrap());
    resume.send(()).unwrap();
    assert_eq!(first.join().unwrap(), b"original");
    assert_eq!(second.join().unwrap(), b"original");
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn obsolete_fetch_result_is_discarded_after_an_identity_change() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    let (started, resume) = source.block("obj-one");
    let reader_fs = fs.clone();
    let reader = thread::spawn(move || read_all(&reader_fs, one).unwrap());
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    source.source.children.lock().get_mut("space-1").unwrap()[0].object_token =
        Some("replacement".into());
    source
        .source
        .content
        .lock()
        .insert("replacement".into(), Ok(b"replacement".to_vec()));
    fs.refresh_now().unwrap();
    resume.send(()).unwrap();
    assert_eq!(reader.join().unwrap(), b"replacement");
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn obsolete_fetch_result_cannot_resurrect_a_removed_inode() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    let (started, resume) = source.block("obj-one");
    let reader_fs = fs.clone();
    let reader = thread::spawn(move || read_all(&reader_fs, one));
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    source
        .source
        .children
        .lock()
        .get_mut("space-1")
        .unwrap()
        .remove(0);
    fs.refresh_now().unwrap();
    resume.send(()).unwrap();
    assert_eq!(reader.join().unwrap(), Err(NOT_FOUND));
    assert!(!fs.inner.content.lock().contains_key(&one));
}

#[test]
fn failure_cooldown_does_not_apply_to_a_replacement_identity() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions::default());
    let one = inode(&fs, b"One.md");
    source
        .source
        .content
        .lock()
        .insert("obj-one".into(), Err("Offline".into()));
    assert_eq!(fs.metadata(one), Err(FsError::IO));
    source.source.children.lock().get_mut("space-1").unwrap()[0].object_token =
        Some("replacement".into());
    source
        .source
        .content
        .lock()
        .insert("replacement".into(), Ok(b"replacement".to_vec()));
    fs.refresh_now().unwrap();
    assert_eq!(read_all(&fs, one).unwrap(), b"replacement");
    assert!(!fs.inner.failures.lock().contains_key(&one));
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn successful_bundle_publication_clears_asset_failure_cooldown() {
    let source = FakeSource::with_root(node("entry", "Team Wiki", "docx"));
    source
        .children
        .lock()
        .insert("space-1".into(), vec![leaf("one", "One", "docx")]);
    let mut bundle = ContentBundle {
        bytes: b"![Board](feishufs-asset:62.jpg)\n".to_vec(),
        assets: vec![crate::ContentAsset {
            name: "62.jpg".into(),
            request: ContentRequest::Whiteboard { token: "b".into() },
            bytes: b"\xff\xd8\xffold".to_vec(),
        }],
    };
    source
        .bundles
        .lock()
        .insert("obj-one".into(), bundle.clone());
    let fs = FeishuFs::connect(
        Arc::new(source.clone()),
        "https://example.feishu.cn/wiki/entry".parse().unwrap(),
        FsOptions {
            cache_ttl: Duration::from_nanos(1),
            ..FsOptions::default()
        },
    )
    .unwrap();
    let owner = inode(&fs, b"One.md");
    read_all(&fs, owner).unwrap();
    let asset_root = inode(&fs, b".feishufs-assets");
    let directory = fs
        .lookup(asset_root, owner.to_string().as_bytes())
        .unwrap()
        .0;
    let image = fs.lookup(directory, b"62.jpg").unwrap().0;
    fs.metadata(image).unwrap();
    assert!(fs.inner.failures.lock().contains_key(&image));
    bundle.assets[0].bytes = b"\xff\xd8\xffnew".to_vec();
    source.bundles.lock().insert("obj-one".into(), bundle);
    fs.metadata(owner).unwrap();
    assert!(!fs.inner.failures.lock().contains_key(&image));
    assert_eq!(read_all(&fs, image).unwrap(), b"\xff\xd8\xffnew");
}

#[test]
fn different_inodes_can_fetch_concurrently_but_respect_the_capacity() {
    let source = ControlledSource::new();
    let fs = source.connect(FsOptions {
        max_concurrent_fetches: 2,
        ..FsOptions::default()
    });
    let mut readers = Vec::new();
    let mut resumes = Vec::new();
    for (name, token) in [
        (b"One.md".as_slice(), "obj-one"),
        (b"Two.md".as_slice(), "obj-two"),
    ] {
        let inode = inode(&fs, name);
        let (started, resume) = source.block(token);
        let fs = fs.clone();
        readers.push(thread::spawn(move || read_all(&fs, inode).unwrap()));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        resumes.push(resume);
    }
    let three = inode(&fs, b"Three.md");
    let (started, resume) = source.block("obj-three");
    readers.push(thread::spawn(move || read_all(&fs, three).unwrap()));
    assert!(started.recv_timeout(Duration::from_millis(30)).is_err());
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 2);
    resumes.remove(0).send(()).unwrap();
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    resume.send(()).unwrap();
    resumes.remove(0).send(()).unwrap();
    for reader in readers {
        assert_eq!(reader.join().unwrap(), b"original");
    }
    assert_eq!(source.fetch_calls.load(Ordering::SeqCst), 3);
}

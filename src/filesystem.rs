use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use fsk::ROOT_INODE;
use fsk::{
    DirectoryEntry as FskDirectoryEntry, DirectorySink, Error as FsError, FileType, Filesystem,
    KernelCachePolicy, Metadata, NetworkConfiguration, NetworkFile, NetworkFilesystem,
    ReadDirectoryResult, SetMetadata, StatFs, now_ns,
    raw::{RawEvent, RawEventMask, RawOutcome},
};
use parking_lot::{Mutex, RwLock};
use tracing::{info, warn};

use crate::{
    link::KnowledgeLink,
    platform::ProjectionController,
    remote::{ContentRequest, RemoteSource, RemoteTree},
    tree::{FsState, VirtualNode, VirtualNodeKind, directory_entries, file_request},
};

const NOT_FOUND: FsError = FsError(2);
const ACCESS_DENIED: FsError = FsError(13);
const NOT_DIRECTORY: FsError = FsError(20);
const IS_DIRECTORY: FsError = FsError(21);

#[derive(Clone, Copy, Debug)]
pub struct FsOptions {
    pub cache_ttl: Duration,
    pub max_nodes: usize,
}

impl Default for FsOptions {
    fn default() -> Self {
        Self {
            cache_ttl: Duration::from_secs(60),
            max_nodes: 10_000,
        }
    }
}

#[derive(Clone)]
pub struct FeishuFs {
    inner: Arc<Inner>,
}

struct Inner {
    source: Arc<dyn RemoteSource>,
    link: KnowledgeLink,
    state: RwLock<FsState>,
    content: Mutex<HashMap<u64, CachedContent>>,
    fetch_gate: Mutex<()>,
    cache_ttl: Duration,
    max_nodes: usize,
    projection: ProjectionController,
}

#[derive(Clone)]
struct CachedContent {
    request: ContentRequest,
    source_bytes: Arc<[u8]>,
    bytes: Arc<[u8]>,
    fetched_at: Instant,
    modified_ns: u64,
    generation: u64,
    tree_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshSummary {
    pub node_count: usize,
    pub revision_changed: bool,
}

impl FeishuFs {
    pub fn connect(
        source: Arc<dyn RemoteSource>,
        link: KnowledgeLink,
        options: FsOptions,
    ) -> Result<Self> {
        if options.cache_ttl.is_zero() {
            bail!("cache_ttl 必须大于 0");
        }
        if options.max_nodes == 0 {
            bail!("max_nodes 必须大于 0");
        }
        let tree = RemoteTree::load(source.as_ref(), &link, options.max_nodes)?;
        let state = FsState::from_remote(&tree, now_ns()).context("无法构建初始文件系统快照")?;
        Ok(Self {
            inner: Arc::new(Inner {
                source,
                link,
                state: RwLock::new(state),
                content: Mutex::new(HashMap::new()),
                fetch_gate: Mutex::new(()),
                cache_ttl: options.cache_ttl,
                max_nodes: options.max_nodes,
                projection: ProjectionController::new(),
            }),
        })
    }

    pub fn refresh_now(&self) -> Result<RefreshSummary> {
        let tree = RemoteTree::load(
            self.inner.source.as_ref(),
            &self.inner.link,
            self.inner.max_nodes,
        )?;
        let (old_paths, new_paths, active_files) = {
            let mut state = self.inner.state.write();
            let old_paths = state.replace(&tree)?;
            (old_paths, state.paths(), state.file_inodes())
        };
        let old_path_set: HashSet<_> = old_paths.iter().collect();
        let new_path_set: HashSet<_> = new_paths.iter().collect();
        let revision_changed = old_path_set != new_path_set;
        self.inner
            .content
            .lock()
            .retain(|inode, _| active_files.contains(inode));
        self.inner.projection.invalidate(old_paths, new_paths);
        Ok(RefreshSummary {
            node_count: tree.node_count(),
            revision_changed,
        })
    }

    #[must_use]
    pub fn start_refresher(&self, interval: Duration) -> RefreshHandle {
        let (stop_tx, stop_rx) = mpsc::channel();
        let filesystem = self.clone();
        let join = thread::Builder::new()
            .name("feishufs-refresh".into())
            .spawn(move || {
                loop {
                    match stop_rx.recv_timeout(interval) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => match filesystem.refresh_now() {
                            Ok(summary) => {
                                info!(nodes = summary.node_count, "知识库目录快照刷新完成")
                            }
                            Err(error) => warn!(%error, "知识库目录刷新失败，继续使用旧快照"),
                        },
                    }
                }
            })
            .expect("failed to spawn refresh thread");
        RefreshHandle {
            stop_tx: Some(stop_tx),
            join: Some(join),
        }
    }

    #[must_use]
    pub fn root_title(&self) -> String {
        self.inner.state.read().root_title.clone()
    }

    /// Number of knowledge-base documents in the current snapshot.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.inner.state.read().document_count
    }

    #[must_use]
    pub fn tree_lines(&self) -> Vec<String> {
        let state = self.inner.state.read();
        let mut lines: Vec<_> = state
            .nodes
            .values()
            .filter(|node| !node.path.is_empty())
            .map(|node| match node.kind {
                VirtualNodeKind::Directory { .. } => format!("{}/", node.path),
                VirtualNodeKind::File { .. } => node.path.clone(),
            })
            .collect();
        lines.sort_by_key(|line| line.to_lowercase());
        lines
    }

    fn node(&self, inode: u64) -> Result<VirtualNode, FsError> {
        self.inner
            .state
            .read()
            .nodes
            .get(&inode)
            .cloned()
            .ok_or(NOT_FOUND)
    }

    fn ensure_content(
        &self,
        inode: u64,
        refresh_if_expired: bool,
    ) -> Result<CachedContent, FsError> {
        let _gate = self.inner.fetch_gate.lock();
        let node = self.node(inode)?;
        let request = file_request(&node).map_err(|_| IS_DIRECTORY)?.clone();

        let existing = self.inner.content.lock().get(&inode).cloned();
        let existing = existing.map(|cached| {
            if cached.request == request {
                let cached = self.rerender_if_needed(inode, cached);
                self.inner.content.lock().insert(inode, cached.clone());
                cached
            } else {
                cached
            }
        });
        if let Some(cached) = &existing
            && cached.request == request
            && (!refresh_if_expired
                || request.is_inline()
                || cached.fetched_at.elapsed() < self.inner.cache_ttl)
        {
            return Ok(cached.clone());
        }

        match self.inner.source.fetch_content(&request) {
            Ok(source) => {
                let source_bytes: Arc<[u8]> = Arc::from(source);
                let (bytes, tree_revision) =
                    self.render_source(inode, &request, source_bytes.clone());
                let unchanged = existing.as_ref().is_some_and(|cached| {
                    cached.request == request && cached.bytes.as_ref() == bytes.as_ref()
                });
                let generation = existing.as_ref().map_or(1, |cached| {
                    if unchanged {
                        cached.generation
                    } else {
                        cached.generation.saturating_add(1)
                    }
                });
                let modified_ns = if unchanged {
                    existing
                        .as_ref()
                        .map_or_else(now_ns, |cached| cached.modified_ns)
                } else {
                    now_ns()
                };
                let cached = CachedContent {
                    request,
                    source_bytes,
                    bytes,
                    fetched_at: Instant::now(),
                    modified_ns,
                    generation,
                    tree_revision,
                };
                self.inner.content.lock().insert(inode, cached.clone());
                Ok(cached)
            }
            Err(error) => {
                if let Some(cached) = existing.filter(|cached| cached.request == request) {
                    warn!(inode, %error, "内容刷新失败，继续提供最后一次成功缓存");
                    Ok(cached)
                } else {
                    warn!(inode, %error, "内容首次获取失败");
                    Err(FsError::IO)
                }
            }
        }
    }

    fn render_source(
        &self,
        inode: u64,
        request: &ContentRequest,
        source: Arc<[u8]>,
    ) -> (Arc<[u8]>, u64) {
        let state = self.inner.state.read();
        let revision = state.revision();
        if !request.is_markdown() {
            return (source, revision);
        }
        let rendered = state.render_markdown(inode, request, &source);
        (Arc::from(rendered), revision)
    }

    fn rerender_if_needed(&self, inode: u64, cached: CachedContent) -> CachedContent {
        if !cached.request.is_markdown() {
            return cached;
        }
        let state = self.inner.state.read();
        if state.revision() == cached.tree_revision {
            return cached;
        }
        let revision = state.revision();
        let rendered = state.render_markdown(inode, &cached.request, &cached.source_bytes);
        drop(state);
        if rendered.as_slice() == cached.bytes.as_ref() {
            return CachedContent {
                tree_revision: revision,
                ..cached
            };
        }
        CachedContent {
            bytes: Arc::from(rendered),
            modified_ns: now_ns(),
            generation: cached.generation.saturating_add(1),
            tree_revision: revision,
            ..cached
        }
    }

    fn read_only_error<T>() -> Result<T, FsError> {
        Err(ACCESS_DENIED)
    }
}

pub struct RefreshHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for RefreshHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            warn!("知识库刷新线程在退出时发生 panic");
        }
    }
}

impl Filesystem for FeishuFs {
    fn statfs(&self) -> Result<StatFs, FsError> {
        let files = u64::try_from(self.inner.state.read().nodes.len()).unwrap_or(u64::MAX);
        Ok(StatFs {
            block_size: 4096,
            io_size: 128 * 1024,
            total_bytes: 0,
            free_bytes: 0,
            available_bytes: 0,
            files,
            free_files: u64::MAX.saturating_sub(files),
        })
    }

    fn metadata(&self, inode: u64) -> Result<Metadata, FsError> {
        let node = self.node(inode)?;
        let (size, allocated_size, generation, modified_ns) = match node.kind {
            VirtualNodeKind::Directory { .. } => (0, 0, node.generation, node.created_ns),
            VirtualNodeKind::File { .. } => {
                let content = self.ensure_content(inode, true)?;
                let size = u64::try_from(content.bytes.len()).unwrap_or(u64::MAX);
                (size, size, content.generation, content.modified_ns)
            }
        };
        let kind = node.file_type();
        Ok(Metadata {
            inode,
            parent: node.parent,
            size,
            allocated_size,
            generation,
            created_ns: node.created_ns,
            modified_ns,
            accessed_ns: modified_ns,
            mode: if kind == FileType::Directory {
                0o555
            } else {
                0o444
            },
            uid: 0,
            gid: 0,
            link_count: if kind == FileType::Directory { 2 } else { 1 },
            kind,
        })
    }

    fn set_metadata(&self, _inode: u64, _update: SetMetadata) -> Result<Metadata, FsError> {
        Self::read_only_error()
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType), FsError> {
        if name == b"." {
            let node = self.node(parent)?;
            return Ok((node.inode, node.file_type()));
        }
        if name == b".." {
            let node = self.node(parent)?;
            let parent_node = self.node(node.parent)?;
            return Ok((parent_node.inode, parent_node.file_type()));
        }
        let node = self.node(parent)?;
        let entries = directory_entries(&node).map_err(|_| NOT_DIRECTORY)?;
        entries
            .iter()
            .find(|entry| platform_name_eq(&entry.name, name))
            .map(|entry| (entry.inode, entry.kind))
            .ok_or_else(missing_lookup_error)
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult, FsError> {
        let node = self.node(inode)?;
        let entries = directory_entries(&node).map_err(|_| NOT_DIRECTORY)?;
        if verifier != 0 && verifier != node.generation {
            return Err(FsError::STALE);
        }
        let start = usize::try_from(cookie).map_err(|_| FsError::INVALID)?;
        if start > entries.len() {
            return Err(FsError::INVALID);
        }
        for (index, entry) in entries.iter().enumerate().skip(start) {
            let next_cookie = u64::try_from(index + 1).unwrap_or(u64::MAX);
            if !sink.push(FskDirectoryEntry {
                name: &entry.name,
                inode: entry.inode,
                kind: entry.kind,
                next_cookie,
            }) {
                return Ok(ReadDirectoryResult {
                    verifier: node.generation,
                    next_cookie: u64::try_from(index).unwrap_or(u64::MAX),
                    eof: false,
                });
            }
        }
        Ok(ReadDirectoryResult {
            verifier: node.generation,
            next_cookie: u64::try_from(entries.len()).unwrap_or(u64::MAX),
            eof: true,
        })
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize, FsError> {
        let node = self.node(inode)?;
        if matches!(node.kind, VirtualNodeKind::Directory { .. }) {
            return Err(IS_DIRECTORY);
        }
        let content = self.ensure_content(inode, false)?;
        let start = usize::try_from(offset).map_err(|_| FsError::INVALID)?;
        let Some(input) = content.bytes.get(start..) else {
            return Ok(0);
        };
        let count = input.len().min(output.len());
        output[..count].copy_from_slice(&input[..count]);
        Ok(count)
    }

    fn write(&self, _inode: u64, _offset: u64, _input: &[u8]) -> Result<usize, FsError> {
        Self::read_only_error()
    }

    fn create(
        &self,
        _parent: u64,
        _name: &[u8],
        _kind: FileType,
        _mode: u32,
    ) -> Result<u64, FsError> {
        Self::read_only_error()
    }

    fn create_symlink(
        &self,
        _parent: u64,
        _name: &[u8],
        _target: &[u8],
        _mode: u32,
    ) -> Result<u64, FsError> {
        Self::read_only_error()
    }

    fn remove(&self, _parent: u64, _name: &[u8], _inode: u64) -> Result<(), FsError> {
        Self::read_only_error()
    }

    fn rename(
        &self,
        _source_parent: u64,
        _source_name: &[u8],
        _destination_parent: u64,
        _destination_name: &[u8],
        _replaced_inode: Option<u64>,
    ) -> Result<(), FsError> {
        Self::read_only_error()
    }

    fn raw_event_mask(&self) -> RawEventMask {
        self.inner.projection.raw_event_mask()
    }

    fn raw_event(&self, event: RawEvent<'_>) -> RawOutcome {
        self.inner.projection.raw_event(event)
    }
}

impl NetworkFilesystem for FeishuFs {
    fn network_configuration(&self) -> NetworkConfiguration {
        NetworkConfiguration {
            lazy: true,
            supports_local_files: false,
            kernel_cache_policy: KernelCachePolicy::MetadataOnly,
        }
    }

    fn open_file(&self, inode: u64, writable: bool) -> Result<NetworkFile, FsError> {
        if writable {
            return Err(ACCESS_DENIED);
        }
        self.ensure_content(inode, true)?;
        Ok(NetworkFile::Remote)
    }
}

#[cfg(target_os = "windows")]
fn missing_lookup_error() -> FsError {
    // ProjFS asks the provider about an unknown path before creating it. Access
    // denied here makes the projection namespace read-only.
    ACCESS_DENIED
}

#[cfg(not(target_os = "windows"))]
fn missing_lookup_error() -> FsError {
    NOT_FOUND
}

#[cfg(target_os = "windows")]
fn platform_name_eq(left: &[u8], right: &[u8]) -> bool {
    match (std::str::from_utf8(left), std::str::from_utf8(right)) {
        (Ok(left), Ok(right)) => left.to_lowercase() == right.to_lowercase(),
        _ => left.eq_ignore_ascii_case(right),
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_name_eq(left: &[u8], right: &[u8]) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use std::thread;

    use crate::remote::test_support::{FakeSource, leaf, node};

    use super::*;

    /// A space whose only top-level document is a childless `Guide` doc.
    fn test_fs(cache_ttl: Duration) -> (FeishuFs, FakeSource) {
        let source = FakeSource::with_root(node("root", "Team Wiki", "docx"));
        source
            .children
            .lock()
            .insert("space-1".into(), vec![leaf("one", "Guide", "docx")]);
        source
            .content
            .lock()
            .insert("obj-one".into(), Ok(b"# Guide\n".to_vec()));
        let link = "https://example.feishu.cn/wiki/root".parse().unwrap();
        let filesystem = FeishuFs::connect(
            Arc::new(source.clone()),
            link,
            FsOptions {
                cache_ttl,
                max_nodes: 100,
            },
        )
        .unwrap();
        (filesystem, source)
    }

    #[test]
    fn reads_markdown_and_rejects_writes() {
        let (filesystem, _) = test_fs(Duration::from_secs(60));
        assert_eq!(filesystem.root_title(), "Team Wiki");
        assert_eq!(filesystem.node_count(), 1);

        let (inode, kind) = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap();
        assert_eq!(kind, FileType::File);
        assert_eq!(filesystem.metadata(inode).unwrap().size, 8);
        let mut output = [0_u8; 32];
        let count = filesystem.read(inode, 0, &mut output).unwrap();
        assert_eq!(&output[..count], b"# Guide\n");
        assert_eq!(filesystem.write(inode, 0, b"no"), Err(ACCESS_DENIED));
        assert_eq!(
            filesystem.create(ROOT_INODE, b"new", FileType::File, 0o644),
            Err(ACCESS_DENIED)
        );
    }

    #[test]
    fn rewrites_links_into_mounted_subtree() {
        let (filesystem, source) = test_fs(Duration::from_secs(60));
        // "Root" gains a child page, so it becomes a directory with index.md.
        source.children.lock().insert(
            "space-1".into(),
            vec![
                node("root", "Root", "docx"),
                leaf("sibling", "Sibling", "docx"),
            ],
        );
        source
            .children
            .lock()
            .insert("root".into(), vec![leaf("child", "Child", "docx")]);
        source
            .content
            .lock()
            .insert("obj-child".into(), Ok(b"# Child\n".to_vec()));
        source
            .content
            .lock()
            .insert("obj-sibling".into(), Ok(b"# Sibling\n".to_vec()));
        source.content.lock().insert(
            "obj-root".into(),
            Ok(b"See [child](https://example.feishu.cn/wiki/child).\n".to_vec()),
        );
        filesystem.refresh_now().unwrap();

        let (root_inode, kind) = filesystem.lookup(ROOT_INODE, b"Root").unwrap();
        assert_eq!(kind, FileType::Directory);
        assert_eq!(
            filesystem.lookup(ROOT_INODE, b"Sibling.md").unwrap().1,
            FileType::File
        );
        let (inode, _) = filesystem.lookup(root_inode, b"index.md").unwrap();
        let mut output = [0_u8; 128];
        let count = filesystem.read(inode, 0, &mut output).unwrap();
        assert_eq!(&output[..count], b"See [child](<Child.md>).\n");

        let (child_inode, kind) = filesystem.lookup(root_inode, b"Child.md").unwrap();
        assert_eq!(kind, FileType::File);
        let count = filesystem.read(child_inode, 0, &mut output).unwrap();
        assert_eq!(&output[..count], b"# Child\n");
    }

    #[test]
    fn refresh_rerenders_links_after_a_cloud_rename() {
        let (filesystem, source) = test_fs(Duration::from_secs(3600));
        source
            .children
            .lock()
            .insert("space-1".into(), vec![node("root", "Root", "docx")]);
        source
            .children
            .lock()
            .insert("root".into(), vec![leaf("child", "Child", "docx")]);
        source
            .content
            .lock()
            .insert("obj-child".into(), Ok(b"child".to_vec()));
        source.content.lock().insert(
            "obj-root".into(),
            Ok(b"See [child](https://example.feishu.cn/wiki/child).\n".to_vec()),
        );
        filesystem.refresh_now().unwrap();

        let (root_inode, _) = filesystem.lookup(ROOT_INODE, b"Root").unwrap();
        let (inode, _) = filesystem.lookup(root_inode, b"index.md").unwrap();
        let mut output = [0_u8; 128];
        let count = filesystem.read(inode, 0, &mut output).unwrap();
        assert_eq!(&output[..count], b"See [child](<Child.md>).\n");

        source
            .children
            .lock()
            .insert("root".into(), vec![leaf("child", "Renamed", "docx")]);
        filesystem.refresh_now().unwrap();

        let (root_inode, _) = filesystem.lookup(ROOT_INODE, b"Root").unwrap();
        let (inode, _) = filesystem.lookup(root_inode, b"index.md").unwrap();
        let count = filesystem.read(inode, 0, &mut output).unwrap();
        assert_eq!(&output[..count], b"See [child](<Renamed.md>).\n");
    }

    #[test]
    fn stale_cache_is_served_when_refresh_fails() {
        let (filesystem, source) = test_fs(Duration::from_millis(1));
        let (inode, _) = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap();
        assert_eq!(filesystem.metadata(inode).unwrap().size, 8);
        source
            .content
            .lock()
            .insert("obj-one".into(), Err("offline".into()));
        thread::sleep(Duration::from_millis(3));
        assert_eq!(filesystem.metadata(inode).unwrap().size, 8);
        let mut output = [0_u8; 16];
        let count = filesystem.read(inode, 0, &mut output).unwrap();
        assert_eq!(&output[..count], b"# Guide\n");
    }

    #[test]
    fn reopening_refreshes_expired_content() {
        let (filesystem, source) = test_fs(Duration::from_millis(1));
        let (inode, _) = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap();
        filesystem.open_file(inode, false).unwrap();

        source
            .content
            .lock()
            .insert("obj-one".into(), Ok(b"# Updated\n".to_vec()));
        thread::sleep(Duration::from_millis(3));
        filesystem.open_file(inode, false).unwrap();

        let mut output = [0_u8; 32];
        let count = filesystem.read(inode, 0, &mut output).unwrap();
        assert_eq!(&output[..count], b"# Updated\n");
    }

    #[test]
    fn unchanged_content_preserves_generation_and_mtime() {
        let (filesystem, _) = test_fs(Duration::from_millis(1));
        let (inode, _) = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap();
        let initial = filesystem.metadata(inode).unwrap();
        thread::sleep(Duration::from_millis(3));
        let refreshed = filesystem.metadata(inode).unwrap();

        assert_eq!(refreshed.generation, initial.generation);
        assert_eq!(refreshed.modified_ns, initial.modified_ns);
    }

    #[test]
    fn refresh_adds_nodes_without_renaming_existing_nodes() {
        let (filesystem, source) = test_fs(Duration::from_secs(60));
        source
            .children
            .lock()
            .insert("space-1".into(), vec![leaf("one", "Guide", "docx")]);
        filesystem.refresh_now().unwrap();
        let (first_inode, _) = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap();

        source.children.lock().insert(
            "space-1".into(),
            vec![leaf("one", "Guide", "docx"), leaf("two", "guide", "docx")],
        );
        source
            .content
            .lock()
            .insert("obj-two".into(), Ok(b"two".to_vec()));
        filesystem.refresh_now().unwrap();
        assert_eq!(
            filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0,
            first_inode
        );
        assert!(
            filesystem
                .tree_lines()
                .iter()
                .any(|line| line.starts_with("guide~") && line.ends_with(".md"))
        );
    }

    #[test]
    fn directory_verifier_detects_refresh() {
        struct Sink;
        impl DirectorySink for Sink {
            fn push(&mut self, _entry: FskDirectoryEntry<'_>) -> bool {
                true
            }
        }

        let (filesystem, _) = test_fs(Duration::from_secs(60));
        let mut sink = Sink;
        let first = filesystem
            .read_directory(ROOT_INODE, 0, 0, &mut sink)
            .unwrap();
        filesystem.refresh_now().unwrap();
        assert_eq!(
            filesystem.read_directory(ROOT_INODE, 0, first.verifier, &mut sink),
            Err(FsError::STALE)
        );
    }
}

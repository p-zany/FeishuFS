use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Weak, mpsc},
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
};
use parking_lot::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::{
    assets::AssetStore,
    link::KnowledgeLink,
    refresh::{FetchSlots, RefreshBackoff},
    remote::{ContentBundle, ContentRequest, RemoteSource, RemoteSpace, RemoteTree},
    tree::{FsState, VirtualNode, VirtualNodeKind, directory_entries, file_request},
};

const NOT_FOUND: FsError = FsError(2);
const ACCESS_DENIED: FsError = FsError(13);
const NOT_DIRECTORY: FsError = FsError(20);
const IS_DIRECTORY: FsError = FsError(21);

#[derive(Clone, Debug)]
pub struct FsOptions {
    /// Optional internal backing cache for downloaded attachments and images.
    /// This is not the visible `.feishufs-assets` directory in the mount.
    /// The CLI uses the user's cache directory; library callers can opt in.
    pub asset_directory: Option<PathBuf>,
    pub cache_ttl: Duration,
    pub content_retry_interval: Duration,
    pub max_concurrent_fetches: usize,
    pub max_nodes: usize,
}

impl Default for FsOptions {
    fn default() -> Self {
        Self {
            asset_directory: None,
            cache_ttl: Duration::from_secs(60),
            content_retry_interval: Duration::from_secs(5),
            max_concurrent_fetches: 4,
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
    space_id: String,
    state: RwLock<FsState>,
    content: Mutex<HashMap<u64, CachedContent>>,
    failures: Mutex<HashMap<u64, FetchFailure>>,
    fetch_gates: Mutex<HashMap<u64, Weak<Mutex<()>>>>,
    fetch_slots: FetchSlots,
    publish_gate: Mutex<()>,
    refresh_gate: Mutex<()>,
    cache_ttl: Duration,
    content_retry_interval: Duration,
    max_nodes: usize,
    asset_store: Option<AssetStore>,
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

struct FetchFailure {
    request: ContentRequest,
    retry_at: Instant,
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
        for (name, duration) in [
            ("cache_ttl", options.cache_ttl),
            ("content_retry_interval", options.content_retry_interval),
        ] {
            if duration.is_zero() || Instant::now().checked_add(duration).is_none() {
                bail!("{name} must be a positive, representable duration");
            }
        }
        if options.max_nodes == 0 || options.max_concurrent_fetches == 0 {
            bail!("max_nodes and max_concurrent_fetches must be greater than zero");
        }
        let tree = RemoteTree::load(source.as_ref(), &link, options.max_nodes)?;
        let state = FsState::from_remote(&tree, now_ns())
            .context("Failed to build the initial filesystem snapshot")?;
        let asset_store = options
            .asset_directory
            .as_deref()
            .map(|directory| {
                AssetStore::new(
                    directory,
                    link.url.host_str().context("Space URL has no hostname")?,
                    &tree.space_id,
                )
            })
            .transpose()?;
        Ok(Self {
            inner: Arc::new(Inner {
                source,
                link,
                space_id: tree.space_id,
                state: RwLock::new(state),
                content: Mutex::new(HashMap::new()),
                failures: Mutex::new(HashMap::new()),
                fetch_gates: Mutex::new(HashMap::new()),
                fetch_slots: FetchSlots::new(options.max_concurrent_fetches),
                publish_gate: Mutex::new(()),
                refresh_gate: Mutex::new(()),
                cache_ttl: options.cache_ttl,
                content_retry_interval: options.content_retry_interval,
                max_nodes: options.max_nodes,
                asset_store,
            }),
        })
    }

    /// Internal on-disk cache namespace, not the user-facing assets entry.
    #[must_use]
    pub fn asset_directory(&self) -> Option<&Path> {
        self.inner.asset_store.as_ref().map(AssetStore::root)
    }

    pub fn refresh_now(&self) -> Result<RefreshSummary> {
        let _refresh = self.inner.refresh_gate.lock();
        let space = RemoteSpace {
            space_id: self.inner.space_id.clone(),
            name: self.root_title(),
        };
        let tree = RemoteTree::load_space(
            self.inner.source.as_ref(),
            &space,
            &self.inner.link,
            self.inner.max_nodes,
        )?;
        // Traversal never holds a publication lock. Only complete snapshots are published.
        let publication = self.inner.publish_gate.lock();
        let changes = {
            let mut state = self.inner.state.write();
            state.replace(&tree)?.map(|_| state.file_inodes())
        };
        let revision_changed = changes.is_some();
        if let Some(active_files) = &changes {
            self.inner
                .content
                .lock()
                .retain(|inode, _| active_files.contains(inode));
            self.inner
                .failures
                .lock()
                .retain(|inode, _| active_files.contains(inode));
            self.inner
                .fetch_gates
                .lock()
                .retain(|inode, gate| active_files.contains(inode) || gate.strong_count() > 0);
        }
        drop(publication);
        Ok(RefreshSummary {
            node_count: tree.node_count(),
            revision_changed,
        })
    }

    #[must_use]
    pub fn start_refresher(&self, interval: Duration) -> RefreshHandle {
        assert!(
            !interval.is_zero()
                && Instant::now()
                    .checked_add(interval.saturating_mul(8))
                    .is_some(),
            "The refresh interval must be positive and representable"
        );
        let (stop_tx, stop_rx) = mpsc::channel();
        let filesystem = self.clone();
        let join = thread::Builder::new()
            .name("feishufs-refresh".into())
            .spawn(move || {
                let mut backoff = RefreshBackoff::new(interval);
                let mut delay = interval;
                while matches!(
                    stop_rx.recv_timeout(delay),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    match filesystem.refresh_now() {
                        Ok(summary) => {
                            delay = backoff.complete(true);
                            if summary.revision_changed {
                                info!(
                                    nodes = summary.node_count,
                                    "Space directory snapshot updated"
                                );
                            } else {
                                debug!(
                                    nodes = summary.node_count,
                                    "Space directory snapshot is unchanged"
                                );
                            }
                        }
                        Err(error) => {
                            delay = backoff.complete(false);
                            crate::logging::failure(
                                "Directory refresh failed; retaining the previous snapshot",
                                &error,
                            );
                            debug!(retry_in = ?delay, "Directory refresh retry scheduled");
                        }
                    }
                }
            })
            .expect("Failed to spawn the directory refresh thread");
        let workers = vec![(stop_tx, join)];
        RefreshHandle { workers }
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

    fn cached_content(
        &self,
        inode: u64,
        refresh_if_expired: bool,
    ) -> Result<Option<CachedContent>, FsError> {
        let _publication = self.inner.publish_gate.lock();
        let node = self.node(inode)?;
        let request = file_request(&node).map_err(|_| IS_DIRECTORY)?;
        let existing = self
            .inner
            .content
            .lock()
            .get(&inode)
            .cloned()
            .filter(|cached| &cached.request == request);
        let existing = existing.map(|cached| {
            let rendered = self.rerender_if_needed(inode, cached);
            self.inner.content.lock().insert(inode, rendered.clone());
            rendered
        });
        let cooling_down = self
            .inner
            .failures
            .lock()
            .get(&inode)
            .is_some_and(|failure| {
                &failure.request == request && Instant::now() < failure.retry_at
            });
        if let Some(cached) = existing {
            if !refresh_if_expired
                || request.is_inline()
                || cached.fetched_at.elapsed() < self.inner.cache_ttl
                || cooling_down
            {
                return Ok(Some(cached));
            }
        } else if cooling_down {
            return Err(FsError::IO);
        }
        Ok(None)
    }

    fn fetch_gate(&self, inode: u64) -> Arc<Mutex<()>> {
        let mut gates = self.inner.fetch_gates.lock();
        if let Some(gate) = gates.get(&inode).and_then(Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(Mutex::new(()));
        gates.insert(inode, Arc::downgrade(&gate));
        gate
    }

    fn ensure_content(
        &self,
        inode: u64,
        refresh_if_expired: bool,
    ) -> Result<CachedContent, FsError> {
        let inode = self.inner.state.read().content_inode(inode);
        loop {
            if let Some(cached) = self.cached_content(inode, refresh_if_expired)? {
                return Ok(cached);
            }
            let gate = self.fetch_gate(inode);
            let fetching = gate.lock();
            // Another reader may have completed the fetch while this one waited.
            if let Some(cached) = self.cached_content(inode, refresh_if_expired)? {
                return Ok(cached);
            }
            let permit = self.inner.fetch_slots.acquire();
            let node = self.node(inode)?;
            let request = file_request(&node).map_err(|_| IS_DIRECTORY)?.clone();
            let fetched = self.inner.source.fetch_bundle(&request).and_then(|bundle| {
                self.persist_downloads(&node, &bundle)?;
                Ok(bundle)
            });
            drop(permit);

            let publication = self.inner.publish_gate.lock();
            let current = self.node(inode)?;
            if file_request(&current).map_err(|_| IS_DIRECTORY)? != &request {
                // The response belongs to an obsolete identity. Discard it and retry.
                continue;
            }
            let existing = self.inner.content.lock().get(&inode).cloned();
            let fetched = fetched.and_then(|bundle| {
                self.publish_assets(inode, &request, &bundle)?;
                Ok(bundle.bytes)
            });
            match fetched {
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
                    self.inner.failures.lock().remove(&inode);
                    drop(publication);
                    drop(fetching);
                    return Ok(cached);
                }
                Err(error) => {
                    self.inner.failures.lock().insert(
                        inode,
                        FetchFailure {
                            request: request.clone(),
                            retry_at: Instant::now() + self.inner.content_retry_interval,
                        },
                    );
                    drop(publication);
                    drop(fetching);
                    if let Some(cached) = existing.filter(|cached| cached.request == request) {
                        crate::logging::node_failure(
                            inode,
                            "Content refresh failed; serving the last successful cache",
                            &error,
                        );
                        return Ok(cached);
                    }
                    crate::logging::node_failure(inode, "Initial content fetch failed", &error);
                    return Err(FsError::IO);
                }
            }
        }
    }

    fn persist_downloads(&self, node: &VirtualNode, bundle: &ContentBundle) -> Result<()> {
        let Some(store) = &self.inner.asset_store else {
            return Ok(());
        };
        let request = file_request(node)?;
        if matches!(request, ContentRequest::Download { .. }) {
            let name = node
                .path
                .rsplit('/')
                .next()
                .context("Attachment has no filename")?;
            store.persist(request, name, &bundle.bytes)?;
        }
        for asset in &bundle.assets {
            let extension = crate::docx::image_extension(&asset.bytes)?;
            if asset.name.rsplit_once('.').map(|(_, value)| value) != Some(extension) {
                bail!(
                    "Asset image format does not match its filename: {}",
                    asset.name
                );
            }
            store.persist(&asset.request, &asset.name, &asset.bytes)?;
        }
        Ok(())
    }

    fn publish_assets(
        &self,
        owner: u64,
        request: &ContentRequest,
        bundle: &ContentBundle,
    ) -> Result<()> {
        if !request.is_markdown() && bundle.assets.is_empty() {
            return Ok(());
        }
        for asset in &bundle.assets {
            let extension = crate::docx::image_extension(&asset.bytes)?;
            if asset.name.rsplit_once('.').map(|(_, value)| value) != Some(extension) {
                bail!(
                    "Asset image format does not match its filename: {}",
                    asset.name
                );
            }
        }
        let specs = bundle
            .assets
            .iter()
            .map(|asset| (asset.name.clone(), asset.request.clone()))
            .collect();
        let mut state = self.inner.state.write();
        state.set_assets(owner, request, specs)?;
        let files = state.asset_files(owner);
        let active = state.file_inodes();
        let mut content = self.inner.content.lock();
        content.retain(|inode, _| active.contains(inode));
        let mut failures = self.inner.failures.lock();
        failures.retain(|inode, _| active.contains(inode));
        for asset in &bundle.assets {
            let inode = files[&asset.name];
            failures.remove(&inode);
            let previous = content.get(&inode);
            let unchanged = previous.is_some_and(|cached| {
                cached.request == asset.request && cached.bytes.as_ref() == asset.bytes
            });
            let generation = previous.map_or(1, |cached| {
                if unchanged {
                    cached.generation
                } else {
                    cached.generation.saturating_add(1)
                }
            });
            let modified_ns = previous
                .filter(|_| unchanged)
                .map_or_else(now_ns, |cached| cached.modified_ns);
            let bytes: Arc<[u8]> = Arc::from(asset.bytes.clone());
            content.insert(
                inode,
                CachedContent {
                    request: asset.request.clone(),
                    source_bytes: bytes.clone(),
                    bytes,
                    fetched_at: Instant::now(),
                    modified_ns,
                    generation,
                    tree_revision: state.revision(),
                },
            );
        }
        Ok(())
    }

    fn render_source(
        &self,
        inode: u64,
        request: &ContentRequest,
        source: Arc<[u8]>,
    ) -> (Arc<[u8]>, u64) {
        let state = self.inner.state.read();
        let revision = state.revision();
        if !request.needs_link_rendering() {
            return (source, revision);
        }
        let rendered = state.render_content(inode, request, &source);
        (Arc::from(rendered), revision)
    }

    fn rerender_if_needed(&self, inode: u64, cached: CachedContent) -> CachedContent {
        if !cached.request.needs_link_rendering() {
            return cached;
        }
        let state = self.inner.state.read();
        if state.revision() == cached.tree_revision {
            return cached;
        }
        let revision = state.revision();
        let rendered = state.render_content(inode, &cached.request, &cached.source_bytes);
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
    workers: Vec<(mpsc::Sender<()>, thread::JoinHandle<()>)>,
}

impl Drop for RefreshHandle {
    fn drop(&mut self) {
        for (stop_tx, _) in &self.workers {
            let _ = stop_tx.send(());
        }
        for (_, join) in self.workers.drain(..) {
            if join.join().is_err() {
                warn!("A refresh worker panicked during shutdown");
            }
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

fn missing_lookup_error() -> FsError {
    NOT_FOUND
}

fn platform_name_eq(left: &[u8], right: &[u8]) -> bool {
    left == right
}

#[cfg(test)]
#[path = "attachment_tests.rs"]
mod attachment_tests;

#[cfg(test)]
#[path = "refresh_tests.rs"]
mod refresh_tests;

#[cfg(test)]
#[path = "table_tests.rs"]
mod table_tests;

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
                ..FsOptions::default()
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

    fn picture_bundle() -> ContentBundle {
        ContentBundle {
            bytes: "# Guide\n\n![Whiteboard](feishufs-asset:62.jpg)\n"
                .as_bytes()
                .to_vec(),
            assets: vec![crate::remote::ContentAsset {
                name: "62.jpg".into(),
                request: ContentRequest::Whiteboard { token: "b".into() },
                bytes: b"\xff\xd8\xffimage".to_vec(),
            }],
        }
    }

    fn read_all(filesystem: &FeishuFs, inode: u64) -> Vec<u8> {
        let mut bytes = vec![0; 4096];
        let count = filesystem.read(inode, 0, &mut bytes).unwrap();
        bytes.truncate(count);
        bytes
    }

    fn asset_inode(filesystem: &FeishuFs, owner: u64) -> u64 {
        let root = filesystem
            .lookup(ROOT_INODE, b".feishufs-assets")
            .unwrap()
            .0;
        let directory = filesystem
            .lookup(root, owner.to_string().as_bytes())
            .unwrap()
            .0;
        filesystem.lookup(directory, b"62.jpg").unwrap().0
    }

    #[test]
    fn publishes_images_inside_the_read_only_mount_without_changing_leaf_layout() {
        let (filesystem, source) = test_fs(Duration::from_secs(60));
        source
            .bundles
            .lock()
            .insert("obj-one".into(), picture_bundle());
        assert!(filesystem.lookup(ROOT_INODE, b".feishufs-assets").is_err());
        let owner = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
        let body = String::from_utf8(read_all(&filesystem, owner)).unwrap();
        assert!(body.contains(&format!("![Whiteboard](<.feishufs-assets/{owner}/62.jpg>)")));
        let image = asset_inode(&filesystem, owner);
        assert_eq!(read_all(&filesystem, image), b"\xff\xd8\xffimage");
        assert_eq!(filesystem.metadata(image).unwrap().mode, 0o444);
        assert_eq!(filesystem.write(image, 0, b"no"), Err(ACCESS_DENIED));
        assert!(filesystem.open_file(image, true).is_err());
        assert_eq!(filesystem.node_count(), 1);
    }

    #[test]
    fn asset_inodes_survive_rename_move_and_leaf_to_branch_transition() {
        let (filesystem, source) = test_fs(Duration::from_secs(3600));
        source
            .bundles
            .lock()
            .insert("obj-one".into(), picture_bundle());
        let owner = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
        read_all(&filesystem, owner);
        let image = asset_inode(&filesystem, owner);
        source
            .children
            .lock()
            .insert("space-1".into(), vec![node("one", "Renamed", "docx")]);
        source
            .children
            .lock()
            .insert("one".into(), vec![leaf("child", "Child", "docx")]);
        filesystem.refresh_now().unwrap();
        let directory = filesystem.lookup(ROOT_INODE, b"Renamed").unwrap().0;
        assert_eq!(filesystem.lookup(directory, b"index.md").unwrap().0, owner);
        assert_eq!(asset_inode(&filesystem, owner), image);
        assert!(
            String::from_utf8(read_all(&filesystem, owner))
                .unwrap()
                .contains(&format!("<../.feishufs-assets/{owner}/62.jpg>"))
        );
        assert_eq!(source.fetches.lock().len(), 1);
        source
            .children
            .lock()
            .insert("space-1".into(), vec![node("parent", "Parent", "docx")]);
        source
            .children
            .lock()
            .insert("parent".into(), vec![leaf("one", "Renamed", "docx")]);
        filesystem.refresh_now().unwrap();
        let parent = filesystem.lookup(ROOT_INODE, b"Parent").unwrap().0;
        assert_eq!(filesystem.lookup(parent, b"Renamed.md").unwrap().0, owner);
        assert_eq!(asset_inode(&filesystem, owner), image);
        assert!(
            String::from_utf8(read_all(&filesystem, owner))
                .unwrap()
                .contains("<../.feishufs-assets/")
        );
    }

    #[test]
    fn refreshed_mermaid_body_removes_obsolete_images_but_never_reuses_inodes() {
        let (filesystem, source) = test_fs(Duration::from_millis(1));
        source
            .bundles
            .lock()
            .insert("obj-one".into(), picture_bundle());
        let owner = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
        read_all(&filesystem, owner);
        let image = asset_inode(&filesystem, owner);
        source.bundles.lock().insert(
            "obj-one".into(),
            b"```mermaid\nA --> B\n```\n".to_vec().into(),
        );
        thread::sleep(Duration::from_millis(3));
        filesystem.open_file(owner, false).unwrap();
        assert!(filesystem.lookup(ROOT_INODE, b".feishufs-assets").is_err());
        assert!(filesystem.metadata(image).is_err());
        source
            .bundles
            .lock()
            .insert("obj-one".into(), picture_bundle());
        thread::sleep(Duration::from_millis(3));
        filesystem.open_file(owner, false).unwrap();
        assert_eq!(asset_inode(&filesystem, owner), image);
    }

    #[test]
    fn removing_a_document_or_changing_its_identity_removes_its_assets() {
        let (filesystem, source) = test_fs(Duration::from_secs(60));
        source
            .bundles
            .lock()
            .insert("obj-one".into(), picture_bundle());
        let owner = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
        read_all(&filesystem, owner);
        let mut changed = leaf("one", "Guide", "docx");
        changed.object_token = Some("different-object".into());
        source
            .children
            .lock()
            .insert("space-1".into(), vec![changed]);
        filesystem.refresh_now().unwrap();
        assert!(filesystem.lookup(ROOT_INODE, b".feishufs-assets").is_err());
        source.children.lock().insert("space-1".into(), vec![]);
        filesystem.refresh_now().unwrap();
        assert!(filesystem.lookup(ROOT_INODE, b"Guide.md").is_err());
    }

    #[test]
    fn reopening_an_image_refreshes_its_bytes() {
        let (filesystem, source) = test_fs(Duration::from_millis(1));
        source
            .bundles
            .lock()
            .insert("obj-one".into(), picture_bundle());
        let owner = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
        read_all(&filesystem, owner);
        let image = asset_inode(&filesystem, owner);
        source
            .content
            .lock()
            .insert("b".into(), Ok(b"\xff\xd8\xffupdated".to_vec()));
        thread::sleep(Duration::from_millis(3));
        filesystem.open_file(image, false).unwrap();
        assert_eq!(read_all(&filesystem, image), b"\xff\xd8\xffupdated");
    }

    #[test]
    fn concurrent_first_reads_publish_only_one_bundle() {
        let (filesystem, source) = test_fs(Duration::from_secs(60));
        source
            .bundles
            .lock()
            .insert("obj-one".into(), picture_bundle());
        let owner = filesystem.lookup(ROOT_INODE, b"Guide.md").unwrap().0;
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let filesystem = filesystem.clone();
                thread::spawn(move || read_all(&filesystem, owner))
            })
            .collect();
        for handle in handles {
            assert!(!handle.join().unwrap().is_empty());
        }
        assert_eq!(source.fetches.lock().len(), 1);
    }

    #[test]
    fn directory_verifier_detects_refresh() {
        struct Sink;
        impl DirectorySink for Sink {
            fn push(&mut self, _entry: FskDirectoryEntry<'_>) -> bool {
                true
            }
        }

        let (filesystem, source) = test_fs(Duration::from_secs(60));
        let mut sink = Sink;
        let first = filesystem
            .read_directory(ROOT_INODE, 0, 0, &mut sink)
            .unwrap();
        source
            .children
            .lock()
            .insert("space-1".into(), vec![leaf("one", "Renamed", "docx")]);
        filesystem.refresh_now().unwrap();
        assert_eq!(
            filesystem.read_directory(ROOT_INODE, 0, first.verifier, &mut sink),
            Err(FsError::STALE)
        );
    }
}

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tracing::{error, info};

use crate::FileSync;
use crate::dbs::SnapshotStore;
use crate::error::Error;
use crate::file_picker::{FFFMode, FilePickerIgnoreOptions};
use crate::index::{
    BigramBuildStats, RowSink, RowSource, build_bigram_index, sniff_binary_for_non_indexable,
};
use crate::parallelism::BACKGROUND_THREAD_POOL;
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::types::ContentCacheBudget;
use crate::watch::{BackgroundWatcher, WatcherOptions};

#[derive(Clone, Default)]
pub(crate) struct ScanSignals {
    /// Set to `true` while any scan phase is running
    pub(crate) scanning: Arc<AtomicBool>,
    /// Set to `true` once the filesystem watcher has been installed
    pub(crate) watcher_ready: Arc<AtomicBool>,
    /// Indicates that that owning picker was requested to shut down
    pub(crate) cancelled: Arc<AtomicBool>,
    /// Used to resolve conflicts if multiple rescans were triggered in a queue
    pub(crate) rescan_pending: Arc<AtomicBool>,
    /// Set by `post_scan_snapshot`, cleared by `PostScanSnapshot::drop`.
    /// DO NOT set or clear this manually — it is managed exclusively by the
    /// PostScanSnapshot lifecycle.
    pub(crate) post_scan_indexing_active: Arc<AtomicBool>,
}

/// Which optional phases a scan should run.
#[derive(Clone, Default)]
pub(crate) struct ScanConfig {
    pub(crate) warmup: bool,
    pub(crate) content_indexing: bool,
    pub(crate) watch: bool,
    pub(crate) auto_cache_budget: bool,
    pub(crate) install_watcher: bool,
    pub(crate) follow_symlinks: bool,
    pub(crate) ignore_options: FilePickerIgnoreOptions,
    pub(crate) enable_fs_root_scanning: bool,
    pub(crate) enable_home_dir_scanning: bool,
    /// `None` means scans leave no Snapshot on disk and behave exactly as before.
    pub(crate) snapshot_store: Option<Arc<SnapshotStore>>,
}

/// A fully-configured scan job ready to run on a background thread.
///
/// Build with [`ScanJob::from_picker`] (reads all state from the
/// current `FilePicker`) or [`ScanJob::initial`] (for the bootstrap
/// scan, before the picker is published to `SharedPicker`).
pub(crate) struct ScanJob {
    shared_picker: SharedFilePicker,
    shared_frecency: SharedFrecency,
    base_path: PathBuf,
    mode: FFFMode,
    signals: ScanSignals,
    config: ScanConfig,
    /// Walker-maintained counter backing `get_scan_progress` on the UI
    /// side. Reset to 0 at scan start, incremented per-file by the
    /// walker. Shared `Arc` so the UI polls the same atomic.
    scanned_files_counter: Arc<AtomicUsize>,
    trace_span: tracing::Span,
}

impl ScanJob {
    pub fn new_rescan(
        shared_picker: &SharedFilePicker,
        shared_frecency: &SharedFrecency,
    ) -> Result<Option<Self>, Error> {
        let guard = shared_picker.read()?;
        let picker = guard.as_ref().ok_or(Error::FilePickerMissing)?;

        if picker.is_scan_active()
            || picker
                .signals
                .post_scan_indexing_active
                .load(Ordering::Acquire)
        {
            return Ok(None);
        }

        let mode = picker.mode();
        let signals = picker.scan_signals();
        let scanned_files_counter = picker.scanned_files_counter();
        let base_path = picker.base_path().to_path_buf();
        let trace_span = picker.trace_span();

        let new_scan_config = ScanConfig {
            warmup: picker.has_mmap_cache(),
            content_indexing: picker.has_content_indexing(),
            watch: picker.has_watcher(),
            auto_cache_budget: !picker.has_explicit_cache_budget(),
            install_watcher: false, // the watcher is independent of rescan, it is not restarting EVER
            follow_symlinks: picker.follows_symlinks(),
            ignore_options: picker.ignore_options().clone(),
            enable_fs_root_scanning: picker.fs_root_scanning_enabled(),
            enable_home_dir_scanning: picker.home_dir_scanning_enabled(),
            snapshot_store: picker.snapshot_store(),
        };

        drop(guard); // just a sanity check

        Ok(Some(Self {
            mode,
            signals,
            base_path,
            scanned_files_counter,
            config: new_scan_config,
            shared_picker: shared_picker.clone(),
            shared_frecency: shared_frecency.clone(),
            trace_span,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_initial(
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        base_path: PathBuf,
        mode: FFFMode,
        signals: ScanSignals,
        scanned_files_counter: Arc<AtomicUsize>,
        trace_span: tracing::Span,
        config: ScanConfig,
    ) -> Self {
        Self {
            shared_picker,
            shared_frecency,
            base_path,
            mode,
            signals,
            scanned_files_counter,
            config,
            trace_span,
        }
    }

    /// Run the job on `BACKGROUND_THREAD_POOL`. Returns immediately.
    ///
    /// Routed through the pool — and not a fresh `std::thread::spawn` — so the
    /// orchestrator inherits rayon's QoS pin (USER_INITIATED). Without that
    /// pin, an interactive nvim's USER_INTERACTIVE main thread spawns a child
    /// at lower QoS, the walker's Zig worker pool inherits the demotion, and
    /// the kernel drifts those workers onto E-cores. On chromium that turns a
    /// ~800 ms walk into ~3 s.
    pub fn spawn(self) {
        self.signals.scanning.store(true, Ordering::Release);
        let span = self.trace_span.clone();
        BACKGROUND_THREAD_POOL.spawn(move || {
            let _g = span.enter();
            self.run();
        });
    }

    fn run(self) {
        let Self {
            shared_picker,
            shared_frecency,
            base_path,
            mode,
            signals,
            scanned_files_counter,
            config,
            trace_span: _,
        } = self;

        let _scanning = ScanningGuard::new(&signals);
        scanned_files_counter.store(0, Ordering::Relaxed);

        // 1. Walk the file system and collect the list of files
        let git = FileSync::discover_git(&base_path, config.ignore_options.disable_git);
        let sync = match FileSync::walk_filesystem(
            &base_path,
            git.clone(),
            &scanned_files_counter,
            &shared_frecency,
            mode,
            config.follow_symlinks,
            &config.ignore_options,
        ) {
            Ok(sync) => sync,
            Err(e) => {
                error!(?e, "scan walk failed");
                return;
            }
        };

        // 2. Populate the file list
        let git_status_worker;
        let previous;
        if let Ok(mut guard) = shared_picker.write()
            && let Some(picker) = guard.as_mut()
        {
            if signals.cancelled.load(Ordering::Acquire) {
                info!("scan cancelled between walk and commit, discarding");
                return;
            }

            let live_count = sync.live_count;
            previous = picker.commit_new_sync(sync);
            git_status_worker = Arc::clone(&picker.git_status_worker);

            if config.auto_cache_budget && !picker.has_explicit_cache_budget() {
                picker.set_cache_budget(ContentCacheBudget::new_for_repo(live_count));
            }
        } else {
            error!("failed to install scan results into picker");
            return;
        }

        // Spawn the git status worker once. BUG PINNNING. If the user initiated git in the folder
        // which is a real use case we need to have a way to start the git worker background thread dynamically
        if git.is_some() && !signals.cancelled.load(Ordering::Acquire) {
            git_status_worker.spawn_once(shared_picker.weaken(), shared_frecency.clone());
            git_status_worker.request_full_rescan(); // this runs anyway
        }

        // BUG pinning: take the snapshot *before* the storing the scan=true, otherwise there is a tiny
        // race window when there scanned is set to true, but `post_scan_indexing_active` flag is `false`
        let snapshot = if !signals.cancelled.load(Ordering::Acquire) {
            shared_picker.read().ok().and_then(|guard| {
                guard
                    .as_ref()
                    .and_then(|picker| unsafe { picker.post_scan_snapshot() })
            })
        } else {
            None
        };

        signals.scanning.store(false, Ordering::Relaxed); // file are searchable

        // in case we do a rescan, we have to resubscribe a watcher to the new set of directories
        // all the already watched directories are not going to be resubscribed (this is internally deduped)
        if !config.install_watcher && !signals.cancelled.load(Ordering::Acquire) {
            rescubscribe_watcher_post_scan(&shared_picker);
        }

        // The post scan skips files the previous index already read as text and that did not change
        let known_text = shared_picker
            .read()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .filter(|picker| picker.base_path() == base_path)
                    .map(|picker| picker.unchanged_text_files(&previous))
            })
            .unwrap_or_default();
        drop(previous);

        // 3. Runs post scna in parallel with git status collection
        if !signals.cancelled.load(Ordering::Acquire)
            && let Some(snap) = snapshot.as_ref()
        {
            Self::run_post_scan(&shared_picker, &signals, &config, snap, &known_text);
        }

        drop(snapshot); // SNAPSHOT SHOULD NOT BE USED AFTER THIS POINT

        // 5. Install filesystem watcher (initial scan only).
        if config.install_watcher && config.watch && !signals.cancelled.load(Ordering::Acquire) {
            let shared_picker: &SharedFilePicker = &shared_picker;
            let shared_frecency: &SharedFrecency = &shared_frecency;
            let base_path: &std::path::Path = &base_path;

            match BackgroundWatcher::new(
                base_path.to_path_buf(),
                git,
                shared_picker.clone(),
                shared_frecency.clone(),
                WatcherOptions {
                    mode,
                    follow_symlinks: config.follow_symlinks,
                    ignore_options: config.ignore_options,
                    enable_fs_root_scanning: config.enable_fs_root_scanning,
                    enable_home_dir_scanning: config.enable_home_dir_scanning,
                },
                git_status_worker,
                tracing::Span::current(),
            ) {
                Ok(watcher) => {
                    if let Ok(mut guard) = shared_picker.write()
                        && let Some(picker) = guard.as_mut()
                        && picker.base_path() == base_path
                        && !signals.cancelled.load(Ordering::Acquire)
                    {
                        picker.background_watcher = Some(watcher);
                        signals.watcher_ready.store(true, Ordering::Release);
                    }
                }
                Err(e) => error!(?e, "failed to initialize background watcher"),
            };
        }

        // 6. Drain any rescan that arrived while we were busy.
        // if user initiated a new rescan we had no way to cancel current post scan, so do it again
        if !signals.cancelled.load(Ordering::Acquire)
            && signals.rescan_pending.swap(false, Ordering::AcqRel)
        {
            match Self::new_rescan(&shared_picker, &shared_frecency) {
                Ok(Some(follow_up)) => {
                    info!("Rescheduling deferred rescan after current scan finished");
                    follow_up.spawn();
                }
                Ok(None) => {
                    // this should be practically impossible because we do not have any
                    // queue, but if somehow a new rescan was triggered JUST IN THIS MOMENT
                    // just ignore it because the ongoing one is fresh enough
                    tracing::warn!("Post scan was re-triggered, ignoring");
                }
                Err(e) => {
                    error!(?e, "Failed to reschedule deferred rescan");
                }
            }
        }
    }

    /// THIS IS VERY VERY IMPORTANT THAT ANYTHING INSIDE THIS FUNCTION TO NOT READ ANYTHING CLEARABLE OUTSIDE
    /// this is a very silly off lock implementation that actually matters, and that's why it is crafted
    /// to never read anything from the picker, it can only WRITE information using single instructions
    ///
    /// Things that are safe and immutable - file list, indexes of files, paths, and signals.
    #[tracing::instrument(skip_all, fields(warmup = ?config.warmup, indexing = ?config.content_indexing))]
    fn run_post_scan(
        shared_picker: &SharedFilePicker,
        signals: &ScanSignals,
        config: &ScanConfig,
        unsafe_snapshot: &crate::file_picker::PostScanUnsafeSnapshot,
        known_text: &[u64],
    ) -> BigramBuildStats {
        let Some(arena) = unsafe_snapshot
            .arena // we are never touching overlays so this arena is always correct
            .as_ref()
            .map(|s| s.as_arena_ptr())
        else {
            tracing::error!("Failed to run post scan: arena is invalid");
            return BigramBuildStats::default();
        };

        let files: &[crate::types::FileItem] = &unsafe_snapshot.files[..unsafe_snapshot.base_count];
        // Nanosecond identities for the same range; a length mismatch can only
        // mean overflow files leaked in, so fall back to "no identity".
        let mtime_ns = unsafe_snapshot
            .mtime_ns
            .get(..files.len())
            .unwrap_or_default();
        if signals.cancelled.load(Ordering::Acquire) {
            return BigramBuildStats::default();
        }

        if config.content_indexing {
            let rows = config
                .snapshot_store
                .as_ref()
                .map(|store| RowSink::new(store, &unsafe_snapshot.base_path));
            let source = config
                .snapshot_store
                .as_ref()
                .map(|store| RowSource::new(store, &unsafe_snapshot.base_path));
            let indexable_count = unsafe_snapshot.indexable_count.min(files.len());
            let (indexable_files, non_indexable_files) = files.split_at(indexable_count);
            let (index, mut stats) = build_bigram_index(
                indexable_files,
                &mtime_ns[..indexable_count],
                &unsafe_snapshot.base_path,
                arena,
                rows.as_ref(),
                source.as_ref(),
            );

            // A root change may have replaced the picker while this index was building.
            if let Ok(mut guard) = shared_picker.write()
                && let Some(picker) = guard.as_mut()
                && !signals.cancelled.load(Ordering::Acquire)
                && picker.base_path() == unsafe_snapshot.base_path
            {
                picker.set_bigram_index(index);
            }
            crate::index::release_thread_buffers();

            // Bigram only sniffs files <= MAX_INDEXABLE_FILE_SIZE; large
            // unknown-extension binaries slip past it and would otherwise be
            // grep-able as text. Cheap header sniff catches those.
            if !signals.cancelled.load(Ordering::Acquire) {
                sniff_binary_for_non_indexable(
                    non_indexable_files,
                    indexable_count,
                    known_text,
                    mtime_ns,
                    &unsafe_snapshot.base_path,
                    arena,
                    &signals.cancelled,
                    source.as_ref(),
                    rows.as_ref(),
                    &mut stats,
                );
            }

            // Every row is already queued; the sweep is what drops the ones whose
            // files disappeared and the only pass that knows what remains.
            if let (Some(rows), Some(store)) = (&rows, config.snapshot_store.as_ref()) {
                let written = rows.finish();
                if source.as_ref().is_some_and(RowSource::was_current) {
                    store.sweep(&unsafe_snapshot.base_path, scan_keys(files, arena));
                } else {
                    store.queue_meta(&unsafe_snapshot.base_path, written);
                }
                log_snapshot_scan(store, &unsafe_snapshot.base_path, &stats);
            }

            return stats;
        }

        // this potentially a long running as we are not parallelizing it but it's okay
        let mut stats = BigramBuildStats::default();
        sniff_binary_for_non_indexable(
            files,
            0,
            known_text,
            mtime_ns,
            &unsafe_snapshot.base_path,
            arena,
            &signals.cancelled,
            None,
            None,
            &mut stats,
        );

        // TODO Skipped as potentially unsafe - figure this out later
        // if config.warmup && !signals.cancelled.load(Ordering::Acquire) {
        //     warmup_mmaps(files, budget, &unsafe_snapshot.base_path, arena);
        // }
        stats
    }
}

// Ensures early returns clear the scanning signal.
struct ScanningGuard<'a> {
    signals: &'a ScanSignals,
}

impl<'a> ScanningGuard<'a> {
    fn new(signals: &'a ScanSignals) -> Self {
        signals.scanning.store(true, Ordering::Relaxed);
        Self { signals }
    }
}

impl Drop for ScanningGuard<'_> {
    fn drop(&mut self) {
        self.signals.scanning.store(false, Ordering::Relaxed);
    }
}

/// If the scan encounters new directories created we have to add them to the watch list
/// this is fine because the watcher does deduplicate the entries and doesn't add a lot of
/// garbage notify watchers / fs events streams
#[tracing::instrument(skip_all)]
fn rescubscribe_watcher_post_scan(shared_picker: &SharedFilePicker) {
    let Ok(guard) = shared_picker.read() else {
        return;
    };
    let Some(picker) = guard.as_ref() else {
        return;
    };
    let Some(watcher) = picker.background_watcher.as_ref() else {
        return;
    };

    picker.for_each_dir(|dir: &std::path::Path| {
        watcher.request_watch_dir(dir.to_path_buf());
        std::ops::ControlFlow::Continue(())
    });
}

// Relative-path hashes of the scan's file set: a row absent from this is an orphan.
fn scan_keys(
    files: &[crate::types::FileItem],
    arena: crate::simd_path::ArenaPtr,
) -> HashSet<[u8; 32]> {
    let mut buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
    files
        .iter()
        .map(|file| {
            let rel = crate::index::snapshot_write::relative_path(file, arena, &mut buf);
            *blake3::hash(rel.as_bytes()).as_bytes()
        })
        .collect()
}

// One line per scan, over both row shapes; a latched store has nothing to say.
fn log_snapshot_scan(store: &SnapshotStore, root: &std::path::Path, stats: &BigramBuildStats) {
    if store.is_disabled() {
        return;
    }
    let hits = stats.row_hits + stats.sniff_hits;
    let misses = stats.row_misses + stats.sniffed.saturating_sub(stats.sniff_hits);
    let asked = hits + misses;
    let hit_rate = if asked == 0 {
        0.0
    } else {
        hits as f64 / asked as f64
    };
    store.record_scan(hits, misses);
    info!(
        root = %root.display(),
        hits,
        misses,
        content_reads = stats.content_reads,
        sniffed = stats.sniffed,
        hit_rate,
        "snapshot hit rate"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_picker::{FilePicker, FilePickerOptions};

    fn collected(root: &std::path::Path) -> FilePicker {
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: root.to_string_lossy().into_owned(),
            enable_mmap_cache: false,
            enable_content_indexing: true,
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();
        picker
    }

    fn scan_with_store(
        root: &std::path::Path,
        store: &std::sync::Arc<SnapshotStore>,
    ) -> (u64, u64) {
        let (_shared, _stats) = run_scan(root, Some(store));
        (
            store.row_count(root).unwrap(),
            store.logical_bytes(root).unwrap(),
        )
    }

    // A whole scan: walk, then post-scan. Returns the picker so tests can grep it.
    fn run_scan(
        root: &std::path::Path,
        store: Option<&std::sync::Arc<SnapshotStore>>,
    ) -> (SharedFilePicker, BigramBuildStats) {
        let result = scan_only(root, store);
        // Writes are off the scan thread; a test reads them back immediately.
        if let Some(store) = store {
            assert!(store.flush(std::time::Duration::from_secs(60)));
        }
        result
    }

    // The scan itself, with the writes still in flight.
    fn scan_only(
        root: &std::path::Path,
        store: Option<&std::sync::Arc<SnapshotStore>>,
    ) -> (SharedFilePicker, BigramBuildStats) {
        let shared = SharedFilePicker::default();
        let picker = collected(root);
        let signals = picker.scan_signals();
        *shared.write().unwrap() = Some(picker);
        let snapshot = shared
            .read()
            .unwrap()
            .as_ref()
            .and_then(|picker| unsafe { picker.post_scan_snapshot() })
            .unwrap();

        let config = ScanConfig {
            content_indexing: true,
            snapshot_store: store.cloned(),
            ..Default::default()
        };
        let stats = ScanJob::run_post_scan(&shared, &signals, &config, &snapshot, &[]);
        drop(snapshot);
        (shared, stats)
    }

    // (relative path, line, text) — stable across two scans of one tree.
    fn matches(shared: &SharedFilePicker, pattern: &str) -> Vec<(String, u64, String)> {
        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let arena = picker.arena_base_ptr();
        let query = crate::grep::parse_grep_query(pattern);
        let options = crate::grep::GrepSearchOptions {
            time_budget_ms: 0,
            ..Default::default()
        };
        let result = picker.grep(&query, &options);
        let mut found: Vec<_> = result
            .matches
            .iter()
            .map(|m| {
                (
                    result.files[m.file_index].relative_path(arena),
                    m.line_number,
                    m.line_content.clone(),
                )
            })
            .collect();
        found.sort();
        found
    }

    fn mtime_ns(path: &std::path::Path) -> u64 {
        crate::walk::mtime_ns(std::fs::metadata(path).unwrap().modified().ok())
    }

    fn set_mtime(path: &std::path::Path, at: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    fn fixture(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("alpha.txt"), "alpha_marker one\n").unwrap();
        std::fs::write(root.join("beta.txt"), "beta_marker two\n").unwrap();
        std::fs::write(root.join("sub").join("gamma.txt"), "gamma_marker three\n").unwrap();
        // Too short to contribute a bigram, but still indexable: it must be a
        // hit too, or the walk's text files would not all come from rows.
        std::fs::write(root.join("tiny.txt"), "x").unwrap();
    }

    #[test]
    fn a_scan_leaves_one_row_per_file_and_a_meta_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "alpha_marker\n").unwrap();
        std::fs::write(root.join("b.txt"), "beta_marker\n").unwrap();
        std::fs::write(root.join("empty.txt"), "").unwrap();

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (rows, bytes) = scan_with_store(root, &store);

        assert_eq!(rows, 3, "one row per file, empty file included");
        let meta = store.read_meta(root).unwrap().expect("meta row");
        assert_eq!(meta.schema, crate::dbs::SNAPSHOT_SCHEMA);
        assert_eq!(meta.logical_bytes, bytes);
        assert!(bytes > 0);
        assert!(meta.last_used > 0);
    }

    // Normalization folds case, so only 69 printable bytes remain: at most
    // 69 * 69 = 4761 keys, i.e. no real file can exceed the row's key cap.
    #[test]
    fn a_file_with_every_printable_bigram_still_fits_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("ok.txt"), "ordinary content\n").unwrap();
        let mut wide = String::new();
        for hi in 32u8..=126 {
            for lo in 32u8..=126 {
                wide.push(hi as char);
                wide.push(lo as char);
            }
        }
        std::fs::write(root.join("wide.txt"), wide).unwrap();

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (rows, bytes) = scan_with_store(root, &store);

        assert_eq!(rows, 2);
        assert!(bytes > 0);
        assert!(store.read_meta(root).unwrap().is_some());
    }

    #[test]
    fn replaced_picker_does_not_install_its_bigram_index_into_the_successor() {
        let old_root = tempfile::tempdir().unwrap();
        // The stale index tracks the marker, but only for a file the new root does not have.
        std::fs::write(old_root.path().join("a.txt"), "unrelated words").unwrap();
        std::fs::write(old_root.path().join("b.txt"), "isolated_marker").unwrap();
        let new_root = tempfile::tempdir().unwrap();
        std::fs::write(new_root.path().join("new.txt"), "isolated_marker").unwrap();

        let shared = SharedFilePicker::default();
        let old = collected(old_root.path());
        let signals = old.scan_signals();
        *shared.write().unwrap() = Some(old);
        let snapshot = shared
            .read()
            .unwrap()
            .as_ref()
            .and_then(|picker| unsafe { picker.post_scan_snapshot() })
            .unwrap();
        // The root changes while the old post-scan is still building its index.
        let old = shared.write().unwrap().replace(collected(new_root.path()));
        let config = ScanConfig {
            warmup: false,
            content_indexing: true,
            watch: false,
            auto_cache_budget: false,
            install_watcher: false,
            follow_symlinks: false,
            ignore_options: Default::default(),
            enable_fs_root_scanning: false,
            enable_home_dir_scanning: false,
            snapshot_store: None,
        };
        ScanJob::run_post_scan(&shared, &signals, &config, &snapshot, &[]);
        drop(snapshot);
        drop(old);

        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let query = crate::grep::parse_grep_query("isolated_marker");
        let options = crate::grep::GrepSearchOptions {
            time_budget_ms: 0,
            ..Default::default()
        };
        assert_eq!(picker.grep(&query, &options).matches.len(), 1);
    }

    // The ns identity must survive the sort and the indexable partition and
    // reach the index stage; the file table keeps whole seconds.
    #[test]
    fn post_scan_carries_nanosecond_mtime_into_the_content_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "alpha_marker").unwrap();
        std::fs::write(root.join("b.txt"), "beta_marker").unwrap();
        std::fs::write(root.join("blob.bin"), [0u8; 32]).unwrap();

        let shared = SharedFilePicker::default();
        let picker = collected(root);
        let signals = picker.scan_signals();
        *shared.write().unwrap() = Some(picker);

        let snapshot = shared
            .read()
            .unwrap()
            .as_ref()
            .and_then(|picker| unsafe { picker.post_scan_snapshot() })
            .unwrap();

        assert_eq!(snapshot.mtime_ns.len(), snapshot.base_count);
        assert!(snapshot.base_count >= 3);

        let arena = snapshot.arena.as_ref().unwrap().as_arena_ptr();
        let mut buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
        for (index, file) in snapshot.files[..snapshot.base_count].iter().enumerate() {
            let abs = file.write_absolute_path(arena, root, &mut buf);
            let expected = crate::walk::mtime_ns(std::fs::metadata(abs).unwrap().modified().ok());

            assert_eq!(snapshot.mtime_ns[index], expected, "{abs:?}");
            // Same file, still whole seconds in the file table.
            assert_eq!(file.modified, expected / 1_000_000_000, "{abs:?}");
        }

        let config = ScanConfig {
            content_indexing: true,
            ..Default::default()
        };
        ScanJob::run_post_scan(&shared, &signals, &config, &snapshot, &[]);
        drop(snapshot);

        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let query = crate::grep::parse_grep_query("alpha_marker");
        let options = crate::grep::GrepSearchOptions {
            time_budget_ms: 0,
            ..Default::default()
        };
        assert_eq!(picker.grep(&query, &options).matches.len(), 1);
    }

    // The indexable partition reorders files, so the ns array has to move with
    // them: "a_large" sorts first by name but lands in the tail by size.
    #[test]
    fn nanosecond_mtime_follows_the_indexable_partition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let large = vec![b'x'; crate::constants::MAX_INDEXABLE_FILE_SIZE + 1];
        std::fs::write(root.join("a_large.txt"), large).unwrap();
        std::fs::write(root.join("b_small.txt"), "small text\n").unwrap();

        let picker = collected(root);
        let snapshot = unsafe { picker.post_scan_snapshot() }.unwrap();
        assert_eq!(snapshot.indexable_count, 1, "the partition moved a file");

        let arena = snapshot.arena.as_ref().unwrap().as_arena_ptr();
        let mut buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
        for (index, file) in snapshot.files[..snapshot.base_count].iter().enumerate() {
            let abs = file.write_absolute_path(arena, root, &mut buf);
            let expected = crate::walk::mtime_ns(std::fs::metadata(abs).unwrap().modified().ok());
            assert_eq!(snapshot.mtime_ns[index], expected, "{abs:?}");
        }
    }

    // The whole point of the Snapshot: the walk still runs, no content is read.
    #[test]
    fn a_second_scan_of_an_unchanged_tree_reads_no_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, cold) = run_scan(root, Some(&store));
        let first_matches = matches(&first, "marker");
        assert_eq!(first_matches.len(), 3);
        assert_eq!(cold.row_hits, 0);
        assert_eq!(cold.content_reads, 4);
        drop(first);

        // A fresh process: the store is reopened before the second scan.
        drop(store);
        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let rows_before = store.logical_bytes(root).unwrap();
        let (second, warm) = run_scan(root, Some(&store));

        assert_eq!(warm.content_reads, 0, "no file content may be read");
        assert_eq!(warm.row_hits, 4);
        assert_eq!(warm.row_misses, 0);
        assert_eq!(matches(&second, "marker"), first_matches);
        // Reused rows were never rewritten, so they must still be counted.
        assert_eq!(store.logical_bytes(root).unwrap(), rows_before);
        assert_eq!(
            store.read_meta(root).unwrap().unwrap().logical_bytes,
            rows_before
        );
    }

    // Counters are process-wide, so a second scan adds to the first instead of
    // resetting: /stats must never look colder than it was.
    #[test]
    fn snapshot_hits_and_misses_accumulate_across_scans() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, cold) = run_scan(root, Some(&store));
        drop(first);
        assert_eq!(cold.row_hits, 0, "a cold scan has nothing to hit");
        let cold_hits = store.hits();
        let cold_misses = store.misses();
        assert_eq!(cold_hits, cold.row_hits + cold.sniff_hits);

        let (second, warm) = run_scan(root, Some(&store));
        drop(second);

        assert!(warm.row_hits > 0, "the second scan replays rows");
        assert_eq!(
            store.hits(),
            cold_hits + warm.row_hits + warm.sniff_hits,
            "hits keep adding up"
        );
        assert!(
            store.misses() >= cold_misses,
            "misses never shrink: {} < {cold_misses}",
            store.misses()
        );
        assert!(store.misses() > 0);
    }

    // A store that died mid-life is a plain miss: the scan must finish and the
    // tree must stay searchable.
    #[test]
    fn a_disabled_store_never_fails_a_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        store.fail(&"the environment was corrupted by the test");

        let (shared, stats) = run_scan(root, Some(&store));

        assert!(store.is_disabled());
        assert_eq!(stats.row_hits, 0, "a disabled store serves no rows");
        assert_eq!(store.row_count(root).unwrap(), 0);
        assert_eq!(store.hits(), 0);
        assert_eq!(matches(&shared, "alpha_marker").len(), 1, "{stats:?}");
        assert_eq!(matches(&shared, "gamma_marker").len(), 1);
    }

    #[test]
    fn changing_one_file_makes_exactly_that_file_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, _) = run_scan(root, Some(&store));
        assert_eq!(matches(&first, "fresh_marker").len(), 0);
        drop(first);

        std::fs::write(root.join("beta.txt"), "beta_marker two\nfresh_marker\n").unwrap();
        let (second, warm) = run_scan(root, Some(&store));

        assert_eq!(warm.row_hits, 3);
        assert_eq!(warm.row_misses, 1);
        assert_eq!(warm.content_reads, 1);
        let found = matches(&second, "fresh_marker");
        assert_eq!(found.len(), 1);
        assert!(found[0].0.ends_with("beta.txt"), "{found:?}");
        assert_eq!(matches(&second, "alpha_marker").len(), 1);
    }

    // The test that pays for nanosecond mtimes: same second, same length, new
    // bytes. Whole-second identity would call this a hit and serve stale matches.
    #[test]
    fn a_rewrite_in_the_same_second_with_the_same_size_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "first_marker\n").unwrap();
        std::fs::write(root.join("b.txt"), "beta_marker\n").unwrap();

        let base =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        set_mtime(
            &root.join("a.txt"),
            base + std::time::Duration::from_millis(200),
        );

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, _) = run_scan(root, Some(&store));
        assert_eq!(matches(&first, "first_marker").len(), 1);
        drop(first);

        std::fs::write(root.join("a.txt"), "secnd_marker\n").unwrap();
        set_mtime(
            &root.join("a.txt"),
            base + std::time::Duration::from_millis(900),
        );
        assert_eq!(mtime_ns(&root.join("a.txt")) / 1_000_000_000, 1_700_000_000);

        let (second, warm) = run_scan(root, Some(&store));
        assert_eq!(warm.row_hits, 1, "only b.txt may be served from its row");
        assert_eq!(warm.row_misses, 1);
        assert_eq!(matches(&second, "first_marker").len(), 0);
        assert_eq!(matches(&second, "secnd_marker").len(), 1);
    }

    #[test]
    fn a_matching_size_with_a_different_mtime_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let base =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        set_mtime(&root.join("alpha.txt"), base);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        drop(run_scan(root, Some(&store)));

        // Same byte length, same content, one second later on disk.
        set_mtime(
            &root.join("alpha.txt"),
            base + std::time::Duration::from_secs(1),
        );
        let (second, warm) = run_scan(root, Some(&store));

        assert_eq!(warm.row_hits, 3);
        assert_eq!(warm.row_misses, 1);
        assert_eq!(warm.content_reads, 1);
        assert_eq!(matches(&second, "marker").len(), 3);
    }

    #[test]
    fn a_matching_mtime_with_a_different_size_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let base =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        set_mtime(&root.join("alpha.txt"), base);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        drop(run_scan(root, Some(&store)));

        std::fs::write(root.join("alpha.txt"), "alpha_marker one and extralong\n").unwrap();
        set_mtime(&root.join("alpha.txt"), base);
        let (second, warm) = run_scan(root, Some(&store));

        assert_eq!(warm.row_hits, 3);
        assert_eq!(warm.row_misses, 1);
        assert_eq!(warm.content_reads, 1);
        assert_eq!(matches(&second, "extralong").len(), 1);
    }

    #[test]
    fn a_meta_from_another_build_or_root_makes_every_row_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        drop(run_scan(root, Some(&store)));
        let meta = store.read_meta(root).unwrap().expect("meta row");
        let rows = store.row_count(root).unwrap();

        let foreign = [
            (
                meta.schema,
                meta.fingerprint ^ 1,
                meta.root.clone(),
                "a foreign fingerprint",
            ),
            (
                meta.schema + 1,
                meta.fingerprint,
                meta.root.clone(),
                "a foreign schema",
            ),
            (
                meta.schema,
                meta.fingerprint,
                "somewhere/else".to_string(),
                "a foreign root",
            ),
        ];
        for (schema, fingerprint, stored_root, why) in foreign {
            store
                .put_foreign_meta(root, schema, fingerprint, &stored_root)
                .unwrap();
            let (_scan, warm) = run_scan(root, Some(&store));
            assert_eq!(warm.row_hits, 0, "{why}");
            assert_eq!(warm.content_reads, 4, "{why}");
            assert_eq!(store.row_count(root).unwrap(), rows, "{why}: no deletion");
        }
    }

    // A hit skips the content read, so the Binary verdict has to come from the
    // row or grep starts reading images as text.
    #[test]
    fn the_binary_verdict_travels_with_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("plain.txt"), "plain_marker\n").unwrap();
        // No known binary extension, so the walk calls it indexable; the content
        // pass is what finds the NUL byte.
        std::fs::write(root.join("mixed.txt"), b"text_marker\0payload\n").unwrap();

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, cold) = run_scan(root, Some(&store));
        assert_eq!(cold.content_reads, 2);
        assert_eq!(matches(&first, "text_marker").len(), 0);
        drop(first);

        let (second, warm) = run_scan(root, Some(&store));
        assert_eq!(warm.row_hits, 2);
        assert_eq!(warm.content_reads, 0);
        assert_eq!(matches(&second, "text_marker").len(), 0);
        assert_eq!(matches(&second, "plain_marker").len(), 1);
    }

    fn assert_same_answers(from_files: &SharedFilePicker, from_rows: &SharedFilePicker) {
        let left = from_files.read().unwrap();
        let right = from_rows.read().unwrap();
        let left = left.as_ref().unwrap().bigram_index().expect("file built");
        let right = right.as_ref().unwrap().bigram_index().expect("row built");

        assert_eq!(left.words(), right.words());
        assert_eq!(left.columns_used(), right.columns_used());
        assert_eq!(left.populated(), right.populated());

        for query in [
            b"alpha".as_slice(),
            b"marker",
            b"three",
            b"gamma",
            b"zzzq",
            b"sub",
        ] {
            assert_eq!(
                left.query(query),
                right.query(query),
                "candidates differ for {:?}",
                std::str::from_utf8(query).unwrap()
            );
        }

        for hi in 32u16..=126 {
            for lo in 32u16..=126 {
                let key = hi << 8 | lo;
                assert_eq!(left.has_key(key), right.has_key(key), "key {key:#06x}");
                assert_eq!(
                    left.column_bitset(key),
                    right.column_bitset(key),
                    "column for {key:#06x}"
                );
            }
        }
    }

    #[test]
    fn a_row_built_index_answers_like_a_file_built_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (from_files, cold) = run_scan(root, None);
        assert_eq!(cold.row_hits, 0);
        assert_eq!(cold.content_reads, 4);

        drop(run_scan(root, Some(&store))); // seeds the rows
        let (from_rows, warm) = run_scan(root, Some(&store));
        assert_eq!(warm.content_reads, 0);
        assert_eq!(warm.row_hits, 4);

        assert_same_answers(&from_files, &from_rows);
    }

    fn non_indexable_fixture(root: &std::path::Path) {
        // Empty: never sniffed, still needs a verdict row.
        std::fs::write(root.join("empty.txt"), "").unwrap();
        // Binary by extension: non-indexable without a content read.
        std::fs::write(root.join("photo.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        // Over the content-index limit: sniffed, never indexed.
        let over = vec![b'a'; crate::constants::MAX_INDEXABLE_FILE_SIZE + 1];
        std::fs::write(root.join("large.txt"), over).unwrap();
        std::fs::write(root.join("plain.txt"), "plain_marker\n").unwrap();
    }

    #[test]
    fn a_second_scan_takes_non_indexable_verdicts_from_their_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        non_indexable_fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, cold) = run_scan(root, Some(&store));
        assert_eq!(cold.sniffed, 2, "the png and the over-limit text file");
        assert_eq!(cold.sniff_hits, 0);
        let first_matches = matches(&first, "plain_marker");
        assert_eq!(first_matches.len(), 1);
        drop(first);
        let rows_before = store.logical_bytes(root).unwrap();
        let written_before = store.rows_written();

        let (second, warm) = run_scan(root, Some(&store));
        assert_eq!(warm.sniffed, 0, "nothing non-indexable was sniffed");
        assert_eq!(
            warm.sniff_hits, 3,
            "empty, png and over-limit came from rows"
        );
        assert_eq!(warm.content_reads, 0);
        assert_eq!(matches(&second, "plain_marker"), first_matches);
        // Every row, verdict-only ones included, was served, so none was rewritten.
        assert_eq!(store.rows_written(), written_before, "no row was rewritten");
        assert_eq!(store.logical_bytes(root).unwrap(), rows_before);
        assert_eq!(
            store.read_meta(root).unwrap().unwrap().logical_bytes,
            rows_before
        );
    }

    #[test]
    fn grep_still_refuses_a_binary_served_from_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        let limit = crate::constants::MAX_INDEXABLE_FILE_SIZE + 1;

        // Over the limit and binary, but with a text-looking prefix.
        let mut blob = vec![0u8; limit];
        blob[..19].copy_from_slice(b"blob_marker_inside\n");
        std::fs::write(root.join("blob.dat"), blob).unwrap();
        // Over the limit and text: non-indexable, yet grep-able.
        let mut text = vec![b'a'; limit];
        text[..23].copy_from_slice(b"large_text_marker_line\n");
        std::fs::write(root.join("large.txt"), text).unwrap();

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, cold) = run_scan(root, Some(&store));
        assert_eq!(cold.sniffed, 2);
        assert_eq!(matches(&first, "blob_marker").len(), 0);
        assert_eq!(matches(&first, "large_text_marker").len(), 1);
        drop(first);

        let (second, warm) = run_scan(root, Some(&store));
        assert_eq!(warm.sniffed, 0);
        assert_eq!(warm.sniff_hits, 2);
        assert_eq!(matches(&second, "blob_marker").len(), 0);
        assert_eq!(matches(&second, "large_text_marker").len(), 1);
    }

    #[test]
    fn a_changed_non_indexable_file_is_sniffed_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        let limit = crate::constants::MAX_INDEXABLE_FILE_SIZE;
        let base =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

        let mut text = vec![b'a'; limit + 1];
        text[..12].copy_from_slice(b"text_marker\n");
        std::fs::write(root.join("big.txt"), &text).unwrap();
        set_mtime(&root.join("big.txt"), base);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (first, cold) = run_scan(root, Some(&store));
        assert_eq!(cold.sniffed, 1);
        assert_eq!(matches(&first, "text_marker").len(), 1);
        drop(first);

        // Same size, new bytes, new ns mtime: the row identity must miss.
        let mut blob = vec![0u8; limit + 1];
        blob[..14].copy_from_slice(b"bin_marker\0abc");
        std::fs::write(root.join("big.txt"), &blob).unwrap();
        set_mtime(
            &root.join("big.txt"),
            base + std::time::Duration::from_secs(1),
        );
        let (second, same_size) = run_scan(root, Some(&store));
        assert_eq!(same_size.sniffed, 1, "a new mtime must force a sniff");
        assert_eq!(same_size.sniff_hits, 0);
        assert_eq!(matches(&second, "bin_marker").len(), 0);
        drop(second);

        // Same path, new size: sniffed again, and the new text verdict is kept.
        let mut bigger = vec![b'b'; limit + 2];
        bigger[..13].copy_from_slice(b"third_marker\n");
        std::fs::write(root.join("big.txt"), &bigger).unwrap();
        set_mtime(
            &root.join("big.txt"),
            base + std::time::Duration::from_secs(2),
        );
        let (third, grown) = run_scan(root, Some(&store));
        assert_eq!(grown.sniffed, 1, "a new size must force a sniff");
        assert_eq!(grown.sniff_hits, 0);
        assert_eq!(matches(&third, "bin_marker").len(), 0);
        assert_eq!(matches(&third, "third_marker").len(), 1);
    }

    // Two Roots scanning at the same time never serialise behind each other's
    // writes: one writer thread takes both queues.
    #[test]
    fn two_roots_scanned_at_once_both_keep_all_their_rows() {
        let db = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fixture(first.path());
        fixture(second.path());

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        std::thread::scope(|scope| {
            let left = scope.spawn(|| run_scan(first.path(), Some(&store)));
            let right = scope.spawn(|| run_scan(second.path(), Some(&store)));
            assert_eq!(left.join().unwrap().1.row_hits, 0);
            assert_eq!(right.join().unwrap().1.row_hits, 0);
        });

        for root in [first.path(), second.path()] {
            assert_eq!(store.row_count(root).unwrap(), 4, "{root:?}");
            let meta = store.read_meta(root).unwrap().expect("meta");
            assert_eq!(meta.logical_bytes, store.logical_bytes(root).unwrap());
        }
    }

    // A row for a file that is gone would be dead weight until the age expiry.
    #[test]
    fn a_file_that_disappeared_loses_its_row_and_no_other_root_is_touched() {
        let db = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        fixture(root.path());
        std::fs::write(other.path().join("keep.txt"), "keep_marker\n").unwrap();

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        drop(run_scan(root.path(), Some(&store)));
        drop(run_scan(other.path(), Some(&store)));
        assert_eq!(store.row_count(root.path()).unwrap(), 4);

        std::fs::remove_file(root.path().join("beta.txt")).unwrap();
        let (second, warm) = run_scan(root.path(), Some(&store));

        assert_eq!(warm.row_hits, 3, "the survivors were still served");
        assert_eq!(store.row_count(root.path()).unwrap(), 3);
        assert_eq!(matches(&second, "marker").len(), 2);
        assert_eq!(
            store.read_meta(root.path()).unwrap().unwrap().logical_bytes,
            store.logical_bytes(root.path()).unwrap(),
            "the meta follows the sweep"
        );
        assert_eq!(store.row_count(other.path()).unwrap(), 1);
        assert!(store.read_meta(other.path()).unwrap().is_some());
    }

    // The anti-regression for the trap: a warm scan must not delete and rewrite.
    #[test]
    fn a_warm_scan_rewrites_no_row() {
        let db = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        drop(run_scan(root.path(), Some(&store)));
        let written = store.rows_written();
        let bytes = store.logical_bytes(root.path()).unwrap();

        let (_, warm) = run_scan(root.path(), Some(&store));

        assert_eq!(warm.content_reads, 0);
        assert_eq!(warm.row_hits, 4);
        assert_eq!(store.rows_written(), written, "not one row was rewritten");
        assert_eq!(store.logical_bytes(root.path()).unwrap(), bytes);
    }

    // A different derivation must throw the rows away, not serve them: a build
    // with another fingerprint reads every file and answers like a cold scan.
    #[test]
    fn an_older_builds_fingerprint_discards_the_rows_and_reads_the_files_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let store = std::sync::Arc::new(SnapshotStore::open(db.path()).unwrap());
        let (cold, cold_stats) = run_scan(root, Some(&store));
        let baseline = matches(&cold, "marker");
        assert_eq!(cold_stats.content_reads, 4);
        drop(cold);

        // What a build one derivation change older would have left behind.
        let meta = store.read_meta(root).unwrap().expect("meta row");
        store
            .put_foreign_meta(root, meta.schema, meta.fingerprint ^ 1, &meta.root)
            .unwrap();
        assert!(!store.root_is_current(root), "the meta must look foreign");

        let (warm, warm_stats) = run_scan(root, Some(&store));
        assert_eq!(warm_stats.row_hits, 0, "no row may be served");
        assert_eq!(warm_stats.row_misses, 4);
        assert_eq!(warm_stats.content_reads, 4, "every file was read again");
        assert_eq!(
            matches(&warm, "marker"),
            baseline,
            "the answers must match a cold scan"
        );
        assert!(store.root_is_current(root), "the scan rewrites the meta");

        let (third, third_stats) = run_scan(root, Some(&store));
        assert_eq!(third_stats.row_hits, 4, "the rewritten meta is current");
        assert_eq!(third_stats.content_reads, 0);
        assert_eq!(matches(&third, "marker"), baseline);
    }

    // The same tree scanned by a process that then dies: a second process reads
    // the rows that reached disk. A second store in this process proves nothing.
    #[test]
    fn a_killed_process_leaves_rows_the_next_scan_serves() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let handshake = tempfile::tempdir().unwrap();
        let root = dir.path();
        let db = db.path().join("snapshot-db");
        fixture(root);

        let ready = handshake.path().join("worker-ready");
        let mut child = spawn_worker("die", root, &db, &[(PROCESS_READY_FILE, &ready)]);
        // The child signals only after its own reader sees the meta row, which
        // the writer queues behind every row: the kill cannot cut a write short.
        wait_for_path(&ready, WORKER_TIMEOUT);
        let status = child.kill_and_reap();
        assert!(!status.success(), "the worker ended on its own: {status}");

        let store = std::sync::Arc::new(SnapshotStore::open(&db).unwrap());
        let (shared, warm) = run_scan(root, Some(&store));

        assert!(
            warm.row_hits > 0,
            "the killed process left nothing to serve"
        );
        assert_eq!(warm.row_hits, 4);
        assert_eq!(warm.row_misses, 0);
        assert_eq!(warm.content_reads, 0, "a hit must not read a file");
        assert_eq!(matches(&shared, "marker").len(), 3);
    }

    // Two processes, one Root, one environment: both scans finish and the rows
    // they leave behind are readable, complete and still match the files.
    #[test]
    fn two_processes_indexing_one_root_leave_a_readable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let handshake = tempfile::tempdir().unwrap();
        let root = dir.path();
        let db = db.path().join("snapshot-db");
        let files = bulk_fixture(root, 1500);

        let start = handshake.path().join("start");
        let left_ready = handshake.path().join("left-ready");
        let right_ready = handshake.path().join("right-ready");
        let mut left = spawn_worker(
            "scan",
            root,
            &db,
            &[
                (PROCESS_READY_FILE, &left_ready),
                (PROCESS_START_FILE, &start),
            ],
        );
        let mut right = spawn_worker(
            "scan",
            root,
            &db,
            &[
                (PROCESS_READY_FILE, &right_ready),
                (PROCESS_START_FILE, &start),
            ],
        );

        // A barrier, so the two content passes really are in flight together.
        wait_for_path(&left_ready, WORKER_TIMEOUT);
        wait_for_path(&right_ready, WORKER_TIMEOUT);
        std::fs::write(&start, b"go").unwrap();
        left.reaped_ok();
        right.reaped_ok();

        let store = std::sync::Arc::new(SnapshotStore::open(&db).unwrap());
        let meta = store.read_meta(root).unwrap().expect("a meta row");
        assert_eq!(
            store.row_count(root).unwrap(),
            files as u64,
            "one row per file"
        );
        assert_eq!(meta.logical_bytes, store.logical_bytes(root).unwrap());

        // Every row the next scan asks for is present, decodable and unchanged.
        let (shared, warm) = run_scan(root, Some(&store));
        assert_eq!(warm.row_hits, files as u64);
        assert_eq!(
            warm.row_misses, 0,
            "a surviving row was unreadable or stale"
        );
        assert_eq!(warm.content_reads, 0);
        let found = matches(&shared, "unique_bulk_marker");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].0.ends_with("f00000.txt"), "{found:?}");
    }

    // Not a test of its own: the child entry point. The parent re-runs this test
    // binary with --exact and the variables below.
    #[test]
    fn snapshot_process_worker() {
        let Ok(mode) = std::env::var(PROCESS_MODE) else {
            return;
        };
        let root = PathBuf::from(std::env::var(PROCESS_ROOT).expect("worker root"));
        let db = PathBuf::from(std::env::var(PROCESS_DB).expect("worker db"));

        if mode == "scan" {
            if let Ok(ready) = std::env::var(PROCESS_READY_FILE) {
                std::fs::write(ready, b"ready").unwrap();
            }
            if let Ok(start) = std::env::var(PROCESS_START_FILE) {
                wait_for_path(std::path::Path::new(&start), WORKER_TIMEOUT);
            }
        }

        let store = std::sync::Arc::new(SnapshotStore::open(&db).expect("child store"));
        let (_shared, _stats) = scan_only(&root, Some(&store));

        if mode == "die" {
            // Rows first, meta last: seeing our own meta means both are on disk.
            wait_for_meta(&store, &root, WORKER_TIMEOUT);
            let ready = std::env::var(PROCESS_READY_FILE).expect("ready file");
            std::fs::write(ready, b"ready").unwrap();
            // The parent kills us here. Nothing below ever runs.
            std::thread::sleep(WORKER_TIMEOUT);
        }
    }

    const WORKER_NAME: &str = "scan::tests::snapshot_process_worker";
    const WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    const PROCESS_MODE: &str = "FFF_SNAPSHOT_PROCESS_MODE";
    const PROCESS_ROOT: &str = "FFF_SNAPSHOT_PROCESS_ROOT";
    const PROCESS_DB: &str = "FFF_SNAPSHOT_PROCESS_DB";
    const PROCESS_READY_FILE: &str = "FFF_SNAPSHOT_PROCESS_READY_FILE";
    const PROCESS_START_FILE: &str = "FFF_SNAPSHOT_PROCESS_START_FILE";

    // Kills on drop, so a failed assertion never leaks a worker process.
    struct Worker(std::process::Child);

    impl Worker {
        fn kill_and_reap(&mut self) -> std::process::ExitStatus {
            let _ = self.0.kill();
            self.0.wait().expect("reap a worker")
        }

        fn reaped_ok(&mut self) {
            let status = self.0.wait().expect("reap a worker");
            assert!(status.success(), "a worker process failed: {status}");
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // A real second OS process: the test binary, run again for one named test.
    fn spawn_worker(
        mode: &str,
        root: &std::path::Path,
        db: &std::path::Path,
        handshake: &[(&str, &std::path::Path)],
    ) -> Worker {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("the test binary"));
        command
            .args(["--exact", WORKER_NAME, "--nocapture", "--test-threads=1"])
            .env(PROCESS_MODE, mode)
            .env(PROCESS_ROOT, root)
            .env(PROCESS_DB, db)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        for (name, path) in handshake {
            command.env(name, path);
        }
        Worker(command.spawn().expect("spawn a worker process"))
    }

    fn wait_for_path(path: &std::path::Path, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("{} never appeared", path.display());
    }

    fn wait_for_meta(store: &SnapshotStore, root: &std::path::Path, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if store.read_meta(root).unwrap().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("no meta row for {}", root.display());
    }

    // Enough files that two scans released together must overlap.
    fn bulk_fixture(root: &std::path::Path, files: usize) -> usize {
        let body = format!("bulk_marker line_0\n{}\n", "x".repeat(1400));
        for index in 0..files {
            let dir = root.join(format!("d{:02}", index % 32));
            std::fs::create_dir_all(&dir).unwrap();
            let mut text = body.clone();
            if index == 0 {
                text.push_str("unique_bulk_marker\n");
            }
            std::fs::write(dir.join(format!("f{index:05}.txt")), &text).unwrap();
        }
        files
    }
}

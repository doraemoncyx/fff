use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use fff::{
    FFFMode, FilePicker, FilePickerIgnoreOptions, FilePickerOptions, SharedFilePicker,
    SharedFrecency, SnapshotStore,
};

use crate::config::Config;
use crate::error::ApiError;

// One indexed Root and the bookkeeping the registry needs around it.
pub(crate) struct Index {
    root: PathBuf,
    key: String,
    picker: SharedFilePicker,
    last_used: AtomicU64,
    scan_announced: AtomicBool,
}

// Bigram columns are one bit per file; 5000 columns of printable pairs.
const BIGRAM_BYTES_PER_FILE: usize = 625;

pub(crate) struct IndexProgress {
    pub files: usize,
    pub dirs: usize,
    pub scanning: bool,
    /// The walk finished but the content index is still being built.
    pub indexing: bool,
    /// The bigram prefilter is installed, so grep can skip files.
    pub prefilter: bool,
    pub scanned: usize,
}

impl Index {
    pub fn create(
        root: &Path,
        config: &Config,
        frecency: &SharedFrecency,
        ignore: &FilePickerIgnoreOptions,
        snapshot: Option<Arc<SnapshotStore>>,
    ) -> Result<Self, ApiError> {
        let picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state_and_ignore_options(
            picker.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                mode: FFFMode::Ai,
                watch: config.watch,
                follow_symlinks: config.follow_symlinks,
                enable_mmap_cache: false,
                enable_content_indexing: config.content_indexing,
                snapshot_store: snapshot,
                ..Default::default()
            },
            ignore.clone(),
        )
        .map_err(|e| ApiError::invalid_params(format!("cannot index {}: {e}", root.display())))?;

        tracing::debug!(
            root = %root.display(),
            watch = config.watch,
            follow_symlinks = config.follow_symlinks,
            "scan started"
        );

        Ok(Self {
            root: root.to_path_buf(),
            key: crate::root::root_key(root),
            picker,
            last_used: AtomicU64::new(0),
            scan_announced: AtomicBool::new(false),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn picker(&self) -> &SharedFilePicker {
        &self.picker
    }

    pub fn last_used(&self) -> u64 {
        self.last_used.load(Ordering::Relaxed)
    }

    pub fn touch(&self, tick: u64) {
        self.last_used.store(tick, Ordering::Relaxed);
    }

    pub fn progress(&self) -> IndexProgress {
        let guard = self.picker.read().ok();
        let Some(picker) = guard.as_ref().and_then(|guard| guard.as_ref()) else {
            return IndexProgress {
                files: 0,
                dirs: 0,
                scanning: true,
                indexing: false,
                prefilter: false,
                scanned: 0,
            };
        };
        let scan = picker.get_scan_progress();
        IndexProgress {
            files: picker.live_file_count(),
            dirs: picker.get_dirs().len(),
            scanning: scan.is_scanning,
            indexing: !scan.is_warmup_complete,
            prefilter: picker.bigram_index().is_some(),
            scanned: scan.scanned_files_count,
        }
    }

    // Block until the initial walk finishes. Partial when the deadline wins.
    pub fn wait_for_scan(&self, timeout: Duration) -> bool {
        self.picker.wait_for_scan(timeout)
    }

    // Block until the walk and the post-scan content index are both done.
    pub fn wait_for_ready(&self, timeout: Duration) -> bool {
        self.picker.wait_for_indexing_complete(timeout)
    }

    pub fn stop(&self) {
        if let Ok(mut guard) = self.picker.write()
            && let Some(picker) = guard.as_mut()
        {
            picker.stop_background_monitor();
        }
    }

    /// True once, the first time this Index is seen with its walk finished.
    pub fn take_scan_completion(&self) -> bool {
        let progress = self.progress();
        if progress.scanning {
            self.scan_announced.store(false, Ordering::Relaxed);
            return false;
        }
        let first = !self.scan_announced.swap(true, Ordering::Relaxed);
        if first {
            tracing::debug!(
                root = %self.root.display(),
                files = progress.files,
                dirs = progress.dirs,
                scanned = progress.scanned,
                "scan completed"
            );
        }
        first
    }

    // Rough per-index footprint for `/stats`; the eviction decision uses RSS.
    pub fn estimated_bytes(&self) -> usize {
        let progress = self.progress();
        let per_file = std::mem::size_of::<fff::types::FileItem>() + 96;
        let prefilter = if progress.prefilter {
            progress.files * BIGRAM_BYTES_PER_FILE
        } else {
            0
        };
        progress.files * per_file + progress.dirs * 64 + prefilter
    }
}

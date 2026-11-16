use serde::Serialize;

use crate::registry::Registry;

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub used_bytes: u64,
    pub budget_bytes: u64,
    /// Initial scans this process started, and the concurrency they reached.
    pub scans_started: u64,
    pub scans_in_flight: usize,
    pub peak_scans_in_flight: usize,
    /// Whether Indexes build the bigram content index used as grep's prefilter.
    pub content_indexing: bool,
    /// `None` when no Snapshot is in use: off by config, unopenable, or latched
    /// off by a failure. Never a zeroed object that reads as a cold cache.
    pub snapshot: Option<SnapshotStats>,
    pub indexes: Vec<IndexStats>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotStats {
    pub hits: u64,
    pub misses: u64,
    pub bytes: u64,
    pub roots: u64,
    /// Drain-thread progress. A `queued_bytes` that stops falling while `commits`
    /// stalls is a writer, not a grep, burning the CPU.
    pub commits: u64,
    pub rows: u64,
    pub dropped: u64,
    pub queued_bytes: u64,
    pub busy: bool,
}

#[derive(Debug, Serialize)]
pub struct IndexStats {
    pub root: String,
    pub files: usize,
    pub dirs: usize,
    pub scanning: bool,
    /// The walk is done but the content index is still building.
    pub indexing: bool,
    /// The bigram prefilter is installed for this Index.
    pub prefilter: bool,
    pub last_used: u64,
    pub estimated_bytes: usize,
}

pub(crate) fn snapshot(registry: &Registry) -> StatsResponse {
    let mut indexes: Vec<_> = registry
        .indices()
        .into_iter()
        .map(|index| {
            let progress = index.progress();
            IndexStats {
                root: index.root().to_string_lossy().into_owned(),
                files: progress.files,
                dirs: progress.dirs,
                scanning: progress.scanning,
                indexing: progress.indexing,
                prefilter: progress.prefilter,
                last_used: index.last_used(),
                estimated_bytes: index.estimated_bytes(),
            }
        })
        .collect();
    indexes.sort_by_key(|index| index.last_used);

    StatsResponse {
        used_bytes: registry.used_bytes(),
        budget_bytes: registry.config().max_memory_bytes,
        scans_started: registry.scans_started(),
        scans_in_flight: registry.scans_in_flight(),
        peak_scans_in_flight: registry.peak_scans_in_flight(),
        content_indexing: registry.config().content_indexing,
        snapshot: registry.snapshot_store().map(|store| {
            let writer = store.writer_stats();
            SnapshotStats {
                hits: store.hits(),
                misses: store.misses(),
                bytes: store.bytes(),
                roots: store.roots(),
                commits: writer.commits,
                rows: writer.rows,
                dropped: writer.dropped,
                queued_bytes: writer.queued_bytes,
                busy: writer.busy,
            }
        }),
        indexes,
    }
}

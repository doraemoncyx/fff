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
    pub indexes: Vec<IndexStats>,
}

#[derive(Debug, Serialize)]
pub struct IndexStats {
    pub root: String,
    pub files: usize,
    pub dirs: usize,
    pub scanning: bool,
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
        indexes,
    }
}

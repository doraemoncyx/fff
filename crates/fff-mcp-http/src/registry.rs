use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fff::{FilePickerIgnoreOptions, SharedFrecency};
use parking_lot::{Mutex, RwLock};

use crate::config::Config;
use crate::cursor::CursorStore;
use crate::error::ApiError;
use crate::index::Index;
use crate::memory::{MemoryProbe, OsMemoryProbe, collect_allocator};
use crate::params::Params;
use crate::search::{self, SearchResponse};
use crate::stats::StatsResponse;

const MAX_EVICTIONS_PER_ROUND: usize = 3;
const EVICT_COOLDOWN_MS: u64 = 30_000;

/// Root -> Index registry shared by every request.
pub struct Registry {
    roots: RwLock<Vec<Arc<Index>>>,
    tick: AtomicU64,
    config: Arc<Config>,
    frecency: SharedFrecency,
    ignore: FilePickerIgnoreOptions,
    cursors: Mutex<CursorStore>,
    last_activity: AtomicU64,
    probe: Arc<dyn MemoryProbe>,
    evicted_at: Mutex<HashMap<String, u64>>,
}

impl Registry {
    pub fn new(config: Config) -> Result<Self, ApiError> {
        Self::with_probe(config, Arc::new(OsMemoryProbe))
    }

    pub fn with_probe(config: Config, probe: Arc<dyn MemoryProbe>) -> Result<Self, ApiError> {
        let frecency = SharedFrecency::default();
        if let Some(path) = &config.frecency_db {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match fff::FrecencyTracker::open(path) {
                Ok(tracker) => {
                    let _ = frecency.init(tracker);
                }
                Err(error) => {
                    tracing::warn!(?error, path = %path.display(), "frecency disabled");
                }
            }
        }

        Ok(Self {
            roots: RwLock::new(Vec::new()),
            tick: AtomicU64::new(0),
            config: Arc::new(config),
            frecency,
            ignore: FilePickerIgnoreOptions::default(),
            cursors: Mutex::new(CursorStore::new()),
            last_activity: AtomicU64::new(now_millis()),
            probe,
            evicted_at: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn cursor_offset(&self, id: &str) -> Option<usize> {
        self.cursors.lock().get(id)
    }

    pub(crate) fn store_cursor(&self, offset: usize) -> String {
        self.cursors.lock().store(offset)
    }

    pub(crate) fn next_tick(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn touch(&self, index: &Index) {
        index.touch(self.next_tick());
    }

    pub(crate) fn indices(&self) -> Vec<Arc<Index>> {
        self.roots.read().clone()
    }

    /// Find the index for exactly this Root, creating and scanning it if missing.
    pub(crate) fn ensure_index(&self, tardir: &Path) -> Result<Arc<Index>, ApiError> {
        let key = crate::root::root_key(tardir);
        if let Some(index) = self.find(&key) {
            self.touch(&index);
            return Ok(index);
        }

        let mut roots = self.roots.write();
        if let Some(index) = roots.iter().find(|index| index.key() == key) {
            index.touch(self.next_tick());
            return Ok(index.clone());
        }

        tracing::info!(root = %tardir.display(), "creating index");
        let index = Arc::new(Index::create(
            tardir,
            &self.config,
            &self.frecency,
            &self.ignore,
        )?);
        index.touch(self.next_tick());
        roots.push(index.clone());
        Ok(index)
    }

    /// Drop every Index whose Root lies strictly below a finished scan.
    /// Removal drops registry references only; requests hold their own Arcs.
    pub(crate) fn subsume_finished(&self) {
        let indexes = self.indices();
        let finished: Vec<String> = indexes
            .iter()
            .filter(|index| !index.progress().scanning)
            .map(|index| index.key().to_string())
            .collect();
        if finished.is_empty() {
            return;
        }

        let mut roots = self.roots.write();
        let before = roots.len();
        roots.retain(|index| {
            !finished.iter().any(|key| {
                index.key().len() > key.len()
                    && index.key().starts_with(key.as_str())
                    && index.key().as_bytes()[key.len()] == b'/'
            })
        });
        if roots.len() != before {
            tracing::info!(
                dropped = before - roots.len(),
                "parent scan subsumed child indexes"
            );
        }
    }

    /// Measured process usage, falling back to the per-Index estimates.
    pub(crate) fn used_bytes(&self) -> u64 {
        self.probe.used_bytes().unwrap_or_else(|| {
            self.indices()
                .iter()
                .map(|index| index.estimated_bytes() as u64)
                .sum()
        })
    }

    /// One eviction round: measure, drop the LRU Index outside `serving`, repeat.
    pub(crate) fn evict_over_budget(&self, serving: &[Arc<Index>]) -> Vec<String> {
        let mut evicted = Vec::new();
        while evicted.len() < MAX_EVICTIONS_PER_ROUND {
            let used = self.used_bytes();
            let budget = self.config.max_memory_bytes;
            if used <= budget {
                break;
            }

            let indexes = self.indices();
            if indexes.len() <= 1 {
                if let Some(index) = indexes.first() {
                    tracing::warn!(
                        root = %index.root().display(),
                        used_bytes = used,
                        budget_bytes = budget,
                        "single index exceeds the memory budget, keeping it"
                    );
                }
                break;
            }

            let victim = {
                let cooling = self.evicted_at.lock();
                pick_victim(&indexes, serving, &cooling, now_millis())
            };
            let Some(victim) = victim else { break };

            victim.stop();
            let key = victim.key().to_string();
            let root = victim.root().to_string_lossy().into_owned();
            drop(victim);
            drop(indexes);
            self.roots.write().retain(|index| index.key() != key);
            collect_allocator();
            self.remember_eviction(&key);
            tracing::info!(
                root = %root,
                used_bytes = used,
                budget_bytes = budget,
                "evicted index over the memory budget"
            );
            evicted.push(key);
        }
        evicted
    }

    /// Test hook: force one round with no serving set.
    pub fn evict_now(&self) -> Vec<String> {
        self.evict_over_budget(&[])
    }

    /// Trigger point: a scan finished, so the room it needed may be free.
    pub(crate) fn evict_finished_scans(&self) {
        let finished = self
            .indices()
            .iter()
            .any(|index| index.take_scan_completion());
        if finished {
            self.evict_over_budget(&[]);
        }
    }

    fn remember_eviction(&self, key: &str) {
        let now = now_millis();
        let mut cooling = self.evicted_at.lock();
        cooling.retain(|_, at| now.saturating_sub(*at) < EVICT_COOLDOWN_MS);
        cooling.insert(key.to_string(), now);
    }

    fn find(&self, key: &str) -> Option<Arc<Index>> {
        self.roots
            .read()
            .iter()
            .find(|index| index.key() == key)
            .cloned()
    }

    pub fn query(&self, params: &Params) -> Result<SearchResponse, ApiError> {
        self.mark_activity();
        search::execute(self, params)
    }

    pub fn stats(&self) -> StatsResponse {
        crate::stats::snapshot(self)
    }

    pub fn shutdown(&self) {
        let indexes = self.indices();
        for index in &indexes {
            index.stop();
        }
        self.roots.write().clear();
    }

    pub(crate) fn mark_activity(&self) {
        self.last_activity.store(now_millis(), Ordering::Relaxed);
    }

    pub fn idle_for(&self) -> std::time::Duration {
        let last = self.last_activity.load(Ordering::Relaxed);
        std::time::Duration::from_millis(now_millis().saturating_sub(last))
    }
}

/// Least recently used Index that is neither serving this request nor cooling
/// down after a recent eviction.
fn pick_victim(
    indexes: &[Arc<Index>],
    serving: &[Arc<Index>],
    cooling: &HashMap<String, u64>,
    now: u64,
) -> Option<Arc<Index>> {
    indexes
        .iter()
        .filter(|index| !serving.iter().any(|entry| entry.key() == index.key()))
        .filter(|index| {
            cooling
                .get(index.key())
                .is_none_or(|at| now.saturating_sub(*at) >= EVICT_COOLDOWN_MS)
        })
        .min_by_key(|index| index.last_used())
        .cloned()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

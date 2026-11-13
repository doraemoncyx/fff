use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// A store's latch and its process-wide counters, shared with the writer thread.
// The first failure turns the store off and is the only one that warns; every
// later failure is the same "no cache" state and stays silent.
#[derive(Default)]
pub(crate) struct SnapshotState {
    disabled: AtomicBool,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl SnapshotState {
    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    pub(crate) fn disable(&self, error: &dyn std::fmt::Display) {
        if self.disabled.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            %error,
            "snapshot disabled for this process; scans read files instead"
        );
    }

    pub(crate) fn record(&self, hits: u64, misses: u64) {
        self.hits.fetch_add(hits, Ordering::Relaxed);
        self.misses.fetch_add(misses, Ordering::Relaxed);
    }

    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub(crate) fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

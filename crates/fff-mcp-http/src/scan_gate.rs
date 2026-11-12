use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Condvar, Mutex};

use crate::index::Index;

#[derive(Default)]
struct Permits {
    active: usize,
    peak: usize,
    started: u64,
}

// Upper bound on in-flight initial scans; excess requests queue with a deadline.
pub(crate) struct ScanGate {
    limit: usize,
    permits: Mutex<Permits>,
    released: Condvar,
}

impl ScanGate {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(1),
            permits: Mutex::new(Permits::default()),
            released: Condvar::new(),
        })
    }

    pub fn started(&self) -> u64 {
        self.permits.lock().started
    }

    pub fn in_flight(&self) -> usize {
        self.permits.lock().active
    }

    pub fn peak(&self) -> usize {
        self.permits.lock().peak
    }

    /// Take a scan slot, queueing until `deadline`. `None` means still queued.
    /// A free slot is taken even past the deadline so a scan can still start.
    pub fn acquire_until(self: &Arc<Self>, deadline: Instant) -> Option<ScanPermit> {
        let mut permits = self.permits.lock();
        loop {
            if permits.active < self.limit {
                permits.active += 1;
                permits.peak = permits.peak.max(permits.active);
                return Some(ScanPermit {
                    gate: Arc::clone(self),
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.released.wait_for(&mut permits, deadline - now);
        }
    }

    pub fn record_started(&self) {
        self.permits.lock().started += 1;
    }
}

// One in-flight initial scan; dropped when the walk publishes its last file.
pub(crate) struct ScanPermit {
    gate: Arc<ScanGate>,
}

impl Drop for ScanPermit {
    fn drop(&mut self) {
        let mut permits = self.gate.permits.lock();
        permits.active = permits.active.saturating_sub(1);
        drop(permits);
        self.gate.released.notify_one();
    }
}

#[derive(Default, PartialEq, Eq)]
enum SlotState {
    #[default]
    Pending,
    Ready,
    Abandoned,
    Failed,
}

pub(crate) enum SlotWait {
    Ready(Arc<Index>),
    Failed(String),
    Abandoned,
    TimedOut,
}

// One Root's pending creation: its first request scans, the rest wait here.
pub(crate) struct ScanSlot {
    inner: Mutex<SlotInner>,
    changed: Condvar,
}

#[derive(Default)]
struct SlotInner {
    state: SlotState,
    index: Option<Arc<Index>>,
    message: String,
}

impl ScanSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SlotInner::default()),
            changed: Condvar::new(),
        })
    }

    pub fn publish(&self, index: Arc<Index>) {
        let mut inner = self.inner.lock();
        inner.state = SlotState::Ready;
        inner.index = Some(index);
        drop(inner);
        self.changed.notify_all();
    }

    pub fn fail(&self, message: String) {
        let mut inner = self.inner.lock();
        inner.state = SlotState::Failed;
        inner.message = message;
        drop(inner);
        self.changed.notify_all();
    }

    /// No Index exists: waiters start over instead of blocking on a dead slot.
    pub fn abandon(&self) {
        let mut inner = self.inner.lock();
        if inner.state == SlotState::Pending {
            inner.state = SlotState::Abandoned;
        }
        drop(inner);
        self.changed.notify_all();
    }

    pub fn wait(&self, deadline: Instant) -> SlotWait {
        let mut inner = self.inner.lock();
        loop {
            match inner.state {
                SlotState::Ready => {
                    let index = inner.index.clone().expect("ready slot has an index");
                    return SlotWait::Ready(index);
                }
                SlotState::Failed => return SlotWait::Failed(inner.message.clone()),
                SlotState::Abandoned => return SlotWait::Abandoned,
                SlotState::Pending => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return SlotWait::TimedOut;
            }
            self.changed.wait_for(&mut inner, deadline - now);
        }
    }
}

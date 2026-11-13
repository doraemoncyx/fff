use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use heed::Database;
use heed::types::Bytes;

use super::env_pool::SharedEnv;
use super::snapshot_budget::SnapshotBudget;
use super::snapshot_ops;
use super::snapshot_state::SnapshotState;
use super::snapshot_store::{ROOT_PREFIX_LEN, SnapshotMeta};

// Pending work the writer may hold before the oldest row batch is dropped.
// A cold scan of a large tree produces more batches than this (137k files is
// ~68 of 2000 rows), so the cap is a backstop, not a guarantee: a scan bigger
// than the queue relies on the writer keeping up, and a dropped batch is a
// handful of misses on the next scan.
const MAX_QUEUED_BATCHES: usize = 32;
const MAX_QUEUED_BYTES: u64 = 64 * 1024 * 1024;
// Teardown waits this long for the queue, then lets the thread go.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(2000);
#[cfg(test)]
const IDLE_WAIT: Duration = Duration::from_millis(50);

pub(crate) enum WriteJob {
    Rows {
        prefix: [u8; ROOT_PREFIX_LEN],
        rows: Vec<([u8; 32], Vec<u8>)>,
        bytes: u64,
    },
    Sweep {
        prefix: [u8; ROOT_PREFIX_LEN],
        root: String,
        keep: Arc<HashSet<[u8; 32]>>,
    },
    Meta {
        prefix: [u8; ROOT_PREFIX_LEN],
        meta: SnapshotMeta,
    },
    Housekeep,
}

impl WriteJob {
    // Only row batches are expendable; a lost batch is a miss next scan.
    fn is_rows(&self) -> bool {
        matches!(self, Self::Rows { .. })
    }

    fn size(&self) -> u64 {
        match self {
            Self::Rows { bytes, .. } => *bytes,
            _ => 0,
        }
    }
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<WriteJob>,
    queued_bytes: u64,
    busy: bool,
    shutdown: bool,
    finished: bool,
    #[cfg(test)]
    paused: bool,
}

#[derive(Default)]
pub(crate) struct WriterCounters {
    pub(crate) commits: AtomicU64,
    pub(crate) rows: AtomicU64,
    pub(crate) dropped: AtomicU64,
}

// The single thread that touches the store's write lock. Producers only ever
// take this mutex to enqueue, so a blocking transaction never stalls a scan.
pub(crate) struct SnapshotWriter {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    counters: Arc<WriterCounters>,
    handle: Option<JoinHandle<()>>,
}

impl SnapshotWriter {
    pub(crate) fn spawn(
        env: SharedEnv,
        db: Database<Bytes, Bytes>,
        budget: Arc<SnapshotBudget>,
        state: Arc<SnapshotState>,
    ) -> Self {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let counters = Arc::new(WriterCounters::default());
        let thread_shared = Arc::clone(&shared);
        let thread_counters = Arc::clone(&counters);

        let handle = std::thread::Builder::new()
            .name("fff-snapshot-writer".into())
            .spawn(move || run(thread_shared, thread_counters, env, db, budget, state));
        let handle = match handle {
            Ok(handle) => Some(handle),
            Err(error) => {
                // No drain thread: the queue stays bounded, so writes simply miss.
                tracing::warn!(?error, "snapshot writer thread failed to start");
                None
            }
        };

        Self {
            shared,
            counters,
            handle,
        }
    }

    pub(crate) fn push_rows(
        &self,
        prefix: [u8; ROOT_PREFIX_LEN],
        rows: Vec<([u8; 32], Vec<u8>)>,
    ) -> u64 {
        let bytes = batch_bytes(&rows);
        if rows.is_empty() {
            return bytes;
        }
        let (lock, wake) = &*self.shared;
        let mut queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
        queue.jobs.push_back(WriteJob::Rows {
            prefix,
            rows,
            bytes,
        });
        queue.queued_bytes += bytes;
        self.drop_overflowed(&mut queue);
        drop(queue);
        wake.notify_one();
        bytes
    }

    pub(crate) fn push_sweep(
        &self,
        prefix: [u8; ROOT_PREFIX_LEN],
        root: String,
        keep: Arc<HashSet<[u8; 32]>>,
    ) {
        self.push(WriteJob::Sweep { prefix, root, keep });
    }

    pub(crate) fn push_meta(&self, prefix: [u8; ROOT_PREFIX_LEN], meta: SnapshotMeta) {
        self.push(WriteJob::Meta { prefix, meta });
    }

    pub(crate) fn housekeep(&self) {
        self.push(WriteJob::Housekeep);
    }

    // True when everything queued before the call has been applied.
    #[cfg(test)]
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let (lock, wake) = &*self.shared;
        let queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (queue, _) = wake
            .wait_timeout_while(queue, timeout, |queue| !queue.jobs.is_empty() || queue.busy)
            .unwrap_or_else(|e| e.into_inner());
        queue.jobs.is_empty() && !queue.busy
    }

    #[cfg(test)]
    pub(crate) fn counters(&self) -> &WriterCounters {
        &self.counters
    }

    #[cfg(test)]
    pub(crate) fn pause(&self) {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap_or_else(PoisonError::into_inner).paused = true;
    }

    #[cfg(test)]
    pub(crate) fn resume(&self) {
        let (lock, wake) = &*self.shared;
        lock.lock().unwrap_or_else(PoisonError::into_inner).paused = false;
        wake.notify_all();
    }

    fn push(&self, job: WriteJob) {
        let (lock, wake) = &*self.shared;
        let mut queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
        queue.jobs.push_back(job);
        self.drop_overflowed(&mut queue);
        drop(queue);
        wake.notify_one();
    }

    // Keeps the newest job: the batch just queued is never the victim.
    fn drop_overflowed(&self, queue: &mut Queue) {
        while queue.jobs.len() > MAX_QUEUED_BATCHES || queue.queued_bytes > MAX_QUEUED_BYTES {
            let last = queue.jobs.len() - 1;
            let Some(index) = queue.jobs.iter().take(last).position(WriteJob::is_rows) else {
                break;
            };
            if let Some(dropped) = queue.jobs.remove(index) {
                queue.queued_bytes = queue.queued_bytes.saturating_sub(dropped.size());
                let dropped = self.counters.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(dropped, "snapshot writer is behind; dropped a row batch");
            }
        }
    }
}

impl Drop for SnapshotWriter {
    fn drop(&mut self) {
        {
            let (lock, wake) = &*self.shared;
            lock.lock().unwrap_or_else(PoisonError::into_inner).shutdown = true;
            wake.notify_all();
        }

        let finished = {
            let (lock, wake) = &*self.shared;
            let queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let (queue, _) = wake
                .wait_timeout_while(queue, DRAIN_TIMEOUT, |queue| !queue.finished)
                .unwrap_or_else(|e| e.into_inner());
            queue.finished
        };

        // Anything still unflushed after the timeout is a miss next scan.
        if finished && let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(
    shared: Arc<(Mutex<Queue>, Condvar)>,
    counters: Arc<WriterCounters>,
    env: SharedEnv,
    db: Database<Bytes, Bytes>,
    budget: Arc<SnapshotBudget>,
    state: Arc<SnapshotState>,
) {
    loop {
        let Some(job) = next_job(&shared) else {
            return;
        };
        // A disabled store has nothing left to write: drop the work, keep the
        // queue draining so no producer blocks.
        if state.is_disabled() {
            settle(&shared);
            continue;
        }
        match apply(&job, &counters, &env, &db, &budget) {
            Err(error) => state.disable(&error),
            Ok(()) if !matches!(job, WriteJob::Housekeep) => {
                if let Err(error) = snapshot_ops::enforce_limits(&env, &db, &budget) {
                    state.disable(&error);
                }
            }
            Ok(()) => {}
        }
        settle(&shared);
    }
}

// Blocks until there is work or the writer is told to stop.
fn next_job(shared: &Arc<(Mutex<Queue>, Condvar)>) -> Option<WriteJob> {
    let (lock, wake) = &**shared;
    let mut queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        #[cfg(test)]
        while queue.paused {
            queue = wake
                .wait_timeout(queue, IDLE_WAIT)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if let Some(job) = queue.jobs.pop_front() {
            queue.queued_bytes = queue.queued_bytes.saturating_sub(job.size());
            queue.busy = true;
            return Some(job);
        }
        if queue.shutdown {
            queue.finished = true;
            wake.notify_all();
            return None;
        }
        queue = wake.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

fn batch_bytes(rows: &[([u8; 32], Vec<u8>)]) -> u64 {
    rows.iter().map(|(_, value)| value.len() as u64).sum()
}

fn settle(shared: &Arc<(Mutex<Queue>, Condvar)>) {
    // Clears the in-flight flag and wakes anything waiting for an empty queue.
    let (lock, wake) = &**shared;
    let mut queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
    queue.busy = false;
    if queue.jobs.is_empty() {
        wake.notify_all();
    }
}

fn apply(
    job: &WriteJob,
    counters: &WriterCounters,
    env: &SharedEnv,
    db: &Database<Bytes, Bytes>,
    budget: &SnapshotBudget,
) -> crate::error::Result<()> {
    match job {
        WriteJob::Rows { prefix, rows, .. } => {
            snapshot_ops::commit_rows(env, db, prefix, rows)?;
            counters.commits.fetch_add(1, Ordering::Relaxed);
            counters
                .rows
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        WriteJob::Sweep { prefix, root, keep } => {
            let sweep = snapshot_ops::sweep_rows(env, db, prefix, keep, root)?;
            counters.commits.fetch_add(1, Ordering::Relaxed);
            if sweep.deleted > 0 {
                tracing::debug!(
                    deleted = sweep.deleted,
                    remaining = sweep.meta.logical_bytes,
                    "snapshot sweep dropped orphan rows"
                );
            }
            budget.record(*prefix, &sweep.meta);
        }
        WriteJob::Meta { prefix, meta } => {
            snapshot_ops::put_meta(env, db, prefix, meta)?;
            counters.commits.fetch_add(1, Ordering::Relaxed);
            budget.record(*prefix, meta);
        }
        WriteJob::Housekeep => {}
    }
    Ok(())
}

use std::borrow::Cow;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::dbs::snapshot_store::{ROW_BATCH, SnapshotStore};
use crate::index::bigram_row::Row;
use crate::simd_path::{ArenaPtr, PATH_BUF_SIZE};
use crate::types::FileItem;

// Collects a scan's Snapshot rows and hands them to the store in batches.
pub(crate) struct RowSink<'a> {
    store: &'a SnapshotStore,
    root: &'a Path,
    pending: Mutex<Vec<([u8; 32], Vec<u8>)>>,
    bytes: AtomicU64,
    failed: AtomicBool,
}

impl<'a> RowSink<'a> {
    pub(crate) fn new(store: &'a SnapshotStore, root: &'a Path) -> Self {
        Self {
            store,
            root,
            pending: Mutex::new(Vec::with_capacity(ROW_BATCH)),
            bytes: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }
    }

    // A file whose row cannot be encoded is dropped, never a scan failure.
    pub(crate) fn push(&self, relative_path: &str, row: &Row) {
        if self.failed.load(Ordering::Relaxed) {
            return;
        }
        let Some(value) = row.encode() else {
            return;
        };
        let payload = *blake3::hash(relative_path.as_bytes()).as_bytes();

        let batch = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.push((payload, value));
            if pending.len() < ROW_BATCH {
                return;
            }
            std::mem::take(&mut *pending)
        };
        self.flush(batch);
    }

    // Verdict-only rows: a file the content pass never opened still records
    // its identity and its Binary verdict.
    pub(crate) fn push_verdicts(
        &self,
        files: &[FileItem],
        offset: usize,
        mtime_ns: &[u64],
        arena: ArenaPtr,
    ) {
        let mut buf = [0u8; PATH_BUF_SIZE];
        let mut row = Row {
            mtime_ns: 0,
            size: 0,
            binary: false,
            consecutive: Vec::new(),
            skip1: Vec::new(),
        };
        for (index, file) in files.iter().enumerate() {
            let Some(&ns) = mtime_ns.get(offset + index) else {
                continue;
            };
            row.mtime_ns = ns;
            row.size = file.size;
            row.binary = file.is_binary();
            let rel = relative_path(file, arena, &mut buf);
            self.push(&rel, &row);
        }
    }

    // Flushes the tail and returns the row bytes this scan wrote.
    pub(crate) fn finish(&self) -> u64 {
        let batch = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *pending)
        };
        if !batch.is_empty() {
            self.flush(batch);
        }
        self.bytes.load(Ordering::Relaxed)
    }

    fn flush(&self, batch: Vec<([u8; 32], Vec<u8>)>) {
        match self.store.put_rows(self.root, batch.into_iter()) {
            Ok(bytes) => {
                self.bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Err(e) => {
                self.failed.store(true, Ordering::Relaxed);
                tracing::warn!(?e, "snapshot write failed; no Snapshot for this scan");
            }
        }
    }
}

// Borrows the arena's text unless the path does not fit, where it must own it.
pub(crate) fn relative_path<'b>(
    file: &FileItem,
    arena: ArenaPtr,
    buf: &'b mut [u8],
) -> Cow<'b, str> {
    if file.relative_path_len() < buf.len() {
        Cow::Borrowed(file.read_relative_path(arena, buf))
    } else {
        Cow::Owned(file.relative_path(arena))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::bigram_filter::MAX_BIGRAM_COLUMNS;

    fn row(consecutive: Vec<u16>, skip1: Vec<u16>) -> Row {
        Row {
            mtime_ns: 1,
            size: 2,
            binary: false,
            consecutive,
            skip1,
        }
    }

    #[test]
    fn a_row_that_cannot_be_encoded_is_dropped_without_losing_the_others() {
        let root = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let store = SnapshotStore::open(db.path()).unwrap();
        let sink = RowSink::new(&store, root.path());

        let over_cap: Vec<u16> = (0..=MAX_BIGRAM_COLUMNS as u16).collect();
        sink.push("over.txt", &row(over_cap, Vec::new()));
        // Descending keys are refused by the codec too.
        sink.push("unsorted.txt", &row(vec![20, 10], Vec::new()));
        sink.push("ok.txt", &row(vec![0x6162], vec![0x6163]));

        assert!(sink.finish() > 0);
        assert_eq!(store.row_count(root.path()).unwrap(), 1);
    }

    #[test]
    fn a_finished_sink_leaves_nothing_pending() {
        let root = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let store = SnapshotStore::open(db.path()).unwrap();
        let sink = RowSink::new(&store, root.path());
        sink.push("a.txt", &row(vec![0x6162], Vec::new()));
        let written = sink.finish();

        assert_eq!(written, store.logical_bytes(root.path()).unwrap());
        assert_eq!(store.row_count(root.path()).unwrap(), 1);
    }
}

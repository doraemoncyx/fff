use std::borrow::Cow;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::dbs::snapshot_store::{ROW_BATCH, SnapshotStore};
use crate::index::bigram_row::Row;
use crate::simd_path::{ArenaPtr, PATH_BUF_SIZE};
use crate::types::FileItem;

// Collects a scan's Snapshot rows and hands whole batches to the store's writer.
pub(crate) struct RowSink<'a> {
    store: &'a SnapshotStore,
    root: &'a Path,
    pending: Mutex<Vec<([u8; 32], Vec<u8>)>>,
    bytes: AtomicU64,
}

impl<'a> RowSink<'a> {
    pub(crate) fn new(store: &'a SnapshotStore, root: &'a Path) -> Self {
        store.mark_active(root);
        Self {
            store,
            root,
            pending: Mutex::new(Vec::with_capacity(ROW_BATCH)),
            bytes: AtomicU64::new(0),
        }
    }

    // A file whose row cannot be encoded is dropped, never a scan failure.
    pub(crate) fn push(&self, relative_path: &str, row: &Row) {
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

    // A row with no keys: the identity and the Binary verdict of a file the
    // content pass never opened.
    pub(crate) fn push_verdict(
        &self,
        file: &FileItem,
        mtime_ns: u64,
        arena: ArenaPtr,
        buf: &mut [u8; PATH_BUF_SIZE],
    ) {
        let row = Row {
            mtime_ns,
            size: file.size,
            binary: file.is_binary(),
            consecutive: Vec::new(),
            skip1: Vec::new(),
        };
        let rel = relative_path(file, arena, buf);
        self.push(&rel, &row);
    }

    // Hands the tail to the writer and returns the row bytes this scan queued.
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
        let bytes = self.store.write_rows(self.root, batch);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Drop for RowSink<'_> {
    fn drop(&mut self) {
        self.store.mark_idle(self.root);
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
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(30);

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
        assert!(store.flush(WAIT));
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

        assert!(store.flush(WAIT));
        assert_eq!(written, store.logical_bytes(root.path()).unwrap());
        assert_eq!(store.row_count(root.path()).unwrap(), 1);
    }
}

use std::path::Path;

use crate::dbs::SnapshotStore;
use crate::index::bigram_row::Row;
use crate::index::snapshot_write::relative_path;
use crate::simd_path::{ArenaPtr, PATH_BUF_SIZE};
use crate::types::FileItem;

// Serves a Root's Snapshot rows to the content-index build, one chunk at a time.
pub(crate) struct RowSource<'a> {
    store: &'a SnapshotStore,
    root: &'a Path,
    current: bool,
}

impl<'a> RowSource<'a> {
    pub(crate) fn new(store: &'a SnapshotStore, root: &'a Path) -> Self {
        Self {
            store,
            root,
            current: store.root_is_current(root),
        }
    }

    // Whether the Root already held rows when this scan started. Without them
    // the scan is a first write and has no orphans to sweep.
    pub(crate) fn was_current(&self) -> bool {
        self.current
    }

    // One read transaction per chunk, opened and dropped on this thread. One
    // slot per file: `None` is a missing row, a stale identity, or a read error.
    pub(crate) fn fetch(
        &self,
        files: &[FileItem],
        mtime_ns: &[u64],
        arena: ArenaPtr,
        buf: &mut [u8; PATH_BUF_SIZE],
    ) -> Vec<Option<Row>> {
        let mut slots = vec![None; files.len()];
        if !self.current || files.is_empty() {
            return slots;
        }

        let mut payloads = Vec::with_capacity(files.len());
        for file in files {
            let rel = relative_path(file, arena, buf);
            payloads.push(*blake3::hash(rel.as_bytes()).as_bytes());
        }

        let mut stored = Vec::with_capacity(files.len());
        // The store answers a broken environment with misses and has already
        // warned once; a read error here is the same "no cache" state.
        if self
            .store
            .get_rows(self.root, &payloads, &mut stored)
            .is_err()
        {
            return slots;
        }

        for (index, entry) in stored.into_iter().enumerate() {
            let Some((row, _)) = entry else {
                continue;
            };
            // Identity is size plus nanosecond mtime and nothing else.
            let mtime = mtime_ns.get(index).copied().unwrap_or(0);
            if row.size != files[index].size || row.mtime_ns != mtime {
                continue;
            }
            if !replayable(&row) {
                continue;
            }
            slots[index] = Some(row);
        }
        slots
    }
}

// Keys must be pairs of printable bytes; a row outside that range would drop a
// column bit and hide a match, so it is read again instead.
fn replayable(row: &Row) -> bool {
    let printable = |key: u16| {
        let (hi, lo) = ((key >> 8) as u8, key as u8);
        (32..=126).contains(&hi) && (32..=126).contains(&lo)
    };
    row.consecutive
        .iter()
        .chain(&row.skip1)
        .all(|&key| printable(key))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn only_printable_key_pairs_are_replayed() {
        assert!(replayable(&row(vec![0x6162, 0x7A7A], vec![0x6163])));
        assert!(replayable(&row(Vec::new(), Vec::new())));
        assert!(!replayable(&row(vec![0x6162], vec![0x0063])));
        assert!(!replayable(&row(vec![0x1F62], Vec::new())));
    }
}

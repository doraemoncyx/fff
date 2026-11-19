use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use crate::dbs::SnapshotStore;
use crate::file_picker::{FilePickerIgnoreOptions, FileSync, expand_symlink_dir_pairs};
use crate::parallelism::BACKGROUND_THREAD_POOL;
use crate::types::FileItem;
use crate::walk::{WalkLimits, walk_collect_files};

// One hash lookup per chunk: a Root's rows never sit in memory all at once.
const ROW_CHUNK: usize = 1024;

/// One file's Snapshot verdict, paired with the extension its row cannot store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictSample {
    /// Lowercase, no leading dot; `None` when the name has no extension at all.
    pub extension: Option<String>,
    pub binary: bool,
}

/// What one Root's join produced, and how much of its walk it accounts for.
#[derive(Debug)]
pub struct RootVerdicts {
    pub root: String,
    /// One sample per file whose row matched its identity on disk.
    pub samples: Vec<VerdictSample>,
    /// Files the name-level walk saw, matched or not.
    pub walked: usize,
    /// Rows the store holds for this Root, replayable or stale.
    pub rows: u64,
    /// The Root's meta names this build, so its rows are replayable at all.
    pub current: bool,
}

/// Joins a Root's Snapshot rows to a name-level walk of the same tree. A row
/// carries no path, so the extension comes from the file and the verdict from
/// the row hashing to that file's relative path.
pub fn root_verdicts(store: &SnapshotStore, root: &Path) -> crate::Result<RootVerdicts> {
    let rows = store.row_count(root)?;
    let current = store.root_is_current(root);
    let walked = walk_names(root)?;

    let mut samples = Vec::with_capacity(walked.len());
    let mut payloads: Vec<[u8; 32]> = Vec::with_capacity(ROW_CHUNK);
    let mut stored = Vec::with_capacity(ROW_CHUNK);
    for chunk in walked.chunks(ROW_CHUNK) {
        payloads.clear();
        for (_, relative, _) in chunk {
            payloads.push(*blake3::hash(relative.as_bytes()).as_bytes());
        }
        store.get_rows(root, &payloads, &mut stored)?;

        for ((file, relative, mtime), row) in chunk.iter().zip(&stored) {
            let Some((row, _)) = row else {
                continue;
            };
            // Size plus nanosecond mtime is the row's identity: the same path
            // holding a different file is a miss, never a sample.
            if row.size != file.size || row.mtime_ns != *mtime {
                continue;
            }
            samples.push(VerdictSample {
                extension: crate::binary_verdict::extension_of(relative)
                    .map(str::to_ascii_lowercase),
                binary: row.binary,
            });
        }
    }

    Ok(RootVerdicts {
        root: root.display().to_string(),
        samples,
        walked: walked.len(),
        rows,
        current,
    })
}

// Names and identities only: the audit never reads a file's contents.
// Mirrors `walk_filesystem`'s file table: symlink-dir targets expanded, the
// markers themselves dropped, so `walked` counts exactly what a scan indexed.
fn walk_names(root: &Path) -> crate::Result<Vec<(FileItem, String, u64)>> {
    let is_git_repo = FileSync::discover_git(root, false).is_some();
    let mut walked = walk_collect_files(
        root,
        is_git_repo,
        false,
        BACKGROUND_THREAD_POOL.current_num_threads(),
        &Arc::new(AtomicUsize::new(0)),
        &FilePickerIgnoreOptions::default(),
        None,
        WalkLimits::unlimited(),
    )?;
    let ignore_rules = walked.ignore_rules.take().map(Arc::new);
    expand_symlink_dir_pairs(
        root,
        &mut walked.pairs,
        is_git_repo,
        ignore_rules.as_deref(),
    );
    Ok(walked
        .pairs
        .into_iter()
        .filter(|(file, _, _)| !file.is_symlink_dir())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::bigram_row::Row;
    use std::fs;

    fn row(size: u64, mtime_ns: u64, binary: bool) -> Row {
        Row {
            mtime_ns,
            size,
            binary,
            consecutive: Vec::new(),
            skip1: Vec::new(),
        }
    }

    // The join is by path hash, so this is the smallest fixture that proves
    // extensions come from the walk while verdicts come from the rows.
    #[test]
    fn rows_join_to_the_files_that_hash_to_them() {
        let tree = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = tree.path();
        for name in ["art/wall.png", "art/notes.mtl", "src/main.rs", "NOEXT"] {
            fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
            fs::write(root.join(name), b"x").unwrap();
        }

        let store = SnapshotStore::open(db.path()).unwrap();
        let mut seeded = Vec::new();
        for name in ["art/wall.png", "art/notes.mtl", "src/main.rs", "NOEXT"] {
            let modified = fs::metadata(root.join(name)).unwrap().modified().ok();
            let binary = name.ends_with(".png");
            seeded.push((
                *blake3::hash(name.as_bytes()).as_bytes(),
                row(1, crate::walk::mtime_ns(modified), binary)
                    .encode()
                    .unwrap(),
            ));
        }
        store.put_rows(root, seeded.into_iter()).unwrap();
        store.put_meta(root, 4).unwrap();

        let joined = root_verdicts(&store, root).unwrap();
        let mut got: Vec<(Option<String>, bool)> = joined
            .samples
            .iter()
            .map(|s| (s.extension.clone(), s.binary))
            .collect();
        got.sort();

        assert_eq!(joined.walked, 4);
        assert!(joined.current);
        assert_eq!(
            got,
            [
                (None, false),
                (Some("mtl".to_string()), false),
                (Some("png".to_string()), true),
                (Some("rs".to_string()), false),
            ]
        );
    }

    // A stale row must never answer: the file changed, so its verdict is unknown
    // until the next scan rewrites it.
    #[test]
    fn a_row_whose_identity_moved_is_not_a_sample() {
        let tree = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let root = tree.path();
        fs::write(root.join("payload.bin"), b"x").unwrap();

        let store = SnapshotStore::open(db.path()).unwrap();
        let payload = *blake3::hash(b"payload.bin").as_bytes();
        // The right path, the wrong size.
        let stale = row(4096, 1, true).encode().unwrap();
        store
            .put_rows(root, [(payload, stale)].into_iter())
            .unwrap();

        let joined = root_verdicts(&store, root).unwrap();
        assert_eq!(joined.walked, 1);
        assert!(joined.samples.is_empty(), "{:?}", joined.samples);
    }

    // A Root the store never saw is not an error: it answers with no samples.
    #[test]
    fn a_root_without_rows_reports_no_samples() {
        let tree = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        fs::write(tree.path().join("a.png"), b"x").unwrap();

        let store = SnapshotStore::open(db.path()).unwrap();
        let joined = root_verdicts(&store, tree.path()).unwrap();

        assert_eq!(joined.walked, 1);
        assert_eq!(joined.rows, 0);
        assert!(!joined.current);
        assert!(joined.samples.is_empty());
    }
}

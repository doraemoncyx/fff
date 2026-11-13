use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use heed::Database;
use heed::types::{Bytes, SerdeBincode};
use serde::{Deserialize, Serialize};

use super::env_pool::SharedEnv;
use crate::constants::MAX_INDEXABLE_FILE_SIZE;
use crate::error::{Error, Result};
use crate::index::bigram_filter::{
    BIGRAM_CHUNK_FILES, MAX_BIGRAM_COLUMNS, SKIP_INDEX_MIN_DENSITY_PCT,
};
use crate::index::bigram_row::Row;
use crate::lmdb::{DbHealth, LmdbStore};

pub const SNAPSHOT_SCHEMA: u16 = 1;

const KIND_META: u8 = 0;
const KIND_FILE: u8 = 1;
const ROOT_PREFIX_LEN: usize = 16;
const KEY_LEN: usize = ROOT_PREFIX_LEN + 1 + 32;
// Commit point for Snapshot rows: one write transaction per this many rows.
pub(crate) const ROW_BATCH: usize = 2000;
const NO_PAYLOAD: [u8; 32] = [0u8; 32];

/// A Root's Snapshot record: what derived its rows, how big they are, and when
/// it was last written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub schema: u16,
    pub fingerprint: u64,
    pub logical_bytes: u64,
    pub last_used: u64,
    pub root: String,
}

/// One LMDB env holding every Root's Snapshot rows, keyed by root prefix.
pub struct SnapshotStore {
    env: SharedEnv,
    db: Database<Bytes, Bytes>,
    health: DbHealth,
}

impl LmdbStore for SnapshotStore {
    const LABEL: &'static str = "snapshot";
    const MAP_SIZE: usize = 2 * 1024 * 1024 * 1024;
    const MAX_DBS: u32 = 0;
    // Never erase the env: an oversized one is shrunk per Root, never wholesale.
    const SIZE_CAP_BYTES: u64 = u64::MAX;

    fn shared_env(&self) -> &SharedEnv {
        &self.env
    }

    fn health(&self) -> &DbHealth {
        &self.health
    }
}

impl SnapshotStore {
    pub fn open(path: &Path) -> Result<Self> {
        let (env, health) = Self::open_env(path)?;
        let db: Database<Bytes, Bytes> = Self::open_database_safe(&env, None)?;
        Ok(Self { env, db, health })
    }

    pub fn put_meta(&self, root: &Path, logical_bytes: u64) -> Result<()> {
        self.write_meta(
            root,
            &SnapshotMeta {
                schema: SNAPSHOT_SCHEMA,
                fingerprint: fingerprint(),
                logical_bytes,
                last_used: now_secs(),
                root: normalized_root(root),
            },
        )
    }

    // A Root's rows may be replayed only when its meta names this Root and was
    // written by this build of the derivation; otherwise every row is a miss.
    pub(crate) fn root_is_current(&self, root: &Path) -> bool {
        let Ok(Some(meta)) = self.read_meta(root) else {
            return false;
        };
        meta.schema == SNAPSHOT_SCHEMA
            && meta.fingerprint == fingerprint()
            && meta.root == normalized_root(root)
    }

    // Reads one chunk's rows in a single read transaction, dropped before the
    // caller handles them. `None` is a missing or undecodable row; the second
    // field is the stored byte length, which the caller counts as still on disk.
    pub(crate) fn get_rows(
        &self,
        root: &Path,
        payloads: &[[u8; 32]],
        out: &mut Vec<Option<(Row, usize)>>,
    ) -> Result<()> {
        out.clear();
        out.reserve(payloads.len());
        let rtxn = self
            .env
            .read_txn()
            .map_err(|source| Error::DbStartReadTxn {
                db: Self::LABEL,
                source,
            })?;
        let prefix = root_prefix(root);
        for payload in payloads {
            let key = key_bytes(&prefix, KIND_FILE, payload);
            let value = self.db.get(&rtxn, &key).map_err(|source| Error::DbRead {
                db: Self::LABEL,
                source,
            })?;
            out.push(value.and_then(|bytes| Row::decode(bytes).map(|row| (row, bytes.len()))));
        }
        Ok(())
    }

    pub fn read_meta(&self, root: &Path) -> Result<Option<SnapshotMeta>> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|source| Error::DbStartReadTxn {
                db: Self::LABEL,
                source,
            })?;
        self.meta_db()
            .get(&rtxn, &meta_key(&root_prefix(root)))
            .map_err(|source| Error::DbRead {
                db: Self::LABEL,
                source,
            })
    }

    // Writes `(relative path hash, encoded row)` pairs, committing every
    // `ROW_BATCH` rows. Returns the bytes written.
    pub(crate) fn put_rows<I>(&self, root: &Path, rows: I) -> Result<u64>
    where
        I: Iterator<Item = ([u8; 32], Vec<u8>)>,
    {
        let prefix = root_prefix(root);
        let mut batch: Vec<([u8; 32], Vec<u8>)> = Vec::with_capacity(ROW_BATCH);
        let mut written = 0u64;
        for (payload, value) in rows {
            written += value.len() as u64;
            batch.push((payload, value));
            if batch.len() >= ROW_BATCH {
                self.commit_rows(&prefix, &mut batch)?;
            }
        }
        if !batch.is_empty() {
            self.commit_rows(&prefix, &mut batch)?;
        }
        Ok(written)
    }

    pub fn row_count(&self, root: &Path) -> Result<u64> {
        let mut count = 0u64;
        self.for_each_row(root, |_| count += 1)?;
        Ok(count)
    }

    pub fn logical_bytes(&self, root: &Path) -> Result<u64> {
        let mut total = 0u64;
        self.for_each_row(root, |value| total += value.len() as u64)?;
        Ok(total)
    }

    fn for_each_row(&self, root: &Path, mut on_value: impl FnMut(&[u8])) -> Result<()> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|source| Error::DbStartReadTxn {
                db: Self::LABEL,
                source,
            })?;
        let prefix = file_prefix(root);
        let iter = self
            .db
            .prefix_iter(&rtxn, &prefix)
            .map_err(|source| Error::DbRead {
                db: Self::LABEL,
                source,
            })?;
        for entry in iter {
            let (_, value) = entry.map_err(|source| Error::DbRead {
                db: Self::LABEL,
                source,
            })?;
            on_value(value);
        }
        Ok(())
    }

    fn meta_db(&self) -> Database<Bytes, SerdeBincode<SnapshotMeta>> {
        self.db.remap_data_type()
    }

    fn write_meta(&self, root: &Path, meta: &SnapshotMeta) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|source| Error::DbStartWriteTxn {
                db: Self::LABEL,
                source,
            })?;
        self.meta_db()
            .put(&mut wtxn, &meta_key(&root_prefix(root)), meta)
            .map_err(|source| Error::DbWrite {
                db: Self::LABEL,
                source,
            })?;
        wtxn.commit().map_err(|source| Error::DbCommit {
            db: Self::LABEL,
            source,
        })
    }

    // Lets tests plant a meta that a future build wrote, or another Root's.
    #[cfg(test)]
    pub(crate) fn put_foreign_meta(
        &self,
        root: &Path,
        schema: u16,
        fingerprint: u64,
        stored_root: &str,
    ) -> Result<()> {
        self.write_meta(
            root,
            &SnapshotMeta {
                schema,
                fingerprint,
                logical_bytes: 0,
                last_used: now_secs(),
                root: stored_root.to_string(),
            },
        )
    }

    fn commit_rows(
        &self,
        prefix: &[u8; ROOT_PREFIX_LEN],
        batch: &mut Vec<([u8; 32], Vec<u8>)>,
    ) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|source| Error::DbStartWriteTxn {
                db: Self::LABEL,
                source,
            })?;
        for (payload, value) in batch.iter() {
            let key = key_bytes(prefix, KIND_FILE, payload);
            self.db
                .put(&mut wtxn, &key, value)
                .map_err(|source| Error::DbWrite {
                    db: Self::LABEL,
                    source,
                })?;
        }
        wtxn.commit().map_err(|source| Error::DbCommit {
            db: Self::LABEL,
            source,
        })?;
        batch.clear();
        Ok(())
    }
}

fn key_bytes(prefix: &[u8; ROOT_PREFIX_LEN], kind: u8, payload: &[u8; 32]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    key[..ROOT_PREFIX_LEN].copy_from_slice(prefix);
    key[ROOT_PREFIX_LEN] = kind;
    key[ROOT_PREFIX_LEN + 1..].copy_from_slice(payload);
    key
}

fn meta_key(prefix: &[u8; ROOT_PREFIX_LEN]) -> [u8; KEY_LEN] {
    key_bytes(prefix, KIND_META, &NO_PAYLOAD)
}

// Shared by every row of a Root and nothing else, so a Root is one range.
fn file_prefix(root: &Path) -> [u8; ROOT_PREFIX_LEN + 1] {
    let prefix = root_prefix(root);
    let mut out = [0u8; ROOT_PREFIX_LEN + 1];
    out[..ROOT_PREFIX_LEN].copy_from_slice(&prefix);
    out[ROOT_PREFIX_LEN] = KIND_FILE;
    out
}

fn root_prefix(root: &Path) -> [u8; ROOT_PREFIX_LEN] {
    let mut prefix = [0u8; ROOT_PREFIX_LEN];
    let hash = blake3::hash(normalized_root(root).as_bytes());
    prefix.copy_from_slice(&hash.as_bytes()[..ROOT_PREFIX_LEN]);
    prefix
}

// Same rule the service keys Roots with: canonical, `/`-separated, and ASCII
// lowercased on Windows.
fn normalized_root(root: &Path) -> String {
    let canonical = crate::path_utils::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let text = canonical.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    let text = text.trim_end_matches('/');
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text.to_string()
    }
}

// Hash of the constants a row's bytes depend on, so a row derived differently
// cannot be mistaken for a hit.
fn fingerprint() -> u64 {
    static FINGERPRINT: OnceLock<u64> = OnceLock::new();
    *FINGERPRINT.get_or_init(|| {
        let mut hasher = blake3::Hasher::new();
        for part in [
            SNAPSHOT_SCHEMA as u64,
            MAX_INDEXABLE_FILE_SIZE as u64,
            MAX_BIGRAM_COLUMNS as u64,
            SKIP_INDEX_MIN_DENSITY_PCT as u64,
            BIGRAM_CHUNK_FILES as u64,
        ] {
            hasher.update(&part.to_le_bytes());
        }
        let hash = hasher.finalize();
        u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("8 bytes"))
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u8, len: usize) -> ([u8; 32], Vec<u8>) {
        ([id; 32], vec![id; len])
    }

    #[test]
    fn meta_survives_a_reopen_of_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let db = dir.path().join("snapshot-db");
        std::fs::create_dir_all(&root).unwrap();

        let first = SnapshotStore::open(&db).unwrap();
        first.put_meta(&root, 1234).unwrap();
        drop(first);

        let second = SnapshotStore::open(&db).unwrap();
        let meta = second.read_meta(&root).unwrap().expect("meta row");
        assert_eq!(meta.schema, SNAPSHOT_SCHEMA);
        assert_eq!(meta.fingerprint, fingerprint());
        assert_eq!(meta.logical_bytes, 1234);
        assert!(meta.last_used > 0);
        assert!(meta.root.ends_with("repo"));
    }

    #[test]
    fn two_roots_in_one_env_never_see_each_others_rows() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let store = SnapshotStore::open(&dir.path().join("snapshot-db")).unwrap();
        store
            .put_rows(&first, [row(1, 10), row(2, 20), row(3, 30)].into_iter())
            .unwrap();
        store.put_rows(&second, [row(4, 40)].into_iter()).unwrap();
        store.put_meta(&first, 60).unwrap();
        store.put_meta(&second, 40).unwrap();

        assert_eq!(store.row_count(&first).unwrap(), 3);
        assert_eq!(store.row_count(&second).unwrap(), 1);
        assert_eq!(store.logical_bytes(&first).unwrap(), 60);
        assert_eq!(store.logical_bytes(&second).unwrap(), 40);
        assert_eq!(store.read_meta(&first).unwrap().unwrap().logical_bytes, 60);
        assert_eq!(store.read_meta(&second).unwrap().unwrap().logical_bytes, 40);
    }

    #[test]
    fn more_rows_than_one_batch_are_all_committed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = SnapshotStore::open(&root.join("snapshot-db")).unwrap();

        let total = ROW_BATCH * 2 + 7;
        let rows = (0..total).map(|i| {
            let mut payload = [0u8; 32];
            payload[..8].copy_from_slice(&(i as u64).to_le_bytes());
            (payload, vec![7u8; 20])
        });
        let written = store.put_rows(root, rows).unwrap();

        assert_eq!(store.row_count(root).unwrap(), total as u64);
        assert_eq!(written, (total * 20) as u64);
        assert_eq!(store.logical_bytes(root).unwrap(), written);
    }

    #[test]
    fn a_root_without_rows_counts_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::open(&dir.path().join("snapshot-db")).unwrap();
        assert_eq!(store.row_count(dir.path()).unwrap(), 0);
        assert_eq!(store.logical_bytes(dir.path()).unwrap(), 0);
        assert!(store.read_meta(dir.path()).unwrap().is_none());
    }

    fn payload(id: u8) -> [u8; 32] {
        [id; 32]
    }

    fn encoded(id: u8) -> Vec<u8> {
        Row {
            mtime_ns: 7,
            size: 11,
            binary: id % 2 == 0,
            consecutive: vec![0x6162, 0x6263],
            skip1: vec![0x6163],
        }
        .encode()
        .expect("encode")
    }

    #[test]
    fn a_chunk_read_returns_one_slot_per_payload_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = SnapshotStore::open(&root.join("snapshot-db")).unwrap();
        store
            .put_rows(
                root,
                [payload(1), payload(2), payload(3)].into_iter().zip([
                    encoded(1),
                    encoded(2),
                    encoded(3),
                ]),
            )
            .unwrap();

        let mut out = Vec::new();
        store
            .get_rows(root, &[payload(2), payload(9), payload(1)], &mut out)
            .unwrap();

        assert_eq!(out.len(), 3);
        let (second, second_len) = out[0].clone().expect("second row");
        assert_eq!(second.mtime_ns, 7);
        assert_eq!(second.size, 11);
        assert!(second.binary);
        assert_eq!(second_len, encoded(2).len());
        assert!(out[1].is_none(), "a missing payload is a miss");
        let (first, _) = out[2].clone().expect("first row");
        assert!(!first.binary);
    }

    #[test]
    fn a_corrupt_value_is_a_miss_and_not_a_chunk_failure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = SnapshotStore::open(&root.join("snapshot-db")).unwrap();
        let good = encoded(1);
        store
            .put_rows(
                root,
                [
                    (payload(1), good[..good.len() - 1].to_vec()),
                    (payload(2), good.clone()),
                ]
                .into_iter(),
            )
            .unwrap();

        let mut out = Vec::new();
        store
            .get_rows(root, &[payload(1), payload(2)], &mut out)
            .unwrap();

        assert!(out[0].is_none());
        assert!(
            out[1].is_some(),
            "a truncated row never hides its neighbours"
        );
    }

    #[test]
    fn a_row_is_replayed_only_when_its_meta_names_this_root_and_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = SnapshotStore::open(&root.join("snapshot-db")).unwrap();
        assert!(!store.root_is_current(root), "no meta, no replay");

        store.put_meta(root, 10).unwrap();
        assert!(store.root_is_current(root));

        store
            .put_foreign_meta(root, SNAPSHOT_SCHEMA, 1, &normalized_root(root))
            .unwrap();
        assert!(!store.root_is_current(root), "a foreign fingerprint");

        store
            .put_foreign_meta(
                root,
                SNAPSHOT_SCHEMA + 1,
                fingerprint(),
                &normalized_root(root),
            )
            .unwrap();
        assert!(!store.root_is_current(root), "a foreign schema");

        store
            .put_foreign_meta(root, SNAPSHOT_SCHEMA, fingerprint(), "elsewhere/repo")
            .unwrap();
        assert!(!store.root_is_current(root), "a foreign root");
    }
}

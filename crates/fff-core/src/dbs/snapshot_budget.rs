use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use heed::BytesDecode;
use heed::Database;
use heed::types::{Bytes, SerdeBincode};

use super::env_pool::SharedEnv;
use super::snapshot_store::{KEY_LEN, KIND_META, ROOT_PREFIX_LEN, SnapshotMeta, SnapshotStore};
use crate::error::{Error, Result};
use crate::lmdb::LmdbStore;

/// A Root unused for this long is deleted by the next budget pass.
pub(crate) const ROOT_MAX_AGE_SECS: u64 = 14 * 24 * 60 * 60;

#[derive(Clone, Copy)]
struct RootRecord {
    last_used: u64,
    logical_bytes: u64,
}

/// The Roots this process knows about, so a commit never walks the environment
/// to find out how much it holds.
pub(crate) struct SnapshotBudget {
    bytes: u64,
    roots: Mutex<HashMap<[u8; ROOT_PREFIX_LEN], RootRecord>>,
    active: Mutex<HashSet<[u8; ROOT_PREFIX_LEN]>>,
}

impl SnapshotBudget {
    pub(crate) fn new(bytes: u64) -> Self {
        Self {
            bytes,
            roots: Mutex::new(HashMap::new()),
            active: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn is_bounded(&self) -> bool {
        self.bytes != u64::MAX
    }

    // Row bytes across every Root this process knows about.
    pub(crate) fn bytes(&self) -> u64 {
        self.roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .fold(0u64, |sum, record| sum + record.logical_bytes)
    }

    pub(crate) fn root_count(&self) -> u64 {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).len() as u64
    }

    // One pass over the keys at startup fills the index; nothing else reads it
    // back from disk, so a commit costs no traversal.
    pub(crate) fn load(&self, env: &SharedEnv, db: &Database<Bytes, Bytes>) -> Result<()> {
        let rtxn = env.read_txn().map_err(|source| Error::DbStartReadTxn {
            db: SnapshotStore::LABEL,
            source,
        })?;
        let iter = db.iter(&rtxn).map_err(|source| Error::DbRead {
            db: SnapshotStore::LABEL,
            source,
        })?;
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        for entry in iter {
            let (key, value) = entry.map_err(|source| Error::DbRead {
                db: SnapshotStore::LABEL,
                source,
            })?;
            if key.len() != KEY_LEN || key[ROOT_PREFIX_LEN] != KIND_META {
                continue;
            }
            let Ok(meta) = SerdeBincode::<SnapshotMeta>::bytes_decode(value) else {
                continue;
            };
            let prefix: [u8; ROOT_PREFIX_LEN] = key[..ROOT_PREFIX_LEN].try_into().expect("16");
            roots.insert(
                prefix,
                RootRecord {
                    last_used: meta.last_used,
                    logical_bytes: meta.logical_bytes,
                },
            );
        }
        Ok(())
    }

    pub(crate) fn record(&self, prefix: [u8; ROOT_PREFIX_LEN], meta: &SnapshotMeta) {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).insert(
            prefix,
            RootRecord {
                last_used: meta.last_used,
                logical_bytes: meta.logical_bytes,
            },
        );
    }

    pub(crate) fn forget(&self, prefix: [u8; ROOT_PREFIX_LEN]) {
        self.roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&prefix);
    }

    pub(crate) fn mark_active(&self, prefix: [u8; ROOT_PREFIX_LEN]) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(prefix);
    }

    pub(crate) fn mark_idle(&self, prefix: [u8; ROOT_PREFIX_LEN]) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&prefix);
    }

    // Roots to delete, oldest first: everything past its age, then least
    // recently used until the rest fits the budget. A Root being scanned is
    // never a candidate.
    pub(crate) fn eviction_plan(&self, now: u64) -> Vec<[u8; ROOT_PREFIX_LEN]> {
        let roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());

        let mut plan = Vec::new();
        let mut survivors = Vec::new();
        let mut total = 0u64;
        for (&prefix, record) in roots.iter() {
            if active.contains(&prefix) {
                total += record.logical_bytes;
                continue;
            }
            if now.saturating_sub(record.last_used) > ROOT_MAX_AGE_SECS {
                plan.push(prefix);
                continue;
            }
            total += record.logical_bytes;
            survivors.push((record.last_used, record.logical_bytes, prefix));
        }

        survivors.sort_unstable_by_key(|(last_used, _, _)| *last_used);
        for (_, bytes, prefix) in survivors {
            if total <= self.bytes {
                break;
            }
            plan.push(prefix);
            total -= bytes;
        }
        plan
    }
}

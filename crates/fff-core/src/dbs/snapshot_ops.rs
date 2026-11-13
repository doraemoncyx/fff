use std::collections::HashSet;
use std::ops::Bound;

use heed::types::{Bytes, SerdeBincode};
use heed::{Database, RwTxn};

use super::env_pool::SharedEnv;
use super::snapshot_budget::SnapshotBudget;
use super::snapshot_store::{
    KIND_FILE, ROOT_PREFIX_LEN, SNAPSHOT_SCHEMA, SnapshotMeta, SnapshotStore, file_prefix,
    fingerprint, key_bytes, meta_key, now_secs,
};
use crate::error::{Error, Result};
use crate::lmdb::LmdbStore;

pub(crate) struct Sweep {
    pub(crate) deleted: u64,
    pub(crate) meta: SnapshotMeta,
}

// One write transaction per batch: the writer thread is the only caller.
pub(crate) fn commit_rows(
    env: &SharedEnv,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
    rows: &[([u8; 32], Vec<u8>)],
) -> Result<()> {
    let mut wtxn = env.write_txn().map_err(|source| Error::DbStartWriteTxn {
        db: SnapshotStore::LABEL,
        source,
    })?;
    for (payload, value) in rows {
        db.put(
            &mut wtxn,
            &key_bytes(prefix, KIND_FILE, payload),
            value.as_slice(),
        )
        .map_err(|source| Error::DbWrite {
            db: SnapshotStore::LABEL,
            source,
        })?;
    }
    wtxn.commit().map_err(|source| Error::DbCommit {
        db: SnapshotStore::LABEL,
        source,
    })
}

pub(crate) fn put_meta(
    env: &SharedEnv,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
    meta: &SnapshotMeta,
) -> Result<()> {
    let mut wtxn = env.write_txn().map_err(|source| Error::DbStartWriteTxn {
        db: SnapshotStore::LABEL,
        source,
    })?;
    write_meta(&mut wtxn, db, prefix, meta)?;
    wtxn.commit().map_err(|source| Error::DbCommit {
        db: SnapshotStore::LABEL,
        source,
    })
}

// Deletes the rows whose relative-path hash is absent from `keep` and rewrites
// the Root's meta with what actually remains, in one transaction.
pub(crate) fn sweep_rows(
    env: &SharedEnv,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
    keep: &HashSet<[u8; 32]>,
    root: &str,
) -> Result<Sweep> {
    let (doomed, remaining) = collect_orphans(env, db, prefix, keep)?;

    let meta = SnapshotMeta {
        schema: SNAPSHOT_SCHEMA,
        fingerprint: fingerprint(),
        logical_bytes: remaining,
        last_used: now_secs(),
        root: root.to_string(),
    };
    let mut wtxn = env.write_txn().map_err(|source| Error::DbStartWriteTxn {
        db: SnapshotStore::LABEL,
        source,
    })?;
    for payload in &doomed {
        db.delete(&mut wtxn, &key_bytes(prefix, KIND_FILE, payload))
            .map_err(|source| Error::DbWrite {
                db: SnapshotStore::LABEL,
                source,
            })?;
    }
    write_meta(&mut wtxn, db, prefix, &meta)?;
    wtxn.commit().map_err(|source| Error::DbCommit {
        db: SnapshotStore::LABEL,
        source,
    })?;

    Ok(Sweep {
        deleted: doomed.len() as u64,
        meta,
    })
}

pub(crate) fn delete_root(
    env: &SharedEnv,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
) -> Result<bool> {
    let mut wtxn = env.write_txn().map_err(|source| Error::DbStartWriteTxn {
        db: SnapshotStore::LABEL,
        source,
    })?;
    let deleted = delete_prefix(&mut wtxn, db, prefix)?;
    wtxn.commit().map_err(|source| Error::DbCommit {
        db: SnapshotStore::LABEL,
        source,
    })?;
    Ok(deleted > 0)
}

// Deletes every Root past its age or over budget. One transaction for the lot.
pub(crate) fn enforce_limits(
    env: &SharedEnv,
    db: &Database<Bytes, Bytes>,
    budget: &SnapshotBudget,
) -> Result<u64> {
    let victims = budget.eviction_plan(now_secs());
    if victims.is_empty() {
        return Ok(0);
    }

    let mut wtxn = env.write_txn().map_err(|source| Error::DbStartWriteTxn {
        db: SnapshotStore::LABEL,
        source,
    })?;
    for prefix in &victims {
        delete_prefix(&mut wtxn, db, prefix)?;
    }
    wtxn.commit().map_err(|source| Error::DbCommit {
        db: SnapshotStore::LABEL,
        source,
    })?;

    for prefix in &victims {
        budget.forget(*prefix);
    }
    Ok(victims.len() as u64)
}

fn write_meta(
    wtxn: &mut RwTxn<'_>,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
    meta: &SnapshotMeta,
) -> Result<()> {
    db.remap_data_type::<SerdeBincode<SnapshotMeta>>()
        .put(wtxn, &meta_key(prefix), meta)
        .map_err(|source| Error::DbWrite {
            db: SnapshotStore::LABEL,
            source,
        })
}

fn collect_orphans(
    env: &SharedEnv,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
    keep: &HashSet<[u8; 32]>,
) -> Result<(Vec<[u8; 32]>, u64)> {
    let rtxn = env.read_txn().map_err(|source| Error::DbStartReadTxn {
        db: SnapshotStore::LABEL,
        source,
    })?;
    let iter = db
        .prefix_iter(&rtxn, &file_prefix(prefix))
        .map_err(|source| Error::DbRead {
            db: SnapshotStore::LABEL,
            source,
        })?;

    let mut doomed = Vec::new();
    let mut remaining = 0u64;
    for entry in iter {
        let (key, value) = entry.map_err(|source| Error::DbRead {
            db: SnapshotStore::LABEL,
            source,
        })?;
        let Some(payload) = payload_of(key) else {
            continue;
        };
        if keep.contains(&payload) {
            remaining += value.len() as u64;
        } else {
            doomed.push(payload);
        }
    }
    Ok((doomed, remaining))
}

fn payload_of(key: &[u8]) -> Option<[u8; 32]> {
    let start = ROOT_PREFIX_LEN + 1;
    key.get(start..)?.try_into().ok()
}

fn delete_prefix(
    wtxn: &mut RwTxn<'_>,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
) -> Result<usize> {
    let start: &[u8] = prefix;
    match next_prefix(prefix) {
        Some(end) => {
            let range = (Bound::Included(start), Bound::Excluded(end.as_slice()));
            db.delete_range(wtxn, &range)
                .map_err(|source| Error::DbWrite {
                    db: SnapshotStore::LABEL,
                    source,
                })
        }
        None => delete_prefix_by_key(wtxn, db, prefix),
    }
}

// Carry into the next byte so `start..end` is exactly this Root's key range.
fn next_prefix(prefix: &[u8; ROOT_PREFIX_LEN]) -> Option<[u8; ROOT_PREFIX_LEN]> {
    let mut end = *prefix;
    for byte in end.iter_mut().rev() {
        *byte = byte.wrapping_add(1);
        if *byte != 0 {
            return Some(end);
        }
    }
    None
}

// The 2^-128 case where no larger prefix exists: enumerate the keys instead.
fn delete_prefix_by_key(
    wtxn: &mut RwTxn<'_>,
    db: &Database<Bytes, Bytes>,
    prefix: &[u8; ROOT_PREFIX_LEN],
) -> Result<usize> {
    let keys: Vec<Vec<u8>> = {
        let iter = db
            .prefix_iter(&*wtxn, prefix)
            .map_err(|source| Error::DbRead {
                db: SnapshotStore::LABEL,
                source,
            })?;
        let mut keys = Vec::new();
        for entry in iter {
            let (key, _) = entry.map_err(|source| Error::DbRead {
                db: SnapshotStore::LABEL,
                source,
            })?;
            keys.push(key.to_vec());
        }
        keys
    };
    for key in &keys {
        db.delete(wtxn, key).map_err(|source| Error::DbWrite {
            db: SnapshotStore::LABEL,
            source,
        })?;
    }
    Ok(keys.len())
}

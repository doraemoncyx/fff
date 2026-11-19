use std::path::Path;

use super::env_pool::SharedEnv;
use crate::error::{Error, Result};

// Doubling keeps the number of remaps logarithmic in the data size.
const GROWTH_FACTOR: u64 = 2;
// Never hand the map the last gigabyte of a volume, nor more than 95% of it.
const RESERVE_MIN_BYTES: u64 = 1024 * 1024 * 1024;
const RESERVE_DIVISOR: u64 = 20;
// A batch larger than one doubling needs several rounds; eight is plenty.
const MAX_GROWTH_ROUNDS: usize = 8;

impl SharedEnv {
    // Bytes the map may reach on the volume that holds the environment.
    pub(crate) fn map_ceiling(&self) -> Option<u64> {
        volume_ceiling(self.db_dir())
    }

    // Grows the map one step; `false` means the volume has no room left.
    pub(crate) fn grow_map(&self, ceiling: u64) -> Result<bool> {
        // Held for the whole remap: an open txn would read an unmapped page.
        let _resize = self.enter_resize();
        let current = self.map_size() as u64;
        let Some(next) = next_map_size(self.db_dir(), current, ceiling) else {
            return Ok(false);
        };

        // SAFETY: no txn is live in this process while `_resize` is held.
        unsafe { self.resize(next) }.map_err(|source| Error::EnvResize {
            path: self.db_dir().to_path_buf(),
            source,
        })?;
        self.note_resized(next);
        tracing::info!(
            db = %self.db_dir().display(),
            from = current,
            to = next,
            ceiling,
            "LMDB map grown"
        );
        Ok(true)
    }
}

// Runs `write`; when LMDB answers `MDB_MAP_FULL`, grows the map and runs it
// again. Any other error, or a volume with no room, travels up untouched.
pub(crate) fn with_map_growth<T>(
    shared: &SharedEnv,
    write: impl FnMut() -> Result<T>,
) -> Result<T> {
    retry_with_growth(shared, || shared.map_ceiling(), write)
}

#[cfg(test)]
pub(crate) fn with_map_growth_capped<T>(
    shared: &SharedEnv,
    ceiling: Option<u64>,
    write: impl FnMut() -> Result<T>,
) -> Result<T> {
    retry_with_growth(shared, || ceiling, write)
}

fn retry_with_growth<T>(
    shared: &SharedEnv,
    ceiling: impl Fn() -> Option<u64>,
    mut write: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut rounds = 0;
    loop {
        match write() {
            Err(error) if error.is_map_full() && rounds < MAX_GROWTH_ROUNDS => {
                rounds += 1;
                match ceiling() {
                    Some(limit) if shared.grow_map(limit)? => continue,
                    _ => return Err(error),
                }
            }
            other => return other,
        }
    }
}

fn next_map_size(db_dir: &Path, current: u64, ceiling: u64) -> Option<usize> {
    let page = page_size::get() as u64;
    let limit = align_down(ceiling, page);
    if limit <= current {
        return None;
    }
    // The file may already hold more than the map we last recorded, because
    // LMDB raises the map to fit a database another process grew.
    let base = on_disk_bytes(db_dir).max(current);
    let next = align_down(base.saturating_mul(GROWTH_FACTOR), page).min(limit);
    (next > current).then_some(next as usize)
}

// Free space on the volume holding `db_dir`, minus a reserve for everything
// else that lives there. `None` when the volume cannot be measured.
pub(crate) fn volume_ceiling(db_dir: &Path) -> Option<u64> {
    volume_space(db_dir).map(ceiling_of)
}

fn ceiling_of((free, total): (u64, u64)) -> u64 {
    free.saturating_sub(RESERVE_MIN_BYTES.max(total / RESERVE_DIVISOR))
}

fn on_disk_bytes(db_dir: &Path) -> u64 {
    std::fs::metadata(db_dir.join("data.mdb"))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

fn align_down(bytes: u64, page: u64) -> u64 {
    bytes - bytes % page
}

#[cfg(windows)]
fn volume_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free = 0u64;
    let mut total = 0u64;
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((free, total))
}

#[cfg(unix)]
fn volume_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let unit = stat.f_frsize as u64;
    Some((stat.f_bavail as u64 * unit, stat.f_blocks as u64 * unit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbs::env_pool::EnvSpec;
    use heed::Database;
    use heed::types::Bytes;

    const LABEL: &str = "test-growth";

    fn open_tiny(path: &Path, pages: u64) -> SharedEnv {
        std::fs::create_dir_all(path).unwrap();
        let spec = EnvSpec {
            label: LABEL,
            map_size: (pages * page_size::get() as u64) as usize,
            max_dbs: 0,
            size_cap_bytes: u64::MAX,
        };
        SharedEnv::get_or_open(path, &spec).unwrap()
    }

    fn create_db(env: &SharedEnv) -> Database<Bytes, Bytes> {
        let mut wtxn = env.write_txn().unwrap();
        let db = env
            .create_database::<Bytes, Bytes>(&mut wtxn, None)
            .unwrap();
        wtxn.commit().unwrap();
        db
    }

    fn put_batch(env: &SharedEnv, db: &Database<Bytes, Bytes>, count: u64) -> Result<()> {
        let _txn = env.enter_txn();
        let mut wtxn = env
            .write_txn()
            .map_err(|source| Error::DbStartWriteTxn { db: LABEL, source })?;
        for i in 0..count {
            db.put(&mut wtxn, &i.to_le_bytes(), [7u8; 400].as_slice())
                .map_err(|source| Error::DbWrite { db: LABEL, source })?;
        }
        wtxn.commit()
            .map_err(|source| Error::DbCommit { db: LABEL, source })
    }

    #[test]
    fn a_write_that_fills_the_map_grows_it_and_then_lands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let env = open_tiny(&path, 16);
        let db = create_db(&env);
        let before = env.map_size();

        let full = put_batch(&env, &db, 256).unwrap_err();
        assert!(full.is_map_full(), "{full}");

        with_map_growth(&env, || put_batch(&env, &db, 256)).unwrap();
        assert!(env.map_size() > before, "{} stayed put", env.map_size());
        assert!(env.map_grows() >= 1);

        let rtxn = env.read_txn().unwrap();
        let first = db.get(&rtxn, &0u64.to_le_bytes()).unwrap().unwrap();
        assert_eq!(first.len(), 400);
        assert!(db.get(&rtxn, &255u64.to_le_bytes()).unwrap().is_some());
    }

    // A full volume is a lost write, never an endless doubling.
    #[test]
    fn the_error_survives_when_the_ceiling_leaves_no_room() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let env = open_tiny(&path, 16);
        let db = create_db(&env);
        let ceiling = Some(env.map_size() as u64);

        let error =
            with_map_growth_capped(&env, ceiling, || put_batch(&env, &db, 256)).unwrap_err();
        assert!(error.is_map_full(), "{error}");
        assert_eq!(env.map_grows(), 0);
    }

    #[test]
    fn grow_map_refuses_a_ceiling_it_already_reached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let env = open_tiny(&path, 16);
        let current = env.map_size() as u64;

        assert!(!env.grow_map(current).unwrap());
        assert_eq!(env.map_size() as u64, current);
        assert!(env.grow_map(current * 4).unwrap());
        assert!(env.map_size() as u64 >= current * 2);
    }

    #[test]
    fn the_next_step_doubles_and_stops_at_the_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        std::fs::create_dir_all(&path).unwrap();
        let page = page_size::get() as u64;

        assert_eq!(
            next_map_size(&path, 2 * page, 8 * page),
            Some((4 * page) as usize)
        );
        assert_eq!(
            next_map_size(&path, 2 * page, 3 * page),
            Some((3 * page) as usize)
        );
        assert_eq!(next_map_size(&path, 4 * page, 4 * page), None);
        assert_eq!(next_map_size(&path, 4 * page, 3 * page), None);

        // A file bigger than the recorded map dominates the next step.
        std::fs::write(path.join("data.mdb"), vec![0u8; (6 * page) as usize]).unwrap();
        assert_eq!(
            next_map_size(&path, 4 * page, 16 * page),
            Some((12 * page) as usize)
        );
    }

    #[test]
    fn the_ceiling_keeps_a_reserve_of_the_volume() {
        let dir = tempfile::tempdir().unwrap();
        let free = 100 * 1024 * 1024 * 1024u64;
        let big = 1024 * 1024 * 1024 * 1024u64;
        assert_eq!(ceiling_of((free, big)), free - big / RESERVE_DIVISOR);
        // A small volume keeps the flat gigabyte instead of the percentage.
        assert_eq!(
            ceiling_of((free, 8 * 1024 * 1024 * 1024)),
            free - RESERVE_MIN_BYTES
        );
        assert_eq!(ceiling_of((0, 0)), 0);
        assert!(
            volume_ceiling(dir.path()).is_some(),
            "the temp volume is measurable"
        );
    }
}

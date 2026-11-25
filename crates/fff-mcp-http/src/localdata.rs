use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fff::root_cap::RootCapRefusal;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::stats::RefusedRoot;

const VERSION: u32 = 1;
// One record over the limit evicts the least recently seen one.
const MAX_RECORDS: usize = 256;
// A record this old is re-walked, whatever its mtime says.
const RECHECK_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
// A last_seen-only change reaches the disk at most this often.
const TOUCH_WRITE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RefusalRecord {
    pub root: String,
    pub cap: usize,
    pub counted: usize,
    pub subdirs: Vec<String>,
    pub first_seen: u64,
    pub last_seen: u64,
    // The directory's own mtime when the walk ran; any other value rechecks.
    pub mtime_ns: u64,
    // When that walk ran, for the age-based recheck.
    pub checked_at: u64,
}

#[derive(Serialize, Deserialize)]
struct LocalDataFile {
    version: u32,
    roots: HashMap<String, RefusalRecord>,
}

// Refusal records for one process: a memory map written through to
// `localdata.json`, so a restarted process answers without walking.
pub(crate) struct Refusals {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

struct State {
    records: HashMap<String, RefusalRecord>,
    dirty: bool,
    warned: bool,
    written_at: Instant,
}

impl Refusals {
    pub(crate) fn open(path: Option<PathBuf>) -> Self {
        let records = path.as_deref().map(load).unwrap_or_default();
        if let Some(path) = &path {
            tracing::debug!(
                path = %path.display(),
                records = records.len(),
                "refusal records opened"
            );
        }
        Self {
            path,
            state: Mutex::new(State {
                records,
                dirty: false,
                warned: false,
                written_at: Instant::now(),
            }),
        }
    }

    // The stored refusal when it still holds; `None` means the cap walks again.
    pub(crate) fn hit(&self, root: &Path, cap: usize) -> Option<RefusalRecord> {
        let key = crate::root::root_key(root);
        let now = now_millis();
        let mtime = dir_mtime_ns(root);
        let mut state = self.state.lock();
        let replayed = {
            let record = state.records.get_mut(&key)?;
            if record.cap != cap || record.mtime_ns != mtime || aged_out(record, now) {
                return None;
            }
            record.last_seen = now;
            record.clone()
        };
        state.dirty = true;
        self.flush(&mut state, false);
        Some(replayed)
    }

    // Record a walk's verdict; `root_text` is the spelling the message prints.
    pub(crate) fn remember(
        &self,
        root: &Path,
        root_text: &str,
        refusal: &RootCapRefusal,
        mtime_ns: u64,
    ) {
        let key = crate::root::root_key(root);
        let now = now_millis();
        let mut state = self.state.lock();
        let first_seen = state
            .records
            .get(&key)
            .map_or(now, |record| record.first_seen);
        state.records.insert(
            key,
            RefusalRecord {
                root: root_text.to_string(),
                cap: refusal.cap,
                counted: refusal.counted,
                subdirs: refusal.subdirs.clone(),
                first_seen,
                last_seen: now,
                mtime_ns,
                checked_at: now,
            },
        );
        evict_oldest(&mut state.records);
        state.dirty = true;
        self.flush(&mut state, true);
    }

    // The directory fits under the cap, so any record for it is void.
    pub(crate) fn forget(&self, root: &Path) {
        let key = crate::root::root_key(root);
        let mut state = self.state.lock();
        if state.records.remove(&key).is_some() {
            state.dirty = true;
            self.flush(&mut state, true);
        }
    }

    // Every held record, most recently seen first.
    pub(crate) fn listed(&self) -> Vec<RefusedRoot> {
        let state = self.state.lock();
        let mut roots: Vec<RefusedRoot> = state
            .records
            .values()
            .map(|record| RefusedRoot {
                root: record.root.clone(),
                cap: record.cap,
                counted: record.counted,
                first_seen: record.first_seen,
                last_seen: record.last_seen,
            })
            .collect();
        roots.sort_by_key(|root| std::cmp::Reverse(root.last_seen));
        roots
    }

    fn flush(&self, state: &mut State, force: bool) {
        if !state.dirty {
            return;
        }
        let Some(path) = &self.path else {
            state.dirty = false;
            return;
        };
        if !force && state.written_at.elapsed() < TOUCH_WRITE {
            return;
        }
        match write_file(path, &state.records) {
            Ok(()) => {
                state.dirty = false;
                state.warned = false;
            }
            Err(error) => {
                if !state.warned {
                    tracing::warn!(path = %path.display(), %error, "cannot write refusal records");
                    state.warned = true;
                }
            }
        }
        state.written_at = Instant::now();
    }
}

// The directory's own mtime, the cheapest witness that its entries changed.
pub(crate) fn dir_mtime_ns(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_nanos() as u64)
}

fn aged_out(record: &RefusalRecord, now: u64) -> bool {
    now.saturating_sub(record.checked_at) >= RECHECK_AFTER.as_millis() as u64
}

fn evict_oldest(records: &mut HashMap<String, RefusalRecord>) {
    while records.len() > MAX_RECORDS {
        let oldest = records
            .iter()
            .min_by_key(|(_, record)| record.last_seen)
            .map(|(key, _)| key.clone());
        let Some(oldest) = oldest else { break };
        records.remove(&oldest);
    }
}

// Missing, corrupt and unknown-version files all read as no record at all.
fn load(path: &Path) -> HashMap<String, RefusalRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read refusal records");
            return HashMap::new();
        }
    };
    let file: LocalDataFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "ignoring unreadable refusal records"
            );
            return HashMap::new();
        }
    };
    if file.version != VERSION {
        tracing::warn!(
            path = %path.display(),
            version = file.version,
            "ignoring refusal records of an unknown version"
        );
        return HashMap::new();
    }
    file.roots
}

fn write_file(path: &Path, records: &HashMap<String, RefusalRecord>) -> Result<(), String> {
    let file = LocalDataFile {
        version: VERSION,
        roots: records.clone(),
    };
    let text = serde_json::to_string(&file).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).map_err(|error| format!("{}: {error}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|error| format!("{}: {error}", path.display()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |age| age.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(last_seen: u64) -> RefusalRecord {
        RefusalRecord {
            root: "C:/work/game".into(),
            cap: 8,
            counted: 9,
            subdirs: vec!["alpha".into()],
            first_seen: 1,
            last_seen,
            mtime_ns: 7,
            checked_at: 1,
        }
    }

    #[test]
    fn one_record_past_the_limit_drops_the_least_recently_seen() {
        let mut records = HashMap::new();
        for index in 0..=MAX_RECORDS as u64 {
            records.insert(format!("k{index}"), record(index));
        }
        evict_oldest(&mut records);
        assert_eq!(records.len(), MAX_RECORDS);
        assert!(!records.contains_key("k0"), "the oldest record survived");
        assert!(records.contains_key(&format!("k{MAX_RECORDS}")));
    }

    #[test]
    fn a_record_inside_the_limit_evicts_nothing() {
        let mut records = HashMap::new();
        for index in 0..MAX_RECORDS as u64 {
            records.insert(format!("k{index}"), record(index));
        }
        evict_oldest(&mut records);
        assert_eq!(records.len(), MAX_RECORDS);
        assert!(records.contains_key("k0"));
    }

    // The age is measured from the walk, not from the request that replayed it.
    #[test]
    fn a_record_ages_out_of_its_recheck_window() {
        let fresh = record(1);
        assert!(!aged_out(&fresh, 1));
        let day = RECHECK_AFTER.as_millis() as u64;
        assert!(!aged_out(&fresh, day), "a record younger than a day stands");
        assert!(aged_out(&fresh, day + 1));
        assert!(aged_out(&fresh, u64::MAX));
    }

    // A store with no path keeps records in memory and writes nothing.
    #[test]
    fn a_store_without_a_path_never_touches_the_disk() {
        let refusals = Refusals::open(None);
        let refusal = RootCapRefusal {
            cap: 8,
            counted: 9,
            subdirs: vec!["alpha".into()],
        };
        refusals.remember(Path::new("C:/work/game"), "C:/work/game", &refusal, 7);
        assert_eq!(refusals.listed().len(), 1);
        refusals.forget(Path::new("C:/work/game"));
        assert!(refusals.listed().is_empty());
    }

    #[test]
    fn a_record_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localdata.json");
        let refusal = RootCapRefusal {
            cap: 8,
            counted: 9,
            subdirs: vec!["alpha".into(), "beta".into()],
        };
        let written = Refusals::open(Some(path.clone()));
        written.remember(Path::new("C:/work/game"), "C:/work/game", &refusal, 7);
        drop(written);

        let read = Refusals::open(Some(path.clone()));
        let listed = read.listed();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].root, "C:/work/game");
        assert_eq!(listed[0].cap, 8);
        assert_eq!(listed[0].counted, 9);

        let text = std::fs::read_to_string(&path).unwrap();
        let file: serde_json::Value = serde_json::from_str(&text).unwrap();
        let roots = file["roots"].as_object().expect("one record per Root");
        assert_eq!(roots.len(), 1, "{text}");
        let record = roots.values().next().unwrap();
        assert_eq!(record["subdirs"], serde_json::json!(["alpha", "beta"]));
        assert_eq!(record["mtime_ns"], 7, "{text}");
    }

    #[test]
    fn a_corrupt_file_reads_as_no_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localdata.json");
        for content in [
            "{ not json",
            "{\"version\": 99, \"roots\": {}}",
            "{\"roots\": {}}",
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(
                Refusals::open(Some(path.clone())).listed().is_empty(),
                "{content}"
            );
        }
    }

    // A record whose directory mtime moved is not believed, and the older one
    // is: the check is the mtime, never the clock alone.
    #[test]
    fn a_changed_mtime_defeats_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let root = dir.path();
        let refusals = Refusals::open(Some(store.path().join("localdata.json")));
        let refusal = RootCapRefusal {
            cap: 8,
            counted: 9,
            subdirs: Vec::new(),
        };
        let mtime = dir_mtime_ns(root);
        refusals.remember(root, &root.to_string_lossy(), &refusal, mtime);
        assert!(
            refusals.hit(root, 8).is_some(),
            "the record was not replayed"
        );

        std::fs::write(root.join("later.txt"), "x").unwrap();
        assert!(
            refusals.hit(root, 8).is_none(),
            "an unchanged-looking mtime let the record stand"
        );
        // A different cap asks a different question of the same directory.
        assert!(refusals.hit(root, 16).is_none());
    }
}

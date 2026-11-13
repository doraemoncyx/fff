mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{ScriptedProbe, TestServer, test_config, write_tree};
use fff::SnapshotStore;
use fff_mcp_http::Config;
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(60);
const BUDGET: u64 = 1_000_000;
const UNDER: u64 = 100_000;
const OVER: u64 = 4_000_000;
const FILES: [&str; 7] = [
    "data/one.csv",
    "data/nested/two.csv",
    "data/nested/deep/three.csv",
    "editor/main.lua",
    "editor/core/init.lua",
    "docs/readme.md",
    "root.txt",
];

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &FILES);
    dir
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn snapshot_config(snapshot_dir: &Path) -> Config {
    Config {
        max_memory_bytes: BUDGET,
        // Rounds are forced through /test/evict, never the periodic tick.
        evict_interval: Duration::from_secs(3600),
        test_hook: true,
        snapshot: true,
        snapshot_db: Some(snapshot_dir.to_path_buf()),
        ..test_config()
    }
}

fn server(snapshot_dir: &Path) -> (TestServer, std::sync::Arc<ScriptedProbe>) {
    let probe = ScriptedProbe::new(UNDER);
    let server = TestServer::start_with_probe(snapshot_config(snapshot_dir), probe.clone());
    (server, probe)
}

fn search(server: &TestServer, root: &Path) -> Value {
    server.search_json(&[
        ("querystr", ""),
        ("tardir", &path_of(root)),
        ("maxResults", "100"),
    ])
}

fn evict(server: &TestServer) -> Vec<String> {
    let response = server.post("/test/evict", "", &[]);
    assert_eq!(response.status, 200, "evict failed: {}", response.body);
    let value: Value = serde_json::from_str(&response.body).expect("evict returns JSON");
    value["evicted"]
        .as_array()
        .unwrap_or_else(|| panic!("no evicted list: {value}"))
        .iter()
        .map(|key| key.as_str().expect("key").to_string())
        .collect()
}

fn roots(server: &TestServer) -> Vec<String> {
    server.stats()["indexes"]
        .as_array()
        .expect("indexes")
        .iter()
        .map(|index| index["root"].as_str().expect("root").to_string())
        .collect()
}

fn snapshot_section(server: &TestServer) -> Value {
    let stats = server.stats();
    let section = stats.get("snapshot").cloned().unwrap_or(Value::Null);
    assert!(!section.is_null(), "the snapshot section is off: {stats}");
    section
}

fn hits(server: &TestServer) -> u64 {
    snapshot_section(server)["hits"].as_u64().unwrap_or(0)
}

fn misses(server: &TestServer) -> u64 {
    snapshot_section(server)["misses"].as_u64().unwrap_or(0)
}

// An Index over a tempdir may be spelled with another case on Windows.
fn same_path(left: &str, right: &Path) -> bool {
    fn normalize(text: &str) -> String {
        text.replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    }
    normalize(left) == normalize(&path_of(right))
}

fn index_entry(server: &TestServer, root: &Path) -> Option<Value> {
    server.stats()["indexes"]
        .as_array()?
        .iter()
        .find(|entry| {
            entry["root"]
                .as_str()
                .is_some_and(|text| same_path(text, root))
        })
        .cloned()
}

fn wait_for_indexing(server: &TestServer, root: &Path) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(entry) = index_entry(server, root)
            && entry["scanning"] == false
            && entry["indexing"] == false
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no finished Index for {}: {}",
            root.display(),
            server.stats()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_root_count(server: &TestServer, expected: usize) -> Vec<String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let current = roots(server);
        if current.len() == expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected} Indexes, got {current:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// A Root's rows are on disk once its meta row is: the writer queues it last.
fn wait_for_rows(store: &SnapshotStore, root: &Path) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(Some(meta)) = store.read_meta(root) {
            assert!(meta.logical_bytes > 0, "{meta:?}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no meta row for {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_hits_above(server: &TestServer, before: u64) -> u64 {
    let deadline = Instant::now() + WAIT;
    loop {
        let current = hits(server);
        if current > before {
            return current;
        }
        assert!(Instant::now() < deadline, "the hits never grew: {current}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

// Eviction drops the Index, not the disk: the next index of that Root is warm.
#[test]
fn an_evicted_root_is_indexed_again_from_its_rows() {
    let first = tree();
    let second = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    let (server, probe) = server(&snapshot_dir);
    let store = SnapshotStore::open(&snapshot_dir).unwrap();

    search(&server, first.path());
    wait_for_indexing(&server, first.path());
    wait_for_rows(&store, first.path());
    search(&server, second.path());
    wait_for_indexing(&server, second.path());
    wait_for_rows(&store, second.path());
    assert_eq!(hits(&server), 0, "both scans were cold");

    // Over budget with two Indexes: the least recently used one goes.
    probe.settle(OVER);
    let evicted = evict(&server);
    assert_eq!(evicted.len(), 1, "{evicted:?}");
    assert_eq!(
        roots(&server),
        vec![path_of(second.path())],
        "the first Root was the LRU Index"
    );

    probe.settle(UNDER);
    let misses_before = misses(&server);
    let hits_before = hits(&server);
    search(&server, first.path());
    wait_for_indexing(&server, first.path());

    wait_for_hits_above(&server, hits_before);
    assert_eq!(
        misses(&server),
        misses_before,
        "a miss is a file content read the Snapshot should have avoided"
    );
}

// Subsumption drops the child's Index; once the parent is gone the child's
// rows are still the ones its next Index replays.
#[test]
fn a_subsumed_child_root_is_indexed_again_from_its_rows() {
    let dir = tree();
    let outside = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    let (server, probe) = server(&snapshot_dir);
    let store = SnapshotStore::open(&snapshot_dir).unwrap();
    let child = dir.path().join("data");

    search(&server, &child);
    wait_for_indexing(&server, &child);
    wait_for_rows(&store, &child);

    // A finished parent scan subsumes the child Index; its rows are untouched.
    search(&server, dir.path());
    wait_for_indexing(&server, dir.path());
    let remaining = wait_for_root_count(&server, 1);
    assert!(
        same_path(&remaining[0], dir.path()),
        "the child Index survived subsumption: {remaining:?}"
    );

    // The parent covers the child, so it must go before the child can be indexed
    // again; a second Root gives the eviction round something to leave behind.
    search(&server, outside.path());
    wait_for_indexing(&server, outside.path());
    probe.settle(OVER);
    let evicted = evict(&server);
    assert_eq!(evicted.len(), 1, "{evicted:?}");
    assert_eq!(
        roots(&server),
        vec![path_of(outside.path())],
        "the parent is not the LRU Index"
    );

    probe.settle(UNDER);
    let hits_before = hits(&server);
    let misses_before = misses(&server);
    search(&server, &child);
    wait_for_indexing(&server, &child);

    wait_for_hits_above(&server, hits_before);
    assert_eq!(
        misses(&server),
        misses_before,
        "a miss is a file content read the child's rows should have served"
    );
}

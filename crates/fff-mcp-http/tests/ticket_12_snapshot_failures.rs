mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{TestServer, test_config, write_tree};
use fff::{SNAPSHOT_SCHEMA, SnapshotMeta, SnapshotStore};
use fff_mcp_http::Config;
use serde_json::Value;

const FILES: [&str; 3] = ["src/main.rs", "docs/readme.md", "plain/notes.txt"];
const WAIT: Duration = Duration::from_secs(60);

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &FILES);
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn compute_total() -> i32 {\n    needle_alpha + 1\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("plain/notes.txt"),
        "needle_alpha line\ntail\n",
    )
    .unwrap();
    dir
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn snapshot_config(path: &Path) -> Config {
    Config {
        snapshot: true,
        snapshot_db: Some(path.to_path_buf()),
        ..test_config()
    }
}

fn search(server: &TestServer, root: &Path) -> Value {
    server.search_json(&[
        ("querystr", ""),
        ("tardir", &path_of(root)),
        ("maxResults", "50"),
    ])
}

fn same_path(value: &Value, expected: &Path) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    let normalize = |text: &str| {
        text.replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    normalize(text) == normalize(&path_of(expected))
}

// The index entry for one Root, once the registry has published it.
fn index_entry(server: &TestServer, root: &Path) -> Option<Value> {
    let stats = server.stats();
    stats["indexes"]
        .as_array()?
        .iter()
        .find(|entry| same_path(&entry["root"], root))
        .cloned()
}

fn wait_for_indexing(server: &TestServer, root: &Path) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Some(entry) = index_entry(server, root)
            && entry["scanning"] == false
            && entry["indexing"] == false
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("content index never finished for {}", root.display());
}

fn snapshot_section(server: &TestServer) -> Value {
    let stats = server.stats();
    let section = stats.get("snapshot").cloned().unwrap_or(Value::Null);
    assert!(!section.is_null(), "the snapshot section is off: {stats}");
    section
}

fn wait_for_roots(server: &TestServer) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let section = snapshot_section(server);
        if section["roots"].as_u64().unwrap_or(0) > 0 && section["bytes"].as_u64().unwrap_or(0) > 0
        {
            return section;
        }
        assert!(
            Instant::now() < deadline,
            "no Root reached /stats: {section}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_meta(path: &Path, root: &Path) -> SnapshotMeta {
    let deadline = Instant::now() + WAIT;
    loop {
        let store = SnapshotStore::open(path).expect("snapshot store opens");
        if let Ok(Some(meta)) = store.read_meta(root) {
            return meta;
        }
        assert!(
            Instant::now() < deadline,
            "no meta row appeared in {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// A marked directory whose environment will not open is ours to recreate: the
// "spec mismatch" and "corrupt environment" cases meet here.
#[test]
fn a_marked_environment_that_will_not_open_is_recreated() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    std::fs::create_dir_all(&snapshot_dir).unwrap();
    std::fs::write(
        snapshot_dir.join(fff::OWNER_MARKER),
        b"fff snapshot store\n",
    )
    .unwrap();
    std::fs::write(snapshot_dir.join("data.mdb"), b"not an lmdb environment").unwrap();
    std::fs::write(snapshot_dir.join("lock.mdb"), b"nor is this").unwrap();

    let server = TestServer::start(snapshot_config(&snapshot_dir));
    search(&server, root.path());
    wait_for_indexing(&server, root.path());

    let section = wait_for_roots(&server);
    assert_eq!(section["hits"], 0, "a cold scan hits nothing: {section}");
    let meta = wait_for_meta(&snapshot_dir, root.path());
    assert_eq!(meta.schema, SNAPSHOT_SCHEMA);
    assert!(meta.logical_bytes > 0, "{meta:?}");
}

// A directory carrying an LMDB environment but no marker is someone else's.
// The frecency store is exactly this shape, so adopting it would delete a
// database that has nothing to do with the Snapshot.
#[test]
fn an_unmarked_environment_is_never_adopted_or_deleted() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    std::fs::create_dir_all(&snapshot_dir).unwrap();
    std::fs::write(snapshot_dir.join("data.mdb"), b"frecency rows").unwrap();
    std::fs::write(snapshot_dir.join("lock.mdb"), b"a lock").unwrap();

    let server = TestServer::start(snapshot_config(&snapshot_dir));
    let response = search(&server, root.path());
    assert!(
        response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()),
        "{response}"
    );
    wait_for_indexing(&server, root.path());

    assert!(
        server.stats()["snapshot"].is_null(),
        "the snapshot must stay off: {}",
        server.stats()
    );
    assert_eq!(
        std::fs::read(snapshot_dir.join("data.mdb")).unwrap(),
        b"frecency rows",
        "another database's data was overwritten"
    );
    assert!(!snapshot_dir.join(fff::OWNER_MARKER).exists());
}

// Anything else in the directory means it is not ours: warn, stay off, and
// leave every byte where it was.
#[test]
fn a_directory_holding_another_file_is_never_touched() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    std::fs::create_dir_all(&snapshot_dir).unwrap();
    std::fs::write(snapshot_dir.join("data.mdb"), b"not an lmdb environment").unwrap();
    std::fs::write(snapshot_dir.join("lock.mdb"), b"nor is this").unwrap();
    std::fs::write(snapshot_dir.join("keep.txt"), b"someone's data").unwrap();

    let server = TestServer::start(snapshot_config(&snapshot_dir));
    let response = search(&server, root.path());
    assert!(
        response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()),
        "{response}"
    );
    wait_for_indexing(&server, root.path());

    assert!(
        server.stats()["snapshot"].is_null(),
        "the snapshot must stay off: {}",
        server.stats()
    );
    assert_eq!(
        std::fs::read(snapshot_dir.join("keep.txt")).unwrap(),
        b"someone's data"
    );
    assert_eq!(
        std::fs::read(snapshot_dir.join("data.mdb")).unwrap(),
        b"not an lmdb environment"
    );
    assert!(!snapshot_dir.join(fff::OWNER_MARKER).exists());
}

// The portability-friendly stand-in for a read-only directory: the env path's
// parent is a regular file, so the directory can never be created.
#[test]
fn an_uncreatable_environment_directory_leaves_the_scan_working() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let blocker = db.path().join("blocker");
    std::fs::write(&blocker, b"a file, not a directory").unwrap();
    let snapshot_dir = blocker.join("snapshot-db");

    let server = TestServer::start(snapshot_config(&snapshot_dir));
    let response = search(&server, root.path());
    assert!(
        response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()),
        "{response}"
    );
    wait_for_indexing(&server, root.path());

    assert!(server.stats()["snapshot"].is_null(), "{}", server.stats());
    assert!(blocker.is_file(), "the blocker was replaced");
}

#[test]
fn no_snapshot_config_reports_no_section() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let server = TestServer::start(Config {
        snapshot: false,
        snapshot_db: Some(db.path().join("snapshot-db")),
        ..test_config()
    });

    search(&server, root.path());
    wait_for_indexing(&server, root.path());

    let stats = server.stats();
    assert!(stats["snapshot"].is_null(), "{stats}");
}

// Process-wide counters: the second run of the same tree is served from the
// Snapshot, and a polled total never goes backwards.
#[test]
fn stats_report_hits_after_a_second_scan_and_only_grow() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");

    // First run: a cold scan that writes the rows.
    {
        let server = TestServer::start(snapshot_config(&snapshot_dir));
        search(&server, root.path());
        wait_for_indexing(&server, root.path());
        wait_for_meta(&snapshot_dir, root.path());
        let cold = wait_for_roots(&server);
        assert_eq!(cold["hits"], 0, "a cold scan hits nothing: {cold}");
        assert!(cold["misses"].as_u64().unwrap() > 0, "{cold}");
        assert_eq!(cold["roots"], 1, "{cold}");
        // The drain thread's own counters, so a stalled writer shows up in
        // /stats instead of needing a debugger.
        assert!(cold["commits"].as_u64().unwrap() > 0, "{cold}");
        assert!(cold["rows"].as_u64().unwrap() > 0, "{cold}");
        assert!(cold["dropped"].is_u64(), "{cold}");
        assert!(cold["queued_bytes"].is_u64(), "{cold}");
        assert!(cold["busy"].is_boolean(), "{cold}");
        assert!(server.shutdown_within(Duration::from_secs(30)));
    }

    // Second run: same tree, same environment, no file content to read.
    let server = TestServer::start(snapshot_config(&snapshot_dir));
    search(&server, root.path());
    wait_for_indexing(&server, root.path());

    let hits = wait_for_hits(&server);
    assert!(hits > 0, "the second scan replayed no rows");
    let later = snapshot_section(&server);
    assert!(
        later["hits"].as_u64().unwrap() >= hits,
        "hits never shrink: {later}"
    );
    assert_eq!(later["roots"], 1, "{later}");
    assert!(later["bytes"].as_u64().unwrap() > 0, "{later}");
}

fn wait_for_hits(server: &TestServer) -> u64 {
    let deadline = Instant::now() + WAIT;
    loop {
        let hits = snapshot_section(server)["hits"].as_u64().unwrap_or(0);
        if hits > 0 {
            return hits;
        }
        assert!(Instant::now() < deadline, "no hit ever reached /stats");
        std::thread::sleep(Duration::from_millis(50));
    }
}

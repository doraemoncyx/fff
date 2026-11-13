mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{TestServer, test_config, write_tree};
use fff::{SNAPSHOT_SCHEMA, SnapshotStore};
use fff_mcp_http::Config;
use serde_json::Value;

const FILES: [&str; 3] = ["src/main.rs", "docs/readme.md", "plain/notes.txt"];

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

fn with_snapshot(path: &Path) -> Config {
    Config {
        snapshot: true,
        snapshot_db: Some(path.to_path_buf()),
        ..test_config()
    }
}

fn index_entry(server: &TestServer) -> Value {
    let stats = server.stats();
    stats["indexes"]
        .as_array()
        .and_then(|indexes| indexes.first())
        .cloned()
        .unwrap_or_else(|| panic!("no index entry: {stats}"))
}

fn wait_for_indexing(server: &TestServer) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let entry = index_entry(server);
        if entry["scanning"] == false && entry["indexing"] == false {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("content index never finished");
}

fn wait_for_meta(path: &Path, root: &Path) -> fff::SnapshotMeta {
    let deadline = Instant::now() + Duration::from_secs(60);
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

// Every other test builds config from Config::default, which names no directory.
#[test]
fn the_default_config_writes_no_snapshot() {
    assert!(test_config().snapshot_path().is_none());
}

#[test]
fn a_scan_leaves_a_meta_and_rows_in_the_named_environment() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir: PathBuf = db.path().join("snapshot-db");
    let server = TestServer::start(with_snapshot(&snapshot_dir));

    server.search_json(&[
        ("querystr", ""),
        ("tardir", &path_of(root.path())),
        ("maxResults", "50"),
    ]);
    let meta = wait_for_meta(&snapshot_dir, root.path());

    assert_eq!(meta.schema, SNAPSHOT_SCHEMA);
    assert!(meta.logical_bytes > 0, "{meta:?}");
    let store = SnapshotStore::open(&snapshot_dir).unwrap();
    assert!(store.row_count(root.path()).unwrap() >= FILES.len() as u64);
}

#[test]
fn no_snapshot_leaves_the_directory_empty() {
    let root = tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    let server = TestServer::start(Config {
        snapshot: false,
        snapshot_db: Some(snapshot_dir.clone()),
        ..test_config()
    });

    server.search_json(&[
        ("querystr", ""),
        ("tardir", &path_of(root.path())),
        ("maxResults", "50"),
    ]);
    wait_for_indexing(&server);
    std::thread::sleep(Duration::from_millis(300));

    let empty = match std::fs::read_dir(&snapshot_dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    };
    assert!(empty, "{} was written to", snapshot_dir.display());
}

#[test]
fn snapshot_reset_empties_a_seeded_environment() {
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    let stale_root = db.path().join("stale-root");
    std::fs::create_dir_all(&stale_root).unwrap();

    {
        let store = SnapshotStore::open(&snapshot_dir).unwrap();
        store.put_meta(&stale_root, 4096).unwrap();
        assert!(store.read_meta(&stale_root).unwrap().is_some());
    }
    std::thread::sleep(Duration::from_millis(100));

    let _server = TestServer::start(Config {
        snapshot_reset: true,
        ..with_snapshot(&snapshot_dir)
    });

    let store = SnapshotStore::open(&snapshot_dir).unwrap();
    assert!(
        store.read_meta(&stale_root).unwrap().is_none(),
        "the seeded Root survived --snapshot-reset"
    );
}

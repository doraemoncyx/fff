mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{ScriptedProbe, TestServer, test_config, write_many, write_tree};
use fff::SnapshotStore;
use fff_mcp_http::Config;
use serde_json::Value;

const CAP: usize = 8;
const SUBDIRS: [&str; 3] = ["alpha", "beta", "gamma"];
const FILES: usize = 120;
const UNDER_CAP: [&str; 7] = [
    "src/main.rs",
    "src/lib.rs",
    "src/deep/one.rs",
    "data/one.csv",
    "data/two.csv",
    "docs/readme.md",
    "root.txt",
];
const WAIT: Duration = Duration::from_secs(60);

// Three subdirectories and 120 files: fifteen times the cap under test.
fn big_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_many(dir.path(), FILES, |index| {
        format!("{}/f{index}.rs", SUBDIRS[index % SUBDIRS.len()])
    });
    dir
}

fn small_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &UNDER_CAP);
    dir
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn capped(cap: usize) -> Config {
    Config {
        root_cap_files: cap,
        ..test_config()
    }
}

fn search(server: &TestServer, root: &Path, extra: &[(&str, &str)]) -> common::Response {
    let tardir = path_of(root);
    let mut params = vec![("querystr", ""), ("tardir", tardir.as_str())];
    params.extend(extra.iter().copied());
    server.search(&params)
}

// The refusal message carries the count it stopped at; nothing else may.
fn stopped_after(message: &str) -> usize {
    let rest = message
        .split_once("stopped after ")
        .unwrap_or_else(|| panic!("no stopping count in {message}"))
        .1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("{digits} is not a count"))
}

#[test]
fn a_tardir_over_the_cap_is_refused_before_it_becomes_a_root() {
    let tree = big_tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    let server = TestServer::start(Config {
        snapshot_db: Some(snapshot_dir.clone()),
        ..capped(CAP)
    });

    let response = search(&server, tree.path(), &[("maxResults", "200")]);
    assert_eq!(response.status, 400, "{}", response.body);
    let body: Value = serde_json::from_str(&response.body).expect("error body is JSON");
    assert_eq!(body["error"], "INVALID_PARAMS", "{body}");
    let message = body["message"].as_str().expect("message");
    assert!(message.contains("Root cap"), "{message}");
    assert!(message.contains(&CAP.to_string()), "{message}");
    for subdir in SUBDIRS {
        assert!(message.contains(subdir), "{message}");
    }
    assert!(message.contains("Narrow the tardir"), "{message}");
    // The cap stopped the walk: the count is near it, nowhere near the tree.
    let counted = stopped_after(message);
    assert!(
        counted > CAP && counted < FILES,
        "{counted} of {FILES} files counted"
    );

    // No Index, no watcher, no Snapshot row: nothing was created for the tardir.
    let stats = server.stats();
    assert!(
        stats["indexes"].as_array().is_some_and(Vec::is_empty),
        "{stats}"
    );
    assert_eq!(
        stats["refused_roots"].as_array().map(Vec::len),
        Some(1),
        "{stats}"
    );
    assert_eq!(stats["refused_roots"][0]["cap"], CAP, "{stats}");
    let store = SnapshotStore::open(&snapshot_dir).expect("snapshot store opens");
    assert!(
        store.read_meta(tree.path()).expect("meta read").is_none(),
        "the refused tardir got a Snapshot meta row"
    );
    assert_eq!(
        store.row_count(tree.path()).expect("row count"),
        0,
        "the refused tardir got Snapshot rows"
    );
}

// A narrow scope does not bypass the gate, and the whole request fails.
#[test]
fn a_narrow_sub_path_cannot_rescue_a_refused_tardir() {
    let tree = big_tree();
    let server = TestServer::start(capped(CAP));

    let by_path = search(&server, tree.path(), &[("path", SUBDIRS[0])]);
    let by_paths = search(&server, tree.path(), &[("paths", SUBDIRS[0])]);
    let by_glob = search(&server, tree.path(), &[("globs", "*.rs")]);
    let both = search(
        &server,
        tree.path(),
        &[("paths", SUBDIRS[0]), ("paths", SUBDIRS[1])],
    );

    for response in [&by_path, &by_paths, &by_glob, &both] {
        assert_eq!(response.status, 400, "{}", response.body);
        let body: Value = serde_json::from_str(&response.body).expect("error body is JSON");
        assert_eq!(body["error"], "INVALID_PARAMS", "{body}");
        assert!(
            body["results"].is_null(),
            "a partial success leaked: {body}"
        );
    }

    let stats = server.stats();
    assert!(
        stats["indexes"].as_array().is_some_and(Vec::is_empty),
        "{stats}"
    );
    // Four requests, one recorded refusal: a replay is not a second walk.
    assert_eq!(
        stats["refused_roots"].as_array().map(Vec::len),
        Some(1),
        "{stats}"
    );
}

#[test]
fn the_mcp_tool_reports_the_refusal_as_invalid_parameters() {
    let tree = big_tree();
    let server = TestServer::start(capped(CAP));

    let reply = server.mcp_call(
        1,
        serde_json::json!({ "querystr": "", "tardir": path_of(tree.path()) }),
    );
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    let message = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("Root cap"), "{reply}");
}

// Exactly at the cap is inside it: the extra file is what refuses a tardir.
#[test]
fn a_root_at_the_cap_is_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<String> = (0..CAP).map(|index| format!("src/f{index}.rs")).collect();
    let names: Vec<&str> = files.iter().map(String::as_str).collect();
    write_tree(dir.path(), &names);
    let server = TestServer::start(capped(CAP));

    let body = server.search_json(&[
        ("querystr", ""),
        ("tardir", &path_of(dir.path())),
        ("maxResults", "200"),
    ]);
    assert_eq!(body["total"], CAP, "{body}");
    assert_eq!(
        server.stats()["refused_roots"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(server.stats()["indexes"].as_array().map(Vec::len), Some(1));

    // A fresh copy one file over the cap: an indexed Root is never re-gated.
    let other = tempfile::tempdir().unwrap();
    let deeper = other.path().join("copy");
    std::fs::create_dir_all(deeper.join("src")).unwrap();
    for name in names.iter().chain(["src/extra.rs"].iter()) {
        std::fs::write(deeper.join(name), "x").unwrap();
    }
    let response = search(&server, &deeper, &[]);
    assert_eq!(response.status, 400, "{}", response.body);
}

// Zero is the escape hatch: the same tree indexes as it did before the cap.
#[test]
fn a_cap_of_zero_indexes_a_tree_of_any_size() {
    let tree = big_tree();
    let server = TestServer::start(capped(0));

    let body = server.search_json(&[
        ("querystr", ""),
        ("tardir", &path_of(tree.path())),
        ("maxResults", "200"),
    ]);
    assert_eq!(body["total"], FILES, "{body}");
    assert_eq!(body["scanning"], false, "{body}");
    assert_eq!(
        server.stats()["refused_roots"].as_array().map(Vec::len),
        Some(0)
    );
}

// An under-cap Root is untouched by the gate: its Snapshot still serves a rescan.
#[test]
fn an_under_cap_root_still_hits_its_snapshot() {
    let first = small_tree();
    let second = small_tree();
    let db = tempfile::tempdir().unwrap();
    let snapshot_dir = db.path().join("snapshot-db");
    let probe = ScriptedProbe::new(1_000_000);
    let server = TestServer::start_with_probe(
        Config {
            max_memory_bytes: 1_000_000,
            // Rounds are forced through /test/evict, never the periodic tick.
            evict_interval: Duration::from_secs(3600),
            test_hook: true,
            snapshot_db: Some(snapshot_dir.clone()),
            ..capped(CAP)
        },
        probe.clone(),
    );
    let store = SnapshotStore::open(&snapshot_dir).expect("snapshot store opens");

    for root in [first.path(), second.path()] {
        search(&server, root, &[("maxResults", "50")]);
        wait_for_indexing(&server, root);
        wait_for_meta(&store, root);
    }
    assert_eq!(snapshot_hits(&server), 0, "both scans were cold");

    // Over budget with two Indexes: the least recently used one goes.
    probe.settle(4_000_000);
    let response = server.post("/test/evict", "", &[]);
    assert_eq!(response.status, 200, "{}", response.body);
    probe.settle(1_000_000);

    let before = snapshot_hits(&server);
    search(&server, first.path(), &[("maxResults", "50")]);
    wait_for_indexing(&server, first.path());
    wait_for_hits(&server, before);
}

fn index_entry(server: &TestServer, root: &Path) -> Option<Value> {
    let stats = server.stats();
    stats["indexes"].as_array()?.iter().find_map(|entry| {
        let listed = entry["root"].as_str()?.replace('\\', "/");
        let wanted = path_of(root).replace('\\', "/");
        listed
            .trim_end_matches('/')
            .eq_ignore_ascii_case(wanted.trim_end_matches('/'))
            .then(|| entry.clone())
    })
}

fn wait_for_indexing(server: &TestServer, root: &Path) {
    let deadline = Instant::now() + WAIT;
    loop {
        let entry = index_entry(server, root);
        if entry.is_some_and(|entry| entry["scanning"] == false && entry["indexing"] == false) {
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

// A Root's rows are on disk once its meta row is: the writer queues it last.
fn wait_for_meta(store: &SnapshotStore, root: &Path) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = store.read_meta(root) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no Snapshot meta row for {}", root.display());
}

fn snapshot_hits(server: &TestServer) -> u64 {
    let stats = server.stats();
    stats["snapshot"]["hits"].as_u64().unwrap_or(0)
}

fn wait_for_hits(server: &TestServer, before: u64) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if snapshot_hits(server) > before {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the Snapshot hits never grew past {before}");
}

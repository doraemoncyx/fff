mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestServer, test_config, write_many};
use fff_mcp_http::Config;

const CAP: usize = 8;
const SUBDIRS: [&str; 3] = ["alpha", "beta", "gamma"];
const FILES: usize = 120;

fn big_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_many(dir.path(), FILES, |index| {
        format!("{}/f{index}.rs", SUBDIRS[index % SUBDIRS.len()])
    });
    dir
}

fn capped(localdata: &Path) -> Config {
    Config {
        root_cap_files: CAP,
        localdata: Some(localdata.to_path_buf()),
        ..test_config()
    }
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn refuse(server: &TestServer, root: &Path) -> String {
    let tardir = path_of(root);
    let response = server.search(&[("querystr", ""), ("tardir", tardir.as_str())]);
    assert_eq!(response.status, 400, "{}", response.body);
    let body: serde_json::Value = serde_json::from_str(&response.body).expect("error body is JSON");
    assert_eq!(body["error"], "INVALID_PARAMS", "{body}");
    body["message"].as_str().expect("message").to_string()
}

fn listed(server: &TestServer) -> Vec<serde_json::Value> {
    server.stats()["refused_roots"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

// Empties the subdirectories and leaves the root's own entries alone: a walk
// would now admit the tree, while the root's mtime still matches its record.
fn empty_the_subdirectories(root: &Path) {
    for name in SUBDIRS {
        let dir = root.join(name);
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
}

fn localdata(store: &tempfile::TempDir) -> PathBuf {
    store.path().join("localdata.json")
}

#[test]
fn a_repeat_request_replays_the_record_instead_of_walking() {
    let tree = big_tree();
    let store = tempfile::tempdir().unwrap();
    let server = TestServer::start(capped(&localdata(&store)));

    let first = refuse(&server, tree.path());
    let records = listed(&server);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["cap"], CAP, "{records:?}");
    assert!(
        records[0]["counted"].as_u64().unwrap_or(0) > CAP as u64,
        "{records:?}"
    );
    assert!(
        records[0]["first_seen"].as_u64().unwrap_or(0) > 0,
        "{records:?}"
    );
    assert!(
        records[0]["last_seen"].as_u64() >= records[0]["first_seen"].as_u64(),
        "{records:?}"
    );

    // The tree is under the cap now, but nothing at the root level changed, so
    // an identical refusal cannot come from a walk.
    empty_the_subdirectories(tree.path());
    assert_eq!(refuse(&server, tree.path()), first);
    assert_eq!(listed(&server).len(), 1);
}

#[test]
fn the_record_survives_a_restart() {
    let tree = big_tree();
    let store = tempfile::tempdir().unwrap();
    let path = localdata(&store);

    let first = {
        let server = TestServer::start(capped(&path));
        let message = refuse(&server, tree.path());
        assert!(server.shutdown_within(Duration::from_secs(30)));
        message
    };
    assert!(path.exists(), "the refusal was never written to disk");

    empty_the_subdirectories(tree.path());
    let server = TestServer::start(capped(&path));
    assert_eq!(refuse(&server, tree.path()), first);
    assert_eq!(listed(&server).len(), 1);
}

#[test]
fn a_shrunk_tree_is_admitted_once_its_root_mtime_changes() {
    let tree = big_tree();
    let store = tempfile::tempdir().unwrap();
    let server = TestServer::start(capped(&localdata(&store)));

    refuse(&server, tree.path());
    empty_the_subdirectories(tree.path());
    // Nothing at the root level moved yet: the record still stands.
    refuse(&server, tree.path());

    std::fs::create_dir(tree.path().join("fresh")).unwrap();
    std::fs::write(tree.path().join("fresh/one.rs"), "x").unwrap();
    let body = server.search_json(&[
        ("querystr", ""),
        ("tardir", &path_of(tree.path())),
        ("maxResults", "50"),
    ]);
    assert_eq!(body["total"], 1, "{body}");
    assert!(
        listed(&server).is_empty(),
        "a record survived the directory being admitted"
    );
}

#[test]
fn a_grown_tree_is_walked_again_rather_than_admitted() {
    let tree = big_tree();
    let store = tempfile::tempdir().unwrap();
    let server = TestServer::start(capped(&localdata(&store)));

    let first = refuse(&server, tree.path());
    let seen = listed(&server)[0]["first_seen"].clone();

    // A new top-level directory: the root's mtime moves, so the cap re-walks.
    write_many(&tree.path().join("extra"), 40, |index| {
        format!("f{index}.rs")
    });
    let second = refuse(&server, tree.path());
    assert_ne!(second, first, "the old record answered a changed tree");
    assert!(second.contains("extra"), "{second}");

    let records = listed(&server);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0]["first_seen"], seen,
        "the first sighting was lost"
    );
}

#[test]
fn a_corrupt_or_missing_record_file_is_ignored_and_rebuilt() {
    let store = tempfile::tempdir().unwrap();
    let path = localdata(&store);
    let contents = [
        Some("{ not json"),
        Some("{\"version\": 99, \"roots\": {}}"),
        None,
    ];
    for content in contents {
        match content {
            Some(text) => std::fs::write(&path, text).unwrap(),
            None => {
                std::fs::remove_file(&path).ok();
            }
        };
        let tree = big_tree();
        let server = TestServer::start(capped(&path));
        assert!(listed(&server).is_empty(), "stale records from {content:?}");

        let message = refuse(&server, tree.path());
        assert!(message.contains("Root cap"), "{message}");
        let text = std::fs::read_to_string(&path).expect("the record file was not rebuilt");
        let file: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(file["version"], 1, "{text}");
        assert!(
            file["roots"]
                .as_object()
                .is_some_and(|roots| roots.len() == 1),
            "{text}"
        );
    }
}

#[test]
fn concurrent_requests_for_one_refused_tardir_share_a_result() {
    let tree = big_tree();
    let store = tempfile::tempdir().unwrap();
    let server = TestServer::start(capped(&localdata(&store)));
    let tardir = path_of(tree.path());

    let bodies: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let server = &server;
                let tardir = tardir.as_str();
                scope.spawn(move || {
                    let response = server.search(&[("querystr", ""), ("tardir", tardir)]);
                    assert_eq!(response.status, 400, "{}", response.body);
                    response.body
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    assert_eq!(bodies.len(), 10);
    assert!(
        bodies.iter().all(|body| body == &bodies[0]),
        "the ten answers differ: {bodies:?}"
    );
    assert_eq!(listed(&server).len(), 1);
}

#[test]
fn an_unwritable_record_file_leaves_the_refusal_intact() {
    let tree = big_tree();
    let store = tempfile::tempdir().unwrap();
    let blocker = store.path().join("blocked");
    std::fs::write(&blocker, "not a directory").unwrap();
    let server = TestServer::start(capped(&blocker.join("localdata.json")));

    let first = refuse(&server, tree.path());
    assert!(first.contains("Root cap"), "{first}");
    assert_eq!(refuse(&server, tree.path()), first);
    assert_eq!(listed(&server).len(), 1);
}

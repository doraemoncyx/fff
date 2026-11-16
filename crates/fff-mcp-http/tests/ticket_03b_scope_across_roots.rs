mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{TestServer, test_config, write_tree};
use serde_json::Value;

const FILES: [&str; 8] = [
    "data/one.png",
    "data/nested/two.png",
    "data/nested/deep/three.png",
    "editor/main.lua",
    "editor/art/hud.png",
    "docs/readme.md",
    "root.txt",
    "bulk/filler.dat",
];

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &FILES);
    dir
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn rels(response: &Value) -> Vec<String> {
    let mut rels: Vec<String> = response["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {response}"))
        .iter()
        .map(|hit| hit["rel"].as_str().expect("rel").to_string())
        .collect();
    rels.sort();
    rels
}

fn wait_for_completed_scans(server: &TestServer) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let stats = server.stats();
        let idle = stats["indexes"]
            .as_array()
            .expect("indexes")
            .iter()
            .all(|index| index["scanning"] == false);
        if idle {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("scans did not finish");
}

/// A scope inside one child must not be widened by a sibling Root's matches.
#[test]
fn scope_outside_a_child_root_excludes_it() {
    let dir = tree();
    let mut config = test_config();
    config.scan_wait = Duration::ZERO;
    let server = TestServer::start(config);

    for child in ["data", "editor"] {
        server.search_json(&[
            ("querystr", ""),
            ("tardir", &path_of(&dir.path().join(child))),
        ]);
    }
    wait_for_completed_scans(&server);

    let parent = path_of(dir.path());
    let scoped = server.search_json(&[
        ("querystr", ""),
        ("tardir", &parent),
        ("path", "editor"),
        ("paths", "editor"),
        ("globs", "!**/*.lua"),
        ("maxResults", "100"),
    ]);
    let rels = rels(&scoped);
    assert!(
        rels.iter().all(|rel| rel.starts_with("editor/")),
        "scope leaked to a sibling: {rels:?}"
    );
    assert_eq!(rels, vec!["editor/art/hud.png"], "{scoped}");
}

/// `paths` with a relative prefix reaches the children too.
#[test]
fn paths_scope_reaches_the_child_roots() {
    let dir = tree();
    let mut config = test_config();
    config.scan_wait = Duration::ZERO;
    let server = TestServer::start(config);

    for child in ["data", "editor"] {
        server.search_json(&[
            ("querystr", ""),
            ("tardir", &path_of(&dir.path().join(child))),
        ]);
    }
    wait_for_completed_scans(&server);

    let parent = path_of(dir.path());
    let scoped = server.search_json(&[
        ("querystr", ""),
        ("tardir", &parent),
        ("paths", "data/nested"),
        ("maxResults", "100"),
    ]);
    let rels = rels(&scoped);
    assert_eq!(
        rels,
        vec!["data/nested/deep/three.png", "data/nested/two.png"],
        "{scoped}"
    );
}

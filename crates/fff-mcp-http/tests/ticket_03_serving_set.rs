mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{TestServer, test_config, write_tree};
use serde_json::Value;

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

fn search(server: &TestServer, tardir: &str) -> Value {
    server.search_json(&[("querystr", ""), ("tardir", tardir), ("maxResults", "100")])
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

fn paths(response: &Value) -> Vec<String> {
    let mut paths: Vec<String> = response["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {response}"))
        .iter()
        .map(|hit| hit["path"].as_str().expect("path").to_string())
        .collect();
    paths.sort();
    paths
}

fn assert_rels(response: &Value, expected: &[&str]) {
    let mut expected: Vec<String> = expected.iter().map(|rel| rel.to_string()).collect();
    expected.sort();
    assert_eq!(rels(response), expected, "{response}");
}

fn stats_roots(server: &TestServer) -> Vec<String> {
    let stats = server.stats();
    let mut roots: Vec<String> = stats["indexes"]
        .as_array()
        .unwrap_or_else(|| panic!("no indexes: {stats}"))
        .iter()
        .map(|index| index["root"].as_str().expect("root").to_string())
        .collect();
    roots.sort();
    roots
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

fn wait_for_index_count(server: &TestServer, expected: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut roots = stats_roots(server);
    while Instant::now() < deadline {
        roots = stats_roots(server);
        if roots.len() == expected {
            return roots;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("expected {expected} indexes, got {roots:?}");
}

#[test]
fn parent_then_direct_child_reuses_the_parent_index() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let parent = path_of(dir.path());
    let child = path_of(&dir.path().join("data"));

    assert_eq!(search(&server, &parent)["merged"], false);
    assert_eq!(stats_roots(&server).len(), 1);

    let response = search(&server, &child);
    assert_eq!(response["merged"], false, "{response}");
    assert_eq!(response["served_by"].as_array().unwrap().len(), 1);
    assert_rels(
        &response,
        &["one.csv", "nested/two.csv", "nested/deep/three.csv"],
    );
    assert_eq!(
        stats_roots(&server).len(),
        1,
        "the child must reuse the parent index"
    );
}

#[test]
fn parent_then_grandchild_reuses_the_parent_index() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let parent = path_of(dir.path());
    let grandchild = path_of(&dir.path().join("data").join("nested"));

    assert_eq!(search(&server, &parent)["merged"], false);

    let response = search(&server, &grandchild);
    assert_eq!(response["merged"], false, "{response}");
    assert_rels(&response, &["two.csv", "deep/three.csv"]);
    assert_eq!(stats_roots(&server).len(), 1);
}

#[test]
fn children_then_parent_merges_and_dedupes() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let parent = path_of(dir.path());
    search(&server, &path_of(&dir.path().join("data")));
    search(&server, &path_of(&dir.path().join("editor")));
    assert_eq!(stats_roots(&server).len(), 2);

    let response = search(&server, &parent);
    assert_eq!(response["merged"], true, "{response}");
    let served = response["served_by"].as_array().unwrap();
    assert_eq!(served.len(), 3, "{response}");
    assert_rels(&response, &FILES);

    let mut unique = paths(&response);
    unique.dedup();
    assert_eq!(unique.len(), FILES.len(), "{response}");
    assert_eq!(response["total"], FILES.len(), "{response}");
    assert!(response.get("cursor").is_none(), "{response}");
}

#[test]
fn children_answer_while_the_parent_is_still_scanning() {
    let dir = tree();
    // Keep the parent walk busy long enough that the children cannot already be
    // subsumed by the time this response is assembled.
    for index in 0..20_000 {
        let path = dir.path().join("bulk").join(format!("f{index}.dat"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "x").unwrap();
    }
    let mut config = test_config();
    config.scan_wait = Duration::ZERO;
    let server = TestServer::start(config);

    search(&server, &path_of(&dir.path().join("data")));
    search(&server, &path_of(&dir.path().join("editor")));
    wait_for_completed_scans(&server);

    // No wait for the parent scan: everything below must come from the children.
    let response = search(&server, &path_of(dir.path()));
    assert_eq!(response["merged"], true, "{response}");
    assert_eq!(response["served_by"].as_array().unwrap().len(), 3);
    let rels = rels(&response);
    for rel in ["data/one.csv", "data/nested/two.csv", "editor/main.lua"] {
        assert!(rels.contains(&rel.to_string()), "missing {rel}: {response}");
    }
    let mut unique = paths(&response);
    unique.dedup();
    assert_eq!(unique.len(), paths(&response).len(), "{response}");

    // The parent scan keeps running after the response; subsumption must not
    // need another search request to fire.
    let parent = path_of(dir.path());
    let roots = wait_for_index_count(&server, 1);
    assert_eq!(roots[0].to_lowercase(), parent.to_lowercase(), "{roots:?}");
}

#[test]
fn a_completed_parent_scan_subsumes_its_children() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let parent = path_of(dir.path());
    search(&server, &path_of(&dir.path().join("data")));
    search(&server, &path_of(&dir.path().join("editor")));
    assert_eq!(stats_roots(&server).len(), 2);

    let merged = search(&server, &parent);
    assert_eq!(merged["merged"], true, "{merged}");

    let roots = stats_roots(&server);
    assert_eq!(roots.len(), 1, "{roots:?}");
    assert_eq!(roots[0].to_lowercase(), parent.to_lowercase());

    let again = search(&server, &parent);
    assert_eq!(again["merged"], false, "{again}");
    assert_eq!(rels(&again), rels(&merged), "results must not change");
    assert_rels(&again, &FILES);
}

#[test]
fn cover_comparison_uses_canonical_paths() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let base = path_of(dir.path());
    let mut spellings = vec![
        base.clone(),
        format!("{base}/"),
        format!("{}/data/..", base.replace('\\', "/")),
    ];
    if cfg!(windows) {
        spellings.push(base.to_uppercase());
    }

    for spelling in &spellings {
        let response = search(&server, spelling);
        assert_eq!(
            response["served_by"].as_array().unwrap().len(),
            1,
            "{spelling}: {response}"
        );
        assert_eq!(
            stats_roots(&server).len(),
            1,
            "{spelling} created a second index"
        );
        assert_rels(&response, &FILES);
    }
}

#[test]
fn a_child_index_never_covers_the_parent_tardir() {
    let dir = tree();
    let server = TestServer::start(test_config());
    search(&server, &path_of(&dir.path().join("data")));
    assert_eq!(stats_roots(&server).len(), 1);

    let response = search(&server, &path_of(dir.path()));
    assert_eq!(response["merged"], true, "{response}");
    assert_eq!(response["served_by"].as_array().unwrap().len(), 2);
    assert_rels(&response, &FILES);
}

#[test]
fn concurrent_requests_survive_subsumption() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let parent = path_of(dir.path());
    search(&server, &path_of(&dir.path().join("data")));
    search(&server, &path_of(&dir.path().join("editor")));

    let (left, right) = std::thread::scope(|scope| {
        let left = scope.spawn(|| search(&server, &parent));
        let right = scope.spawn(|| search(&server, &parent));
        (left.join().unwrap(), right.join().unwrap())
    });

    for response in [&left, &right] {
        assert_rels(response, &FILES);
        let mut unique = paths(response);
        unique.dedup();
        assert_eq!(unique.len(), FILES.len(), "{response}");
    }
}

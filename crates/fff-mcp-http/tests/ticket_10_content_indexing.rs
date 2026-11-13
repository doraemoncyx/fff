mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{TestServer, test_config, write_tree};
use fff_mcp_http::Config;
use serde_json::{Value, json};

const FILES: [&str; 5] = [
    "src/main.rs",
    "src/helper.rs",
    "docs/readme.md",
    "data/table.csv",
    "plain/notes.txt",
];

const MAIN_RS: &str = "fn compute_total() -> i32 {\n    needle_alpha + 1\n}\n";
const NOTES_TXT: &str = "needle_alpha line\ntail\n";

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &FILES);
    std::fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();
    std::fs::write(dir.path().join("plain/notes.txt"), NOTES_TXT).unwrap();
    dir
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn search(server: &TestServer, tardir: &str) -> Value {
    server.search_json(&[("querystr", ""), ("tardir", tardir), ("maxResults", "50")])
}

fn stats_index(server: &TestServer) -> Value {
    let stats = server.stats();
    stats["indexes"]
        .as_array()
        .and_then(|indexes| indexes.first())
        .cloned()
        .unwrap_or_else(|| panic!("no index entry: {stats}"))
}

fn wait_for(server: &TestServer, timeout: Duration, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + timeout;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = stats_index(server);
        if done(&last) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("condition not reached, last stats: {last}");
}

fn enable_content_indexing() -> Config {
    Config {
        content_indexing: true,
        ..test_config()
    }
}

#[test]
fn the_prefilter_is_built_by_default() {
    let dir = tree();
    let server = TestServer::start(enable_content_indexing());
    let tardir = path_of(dir.path());

    search(&server, &tardir);
    let index = wait_for(&server, Duration::from_secs(30), |index| {
        index["prefilter"] == true
    });
    assert_eq!(index["indexing"], false, "{index}");
    assert_eq!(server.stats()["content_indexing"], true);

    let grep = server.mcp_tool(
        1,
        "grep",
        json!({ "tardir": tardir, "query": "needle_alpha" }),
    );
    let text = grep["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("needle_alpha"), "{grep}");
    assert!(text.contains("src/main.rs"), "{grep}");
    assert!(text.contains("plain/notes.txt"), "{grep}");
}

#[test]
fn disabling_it_keeps_the_prefilter_absent() {
    let dir = tree();
    let server = TestServer::start(Config {
        content_indexing: false,
        ..test_config()
    });
    let tardir = path_of(dir.path());

    search(&server, &tardir);
    wait_for(&server, Duration::from_secs(30), |index| {
        index["scanning"] == false
    });
    for _ in 0..5 {
        let index = stats_index(&server);
        assert_eq!(index["prefilter"], false, "{index}");
        assert_eq!(index["indexing"], false, "{index}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(server.stats()["content_indexing"], false);

    let grep = server.mcp_tool(
        1,
        "grep",
        json!({ "tardir": tardir, "query": "needle_alpha" }),
    );
    let text = grep["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("needle_alpha"), "{grep}");
}

#[test]
fn results_are_identical_with_and_without_the_prefilter() {
    let dir = tree();
    let tardir = path_of(dir.path());

    let indexed = TestServer::start(enable_content_indexing());
    search(&indexed, &tardir);
    wait_for(&indexed, Duration::from_secs(30), |index| {
        index["prefilter"] == true
    });
    let with_prefilter = indexed.mcp_tool(
        1,
        "grep",
        json!({ "tardir": tardir, "query": "needle_alpha", "maxResults": 20 }),
    );

    let plain = TestServer::start(Config {
        content_indexing: false,
        ..test_config()
    });
    search(&plain, &tardir);
    wait_for(&plain, Duration::from_secs(30), |index| {
        index["scanning"] == false
    });
    let without_prefilter = plain.mcp_tool(
        1,
        "grep",
        json!({ "tardir": tardir, "query": "needle_alpha", "maxResults": 20 }),
    );

    assert_eq!(with_prefilter, without_prefilter);
}

#[test]
fn the_content_index_counts_against_the_estimate() {
    let dir = tree();
    let server = TestServer::start(enable_content_indexing());
    let tardir = path_of(dir.path());
    search(&server, &tardir);
    wait_for(&server, Duration::from_secs(30), |index| {
        index["prefilter"] == true
    });

    let index = stats_index(&server);
    let files = index["files"].as_u64().unwrap();
    let estimate = index["estimated_bytes"].as_u64().unwrap();
    // 625 B/file is the bigram filter alone, on top of the file list.
    assert!(
        estimate >= files * 625,
        "prefilter bytes missing from the estimate: {index}"
    );
}

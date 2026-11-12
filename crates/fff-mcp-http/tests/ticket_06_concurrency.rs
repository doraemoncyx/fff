mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{ScriptedProbe, TestServer, test_config, write_tree};
use serde_json::Value;

/// Enough files that a walk outlives the first follow-up request.
fn wide_tree(root: &Path, count: usize) {
    for index in 0..count {
        let parent = root.join(format!("d{:02}/s{:02}", index % 40, index % 13));
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join(format!("file{index}.txt")), "x").unwrap();
    }
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn search(server: &TestServer, tardir: &str) -> Value {
    server.search_json(&[("querystr", ""), ("tardir", tardir), ("maxResults", "100")])
}

fn stats_roots(server: &TestServer) -> Vec<String> {
    let stats = server.stats();
    stats["indexes"]
        .as_array()
        .unwrap_or_else(|| panic!("no indexes: {stats}"))
        .iter()
        .map(|index| index["root"].as_str().expect("root").to_string())
        .collect()
}

fn wait_for_scans(server: &TestServer) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let idle = server.stats()["indexes"]
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

fn assert_unique_paths(response: &Value) {
    let mut paths: Vec<&str> = response["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {response}"))
        .iter()
        .map(|hit| hit["path"].as_str().expect("path"))
        .collect();
    let seen = paths.len();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(seen, paths.len(), "duplicate file in {response}");
}

fn evict(server: &TestServer) {
    let response = server.post("/test/evict", "", &[]);
    assert_eq!(response.status, 200, "evict failed: {}", response.body);
}

fn healthz_within(server: Arc<TestServer>, limit: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let status = server.get("/healthz").status;
        let _ = tx.send(status);
    });
    rx.recv_timeout(limit)
        .map(|status| status == 200)
        .unwrap_or(false)
}

fn concurrent_searches(server: &TestServer, tardirs: &[String]) -> Vec<Value> {
    let barrier = std::sync::Barrier::new(tardirs.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = tardirs
            .iter()
            .map(|tardir| {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    search(server, tardir)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("search thread"))
            .collect()
    })
}

#[test]
fn eight_concurrent_requests_share_one_scan() {
    let dir = tempfile::tempdir().unwrap();
    wide_tree(dir.path(), 3000);
    let tardir = path_of(dir.path());
    let server = TestServer::start(test_config());

    let tardirs = vec![tardir; 8];
    let responses = concurrent_searches(&server, &tardirs);

    for response in &responses {
        assert_eq!(response["total"].as_u64(), Some(3000), "{response}");
        assert_eq!(response["scanning"], false, "{response}");
        assert_eq!(
            response["served_by"].as_array().unwrap().len(),
            1,
            "{response}"
        );
    }

    let stats = server.stats();
    assert_eq!(stats_roots(&server).len(), 1, "{stats}");
    assert_eq!(stats["scans_started"].as_u64(), Some(1), "{stats}");
    assert_eq!(stats["peak_scans_in_flight"].as_u64(), Some(1), "{stats}");
}

#[test]
fn the_scan_cap_queues_extra_requests_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    let mut tardirs = Vec::new();
    for index in 0..6 {
        let root = dir.path().join(format!("root{index}"));
        wide_tree(&root, 800);
        tardirs.push(path_of(&root));
    }

    let mut config = test_config();
    config.max_concurrent_scans = 2;
    config.scan_wait = Duration::from_secs(60);
    let server = TestServer::start(config);

    let responses = concurrent_searches(&server, &tardirs);
    for response in &responses {
        assert_eq!(response["total"].as_u64(), Some(800), "{response}");
        assert_eq!(response["scanning"], false, "{response}");
    }

    let stats = server.stats();
    assert_eq!(stats_roots(&server).len(), 6, "{stats}");
    assert_eq!(stats["scans_started"].as_u64(), Some(6), "{stats}");
    // Permit release trails the last published file by a scheduler hop.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut in_flight = stats["scans_in_flight"].as_u64().unwrap_or(1);
    while in_flight != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        in_flight = server.stats()["scans_in_flight"].as_u64().unwrap_or(1);
    }
    assert_eq!(in_flight, 0, "{stats}");
    assert!(
        server.stats()["peak_scans_in_flight"].as_u64().unwrap_or(0) <= 2,
        "the cap was exceeded: {stats}"
    );
}

#[test]
fn a_short_deadline_returns_partial_results_and_a_retry_completes() {
    let dir = tempfile::tempdir().unwrap();
    wide_tree(dir.path(), 4000);
    let tardir = path_of(dir.path());

    let mut config = test_config();
    config.scan_wait = Duration::ZERO;
    let server = TestServer::start(config);

    let started = Instant::now();
    let partial = search(&server, &tardir);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "partial answer waited {elapsed:?}"
    );
    assert_eq!(partial["scanning"], true, "{partial}");
    assert!(
        partial["scanned"].as_u64().unwrap_or(u64::MAX) < 4000,
        "{partial}"
    );

    wait_for_scans(&server);
    let complete = search(&server, &tardir);
    assert_eq!(complete["scanning"], false, "{complete}");
    assert_eq!(complete["total"].as_u64(), Some(4000), "{complete}");
}

#[test]
fn scanned_grows_until_it_matches_the_file_count() {
    let dir = tempfile::tempdir().unwrap();
    wide_tree(dir.path(), 6000);
    let tardir = path_of(dir.path());

    let mut config = test_config();
    config.scan_wait = Duration::ZERO;
    let server = TestServer::start(config);

    let mut scanned = search(&server, &tardir)["scanned"]
        .as_u64()
        .expect("scanned");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let response = search(&server, &tardir);
        let current = response["scanned"].as_u64().expect("scanned");
        assert!(current >= scanned, "scanned went backwards: {response}");
        scanned = current;
        if response["scanning"] == false {
            assert_eq!(
                scanned, 6000,
                "scanned must equal the file count: {response}"
            );
            assert_eq!(response["total"].as_u64(), Some(6000), "{response}");
            let stats = server.stats();
            assert_eq!(stats["indexes"][0]["files"].as_u64(), Some(6000), "{stats}");
            return;
        }
        assert!(Instant::now() < deadline, "scan did not finish");
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[test]
fn a_scanning_parent_keeps_the_child_results() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["top.txt"]);
    let child = dir.path().join("child");
    wide_tree(&child, 4000);
    let child = path_of(&child);
    let parent = path_of(dir.path());

    let mut config = test_config();
    config.scan_wait = Duration::ZERO;
    let server = TestServer::start(config);

    search(&server, &child);
    wait_for_scans(&server);

    // No wait for the parent: everything already indexed below it must survive.
    let response = search(&server, &parent);
    assert_eq!(response["merged"], true, "{response}");
    assert_eq!(response["scanning"], true, "{response}");
    assert_eq!(
        response["served_by"].as_array().unwrap().len(),
        2,
        "{response}"
    );
    let rels: Vec<&str> = response["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|hit| hit["rel"].as_str().expect("rel"))
        .collect();
    assert!(
        rels.iter().any(|rel| rel.starts_with("child/")),
        "child results were dropped: {response}"
    );
    assert_unique_paths(&response);
}

#[test]
fn a_queued_request_past_its_deadline_returns_partial_results() {
    let dir = tempfile::tempdir().unwrap();
    let busy = dir.path().join("busy");
    wide_tree(&busy, 6000);
    let first_dir = dir.path().join("first");
    let second_dir = dir.path().join("second");
    write_tree(&first_dir, &["a.txt"]);
    write_tree(&second_dir, &["b.txt"]);

    let mut config = test_config();
    config.max_concurrent_scans = 1;
    config.scan_wait = Duration::ZERO;
    let server = TestServer::start(config);

    // Takes the only permit; its walk keeps running after the response.
    let busy_response = search(&server, &path_of(&busy));
    assert_eq!(busy_response["scanning"], true, "{busy_response}");

    let queued = [
        search(&server, &path_of(&first_dir)),
        search(&server, &path_of(&second_dir)),
    ];
    for response in &queued {
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["scanning"], true, "{response}");
        assert_eq!(
            response["served_by"].as_array().unwrap().len(),
            0,
            "{response}"
        );
        assert_eq!(response["scanned"].as_u64(), Some(0), "{response}");
        assert_eq!(
            response["results"].as_array().unwrap().len(),
            0,
            "{response}"
        );
    }
    assert_eq!(
        stats_roots(&server).len(),
        1,
        "a queued request must not start a second scan: {}",
        server.stats()
    );
}

#[test]
fn timeouts_and_evictions_still_answer_healthz() {
    let dir = tempfile::tempdir().unwrap();
    let mut tardirs = Vec::new();
    for index in 0..3 {
        let root = dir.path().join(format!("root{index}"));
        wide_tree(&root, 400);
        tardirs.push(path_of(&root));
    }

    let mut config = test_config();
    config.max_memory_bytes = 1024 * 1024;
    config.max_concurrent_scans = 1;
    config.scan_wait = Duration::ZERO;
    config.evict_interval = Duration::from_millis(200);
    config.test_hook = true;
    let probe = ScriptedProbe::new(8 * 1024 * 1024);
    let server = Arc::new(TestServer::start_with_probe(config, probe));

    std::thread::scope(|scope| {
        for tardir in &tardirs {
            let server = Arc::clone(&server);
            scope.spawn(move || {
                for _ in 0..6 {
                    let response = search(&server, tardir);
                    assert!(response.get("error").is_none(), "{response}");
                }
            });
        }
        let server = Arc::clone(&server);
        scope.spawn(move || {
            for _ in 0..20 {
                evict(&server);
            }
        });
    });

    assert!(
        healthz_within(Arc::clone(&server), Duration::from_secs(10)),
        "the server stopped answering /healthz"
    );
}

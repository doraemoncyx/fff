mod common;

use std::time::{Duration, Instant};

use common::{ScriptedProbe, TestServer, test_config, write_tree};
use fff_mcp_http::Config;
use serde_json::Value;

const BUDGET: u64 = 1_000_000;
const OVER: u64 = 4_000_000;
const UNDER: u64 = 100_000;

fn evict_config() -> Config {
    let mut config = test_config();
    config.max_memory_bytes = BUDGET;
    // Keep the periodic trigger out of the way; rounds are forced explicitly.
    config.evict_interval = Duration::from_secs(3600);
    config.test_hook = true;
    config
}

/// Sibling Roots under one parent: no Root covers another, so no subsumption.
fn roots_tree(count: usize) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut roots = Vec::new();
    for index in 0..count {
        let path = dir.path().join(format!("root{index}"));
        let file = format!("file{index}.txt");
        write_tree(&path, &[&file]);
        roots.push(path.to_string_lossy().into_owned());
    }
    (dir, roots)
}

fn search(server: &TestServer, tardir: &str) -> Value {
    server.search_json(&[("querystr", ""), ("tardir", tardir), ("maxResults", "100")])
}

/// `/stats` order is LRU order: least recently requested first.
fn ordered_roots(server: &TestServer) -> Vec<String> {
    server.stats()["indexes"]
        .as_array()
        .unwrap_or_else(|| panic!("no indexes"))
        .iter()
        .map(|index| index["root"].as_str().expect("root").to_string())
        .collect()
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

fn wait_for_scans(server: &TestServer) {
    let deadline = Instant::now() + Duration::from_secs(60);
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

/// Let the housekeeping tick observe completed scans before scripting a round.
fn drain_ticks() {
    std::thread::sleep(Duration::from_millis(1200));
}

#[test]
fn over_budget_eviction_follows_lru_order() {
    let (_dir, roots) = roots_tree(2);
    let probe = ScriptedProbe::new(UNDER);
    let server = TestServer::start_with_probe(evict_config(), probe.clone());

    search(&server, &roots[0]);
    search(&server, &roots[1]);
    wait_for_scans(&server);
    drain_ticks();
    assert_eq!(ordered_roots(&server), roots, "creation order is LRU order");

    probe.settle(OVER);
    let stats = server.stats();
    assert_eq!(stats["used_bytes"].as_u64(), Some(OVER), "{stats}");
    assert_eq!(stats["budget_bytes"].as_u64(), Some(BUDGET), "{stats}");

    // The request itself triggers the round: its own Root is exempt, so the
    // least recently used one goes even though it is not the newest.
    search(&server, &roots[1]);
    assert_eq!(ordered_roots(&server), vec![roots[1].clone()]);

    // The next request recreates its Root and drops the one used before it.
    search(&server, &roots[0]);
    assert_eq!(ordered_roots(&server), vec![roots[0].clone()]);
}

#[test]
fn the_serving_set_is_never_evicted() {
    let (_dir, roots) = roots_tree(4);
    let probe = ScriptedProbe::new(UNDER);
    let server = TestServer::start_with_probe(evict_config(), probe.clone());

    for root in &roots {
        search(&server, root);
    }
    wait_for_scans(&server);
    drain_ticks();
    assert_eq!(ordered_roots(&server), roots);

    // roots[0] is the least recently used Index, and the only one this request
    // serves; the round it triggers must take the three newer Indexes instead.
    probe.settle(OVER);
    let response = search(&server, &roots[0]);
    assert_eq!(
        response["served_by"].as_array().unwrap().len(),
        1,
        "{response}"
    );
    assert_eq!(ordered_roots(&server), vec![roots[0].clone()]);
}

#[test]
fn a_lone_index_over_budget_is_kept() {
    let (_dir, roots) = roots_tree(1);
    let probe = ScriptedProbe::new(UNDER);
    let server = TestServer::start_with_probe(evict_config(), probe.clone());

    search(&server, &roots[0]);
    wait_for_scans(&server);
    drain_ticks();

    probe.settle(OVER);
    probe.reset_calls();
    assert!(evict(&server).is_empty());
    assert_eq!(probe.calls(), 1, "the loop must not spin on a single Index");
    assert_eq!(ordered_roots(&server), roots);
    assert_eq!(server.stats()["used_bytes"].as_u64(), Some(OVER));
}

#[test]
fn a_round_stops_after_three_evictions_and_re_measures() {
    let (_dir, roots) = roots_tree(6);
    let probe = ScriptedProbe::new(UNDER);
    let server = TestServer::start_with_probe(evict_config(), probe.clone());

    for root in &roots {
        search(&server, root);
    }
    wait_for_scans(&server);
    drain_ticks();

    // Over budget, still over after the first eviction, back under after the
    // second: the round must re-measure instead of evicting its full quota.
    probe.arm(&[OVER, OVER, UNDER]);
    probe.reset_calls();
    let evicted = evict(&server);
    assert_eq!(evicted.len(), 2, "{evicted:?}");
    assert_eq!(probe.calls(), 3, "measure, measure, then stop");
    assert_eq!(ordered_roots(&server), roots[2..].to_vec());

    probe.settle(OVER);
    probe.reset_calls();
    let evicted = evict(&server);
    assert_eq!(evicted.len(), 3, "at most three per round: {evicted:?}");
    assert_eq!(probe.calls(), 3, "{evicted:?}");
    assert_eq!(ordered_roots(&server), vec![roots[5].clone()]);
}

#[test]
fn an_evicted_root_waits_out_a_cooldown() {
    let (_dir, roots) = roots_tree(2);
    let probe = ScriptedProbe::new(UNDER);
    let server = TestServer::start_with_probe(evict_config(), probe.clone());

    search(&server, &roots[0]);
    search(&server, &roots[1]);
    wait_for_scans(&server);
    drain_ticks();

    probe.settle(OVER);
    evict(&server);
    assert_eq!(ordered_roots(&server), vec![roots[1].clone()]);

    // Recreate the evicted Root and make it the least recently used one again.
    probe.settle(UNDER);
    search(&server, &roots[0]);
    wait_for_scans(&server);
    search(&server, &roots[1]);
    drain_ticks();
    assert_eq!(ordered_roots(&server), roots);

    probe.settle(OVER);
    let evicted = evict(&server);
    assert_eq!(evicted.len(), 1, "{evicted:?}");
    assert_eq!(
        ordered_roots(&server),
        vec![roots[0].clone()],
        "the recreated Root was LRU but still cooling down"
    );
}

#[test]
fn stats_fall_back_to_estimates_without_a_probe() {
    let (_dir, roots) = roots_tree(1);
    let probe = ScriptedProbe::new(UNDER);
    let server = TestServer::start_with_probe(evict_config(), probe.clone());

    search(&server, &roots[0]);
    wait_for_scans(&server);

    probe.disable();
    let stats = server.stats();
    let estimate = stats["indexes"][0]["estimated_bytes"]
        .as_u64()
        .expect("estimated_bytes");
    assert!(estimate > 0, "{stats}");
    assert_eq!(stats["used_bytes"].as_u64(), Some(estimate), "{stats}");
}

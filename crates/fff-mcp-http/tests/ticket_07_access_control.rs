mod common;

use std::time::Duration;

use common::{TestServer, test_config, write_tree};
use fff_mcp_http::{Config, spawn};
use serde_json::json;

const TOKEN: &str = "s3cret-token";

fn auth() -> (&'static str, &'static str) {
    ("Authorization", "Bearer s3cret-token")
}

fn token_config() -> Config {
    Config {
        token: Some(TOKEN.into()),
        ..test_config()
    }
}

fn initialize_message() -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "ticket-07", "version": "0.0.0" }
        }
    })
}

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["src/main.rs", "README.md"]);
    dir
}

#[test]
fn default_config_only_binds_loopback() {
    let server = TestServer::start(test_config());
    assert!(
        server.addr.ip().is_loopback(),
        "default bind is not loopback: {}",
        server.addr
    );

    let health = server.get("/healthz");
    assert_eq!(health.status, 200, "{}", health.body);
}

#[test]
fn non_loopback_bind_without_a_token_refuses_to_start() {
    let config = Config {
        bind: "0.0.0.0".parse().unwrap(),
        port: 0,
        ..Config::default()
    };
    let error = spawn(config).err().expect("start must fail");
    let message = error.to_string();
    assert!(message.contains("--token"), "{message}");
    assert!(message.contains("openssl rand -hex 24"), "{message}");
    assert!(message.contains("0.0.0.0"), "{message}");
}

#[test]
fn every_protected_endpoint_needs_the_token() {
    let dir = tree();
    let server = TestServer::start(token_config());
    let tardir = dir.path().to_string_lossy().to_string();
    let params = [("querystr", "main"), ("tardir", tardir.as_str())];

    let wrong = [("Authorization", "Bearer not-the-token")];
    let good = [auth()];

    assert_eq!(server.search_with(&params, &[]).status, 401);
    assert_eq!(server.search_with(&params, &wrong).status, 401);
    let allowed = server.search_with(&params, &good);
    assert_eq!(allowed.status, 200, "{}", allowed.body);
    let body: serde_json::Value = serde_json::from_str(&allowed.body).unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 1, "{body}");

    assert_eq!(server.get("/stats").status, 401);
    assert_eq!(server.get_with("/stats", &wrong).status, 401);
    assert_eq!(server.get_with("/stats", &good).status, 200);

    let initialize = initialize_message();
    assert_eq!(server.mcp_with(initialize.clone(), &[]).status, 401);
    assert_eq!(server.mcp_with(initialize.clone(), &wrong).status, 401);
    let accepted = server.mcp_with(initialize, &good);
    assert_eq!(accepted.status, 200, "{}", accepted.body);
    let response: serde_json::Value = serde_json::from_str(&accepted.body).unwrap();
    assert_eq!(response["result"]["serverInfo"]["name"], "fff-mcp-http");
}

#[test]
fn healthz_stays_open_with_a_token_configured() {
    let server = TestServer::start(token_config());
    let health = server.get("/healthz");
    assert_eq!(health.status, 200, "{}", health.body);
    assert!(health.body.contains("\"ok\""), "{}", health.body);
}

#[test]
fn unexpected_host_is_rejected_on_mcp() {
    let config = Config {
        allowed_hosts: vec!["box.test".into()],
        ..token_config()
    };
    let server = TestServer::start(config);
    let headers = [auth(), ("Host", "evil.example")];

    let rejected = server.mcp_with(initialize_message(), &headers);
    assert_eq!(rejected.status, 403, "{}", rejected.body);

    let allowed = server.mcp_with(initialize_message(), &[auth(), ("Host", "box.test")]);
    assert_eq!(allowed.status, 200, "{}", allowed.body);
}

#[test]
fn token_never_appears_in_stats_or_logs() {
    let log_dir = tempfile::tempdir().unwrap();
    let hint = log_dir.path().join("access.log");
    let log_path = fff::log::init_tracing(hint.to_str().unwrap(), Some("debug"), None)
        .expect("logging starts");

    let dir = tree();
    let server = TestServer::start(token_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let good = [auth()];
    let stats = server.get_with("/stats", &good);
    assert_eq!(stats.status, 200, "{}", stats.body);
    assert!(!stats.body.contains(TOKEN), "{}", stats.body);

    // A rejected request logs the path only, never the header value.
    let rejected = server.get_with("/stats", &[("Authorization", "Bearer s3cret-tokeX")]);
    assert_eq!(rejected.status, 401);
    assert!(!rejected.body.contains(TOKEN), "{}", rejected.body);

    let searched = server.search_with(&[("querystr", "main"), ("tardir", tardir.as_str())], &good);
    assert_eq!(searched.status, 200, "{}", searched.body);

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        std::path::Path::new(&log_path).exists(),
        "no log file at {log_path}"
    );
    let logs = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(!logs.contains(TOKEN), "token leaked into {log_path}");
}

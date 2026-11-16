mod common;

use common::{TestServer, make_git_repo, test_config, write_tree};
use serde_json::{Value, json};

const GREP_KEYS: [&str; 3] = ["query", "pattern", "querystr"];
const FIND_FILES_KEYS: [&str; 3] = ["querystr", "query", "pattern"];
const MAIN_RS: &str = "fn main() {
    let needle_alpha = 1;
    println!(\"{needle_alpha}\");
}
";

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["src/main.rs", "docs/readme.md"]);
    std::fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();
    make_git_repo(dir.path());
    dir
}

fn initialize(server: &TestServer) {
    let response = server.mcp(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "schema-parity", "version": "0.0.0" }
        }
    }));
    assert_eq!(response["result"]["serverInfo"]["name"], "fff-mcp-http");
}

#[test]
fn tools_list_advertises_every_identifier_key() {
    let server = TestServer::start(test_config());
    initialize(&server);
    let listing = server.mcp(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));

    for (name, keys) in [("grep", GREP_KEYS), ("find_files", FIND_FILES_KEYS)] {
        let schema = tool_schema(&listing, name).clone();
        for key in keys {
            assert_eq!(
                schema["properties"][key]["type"], "string",
                "{name}: {schema}"
            );
            assert!(
                schema["properties"][key]["description"].is_string(),
                "{name}: {schema}"
            );
        }
        assert_eq!(required_keys(&schema), vec!["tardir"], "{name}: {schema}");
        assert!(schema.get("anyOf").is_none(), "{name}: {schema}");
    }
}

#[test]
fn every_advertised_grep_key_reaches_a_match() {
    let dir = tree();
    let server = TestServer::start(test_config());
    initialize(&server);
    let tardir = dir.path().to_string_lossy().to_string();

    for key in GREP_KEYS {
        let response = server.mcp_tool(
            2,
            "grep",
            json!({ "tardir": tardir, key: "needle_alpha", "mode": "literal", "context": 0 }),
        );
        assert!(response["error"].is_null(), "{key}: {response}");
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{key}: no text in {response}"));
        assert!(text.contains("src/main.rs"), "{key}: {text}");
    }
}

#[test]
fn every_advertised_find_files_key_reaches_the_file() {
    let dir = tree();
    let server = TestServer::start(test_config());
    initialize(&server);
    let tardir = dir.path().to_string_lossy().to_string();

    for key in FIND_FILES_KEYS {
        let response = server.mcp_tool(2, "find_files", json!({ "tardir": tardir, key: "main" }));
        assert!(response["error"].is_null(), "{key}: {response}");
        let rels: Vec<&str> = response["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{key}: no results in {response}"))
            .iter()
            .filter_map(|hit| hit["rel"].as_str())
            .collect();
        assert!(rels.contains(&"src/main.rs"), "{key}: {rels:?}");
    }
}

// The GET routes have no schema, so their parsing must not move with the tools.
#[test]
fn get_routes_still_accept_the_alias_keys() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let search = server.search_json(&[("pattern", "main"), ("tardir", &tardir)]);
    let rels: Vec<&str> = search["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results in {search}"))
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert!(rels.contains(&"src/main.rs"), "{search}");

    let query = common::encode(&[
        ("pattern", "needle_alpha"),
        ("tardir", &tardir),
        ("mode", "literal"),
        ("context", "0"),
    ]);
    let grep = server.get(&format!("/grep?{query}"));
    assert_eq!(grep.status, 200, "{}", grep.body);
    assert!(grep.body.contains("src/main.rs"), "{}", grep.body);
}

// Clients send only the keys they have; an omitted identifier means an empty one.
#[test]
fn a_call_without_any_identifier_key_lists_everything() {
    let dir = tree();
    let server = TestServer::start(test_config());
    initialize(&server);
    let tardir = dir.path().to_string_lossy().to_string();

    let response = server.mcp_tool(2, "find_files", json!({ "tardir": tardir }));
    assert!(response["error"].is_null(), "{response}");
    let rels: Vec<&str> = response["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results in {response}"))
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert!(
        rels.contains(&"src/main.rs") && rels.contains(&"docs/readme.md"),
        "{rels:?}"
    );

    let omitted = server.mcp_tool(3, "grep", json!({ "tardir": tardir, "context": 0 }));
    let empty = server.mcp_tool(4, "grep", json!({ "tardir": tardir, "context": 0, "query": "" }));
    assert!(omitted["error"].is_null(), "{omitted}");
    assert_eq!(omitted["result"], empty["result"]);
}

fn tool_schema(listing: &Value, name: &str) -> Value {
    let tool = listing["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {listing}"))
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not listed: {listing}"));
    tool["inputSchema"].clone()
}

fn required_keys(schema: &Value) -> Vec<&str> {
    schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| key.as_str())
        .collect()
}

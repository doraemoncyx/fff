mod common;

use common::{TestServer, test_config, write_tree};
use serde_json::json;

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(
        dir.path(),
        &[
            "src/main.rs",
            "src/lib.rs",
            "src/nested/deep.rs",
            "docs/readme.md",
            "docs/guide.md",
            "docs/notes.txt",
            "vendor/crate/lib.rs",
            ".hidden/secret.rs",
            ".dotfile.rs",
        ],
    );
    common::make_git_repo(dir.path());
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
            "clientInfo": { "name": "fff-mcp-http-test", "version": "0.0.0" }
        }
    }));
    assert_eq!(
        response["result"]["serverInfo"]["name"], "fff-mcp-http",
        "{response}"
    );
}

fn call(server: &TestServer, arguments: serde_json::Value) -> serde_json::Value {
    let response = server.mcp_call(2, arguments);
    assert!(response["error"].is_null(), "unexpected error: {response}");
    response["result"]["structuredContent"].clone()
}

#[test]
fn initialize_then_list_then_call_roundtrip() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    initialize(&server);

    let listing = server.mcp(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let tools = listing["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3, "{listing}");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, vec!["find_files", "grep", "multi_grep"], "{listing}");
    let find_files = tools
        .iter()
        .find(|tool| tool["name"] == "find_files")
        .expect("find_files is listed");

    let schema = &find_files["inputSchema"];
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(required.contains(&"querystr"), "{schema}");
    assert!(required.contains(&"tardir"), "{schema}");
    for property in [
        "path",
        "paths",
        "globs",
        "hidden",
        "mode",
        "filters",
        "maxResults",
        "cursor",
    ] {
        assert!(
            !schema["properties"][property].is_null(),
            "missing {property}: {schema}"
        );
    }

    let result = call(
        &server,
        json!({ "querystr": "main", "tardir": tardir, "path": "src" }),
    );
    let over_http =
        server.search_json(&[("querystr", "main"), ("tardir", &tardir), ("path", "src")]);
    assert_eq!(result, over_http, "mcp result must match GET /search");
    assert_eq!(result["results"][0]["rel"], "src/main.rs");
}

#[test]
fn path_scope_cannot_escape_the_tardir() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let outside = dir.path().parent().unwrap().join("elsewhere");
    for path in [
        outside.to_string_lossy().to_string(),
        "../escape".to_string(),
    ] {
        let response =
            server.mcp_call(9, json!({ "querystr": "", "tardir": tardir, "path": path }));
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }

    let paths_escape = server.mcp_call(
        9,
        json!({ "querystr": "", "tardir": tardir, "paths": ["..", "src"] }),
    );
    assert_eq!(paths_escape["error"]["code"], -32602, "{paths_escape}");
}

#[test]
fn hidden_false_excludes_dot_paths_unless_named() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let visible = call(
        &server,
        json!({ "querystr": "", "tardir": tardir, "hidden": false }),
    );
    let rels: Vec<&str> = visible["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert!(!rels.iter().any(|rel| rel.contains(".hidden")), "{rels:?}");
    assert!(!rels.iter().any(|rel| rel.contains(".dotfile")), "{rels:?}");

    let named = call(
        &server,
        json!({ "querystr": "", "tardir": tardir, "hidden": false, "path": ".hidden" }),
    );
    let rels: Vec<&str> = named["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert_eq!(rels, vec![".hidden/secret.rs"], "{named}");
}

#[test]
fn globs_filters_and_modes_match_the_stdio_semantics() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let includes = call(
        &server,
        json!({ "querystr": "", "tardir": tardir, "globs": ["*.md"] }),
    );
    let rels: Vec<&str> = includes["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert_eq!(rels.len(), 2, "{includes}");
    assert!(rels.iter().all(|rel| rel.ends_with(".md")), "{rels:?}");

    let excluded = call(
        &server,
        json!({ "querystr": "", "tardir": tardir, "globs": ["*.rs", "!vendor"] }),
    );
    let rels: Vec<&str> = excluded["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert!(
        rels.iter().all(|rel| !rel.starts_with("vendor/")),
        "{rels:?}"
    );
    assert!(
        rels.iter().any(|rel| rel.ends_with("src/main.rs")),
        "{rels:?}"
    );

    let filtered = call(
        &server,
        json!({ "querystr": "", "tardir": tardir, "filters": ["^docs/"] }),
    );
    let rels: Vec<&str> = filtered["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert_eq!(rels.len(), 3, "{filtered}");
    assert!(rels.iter().all(|rel| rel.starts_with("docs/")), "{rels:?}");

    let glob_mode = call(
        &server,
        json!({ "querystr": "src/*.rs", "tardir": tardir, "mode": "glob" }),
    );
    let rels: Vec<&str> = glob_mode["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert_eq!(rels, vec!["src/lib.rs", "src/main.rs"], "{glob_mode}");

    let regex_mode = call(
        &server,
        json!({ "querystr": "^docs/.*\\.md$", "tardir": tardir, "mode": "regex" }),
    );
    assert_eq!(regex_mode["total"], 2, "{regex_mode}");

    let empty_paths = call(
        &server,
        json!({ "querystr": "main", "tardir": tardir, "paths": [""] }),
    );
    assert_eq!(
        empty_paths["results"][0]["rel"], "src/main.rs",
        "{empty_paths}"
    );
}

#[test]
fn cursor_paging_does_not_overlap_and_is_opaque() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let first = server.search_json(&[("querystr", ""), ("tardir", &tardir), ("limit", "3")]);
    let cursor = first["cursor"]
        .as_str()
        .expect("first page has cursor")
        .to_string();
    assert!(first["total"].as_u64().unwrap() > 3, "{first}");
    assert_eq!(first["results"].as_array().unwrap().len(), 3);
    assert_ne!(cursor, "3", "cursor must not be a plain offset");

    let mut seen: Vec<String> = first["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["rel"].as_str().unwrap().to_string())
        .collect();
    let mut cursor = Some(cursor);
    let mut pages = 1;
    while let Some(id) = cursor {
        let page = server.search_json(&[
            ("querystr", ""),
            ("tardir", &tardir),
            ("limit", "3"),
            ("cursor", &id),
        ]);
        let rels: Vec<String> = page["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["rel"].as_str().unwrap().to_string())
            .collect();
        assert!(!rels.is_empty(), "page must not be empty: {page}");
        assert!(
            rels.iter().all(|rel| !seen.contains(rel)),
            "pages overlap: {rels:?} vs {seen:?}"
        );
        seen.extend(rels);
        cursor = page["cursor"].as_str().map(str::to_string);
        pages += 1;
    }
    assert_eq!(pages, 3, "{seen:?}");
    assert_eq!(seen.len(), 9, "{seen:?}");
}

#[test]
fn invalid_regex_and_glob_are_parameter_errors() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let bad_regex = server.search(&[
        ("querystr", "x"),
        ("tardir", &tardir),
        ("filters", "([unclosed"),
    ]);
    assert_eq!(bad_regex.status, 400, "{}", bad_regex.body);
    let body: serde_json::Value = serde_json::from_str(&bad_regex.body).unwrap();
    assert_eq!(body["error"], "INVALID_PARAMS");

    let bad_glob = server.search(&[("querystr", "x"), ("tardir", &tardir), ("globs", "[")]);
    assert_eq!(bad_glob.status, 400, "{}", bad_glob.body);

    let bad_regex_mode = server.search(&[
        ("querystr", "([unclosed"),
        ("tardir", &tardir),
        ("mode", "regex"),
    ]);
    assert_eq!(bad_regex_mode.status, 400, "{}", bad_regex_mode.body);
}

#[test]
fn total_counts_all_matches_not_the_page() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let value = server.search_json(&[("querystr", ""), ("tardir", &tardir), ("limit", "2")]);
    assert_eq!(value["results"].as_array().unwrap().len(), 2, "{value}");
    assert_eq!(value["total"].as_u64().unwrap(), 9, "{value}");
}

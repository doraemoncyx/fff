mod common;

use std::time::Duration;

use common::{TestServer, make_git_repo, test_config, write_tree};
use fff_mcp_http::Config;
use serde_json::{Value, json};

const MAIN_RS: &str = "use std::io;

fn compute_total(a: i32, b: i32) -> i32 {
    let total = a + b;
    total
}

fn report() {
    let value = compute_total(1, 2);
    println!(\"{value}\");
    audit(needle_alpha);
}
";

const NOTES_TXT: &str = "note one
needle_plain
note three
";

const LITERALS_TXT: &str = "a.b
axb
a-b
";

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_tree(
        root,
        &[
            "src/helper.rs",
            "docs/readme.md",
            "data/literals.txt",
            "plain/notes.txt",
            "many/needles.txt",
            ".svn/x.png",
            ".svn/note.txt",
        ],
    );
    std::fs::write(root.join("src/main.rs"), MAIN_RS).unwrap();
    std::fs::write(
        root.join("src/helper.rs"),
        "pub fn helper() {\n    needle_beta();\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("docs/readme.md"), "# docs\n\nneedle_docs\n").unwrap();
    std::fs::write(root.join("data/literals.txt"), LITERALS_TXT).unwrap();
    std::fs::write(root.join("plain/notes.txt"), NOTES_TXT).unwrap();
    let pages: String = (1..=8).map(|n| format!("needle_page_{n}\n")).collect();
    std::fs::write(root.join("many/needles.txt"), pages).unwrap();
    std::fs::write(root.join(".svn/x.png"), "needle_vcs hidden\n").unwrap();
    std::fs::write(root.join(".svn/note.txt"), "needle_vcs_text hidden\n").unwrap();
    make_git_repo(root);
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
            "clientInfo": { "name": "ticket-09", "version": "0.0.0" }
        }
    }));
    assert_eq!(response["result"]["serverInfo"]["name"], "fff-mcp-http");
}

fn call(server: &TestServer, tool: &str, arguments: Value) -> String {
    let response = server.mcp_tool(2, tool, arguments);
    assert!(response["error"].is_null(), "unexpected error: {response}");
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool returned no text: {response}"))
        .to_string()
}

fn grep(server: &TestServer, tardir: &str, extra: Value) -> String {
    let mut arguments = json!({ "tardir": tardir });
    merge(&mut arguments, extra);
    call(server, "grep", arguments)
}

fn merge(target: &mut Value, extra: Value) {
    let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) else {
        panic!("arguments must be objects");
    };
    target.extend(extra);
}

fn cursor_of(text: &str) -> Option<String> {
    text.rsplit("cursor: ")
        .next()
        .filter(|id| *id != text)
        .map(|id| id.trim().to_string())
}

#[test]
fn tools_list_includes_all_three_tools() {
    let server = TestServer::start(test_config());
    initialize(&server);

    let listing = server.mcp(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let tools = listing["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, vec!["find_files", "grep", "multi_grep"], "{listing}");

    let required = |name: &str| -> Vec<String> {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect()
    };
    assert!(required("grep").contains(&"tardir".to_string()));
    assert!(required("grep").contains(&"query".to_string()));
    assert!(required("multi_grep").contains(&"tardir".to_string()));
    assert!(required("multi_grep").contains(&"patterns".to_string()));

    let instructions = server
        .mcp(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "ticket-09", "version": "0.0.0" }
            }
        }))
        .to_string();
    assert!(instructions.contains("tardir"), "{instructions}");
    assert!(instructions.contains("grep"), "{instructions}");
}

#[test]
fn grep_finds_a_literal_needle_with_line_numbers() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let text = grep(
        &server,
        &tardir,
        json!({ "query": "needle_alpha", "context": 0 }),
    );
    assert!(text.contains("src/main.rs"), "{text}");
    let line = MAIN_RS
        .lines()
        .position(|line| line.contains("needle_alpha"))
        .expect("needle is in the fixture")
        + 1;
    assert!(
        text.lines()
            .any(|row| row.starts_with(&format!(" {line}: ")) && row.contains("needle_alpha")),
        "expected line {line} with a trailing colon: {text}"
    );

    let context = grep(
        &server,
        &tardir,
        json!({ "query": "needle_alpha", "context": 2 }),
    );
    assert!(
        context.lines().any(|row| row.starts_with(" 9- ")),
        "context must add the lines before the match: {context}"
    );
    assert!(
        context
            .lines()
            .any(|row| row.starts_with(&format!(" {line}: "))),
        "{context}"
    );
}

#[test]
fn output_modes_match_the_stdio_shape() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let files = grep(
        &server,
        &tardir,
        json!({ "query": "needle_alpha", "output_mode": "files_with_matches" }),
    );
    assert_eq!(files.lines().next(), Some("src/main.rs"), "{files}");
    assert!(files.contains("needle_alpha"), "{files}");

    let count = grep(
        &server,
        &tardir,
        json!({ "query": "needle_alpha", "output_mode": "count" }),
    );
    assert_eq!(count.trim(), "src/main.rs: 1", "{count}");

    let missing = grep(&server, &tardir, json!({ "query": "needle_absent_token" }));
    assert_eq!(missing.trim(), "0 matches.", "{missing}");
}

#[test]
fn modes_and_casing_follow_the_stdio_rules() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let alternation = grep(
        &server,
        &tardir,
        json!({ "query": "needle_alpha|needle_docs", "mode": "regex", "context": 0 }),
    );
    assert!(alternation.contains("src/main.rs"), "{alternation}");
    assert!(alternation.contains("docs/readme.md"), "{alternation}");

    let bad_regex = server.mcp_tool(
        4,
        "grep",
        json!({ "query": "([unclosed", "tardir": tardir, "mode": "regex" }),
    );
    assert_eq!(bad_regex["error"]["code"], -32602, "{bad_regex}");

    let literal = grep(
        &server,
        &tardir,
        json!({ "query": "a.b", "mode": "literal", "context": 0, "path": "data" }),
    );
    assert!(literal.contains("a.b"), "{literal}");
    assert!(!literal.contains("axb"), "{literal}");
    assert!(!literal.contains("a-b"), "{literal}");

    let insensitive = grep(
        &server,
        &tardir,
        json!({ "query": "NEEDLE_ALPHA", "case": "insensitive", "context": 0 }),
    );
    assert!(insensitive.contains("needle_alpha"), "{insensitive}");

    let smart = grep(
        &server,
        &tardir,
        json!({ "query": "NEEDLE_ALPHA", "context": 0 }),
    );
    assert!(
        smart.starts_with("0 exact matches") || smart.trim() == "0 matches.",
        "smart case must not report an exact match: {smart}"
    );
}

#[test]
fn cursor_pages_are_disjoint_and_end_without_a_cursor() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut arguments = json!({
            "query": "needle_page",
            "tardir": tardir,
            "maxResults": 3,
            "context": 0
        });
        if let Some(id) = &cursor {
            arguments["cursor"] = json!(id);
        }
        let text = call(&server, "grep", arguments);
        let lines: Vec<String> = text
            .lines()
            .filter(|line| line.starts_with(' ') && line.contains("needle_page_"))
            .map(|line| line.to_string())
            .collect();
        assert!(!lines.is_empty(), "page must not be empty: {text}");
        for line in &lines {
            assert!(!seen.contains(line), "pages overlap: {line} in {seen:?}");
            seen.push(line.clone());
        }
        pages += 1;
        cursor = cursor_of(&text);
        if cursor.is_none() {
            break;
        }
        assert!(pages < 10, "paging did not end: {seen:?}");
    }

    assert_eq!(seen.len(), 8, "{seen:?}");
    assert_eq!(pages, 3, "{seen:?}");
}

#[test]
fn definitions_only_reports_the_documented_fallbacks() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let others = grep(
        &server,
        &tardir,
        json!({ "query": "needle_plain", "definitions_only": true }),
    );
    assert!(
        others.contains("No matching definitions found. Showing other matches:"),
        "{others}"
    );

    let nothing = grep(
        &server,
        &tardir,
        json!({ "query": "zzz_missing_token", "definitions_only": true }),
    );
    assert_eq!(
        nothing.trim(),
        "No matching definitions or other matches found.",
        "{nothing}"
    );
}

#[test]
fn multi_grep_is_an_or_and_constraints_narrow_it() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let both = call(
        &server,
        "multi_grep",
        json!({ "patterns": ["needle_alpha", "needle_docs"], "tardir": tardir }),
    );
    assert!(both.contains("src/main.rs"), "{both}");
    assert!(both.contains("docs/readme.md"), "{both}");

    let narrowed = call(
        &server,
        "multi_grep",
        json!({
            "patterns": ["needle_alpha", "needle_docs"],
            "tardir": tardir,
            "constraints": "src/"
        }),
    );
    assert!(narrowed.contains("src/main.rs"), "{narrowed}");
    assert!(!narrowed.contains("docs/readme.md"), "{narrowed}");
}

#[test]
fn a_child_tardir_searches_only_its_own_subtree() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["parent/child/a.txt", "parent/sibling/b.txt"]);
    std::fs::write(dir.path().join("parent/child/a.txt"), "needle_child\n").unwrap();
    std::fs::write(dir.path().join("parent/sibling/b.txt"), "needle_child\n").unwrap();
    let parent = dir.path().join("parent");
    let server = TestServer::start(test_config());
    initialize(&server);

    let all = grep(
        &server,
        &parent.to_string_lossy(),
        json!({ "query": "needle_child" }),
    );
    assert!(all.contains("child/a.txt"), "{all}");
    assert!(all.contains("sibling/b.txt"), "{all}");
    assert_eq!(server.stats()["indexes"].as_array().unwrap().len(), 1);

    let child = grep(
        &server,
        &parent.join("child").to_string_lossy(),
        json!({ "query": "needle_child" }),
    );
    assert!(child.contains("child/a.txt"), "{child}");
    assert!(!child.contains("sibling"), "sibling leaked: {child}");
    assert_eq!(
        server.stats()["indexes"].as_array().unwrap().len(),
        1,
        "a covered tardir must not create a second Index"
    );
}

#[test]
fn an_invalid_tardir_is_a_parameter_error() {
    let dir = tree();
    let server = TestServer::start(test_config());
    initialize(&server);
    let file = dir.path().join("src/main.rs").to_string_lossy().to_string();

    let tardirs = [
        Value::Null,
        json!("   "),
        json!("C:/fff-no-such-tardir-xyz"),
        json!(file),
    ];
    for (index, tardir) in tardirs.into_iter().enumerate() {
        let mut arguments = json!({ "query": "needle" });
        if !tardir.is_null() {
            arguments["tardir"] = tardir.clone();
        }
        let response = server.mcp_tool(10 + index as u64, "grep", arguments.clone());
        assert_eq!(response["error"]["code"], -32602, "{arguments}: {response}");

        let mut arguments = json!({ "patterns": ["needle"] });
        if !tardir.is_null() {
            arguments["tardir"] = tardir;
        }
        let response = server.mcp_tool(20 + index as u64, "multi_grep", arguments);
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }

    let missing_query = server.mcp_tool(
        30,
        "grep",
        json!({ "tardir": dir.path().to_string_lossy() }),
    );
    assert_eq!(missing_query["error"]["code"], -32602, "{missing_query}");

    let outside = server.mcp_tool(
        31,
        "grep",
        json!({
            "query": "needle",
            "tardir": dir.path().to_string_lossy(),
            "path": "../escape"
        }),
    );
    assert_eq!(outside["error"]["code"], -32602, "{outside}");
}

#[test]
fn a_zero_scan_wait_answers_partially_and_then_completes() {
    let dir = tempfile::tempdir().unwrap();
    let mut files: Vec<String> = (0..1200).map(|n| format!("bulk/f{n}.txt")).collect();
    files.push("bulk/target.txt".into());
    let borrowed: Vec<&str> = files.iter().map(String::as_str).collect();
    write_tree(dir.path(), &borrowed);
    std::fs::write(dir.path().join("bulk/target.txt"), "needle_big\n").unwrap();
    let tardir = dir.path().to_string_lossy().to_string();

    let config = Config {
        scan_wait: Duration::ZERO,
        ..test_config()
    };
    let server = TestServer::start(config);
    initialize(&server);

    let first = grep(&server, &tardir, json!({ "query": "needle_big" }));
    assert!(first.contains("partial"), "{first}");

    let mut scanning = true;
    for _ in 0..400 {
        let response = server.search_json(&[("querystr", ""), ("tardir", &tardir)]);
        scanning = response["scanning"].as_bool().unwrap_or(false);
        if !scanning {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!scanning, "the scan never finished");

    let second = grep(&server, &tardir, json!({ "query": "needle_big" }));
    assert!(!second.contains("partial"), "{second}");
    assert!(second.contains("target.txt"), "{second}");
    assert!(second.contains("needle_big"), "{second}");
}

#[test]
fn http_routes_answer_with_text_and_honour_the_token() {
    let dir = tree();
    let tardir = dir.path().to_string_lossy().to_string();
    let params = [
        ("query", "needle_alpha"),
        ("tardir", tardir.as_str()),
        ("context", "0"),
    ];
    let patterns = [
        ("patterns", "needle_alpha"),
        ("patterns", "needle_docs"),
        ("tardir", tardir.as_str()),
    ];

    let open = TestServer::start(test_config());
    let response = open.get(&format!("/grep?{}", common::encode(&params)));
    assert_eq!(response.status, 200, "{}", response.body);
    let body: Value = serde_json::from_str(&response.body).unwrap();
    assert!(
        body["text"].as_str().unwrap().contains("needle_alpha"),
        "{body}"
    );
    assert!(body["scanning"].is_boolean(), "{body}");
    assert!(body["scanned"].as_u64().unwrap() > 0, "{body}");

    let response = open.get(&format!("/multi_grep?{}", common::encode(&patterns)));
    assert_eq!(response.status, 200, "{}", response.body);
    let body: Value = serde_json::from_str(&response.body).unwrap();
    let text = body["text"].as_str().unwrap();
    assert!(text.contains("src/main.rs"), "{body}");
    assert!(text.contains("docs/readme.md"), "{body}");

    let guarded = TestServer::start(Config {
        token: Some("s3cret-token".into()),
        ..test_config()
    });
    let auth = [("Authorization", "Bearer s3cret-token")];
    let grep_target = format!("/grep?{}", common::encode(&params));
    let multi_target = format!("/multi_grep?{}", common::encode(&patterns));
    assert_eq!(guarded.get(&grep_target).status, 401);
    assert_eq!(guarded.get(&multi_target).status, 401);
    assert_eq!(guarded.get_with(&grep_target, &auth).status, 200);
    assert_eq!(guarded.get_with(&multi_target, &auth).status, 200);
}

#[test]
fn skip_rules_apply_to_content_search() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();
    initialize(&server);

    let hidden = grep(&server, &tardir, json!({ "query": "needle_vcs_text" }));
    assert!(!hidden.contains(".svn"), "skipped dir leaked: {hidden}");

    let skipped_png = grep(&server, &tardir, json!({ "query": "needle_vcs" }));
    assert!(
        !skipped_png.contains(".svn"),
        "skipped dir leaked: {skipped_png}"
    );

    let unskipped = TestServer::start(Config {
        default_skips: false,
        ..test_config()
    });
    let found = grep(&unskipped, &tardir, json!({ "query": "needle_vcs_text" }));
    assert!(found.contains(".svn/note.txt"), "{found}");
}

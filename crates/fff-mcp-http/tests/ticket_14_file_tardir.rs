mod common;

use std::path::Path;
use std::time::Duration;

use common::{TestServer, make_git_repo, test_config, write_tree};
use fff_mcp_http::Config;
use serde_json::{Value, json};

const MAIN_RS: &str = "fn main() {\n    let needle_target = 1;\n}\n";
const SIBLING_RS: &str = "fn sibling() {\n    let needle_target = 2;\n}\n";

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["src/main.rs", "src/sibling.rs"]);
    std::fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();
    std::fs::write(dir.path().join("src/sibling.rs"), SIBLING_RS).unwrap();
    make_git_repo(dir.path());
    dir
}

fn merge(target: &mut Value, extra: Value) {
    let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) else {
        panic!("arguments must be objects");
    };
    target.extend(extra);
}

fn call(server: &TestServer, tool: &str, arguments: Value) -> Value {
    let response = server.mcp_tool(2, tool, arguments);
    assert!(response["error"].is_null(), "unexpected error: {response}");
    response["result"].clone()
}

fn grep(server: &TestServer, tardir: &str, extra: Value) -> String {
    let mut arguments = json!({ "tardir": tardir });
    merge(&mut arguments, extra);
    let result = call(server, "grep", arguments);
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("grep returned no text: {result}"))
        .to_string()
}

fn find(server: &TestServer, tardir: &str, extra: Value) -> Value {
    let mut arguments = json!({ "tardir": tardir, "querystr": "" });
    merge(&mut arguments, extra);
    call(server, "find_files", arguments)["structuredContent"].clone()
}

#[test]
fn grep_reads_only_the_named_file() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let file = path_of(&dir.path().join("src/main.rs"));

    let text = grep(
        &server,
        &file,
        json!({ "query": "needle_target", "context": 0 }),
    );
    assert!(text.contains("main.rs"), "{text}");
    assert!(text.contains("needle_target"), "{text}");
    assert!(!text.contains("sibling"), "sibling leaked: {text}");
}

#[test]
fn multi_grep_reads_only_the_named_file() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let file = path_of(&dir.path().join("src/main.rs"));

    let result = call(
        &server,
        "multi_grep",
        json!({ "patterns": ["needle_target"], "tardir": file }),
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("main.rs"), "{text}");
    assert!(!text.contains("sibling"), "sibling leaked: {text}");
}

#[test]
fn find_files_answers_for_the_named_file_alone() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let target = dir.path().join("src/main.rs");

    let listed = find(&server, &path_of(&target), json!({ "maxResults": 10 }));
    assert_eq!(listed["total"], 1, "{listed}");
    let results = listed["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{listed}");
    assert_eq!(results[0]["rel"], "main.rs", "{listed}");
    assert_eq!(
        Path::new(results[0]["path"].as_str().unwrap()),
        dunce::canonicalize(&target).unwrap(),
        "{listed}"
    );

    let miss = find(
        &server,
        &path_of(&target),
        json!({ "querystr": "zzz_absent" }),
    );
    assert_eq!(miss["total"], 0, "{miss}");
    assert_eq!(miss["results"].as_array().unwrap().len(), 0, "{miss}");
}

#[test]
fn naming_the_file_in_the_scope_is_a_no_op() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let file = path_of(&dir.path().join("src/main.rs"));

    let plain = grep(
        &server,
        &file,
        json!({ "query": "needle_target", "context": 0 }),
    );
    let named = grep(
        &server,
        &file,
        json!({ "query": "needle_target", "context": 0, "path": "main.rs" }),
    );
    assert_eq!(plain, named);
    let listed = grep(
        &server,
        &file,
        json!({ "query": "needle_target", "context": 0, "paths": ["main.rs"] }),
    );
    assert_eq!(plain, listed);

    let searched = find(&server, &file, json!({ "path": "main.rs" }));
    assert_eq!(searched["total"], 1, "{searched}");
    assert_eq!(searched["results"][0]["rel"], "main.rs", "{searched}");
}

#[test]
fn a_scope_pointing_elsewhere_is_empty() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let file = path_of(&dir.path().join("src/main.rs"));

    let text = grep(
        &server,
        &file,
        json!({ "query": "needle_target", "path": "sibling.rs" }),
    );
    assert!(
        text.starts_with("Path 'sibling.rs' matches no indexed file or directory."),
        "{text}"
    );
    assert!(
        text.contains("No matches in the requested scope."),
        "{text}"
    );
}

#[test]
fn a_covered_file_reuses_its_parent_index() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let root = path_of(dir.path());

    let all = grep(&server, &root, json!({ "query": "needle_target" }));
    assert!(all.contains("main.rs"), "{all}");
    assert!(all.contains("sibling.rs"), "{all}");
    let before = server.stats();
    assert_eq!(before["scans_started"], 1, "{before}");

    let file = path_of(&dir.path().join("src/main.rs"));
    let only = grep(&server, &file, json!({ "query": "needle_target" }));
    assert!(only.contains("main.rs"), "{only}");
    assert!(!only.contains("sibling"), "{only}");

    let after = server.stats();
    assert_eq!(after["indexes"].as_array().unwrap().len(), 1, "{after}");
    assert_eq!(after["scans_started"], 1, "a covered file must not rescan");
}

#[test]
fn an_uncovered_file_indexes_its_parent_directory() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let file = path_of(&dir.path().join("src/main.rs"));

    let text = grep(&server, &file, json!({ "query": "needle_target" }));
    assert!(text.contains("needle_target"), "{text}");

    let stats = server.stats();
    let indexes = stats["indexes"].as_array().unwrap();
    assert_eq!(indexes.len(), 1, "{stats}");
    assert_eq!(
        Path::new(indexes[0]["root"].as_str().unwrap()),
        dunce::canonicalize(dir.path().join("src")).unwrap(),
        "the Root must stay the parent directory: {stats}"
    );
}

#[test]
fn an_ignored_file_says_it_is_not_in_the_index() {
    let dir = tempfile::tempdir().unwrap();
    make_git_repo(dir.path());
    write_tree(dir.path(), &["secret.txt", "visible.txt"]);
    std::fs::write(dir.path().join(".ignore"), "secret.txt\n").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "needle_secret\n").unwrap();
    let server = TestServer::start(test_config());

    let text = grep(
        &server,
        &path_of(&dir.path().join("secret.txt")),
        json!({ "query": "needle_secret" }),
    );
    assert!(text.contains("not in the Index"), "{text}");
    assert!(text.contains("0 matches."), "{text}");
}

#[test]
fn a_binary_file_says_it_is_not_in_the_index() {
    let dir = tempfile::tempdir().unwrap();
    make_git_repo(dir.path());
    write_tree(dir.path(), &["blob.bin", "visible.txt"]);
    std::fs::write(dir.path().join("blob.bin"), b"\x00\x01needle_target").unwrap();
    let server = TestServer::start(test_config());

    let text = grep(
        &server,
        &path_of(&dir.path().join("blob.bin")),
        json!({ "query": "needle_target" }),
    );
    assert!(text.contains("not in the Index"), "{text}");
    assert!(text.contains("0 matches."), "{text}");
}

#[test]
fn a_scanning_file_target_reports_partial_not_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut files: Vec<String> = (0..1200).map(|n| format!("bulk/f{n}.txt")).collect();
    files.push("bulk/target.txt".into());
    let borrowed: Vec<&str> = files.iter().map(String::as_str).collect();
    write_tree(dir.path(), &borrowed);
    std::fs::write(dir.path().join("bulk/target.txt"), "needle_big\n").unwrap();
    let server = TestServer::start(Config {
        scan_wait: Duration::ZERO,
        ..test_config()
    });

    let text = grep(
        &server,
        &path_of(&dir.path().join("bulk/target.txt")),
        json!({ "query": "needle_big" }),
    );
    assert!(text.contains("partial"), "{text}");
    assert!(!text.contains("not in the Index"), "{text}");
}

#[cfg(windows)]
#[test]
fn a_trailing_separator_on_a_file_names_the_separator() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let file = format!("{}\\", path_of(&dir.path().join("src/main.rs")));

    let response = server.mcp_tool(3, "grep", json!({ "query": "needle", "tardir": file }));
    assert_eq!(response["error"]["code"], -32602, "{response}");
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.contains("separator"), "{message}");
    assert!(!message.contains("does not exist"), "{message}");
}

#[test]
fn http_routes_accept_a_file_tardir() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let file = path_of(&dir.path().join("src/main.rs"));

    let grep = [
        ("query", "needle_target"),
        ("tardir", file.as_str()),
        ("context", "0"),
    ];
    let response = server.get(&format!("/grep?{}", common::encode(&grep)));
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(response.body.contains("main.rs"), "{}", response.body);
    assert!(!response.body.contains("sibling"), "{}", response.body);

    let multi = [("patterns", "needle_target"), ("tardir", file.as_str())];
    let response = server.get(&format!("/multi_grep?{}", common::encode(&multi)));
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(response.body.contains("main.rs"), "{}", response.body);

    let search = server.search_json(&[
        ("querystr", ""),
        ("tardir", file.as_str()),
        ("maxResults", "10"),
    ]);
    assert_eq!(search["total"], 1, "{search}");
}

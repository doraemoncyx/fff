mod common;

use common::Session;
use serde_json::{Value, json};

const IDENTIFIER_KEYS: [&str; 3] = ["query", "pattern", "querystr"];
const MAIN_RS: &str = "fn main() {
    let needle_alpha = 1;
    println!(\"{needle_alpha}\");
}
";
const README: &str = "# docs\n\nnothing to see\n";

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();
    std::fs::write(dir.path().join("docs/readme.md"), README).unwrap();
    dir
}

// The advertised schema is the only thing a client validates against, so it
// must name every key the handler deserializes (#311).
#[test]
fn tools_list_advertises_every_identifier_key() {
    let dir = tree();
    let mut session = Session::start(dir.path(), &[]);
    let listing = session.call("tools/list", json!({}));

    for name in ["grep", "find_files"] {
        let schema = tool_schema(&listing, name);
        for key in IDENTIFIER_KEYS {
            assert_eq!(
                schema["properties"][key]["type"], "string",
                "{name}: {schema}"
            );
            assert!(
                schema["properties"][key]["description"].is_string(),
                "{name}: {schema}"
            );
        }
        assert_eq!(
            required_keys(&schema),
            Vec::<&str>::new(),
            "{name}: {schema}"
        );
        assert!(schema.get("anyOf").is_none(), "{name}: {schema}");
    }
}

#[test]
fn every_advertised_key_is_accepted_by_tools_call() {
    let dir = tree();
    let mut session = Session::start(dir.path(), &[]);

    for key in IDENTIFIER_KEYS {
        let response = session.tool(
            "grep",
            json!({ key: "needle_alpha", "mode": "literal", "context": 0 }),
        );
        let text = tool_text(&response, key);
        assert!(text.contains("src/main.rs"), "{key}: {text}");
    }

    for key in IDENTIFIER_KEYS {
        let response = session.tool("find_files", json!({ key: "main" }));
        let text = tool_text(&response, key);
        assert!(text.contains("src/main.rs"), "{key}: {text}");
    }
}

// Clients send only the keys they have; an omitted identifier means an empty one.
#[test]
fn a_call_without_any_identifier_key_lists_everything() {
    let dir = tree();
    let mut session = Session::start(dir.path(), &[]);

    let response = session.tool("find_files", json!({}));
    let text = tool_text(&response, "find_files");
    assert!(
        text.contains("src/main.rs") && text.contains("docs/readme.md"),
        "{text}"
    );

    let omitted = session.tool("grep", json!({}));
    let empty = session.tool("grep", json!({ "query": "" }));
    assert_eq!(
        tool_text(&omitted, "omitted"),
        tool_text(&empty, "empty"),
        "{omitted}"
    );
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

fn tool_text(response: &Value, label: &str) -> String {
    assert!(response["error"].is_null(), "{label}: {response}");
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{label}: no text in {response}"))
        .to_string()
}

fn required_keys(schema: &Value) -> Vec<&str> {
    schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| key.as_str())
        .collect()
}

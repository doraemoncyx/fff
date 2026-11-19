mod common;

use std::path::Path;

use common::{TestServer, encode, test_config};
use fff::binary_verdict::BINARY_EXTENSIONS;
use fff_mcp_http::Config;
use serde_json::Value;

const MARKER: &str = "zzmarkerq";

// The same bytes in every file: only the extension can tell them apart.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "art/wall.mtl",
        "art/mesh.atlas",
        "art/twin.mtq",
        "art/photo.png",
        "src/main.rs",
    ] {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{MARKER}\n")).unwrap();
    }
    dir
}

// No skip rules: a ripgrep-only build refuses them, and this slice needs none.
fn base() -> Config {
    Config {
        default_skips: false,
        ..test_config()
    }
}

fn configured() -> Config {
    Config {
        extra_binary_extensions: vec!["mtl".into(), ".ATLAS".into()],
        ..base()
    }
}

fn grep_file(server: &TestServer, file: &Path) -> String {
    let target = file.to_string_lossy().into_owned();
    let query = encode(&[("query", MARKER), ("tardir", target.as_str())]);
    let response = server.get(&format!("/grep?{query}"));
    assert_eq!(response.status, 200, "{}", response.body);
    let body: Value = serde_json::from_str(&response.body).expect("grep returns JSON");
    body["text"].as_str().expect("text").to_string()
}

#[test]
fn a_configured_extension_is_binary_by_name() {
    let tree = tree();
    let server = TestServer::start(configured());

    // The control: identical bytes under an extension nobody configured.
    let twin = grep_file(&server, &tree.path().join("art/twin.mtq"));
    assert!(twin.contains(MARKER), "{twin}");
    assert!(!twin.contains("not in the Index"), "{twin}");

    // Configured extensions answer from the name, so the bytes stay unread.
    for name in ["art/wall.mtl", "art/mesh.atlas"] {
        let text = grep_file(&server, &tree.path().join(name));
        let file = name.rsplit_once('/').unwrap().1;
        assert!(
            text.contains(&format!("{file} is not in the Index")),
            "{name}: {text}"
        );
    }

    // Additive: the built-in table still answers, and text is still searchable.
    let png = grep_file(&server, &tree.path().join("art/photo.png"));
    assert!(png.contains("photo.png is not in the Index"), "{png}");
    let source = grep_file(&server, &tree.path().join("src/main.rs"));
    assert!(source.contains(MARKER), "{source}");
}

// The two numbers of the startup log, from the same call site. Both tests use
// the same additions, so this one cannot poison the first through the
// process-wide table that keeps its first value.
#[test]
fn the_merged_table_is_the_builtin_table_plus_the_additions() {
    let merged = configured()
        .apply_binary_extensions()
        .expect("valid extensions");
    assert_eq!(merged, BINARY_EXTENSIONS.len() + 2);
    assert_eq!(
        configured().apply_binary_extensions().expect("asked twice"),
        merged
    );

    // No list removes a built-in: the empty one leaves the table as it is.
    let empty = Config {
        extra_binary_extensions: Vec::new(),
        ..base()
    };
    assert_eq!(empty.apply_binary_extensions().expect("empty list"), merged);
}

#[test]
fn a_bad_extension_fails_the_startup_and_names_the_field() {
    for (value, reason) in [
        ("tar.gz", "not a bare file extension"),
        ("rs", "text table"),
    ] {
        let config = Config {
            extra_binary_extensions: vec![value.into()],
            ..base()
        };
        let error = match fff_mcp_http::spawn(config) {
            Ok(_) => panic!("{value} must fail the startup"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("extra_binary_extensions"), "{error}");
        assert!(error.contains(value), "{error}");
        assert!(error.contains(reason), "{error}");
    }
}

mod common;

use common::{TestServer, test_config, write_tree};

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(
        dir.path(),
        &[
            "src/main.rs",
            "src/lib.rs",
            "src/util.rs",
            "docs/readme.md",
            "images/logo.png",
            "images/icon.png",
            "data/sample.txt",
            "test_unit.py",
        ],
    );
    dir
}

#[test]
fn finds_files_with_absolute_and_relative_paths() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let value = server.search_json(&[("querystr", "main"), ("tardir", &tardir)]);
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{value}");
    assert_eq!(results[0]["rel"], "src/main.rs", "{value}");
    assert_eq!(results[0]["is_dir"], false);

    let path = std::path::Path::new(results[0]["path"].as_str().unwrap());
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(
        path,
        dunce::canonicalize(dir.path().join("src/main.rs")).unwrap()
    );

    assert_eq!(value["merged"], false);
    assert_eq!(value["scanning"], false);
    assert_eq!(value["served_by"].as_array().unwrap().len(), 1);
    assert!(value["total"].as_u64().unwrap() >= 1);
    assert!(value["scanned"].as_u64().unwrap() >= 8);
}

#[test]
fn querystr_syntax_covers_extension_segments_and_paging() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    let extensions = server.search_json(&[("querystr", "*.png"), ("tardir", &tardir)]);
    let rels: Vec<&str> = extensions["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert_eq!(rels.len(), 2, "{extensions}");
    assert!(rels.iter().all(|rel| rel.ends_with(".png")), "{rels:?}");

    let segment = server.search_json(&[("querystr", "/images/"), ("tardir", &tardir)]);
    let rels: Vec<&str> = segment["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["rel"].as_str())
        .collect();
    assert_eq!(rels.len(), 2, "{segment}");
    assert!(
        rels.iter().all(|rel| rel.starts_with("images/")),
        "{rels:?}"
    );

    let first = server.search_json(&[("querystr", ""), ("tardir", &tardir), ("limit", "3")]);
    let first_page: Vec<String> = first["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["rel"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(first_page.len(), 3, "{first}");
    assert!(first["total"].as_u64().unwrap() >= 8);

    let second = server.search_json(&[
        ("querystr", ""),
        ("tardir", &tardir),
        ("limit", "3"),
        ("offset", "3"),
    ]);
    let second_page: Vec<String> = second["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["rel"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(second_page.len(), 3, "{second}");
    assert!(first_page.iter().all(|rel| !second_page.contains(rel)));
}

#[test]
fn missing_tardir_is_a_parameter_error_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");
    let server = TestServer::start(test_config());

    let response = server.search(&[
        ("querystr", "x"),
        ("tardir", missing.to_string_lossy().as_ref()),
    ]);
    assert_eq!(response.status, 400, "{}", response.body);
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["error"], "INVALID_PARAMS");
    assert!(!missing.exists(), "tardir must not be created");
}

#[test]
fn second_request_reuses_the_same_index() {
    let dir = tree();
    let server = TestServer::start(test_config());
    let tardir = dir.path().to_string_lossy().to_string();

    server.search_json(&[("querystr", "main"), ("tardir", &tardir)]);
    let stats = server.stats();
    assert_eq!(stats["indexes"].as_array().unwrap().len(), 1, "{stats}");
    assert_eq!(stats["indexes"][0]["scanning"], false, "{stats}");

    server.search_json(&[("querystr", "lib"), ("tardir", &tardir)]);
    let stats = server.stats();
    assert_eq!(stats["indexes"].as_array().unwrap().len(), 1, "{stats}");
    assert_eq!(stats["indexes"][0]["scanning"], false, "{stats}");
}

#[test]
fn healthz_is_unauthenticated_and_reports_version() {
    let server = TestServer::start(test_config());
    let response = server.get("/healthz");
    assert_eq!(response.status, 200);
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["status"], "ok");
    assert!(
        body["version"]
            .as_str()
            .unwrap()
            .contains(env!("CARGO_PKG_VERSION"))
    );
}

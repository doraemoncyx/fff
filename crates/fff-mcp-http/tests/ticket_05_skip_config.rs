mod common;

use std::path::Path;
use std::process::Command;

use clap::Parser;
use common::{TestServer, make_git_repo, test_config, write_tree};
use fff_mcp_http::{Args, Config, parse_memory};
use serde_json::Value;

const ALL: (&str, &str) = ("querystr", "");

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn search(server: &TestServer, tardir: &str) -> Value {
    server.search_json(&[ALL, ("tardir", tardir), ("maxResults", "100")])
}

fn rels(response: &Value) -> Vec<String> {
    let mut rels: Vec<String> = response["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {response}"))
        .iter()
        .map(|hit| hit["rel"].as_str().expect("rel").to_string())
        .collect();
    rels.sort();
    rels
}

fn assert_rels(response: &Value, expected: &[&str]) {
    let mut expected: Vec<String> = expected.iter().map(|rel| rel.to_string()).collect();
    expected.sort();
    assert_eq!(rels(response), expected, "{response}");
}

fn vcs_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    make_git_repo(dir.path());
    write_tree(
        dir.path(),
        &[
            ".svn/x.png",
            ".hg/x.png",
            "CVS/x.png",
            "data/keep.png",
            "data/keep.txt",
        ],
    );
    dir
}

#[test]
fn default_skips_hide_vcs_backup_dirs() {
    let dir = vcs_tree();
    let server = TestServer::start(test_config());
    let response = search(&server, &path_of(dir.path()));
    assert_rels(&response, &["data/keep.png", "data/keep.txt"]);
}

#[test]
fn no_default_skips_reveals_vcs_backup_dirs() {
    let dir = vcs_tree();
    let config = Config {
        default_skips: false,
        ..test_config()
    };
    let server = TestServer::start(config);
    let response = search(&server, &path_of(dir.path()));
    assert_rels(
        &response,
        &[
            "CVS/x.png",
            ".hg/x.png",
            ".svn/x.png",
            "data/keep.png",
            "data/keep.txt",
        ],
    );
}

#[test]
fn skip_patterns_are_gitignore_shaped() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(
        dir.path(),
        &[
            "cache/deep/blob.dat",
            "cache.txt",
            "cache_extra/keep.txt",
            "archive/old.bak",
            "src/main.rs",
        ],
    );
    let config = Config {
        skip: vec!["*.bak".into(), "cache/".into()],
        ..test_config()
    };
    let server = TestServer::start(config);
    let response = search(&server, &path_of(dir.path()));
    assert_rels(
        &response,
        &["cache.txt", "cache_extra/keep.txt", "src/main.rs"],
    );
}

fn config_args(path: &Path, extra: &[&str]) -> Args {
    let mut argv: Vec<String> = vec![
        "fff-mcp-http".into(),
        "--config".into(),
        path_of(path),
        "--port".into(),
        "0".into(),
        "--max-memory".into(),
        "128MB".into(),
        "--skip".into(),
        "*.tmp".into(),
    ];
    argv.extend(extra.iter().map(|arg| arg.to_string()));
    Args::parse_from(argv)
}

#[test]
fn config_file_merges_skip_and_cli_overrides_scalars() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["a.log", "b.tmp", "keep/c.txt"]);
    let config_home = tempfile::tempdir().unwrap();
    let config_path = config_home.path().join("settings.toml");
    std::fs::write(
        &config_path,
        "skip = [\"*.log\"]\nmax_memory = \"64MB\"\nport = 65000\n",
    )
    .unwrap();

    let config = config_args(&config_path, &[]).into_config().unwrap();
    assert_eq!(config.skip, vec!["*.log".to_string(), "*.tmp".to_string()]);
    assert_eq!(config.port, 0);
    assert_eq!(config.max_memory_bytes, parse_memory("128MB").unwrap());

    let server = TestServer::start(config);
    let response = search(&server, &path_of(dir.path()));
    assert_rels(&response, &["keep/c.txt"]);
    assert_eq!(
        server.stats()["budget_bytes"],
        parse_memory("128MB").unwrap(),
        "the CLI budget must win"
    );
}

#[test]
fn config_file_alone_is_used_without_cli_flags() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["a.log", "keep/c.txt"]);
    let config_home = tempfile::tempdir().unwrap();
    let config_path = config_home.path().join("settings.toml");
    std::fs::write(
        &config_path,
        "skip = [\"*.log\"]\nport = 0\ndefault_skips = false\n",
    )
    .unwrap();

    let config = Args::parse_from(["fff-mcp-http", "--config", &path_of(&config_path)])
        .into_config()
        .unwrap();
    assert!(!config.default_skips);
    assert_eq!(config.port, 0);

    let server = TestServer::start(config);
    let response = search(&server, &path_of(dir.path()));
    assert_rels(&response, &["keep/c.txt"]);
}

#[test]
fn no_git_reveals_gitignored_files() {
    let dir = tempfile::tempdir().unwrap();
    make_git_repo(dir.path());
    write_tree(dir.path(), &["secret.txt", "visible.txt"]);
    std::fs::write(dir.path().join(".gitignore"), "secret.txt\n").unwrap();
    let tardir = path_of(dir.path());

    let respecting = TestServer::start(test_config());
    let response = search(&respecting, &tardir);
    assert!(
        !rels(&response).contains(&"secret.txt".to_string()),
        "{response}"
    );
    assert!(
        rels(&response).contains(&"visible.txt".to_string()),
        "{response}"
    );

    let config = Config {
        disable_git: true,
        ..test_config()
    };
    let ignoring = TestServer::start(config);
    let response = search(&ignoring, &tardir);
    assert!(
        rels(&response).contains(&"secret.txt".to_string()),
        "{response}"
    );
}

#[test]
fn no_ignore_files_reveals_dot_ignore_excluded_files() {
    let dir = tempfile::tempdir().unwrap();
    make_git_repo(dir.path());
    write_tree(dir.path(), &["ignored.txt", "kept.txt"]);
    std::fs::write(dir.path().join(".ignore"), "ignored.txt\n").unwrap();
    let tardir = path_of(dir.path());

    let respecting = TestServer::start(test_config());
    let response = search(&respecting, &tardir);
    assert!(
        !rels(&response).contains(&"ignored.txt".to_string()),
        "{response}"
    );
    assert!(
        rels(&response).contains(&"kept.txt".to_string()),
        "{response}"
    );

    let config = Config {
        disable_ignore_files: true,
        ..test_config()
    };
    let ignoring = TestServer::start(config);
    let response = search(&ignoring, &tardir);
    assert!(
        rels(&response).contains(&"ignored.txt".to_string()),
        "{response}"
    );
}

fn run_binary(config_path: &Path, log_dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fff-mcp-http"))
        .arg("--config")
        .arg(config_path)
        .arg("--log-file")
        .arg(log_dir.join("run.log"))
        .output()
        .expect("binary runs")
}

#[test]
fn missing_and_malformed_config_files_fail_readably() {
    let dir = tempfile::tempdir().unwrap();

    let missing = dir.path().join("absent.toml");
    let output = run_binary(&missing, dir.path());
    assert!(!output.status.success(), "missing config must not start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("absent.toml"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");

    let malformed = dir.path().join("broken.toml");
    std::fs::write(&malformed, "skip = [\n").unwrap();
    let output = run_binary(&malformed, dir.path());
    assert!(!output.status.success(), "malformed config must not start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("broken.toml"), "{stderr}");
    assert!(stderr.contains("config"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

// The crate's own feature set decides: a ripgrep-only build must refuse to
// start with skip rules rather than silently dropping them.
#[cfg(not(feature = "zlob"))]
#[test]
fn skip_rules_refuse_to_start_without_the_zlob_walker() {
    let error = fff_mcp_http::spawn(test_config())
        .err()
        .expect("ripgrep-only build must refuse skip rules");
    assert!(error.to_string().contains("zlob"), "{error}");
}

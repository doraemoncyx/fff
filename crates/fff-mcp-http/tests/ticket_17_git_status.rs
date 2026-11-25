mod common;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use common::{TestServer, encode, make_git_repo, test_config, write_tree};
use fff_mcp_http::Config;
use serde_json::Value;

const ALL: (&str, &str) = ("querystr", "");
const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp-http");
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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

fn search(server: &TestServer, tardir: &str) -> Value {
    server.search_json(&[ALL, ("tardir", tardir), ("maxResults", "100")])
}

fn commit_files(root: &Path, names: &[&str]) {
    let repo = git2::Repository::open(root).expect("open repo");
    let mut index = repo.index().expect("index");
    for name in names {
        index.add_path(Path::new(name)).expect("add path");
    }
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("tree");
    let signature = git2::Signature::now("test", "test@example.com").expect("signature");
    let parent = repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|id| repo.find_commit(id).ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "test commit",
        &tree,
        &parents,
    )
    .expect("commit");
}

fn stage(root: &Path, name: &str) {
    let repo = git2::Repository::open(root).expect("open repo");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new(name)).expect("add path");
    index.write().expect("write index");
}

fn repo_with_dirty_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    make_git_repo(dir.path());
    write_tree(dir.path(), &["clean.txt", "dirty.txt", "untracked.txt"]);
    commit_files(dir.path(), &["clean.txt", "dirty.txt"]);
    std::fs::write(dir.path().join("dirty.txt"), "edited after the commit").unwrap();
    dir
}

// Ignore rules are `disable_git`'s business: the status switch must not touch them.
#[test]
fn gitignore_and_untracked_files_are_unaffected_by_the_status_switch() {
    let dir = tempfile::tempdir().unwrap();
    make_git_repo(dir.path());
    write_tree(dir.path(), &["secret.txt", "untracked.txt"]);
    std::fs::write(dir.path().join(".gitignore"), "secret.txt\n").unwrap();
    let tardir = path_of(dir.path());

    let server = TestServer::start(test_config());
    assert!(!test_config().git_status, "git status is off by default");
    let off_rels = rels(&search(&server, &tardir));
    assert!(
        !off_rels.contains(&"secret.txt".to_string()),
        "{off_rels:?}"
    );
    assert!(
        off_rels.contains(&"untracked.txt".to_string()),
        "{off_rels:?}"
    );

    let with_status = Config {
        git_status: true,
        ..test_config()
    };
    let server = TestServer::start(with_status);
    let on_rels = rels(&search(&server, &tardir));
    assert!(!on_rels.contains(&"secret.txt".to_string()), "{on_rels:?}");
    assert!(
        on_rels.contains(&"untracked.txt".to_string()),
        "{on_rels:?}"
    );
}

// The dirty file only loses its ranking boost: the answer set is the same.
#[test]
fn the_same_query_returns_the_same_set_with_the_status_switch_either_way() {
    let dir = repo_with_dirty_file();
    let tardir = path_of(dir.path());

    let off = TestServer::start(test_config());
    let off_rels = rels(&search(&off, &tardir));

    let on = TestServer::start(Config {
        git_status: true,
        ..test_config()
    });
    let on_rels = rels(&search(&on, &tardir));

    assert_eq!(off_rels, on_rels, "status must not change the answer set");
    for expected in ["clean.txt", "dirty.txt", "untracked.txt"] {
        assert!(off_rels.contains(&expected.to_string()), "{off_rels:?}");
    }
}

fn log_paths(dir: &Path, stem: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = format!("{stem}+");
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".log"))
        })
        .collect();
    paths.sort();
    paths
}

fn read_logs(dir: &Path, stem: &str) -> String {
    let mut text = String::new();
    for path in log_paths(dir, stem) {
        text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
    }
    text
}

fn wait_for_log(dir: &Path, stem: &str, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if read_logs(dir, stem).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

struct ServerProcess {
    child: Child,
    addr: SocketAddr,
    _stdout: BufReader<ChildStdout>,
}

impl ServerProcess {
    fn start(extra: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(["--port", "0"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fff-mcp-http");

        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let read = stdout.read_line(&mut line).unwrap_or(0);
        assert!(read > 0, "fff-mcp-http exited before printing its address");
        let addr = line
            .trim()
            .rsplit_once("http://")
            .and_then(|(_, addr)| addr.parse().ok())
            .unwrap_or_else(|| panic!("unexpected startup line: {line:?}"));
        Self {
            child,
            addr,
            _stdout: stdout,
        }
    }

    fn search(&self, tardir: &str) -> common::Response {
        let target = format!(
            "/search?{}",
            encode(&[ALL, ("tardir", tardir), ("maxResults", "100")])
        );
        common::request(self.addr, "GET", &target, &[], None).expect("GET succeeds")
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Off means the whole chain is absent from the log; the flag brings it back.
#[test]
fn git_status_off_logs_no_status_read_and_the_flag_brings_it_back() {
    let dir = repo_with_dirty_file();
    let tardir = path_of(dir.path());
    let logs = dir.path().join("logs");

    let hint = logs.join("off.log");
    let mut off = ServerProcess::start(&[
        "--log-file",
        hint.to_str().unwrap(),
        "--idle-timeout-secs",
        "2",
    ]);
    assert_eq!(off.search(&tardir).status, 200);
    // A staging change is the one event that would queue a refresh.
    std::fs::write(dir.path().join("staged.txt"), "later").unwrap();
    stage(dir.path(), "staged.txt");
    assert!(wait_for_log(
        &logs,
        "off",
        "SCAN: Walk completed",
        EXIT_TIMEOUT
    ));
    std::thread::sleep(Duration::from_millis(1500));
    let status = off.wait_exit(EXIT_TIMEOUT).expect("process exits");
    assert!(status.success(), "shutdown was not clean: {status:?}");
    let text = read_logs(&logs, "off");
    for needle in ["refresh_git_status", "read_status"] {
        assert!(
            !text.contains(needle),
            "git status off still logged {needle:?}:\n{text}"
        );
    }

    let on_logs = tempfile::tempdir().unwrap();
    let hint = on_logs.path().join("on.log");
    let mut on = ServerProcess::start(&[
        "--git-status",
        "--log-file",
        hint.to_str().unwrap(),
        "--idle-timeout-secs",
        "2",
    ]);
    assert_eq!(on.search(&tardir).status, 200);
    assert!(
        wait_for_log(on_logs.path(), "on", "refresh_git_status", EXIT_TIMEOUT),
        "the flag did not bring the git status refresh back"
    );
    let status = on.wait_exit(EXIT_TIMEOUT).expect("process exits");
    assert!(status.success(), "shutdown was not clean: {status:?}");
}

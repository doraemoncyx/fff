// Ticket 04: a busy watched directory must not turn the watcher into a log
// writer, and a skipped directory must not reach the index or the log at all.

mod common;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use common::{encode, write_tree};
use serde_json::Value;

const ALL: (&str, &str) = ("querystr", "");
const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp-http");
const TIMEOUT: Duration = Duration::from_secs(30);

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

fn count_log(text: &str, needle: &str) -> usize {
    text.lines().filter(|line| line.contains(needle)).count()
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

    fn search(&self, tardir: &str) -> Value {
        let target = format!("/search?{}", encode(&[ALL, ("tardir", tardir)]));
        let response = common::request(self.addr, "GET", &target, &[], None).expect("GET succeeds");
        assert_eq!(response.status, 200, "search failed: {}", response.body);
        serde_json::from_str(&response.body).expect("search returns JSON")
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

// A skipped directory is not indexed, and its writes never reach the log.
#[test]
fn a_skipped_directory_stays_out_of_the_index_and_the_log() {
    let root = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    write_tree(root.path(), &["src/real.txt"]);
    let tardir = path_of(root.path());
    let hint = logs.path().join("run.log");

    let mut server = ServerProcess::start(&[
        "--skip",
        "agent-tmp/",
        "--log-file",
        hint.to_str().unwrap(),
        "--idle-timeout-secs",
        "2",
    ]);
    assert!(!rels(&server.search(&tardir)).contains(&"agent-tmp/0.txt".to_string()));
    assert!(wait_for_log(logs.path(), "run", "Walk completed", TIMEOUT));

    std::fs::create_dir_all(root.path().join("agent-tmp")).unwrap();
    for i in 0..20 {
        std::fs::write(root.path().join(format!("agent-tmp/{i}.txt")), "noise").unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }
    // Let the debouncer flush whatever it wanted to say, then read it.
    std::thread::sleep(Duration::from_millis(1200));
    let text = read_logs(logs.path(), "run");
    assert!(
        !text.contains("agent-tmp"),
        "a skipped directory leaked into the log:\n{text}"
    );
    assert_eq!(
        count_log(&text, "File index changes applied"),
        0,
        "skipped writes must not produce an applied-changes line:\n{text}"
    );
    assert_eq!(
        count_log(&text, "Event processing summary"),
        0,
        "the per-batch summary belongs to debug:\n{text}"
    );

    let response = server.search(&tardir);
    assert!(
        rels(&response)
            .iter()
            .all(|rel| !rel.starts_with("agent-tmp")),
        "a skipped directory reached the index: {response}"
    );

    // Positive control: a real change still announces itself at info.
    std::fs::write(root.path().join("src/real.txt"), "edited").unwrap();
    assert!(
        wait_for_log(logs.path(), "run", "File index changes applied", TIMEOUT),
        "a real change must stay visible at info:\n{}",
        read_logs(logs.path(), "run")
    );

    let status = server.wait_exit(TIMEOUT).expect("process exits");
    assert!(status.success(), "shutdown was not clean: {status:?}");
}

// A burst of real changes is one INFO line per window, never one per batch.
#[test]
fn a_burst_of_changes_logs_at_most_one_line_per_second() {
    let root = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    write_tree(root.path(), &["src/real.txt"]);
    let tardir = path_of(root.path());
    let hint = logs.path().join("burst.log");

    let mut server = ServerProcess::start(&[
        "--log-file",
        hint.to_str().unwrap(),
        "--idle-timeout-secs",
        "3",
    ]);
    server.search(&tardir);
    assert!(wait_for_log(
        logs.path(),
        "burst",
        "Walk completed",
        TIMEOUT
    ));

    let started = Instant::now();
    let mut batch = 0;
    while started.elapsed() < Duration::from_secs(3) {
        for i in 0..5 {
            let path = root.path().join(format!("busy/b{batch}-{i}.txt"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        batch += 1;
        std::thread::sleep(Duration::from_millis(60));
    }
    std::thread::sleep(Duration::from_millis(1200));

    let text = read_logs(logs.path(), "burst");
    let applied = count_log(&text, "File index changes applied");
    assert!(
        applied > 0,
        "the burst produced no applied-changes line:\n{text}"
    );
    assert!(
        applied <= 6,
        "{batch} batches over 3s produced {applied} applied-changes lines:\n{text}"
    );
    assert_eq!(
        count_log(&text, "Event processing summary"),
        0,
        "the per-batch summary belongs to debug:\n{text}"
    );

    let _ = server.wait_exit(TIMEOUT);
}

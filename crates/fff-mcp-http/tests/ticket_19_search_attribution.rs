mod common;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use common::{TestServer, encode, test_config, write_tree};
use fff_mcp_http::Config;
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp-http");
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &["alpha.txt", "beta.txt"]);
    std::fs::write(dir.path().join("alpha.txt"), "needle here\n").unwrap();
    std::fs::write(dir.path().join("beta.txt"), "nothing to find\n").unwrap();
    dir
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn log_files(dir: &Path, stem: &str) -> Vec<PathBuf> {
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
    for path in log_files(dir, stem) {
        text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
    }
    text
}

fn close_lines<'a>(log: &'a str, span: &str) -> Vec<&'a str> {
    log.lines()
        .filter(|line| line.contains(span) && line.contains("close time.busy="))
        .collect()
}

fn wait_healthy(addr: SocketAddr) {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(response) = common::request(addr, "GET", "/healthz", &[], None)
            && response.status == 200
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not become healthy");
}

// The request spans are the only lines that carry Root, query length and mode;
// `file_count` stays first so tools/profile.ps1 can still parse the span.
#[test]
fn request_spans_carry_root_query_len_and_mode() {
    let dir = tree();
    let tardir = path_of(dir.path());
    let logs = tempfile::tempdir().unwrap();
    let hint = logs.path().join("attr.log");

    let mut server = ServerProcess::start(&[
        "--log-file",
        hint.to_str().unwrap(),
        "--idle-timeout-secs",
        "2",
    ]);
    wait_healthy(server.addr);

    let plain = format!(
        "/grep?{}",
        encode(&[("query", "needle"), ("tardir", &tardir)])
    );
    let response = common::request(server.addr, "GET", &plain, &[], None).unwrap();
    assert_eq!(response.status, 200, "{}", response.body);

    let regex = format!(
        "/grep?{}",
        encode(&[("query", "needle.*here"), ("tardir", &tardir)])
    );
    let response = common::request(server.addr, "GET", &regex, &[], None).unwrap();
    assert_eq!(response.status, 200, "{}", response.body);

    let search = format!(
        "/search?{}",
        encode(&[("querystr", "alpha"), ("tardir", &tardir)])
    );
    let response = common::request(server.addr, "GET", &search, &[], None).unwrap();
    assert_eq!(response.status, 200, "{}", response.body);

    let status = server.wait_exit(EXIT_TIMEOUT).expect("process exits");
    assert!(status.success(), "shutdown was not clean: {status:?}");

    let log = read_logs(logs.path(), "attr");
    let greps = close_lines(&log, "grep_search{");
    assert_eq!(
        greps.len(),
        2,
        "one close line per grep call, no extra lines:\n{log}"
    );
    for line in &greps {
        assert!(line.contains(&format!("root={tardir}")), "{line}");
        assert!(line.contains("query_len="), "{line}");
    }
    assert!(
        greps
            .iter()
            .any(|line| line.contains("query_len=6 mode=literal")),
        "no literal close line:\n{log}"
    );
    assert!(
        greps
            .iter()
            .any(|line| line.contains("query_len=12 mode=regex")),
        "no regex close line:\n{log}"
    );

    let searches = close_lines(&log, "Fuzzy file search{");
    assert_eq!(searches.len(), 1, "one close line per search call:\n{log}");
    let line = searches[0];
    assert!(line.contains(&format!("root={tardir}")), "{line}");
    assert!(line.contains("query_len=5"), "{line}");
}

fn stats_index(server: &TestServer) -> Value {
    let stats = server.stats();
    stats["indexes"]
        .as_array()
        .and_then(|indexes| indexes.first())
        .cloned()
        .unwrap_or_else(|| panic!("no index entry: {stats}"))
}

fn wait_for(server: &TestServer, timeout: Duration, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + timeout;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = stats_index(server);
        if done(&last) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("condition not reached, last stats: {last}");
}

// `content_index` separates the states behind `indexing`: a disabled build can
// never reach `ready`, and an enabled one must say `ready` once the prefilter is on.
#[test]
fn stats_reports_the_real_content_index_state() {
    let dir = tree();
    let tardir = path_of(dir.path());

    let server = TestServer::start(test_config());
    server.search_json(&[
        ("querystr", "alpha"),
        ("tardir", &tardir),
        ("maxResults", "10"),
    ]);
    let index = wait_for(&server, Duration::from_secs(30), |index| {
        index["content_index"] == "ready"
    });
    assert_eq!(index["content_index"], "ready");
    assert_eq!(index["prefilter"], true);

    let plain = TestServer::start(Config {
        content_indexing: false,
        ..test_config()
    });
    plain.search_json(&[
        ("querystr", "alpha"),
        ("tardir", &tardir),
        ("maxResults", "10"),
    ]);
    let index = wait_for(&plain, Duration::from_secs(30), |index| {
        index["indexing"] == false
    });
    assert_eq!(index["content_index"], "disabled");
    assert_eq!(index["prefilter"], false);
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

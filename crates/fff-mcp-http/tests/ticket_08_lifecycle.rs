mod common;

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::TestServer;

const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp-http");
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

fn temp_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    common::write_tree(dir.path(), &["a.txt", "src/b.rs", "docs/readme.md"]);
    dir
}

fn search_target(dir: &Path, query: &str) -> String {
    let tardir = dir.to_string_lossy().into_owned();
    format!(
        "/search?{}",
        common::encode(&[("querystr", query), ("tardir", tardir.as_str())])
    )
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

struct RunOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_with_deadline(args: &[&str], timeout: Duration) -> RunOutput {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fff-mcp-http");
    let stdout = spawn_reader(child.stdout.take().unwrap());
    let stderr = spawn_reader(child.stderr.take().unwrap());
    let Some(status) = wait_for(&mut child, timeout) else {
        let _ = child.kill();
        panic!("fff-mcp-http did not exit within {timeout:?}");
    };
    RunOutput {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }
}

fn spawn_reader(mut stream: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stream.read_to_string(&mut text);
        text
    })
}

fn wait_for(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

struct ServerProcess {
    child: Child,
    addr: SocketAddr,
    _stdout: BufReader<ChildStdout>,
}

impl ServerProcess {
    fn start(dir: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(BIN);
        command.arg("--port").arg("0");
        if !extra.contains(&"--log-file") {
            command
                .arg("--log-file")
                .arg(dir.join("logs").join("server.log"));
        }
        if !extra.contains(&"--idle-timeout-secs") {
            command.args(["--idle-timeout-secs", "600"]);
        }
        let mut child = command
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

    fn get(&self, target: &str) -> common::Response {
        common::request(self.addr, "GET", target, &[], None).expect("GET succeeds")
    }

    fn exit_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().unwrap()
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        wait_for(&mut self.child, timeout)
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn consecutive_launches_write_distinct_log_files() {
    let dir = temp_tree();
    let logs = dir.path().join("logs");
    let hint = logs.join("fff_mcp_http.log");
    for _ in 0..2 {
        let output = run_with_deadline(
            &[
                "--port",
                "0",
                "--log-file",
                hint.to_str().unwrap(),
                "--idle-timeout-secs",
                "1",
                "--no-watch",
            ],
            STARTUP_TIMEOUT,
        );
        assert!(output.status.success(), "launch failed: {}", output.stderr);
    }

    let paths = log_paths(&logs, "fff_mcp_http");
    assert_eq!(paths.len(), 2, "expected two log files, got {paths:?}");
    let names: Vec<String> = paths
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_ne!(names[0], names[1], "second launch overwrote the first log");
    for path in &paths {
        let size = std::fs::metadata(path).unwrap().len();
        assert!(size > 0, "log file {} is empty", path.display());
    }
}

#[test]
fn idle_timeout_exits_and_requests_reset_the_clock() {
    let dir = temp_tree();
    let logs = dir.path().join("logs");
    let hint = logs.join("idle.log");
    let mut server = ServerProcess::start(
        dir.path(),
        &[
            "--log-file",
            hint.to_str().unwrap(),
            "--idle-timeout-secs",
            "1",
        ],
    );

    // One real search leaves an Index (with its watcher) behind.
    let response = server.get(&search_target(dir.path(), "a.txt"));
    assert_eq!(response.status, 200, "{}", response.body);

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        let response = server.get("/stats");
        assert_eq!(response.status, 200);
        assert!(
            server.exit_status().is_none(),
            "exited while requests were still arriving"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // /healthz is a probe, not usage: it must not keep the process alive.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut probes = 0;
    let status = loop {
        if let Some(status) = server.exit_status() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit after the idle timeout"
        );
        if let Ok(response) = common::request(server.addr, "GET", "/healthz", &[], None)
            && response.status == 200
        {
            probes += 1;
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    assert!(status.success(), "idle shutdown was not clean: {status:?}");
    assert!(
        probes >= 3,
        "only {probes} health probes landed; the test proved nothing"
    );

    let text = read_logs(&logs, "idle");
    assert!(
        text.contains("exiting after the idle timeout"),
        "idle exit missing from logs:\n{text}"
    );
    assert!(
        text.contains("shutdown complete"),
        "graceful shutdown missing from logs:\n{text}"
    );
}

#[test]
fn debug_logs_cover_scans_indexes_eviction_and_memory() {
    let first = temp_tree();
    let second = temp_tree();
    let logs = first.path().join("logs");
    let hint = logs.join("debug.log");
    let mut server = ServerProcess::start(
        first.path(),
        &[
            "--log-file",
            hint.to_str().unwrap(),
            "--log-level",
            "debug",
            "--max-memory",
            "1M",
            "--test-hook",
            "--idle-timeout-secs",
            "2",
        ],
    );

    for tree in [first.path(), second.path()] {
        let response = server.get(&search_target(tree, "a.txt"));
        assert_eq!(response.status, 200, "{}", response.body);
    }
    let evicted = common::request(server.addr, "POST", "/test/evict", &[], Some("{}"))
        .expect("eviction hook responds");
    assert_eq!(evicted.status, 200, "{}", evicted.body);

    let status = server.wait_exit(EXIT_TIMEOUT).expect("process exits");
    assert!(status.success(), "shutdown was not clean: {status:?}");

    let text = read_logs(&logs, "debug");
    for needle in [
        "scan started",
        "scan completed",
        "creating index",
        "evicted index over the memory budget",
        "eviction round memory measurement",
    ] {
        assert!(
            text.contains(needle),
            "debug log is missing {needle:?}:\n{text}"
        );
    }
}

#[test]
fn occupied_port_exits_non_zero_with_a_readable_message() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let dir = temp_tree();
    let hint = dir.path().join("logs").join("bind.log");

    let output = run_with_deadline(
        &[
            "--port",
            port.as_str(),
            "--log-file",
            hint.to_str().unwrap(),
            "--idle-timeout-secs",
            "1",
        ],
        EXIT_TIMEOUT,
    );
    assert!(
        !output.status.success(),
        "an occupied port must fail: {}",
        output.stderr
    );
    assert!(
        output.stderr.contains("cannot bind"),
        "unreadable stderr: {}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&port),
        "stderr does not name the port: {}",
        output.stderr
    );
    drop(listener);
}

#[test]
fn missing_config_file_exits_non_zero_with_a_readable_message() {
    let dir = temp_tree();
    let missing = dir.path().join("absent.toml");
    let output = run_with_deadline(&["--config", missing.to_str().unwrap()], EXIT_TIMEOUT);
    assert!(!output.status.success(), "missing config must fail");
    assert!(
        output.stderr.contains("absent.toml"),
        "unreadable stderr: {}",
        output.stderr
    );
}

#[test]
fn non_loopback_bind_without_a_token_exits_non_zero() {
    let dir = temp_tree();
    let hint = dir.path().join("logs").join("token.log");
    let output = run_with_deadline(
        &[
            "--bind",
            "0.0.0.0",
            "--port",
            "0",
            "--log-file",
            hint.to_str().unwrap(),
        ],
        EXIT_TIMEOUT,
    );
    assert!(!output.status.success(), "an unguarded bind must fail");
    assert!(
        output.stderr.contains("--token"),
        "unreadable stderr: {}",
        output.stderr
    );
}

#[test]
fn version_prints_the_crate_version_and_git_hash() {
    let output = run_with_deadline(&["--version"], EXIT_TIMEOUT);
    assert!(output.status.success(), "{}", output.stderr);
    let expected = format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("FFF_GIT_HASH"));
    assert!(
        output.stdout.contains(&expected),
        "--version printed {:?}, expected {expected:?}",
        output.stdout
    );
}

#[test]
fn shutdown_stops_the_serving_thread_and_the_indexes() {
    let dir = temp_tree();
    let server = TestServer::start(common::test_config());
    let addr = server.addr;

    let response = server.get(&search_target(dir.path(), "a.txt"));
    assert_eq!(response.status, 200, "{}", response.body);

    assert!(
        server.shutdown_within(Duration::from_secs(20)),
        "serving thread did not join after shutdown"
    );
    assert!(
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err(),
        "listener still accepts connections after shutdown"
    );
}

/// Windows has no reliable way to raise a console control event from a test,
/// so the graceful path is covered in-process above and by SIGTERM on unix.
#[cfg(unix)]
#[test]
fn sigterm_exits_the_process_cleanly() {
    let dir = temp_tree();
    let logs = dir.path().join("logs");
    let hint = logs.join("sigterm.log");
    let mut server = ServerProcess::start(
        dir.path(),
        &[
            "--log-file",
            hint.to_str().unwrap(),
            "--idle-timeout-secs",
            "0",
        ],
    );

    let pid = server.child.id().to_string();
    let killed = Command::new("kill")
        .args(["-TERM", pid.as_str()])
        .status()
        .expect("kill runs");
    assert!(killed.success(), "kill -TERM failed");

    let status = server.wait_exit(EXIT_TIMEOUT).expect("exits on SIGTERM");
    assert!(
        status.success(),
        "SIGTERM shutdown was not clean: {status:?}"
    );

    let text = read_logs(&logs, "sigterm");
    assert!(
        text.contains("SIGTERM received"),
        "missing SIGTERM reason:\n{text}"
    );
    assert!(
        text.contains("shutdown complete"),
        "missing graceful shutdown:\n{text}"
    );
}

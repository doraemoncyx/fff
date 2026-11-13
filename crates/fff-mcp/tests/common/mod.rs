#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp");

// A live fff-mcp over stdio, past the initialize handshake.
pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Session {
    // Watching, warmup, and update checks stay off so the fixture is stable.
    pub fn start(dir: &Path, extra: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .arg(dir)
            .args([
                "--no-update-check",
                "--no-warmup",
                "--no-watch",
                "--log-file",
            ])
            .arg(dir.join("test.log"))
            .args(extra)
            .env("FFF_MCP_TEST_WATCHDOG_INTERVAL_MS", "100")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("fff-mcp starts");
        let mut stdin = child.stdin.take().unwrap();
        let lines = line_reader(child.stdout.take().unwrap());
        handshake(&mut stdin, &lines);
        Self {
            child,
            stdin: Some(stdin),
            lines,
            next_id: 2,
        }
    }

    pub fn send(&mut self, message: serde_json::Value) {
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    // Sends a request and returns its response, ignoring unrelated wire traffic.
    pub fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no response to {method} within 60s"));
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value["id"].as_u64() == Some(id) {
                return value;
            }
        }
    }

    pub fn tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.call(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
    }

    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().unwrap()
    }

    // Ending the transport is how a client asks the server to shut down.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.try_wait().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.child.kill().ok();
        panic!("fff-mcp did not exit within {:?}", timeout);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

pub fn handshake(stdin: &mut ChildStdin, lines: &mpsc::Receiver<String>) {
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "fff-mcp-test", "version": "0.0.0" }
        }
    });
    writeln!(stdin, "{}", initialize).unwrap();
    stdin.flush().unwrap();

    let response = lines
        .recv_timeout(Duration::from_secs(30))
        .expect("no initialize response within 30s");
    assert!(
        response.contains("\"serverInfo\""),
        "unexpected initialize response: {}",
        response
    );

    writeln!(
        stdin,
        "{}",
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .unwrap();
    stdin.flush().unwrap();
}

pub fn line_reader(stdout: ChildStdout) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

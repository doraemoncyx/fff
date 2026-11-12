#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use fff_mcp_http::{Config, RunningServer, spawn};

pub struct TestServer {
    server: Option<RunningServer>,
    pub addr: SocketAddr,
}

impl TestServer {
    pub fn start(config: Config) -> Self {
        let server = spawn(config).expect("server binds");
        let addr = server.addr();
        let mut test = Self {
            server: Some(server),
            addr,
        };
        test.wait_until_ready();
        test
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(response) = request(self.addr, "GET", "/healthz", &[], None)
                && response.status == 200
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("server did not become healthy");
    }

    pub fn get(&self, target: &str) -> Response {
        request(self.addr, "GET", target, &[], None).expect("GET succeeds")
    }

    pub fn post(&self, target: &str, body: &str, headers: &[(&str, &str)]) -> Response {
        request(self.addr, "POST", target, headers, Some(body)).expect("POST succeeds")
    }

    pub fn search(&self, params: &[(&str, &str)]) -> Response {
        self.get(&format!("/search?{}", encode(params)))
    }

    pub fn search_json(&self, params: &[(&str, &str)]) -> serde_json::Value {
        let response = self.search(params);
        assert_eq!(
            response.status, 200,
            "unexpected status: body={}",
            response.body
        );
        serde_json::from_str(&response.body).expect("search returns JSON")
    }

    pub fn stats(&self) -> serde_json::Value {
        let response = self.get("/stats");
        assert_eq!(response.status, 200, "stats failed: {}", response.body);
        serde_json::from_str(&response.body).expect("stats returns JSON")
    }

    pub fn mcp(&self, message: serde_json::Value) -> serde_json::Value {
        let body = serde_json::to_string(&message).unwrap();
        let response = self.post(
            "/mcp",
            &body,
            &[("Accept", "application/json, text/event-stream")],
        );
        assert_eq!(response.status, 200, "mcp failed: {}", response.body);
        serde_json::from_str(&response.body).expect("mcp returns JSON")
    }

    pub fn mcp_call(&self, id: u64, arguments: serde_json::Value) -> serde_json::Value {
        self.mcp(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "find_files", "arguments": arguments }
        }))
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown();
        }
    }
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

pub fn test_config() -> Config {
    Config {
        port: 0,
        scan_wait: Duration::from_secs(20),
        ..Config::default()
    }
}

pub fn write_tree(root: &Path, files: &[&str]) {
    for file in files {
        let path = root.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "placeholder").unwrap();
    }
}

/// Make the tree a git repo: hidden files are only walked inside one.
pub fn make_git_repo(root: &Path) {
    git2::Repository::init(root).expect("init git repo");
}

pub fn encode(params: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (index, (key, value)) in params.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&percent(key));
        out.push('=');
        out.push_str(&percent(value));
    }
    out
}

fn percent(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn request(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;

    let mut request =
        format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(body) => {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ));
        }
        None => request.push_str("\r\n"),
    }

    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    Ok(parse_response(&raw))
}

fn parse_response(raw: &[u8]) -> Response {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = &raw[(split + 4).min(raw.len())..];
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let chunked = head.lines().any(|line| {
        line.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    });
    let body = if chunked {
        decode_chunked(body)
    } else {
        String::from_utf8_lossy(body).to_string()
    };
    Response { status, body }
}

fn decode_chunked(raw: &[u8]) -> String {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(position) = rest.windows(2).position(|window| window == b"\r\n") {
        let size_text = String::from_utf8_lossy(&rest[..position]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let start = position + 2;
        let end = (start + size).min(rest.len());
        out.extend_from_slice(&rest[start..end]);
        rest = &rest[(end + 2).min(rest.len())..];
    }
    String::from_utf8_lossy(&out).to_string()
}

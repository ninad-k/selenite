//! End-to-end over real HTTP: start the server on an ephemeral loopback port with a temp vault
//! and talk to it with a raw HTTP client.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;

use reqwest::blocking::{Client, RequestBuilder, Response};
use selenite_server::{ServerConfig, ServerHandle, run};
use serde_json::{Value, json};

const TOKEN: &str = "test-token-0123456789-abcdefghij";

struct Fixture {
    base: String,
    http: Client,
    vault: tempfile::TempDir,
    handle: Option<ServerHandle>,
}

impl Fixture {
    fn start() -> Self {
        let vault = tempfile::tempdir().expect("vault dir");
        let (addr, handle): (SocketAddr, ServerHandle) = run(ServerConfig {
            vault: vault.path().to_path_buf(),
            listen: "127.0.0.1:0".parse().unwrap(),
            token: TOKEN.into(),
            sync_interval: None,
            allowed_origins: vec![],
            allowed_hosts: vec![],
        })
        .expect("start server");
        Fixture {
            base: format!("http://{addr}"),
            http: Client::new(),
            vault,
            handle: Some(handle),
        }
    }

    fn mcp(&self) -> RequestBuilder {
        self.http
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
    }

    fn authed(&self) -> RequestBuilder {
        self.mcp().bearer_auth(TOKEN)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

fn initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "http-test", "version": "0.0.0"}
        }
    })
}

/// The JSON-RPC message in a response that is either plain JSON or a short SSE stream.
fn rpc_message(resp: Response) -> Value {
    let body = resp.text().expect("body");
    if let Ok(v) = serde_json::from_str::<Value>(&body) {
        return v;
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .find(|v| v.get("result").is_some() || v.get("error").is_some())
        .unwrap_or_else(|| panic!("no JSON-RPC message in body: {body}"))
}

/// Initialize + `notifications/initialized`; returns the session id.
fn handshake(f: &Fixture) -> String {
    let resp = f.authed().json(&initialize_body()).send().expect("init");
    assert_eq!(resp.status(), 200);
    let session = resp
        .headers()
        .get("mcp-session-id")
        .expect("session id header")
        .to_str()
        .unwrap()
        .to_string();
    let msg = rpc_message(resp);
    assert_eq!(msg["id"], 1);
    assert!(msg["result"]["serverInfo"].is_object(), "{msg}");
    let ack = f
        .authed()
        .header("Mcp-Session-Id", &session)
        .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .expect("initialized");
    assert_eq!(ack.status(), 202);
    session
}

fn call(f: &Fixture, session: &str, id: i64, method: &str, params: Value) -> Value {
    let resp = f
        .authed()
        .header("Mcp-Session-Id", session)
        .json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        .send()
        .expect("call");
    assert_eq!(resp.status(), 200);
    rpc_message(resp)
}

#[test]
fn healthz_needs_no_token_and_reveals_nothing() {
    let f = Fixture::start();
    let resp = f.http.get(format!("{}/healthz", f.base)).send().unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().unwrap(), "ok");
}

#[test]
fn missing_or_wrong_token_is_401_with_no_body() {
    let f = Fixture::start();
    let none = f.mcp().json(&initialize_body()).send().unwrap();
    assert_eq!(none.status(), 401);
    assert!(none.text().unwrap().is_empty());
    let wrong = f
        .mcp()
        .bearer_auth("not-the-right-token-at-all-0000")
        .json(&initialize_body())
        .send()
        .unwrap();
    assert_eq!(wrong.status(), 401);
    assert!(wrong.text().unwrap().is_empty());
    let basic = f
        .mcp()
        .header("Authorization", format!("Basic {TOKEN}"))
        .json(&initialize_body())
        .send()
        .unwrap();
    assert_eq!(basic.status(), 401);
}

#[test]
fn foreign_origin_is_403_even_with_a_valid_token() {
    let f = Fixture::start();
    let resp = f
        .authed()
        .header("Origin", "https://evil.example")
        .json(&initialize_body())
        .send()
        .unwrap();
    assert_eq!(resp.status(), 403);
    let local = f
        .authed()
        .header("Origin", "http://localhost:3000")
        .json(&initialize_body())
        .send()
        .unwrap();
    assert_eq!(local.status(), 200);
}

#[test]
fn initialize_lists_tools_without_any_delete() {
    let f = Fixture::start();
    let session = handshake(&f);
    let msg = call(&f, &session, 2, "tools/list", json!({}));
    let names: Vec<String> = msg["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"search".to_string()), "{names:?}");
    for n in &names {
        let lower = n.to_lowercase();
        assert!(
            !lower.contains("delete") && !lower.contains("remove"),
            "unexpected tool {n}"
        );
    }
}

#[test]
fn write_tool_creates_a_file_and_an_http_audit_entry() {
    let f = Fixture::start();
    let session = handshake(&f);
    let msg = call(
        &f,
        &session,
        3,
        "tools/call",
        json!({"name": "create_page", "arguments": {"path": "Inbox/Hello.md", "markdown": "# Hello\n\nfrom http\n"}}),
    );
    assert_ne!(msg["result"]["isError"], json!(true), "{msg}");
    let file = f.vault.path().join("Inbox").join("Hello.md");
    let text = std::fs::read_to_string(&file).expect("page file");
    assert!(text.contains("from http"));

    let audit_dir = f.vault.path().join(".selenite").join("audit");
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(audit_dir).expect("audit dir") {
        let content = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        lines.extend(content.lines().map(str::to_string));
    }
    let entries: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let entry = entries
        .iter()
        .find(|e| e["tool"] == "create_page")
        .expect("create_page audit entry");
    assert_eq!(entry["transport"], "http");
    assert_eq!(entry["ok"], true);
}

#[test]
fn shutdown_stops_the_listener() {
    let mut f = Fixture::start();
    let base = f.base.clone();
    f.handle.take().unwrap().shutdown();
    let result = Client::new().get(format!("{base}/healthz")).send();
    assert!(result.is_err());
}

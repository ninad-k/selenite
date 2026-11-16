//! End-to-end: spawn the stdio server binary and talk JSON-RPC to it over pipes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use selenite_index::Engine;
use serde_json::{Value, json};

struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Client {
    fn spawn(vault: &std::path::Path) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_selenite-mcp-stdio"))
            .arg(vault)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Client {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").expect("write request");
        self.stdin.flush().expect("flush");
    }

    /// Read lines until the response with this id arrives (stdout must carry only JSON-RPC).
    fn response(&mut self, id: i64) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect("timed out waiting for the server");
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
            assert_eq!(v["jsonrpc"], "2.0", "{line}");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(id)
    }
}

#[test]
fn initialize_list_and_call_over_stdio() {
    let dir = tempfile::tempdir().expect("tempdir");
    drop(Engine::init_local(dir.path()).expect("init vault"));
    let mut c = Client::spawn(dir.path());

    let init = c.call(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "stdio-test", "version": "0.0.1"}
        }),
    );
    assert!(init.get("error").is_none(), "{init}");
    let result = &init["result"];
    assert_eq!(result["serverInfo"]["name"], selenite_core::APP_NAME);
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["capabilities"]["resources"].is_object());
    assert!(result["capabilities"]["prompts"].is_object());
    assert!(
        result["instructions"]
            .as_str()
            .expect("instructions")
            .contains("archive")
    );

    c.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let tools = c.call(2, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert!(names.contains(&"search") && names.contains(&"append_blocks"));
    assert!(!names.iter().any(|n| n.contains("delete")));

    let search = c.call(
        3,
        "tools/call",
        json!({"name": "search", "arguments": {"query": "welcome"}}),
    );
    assert!(search.get("error").is_none(), "{search}");
    assert_ne!(search["result"]["isError"], json!(true), "{search}");
    let text = search["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: Value = serde_json::from_str(text).expect("json payload");
    assert_eq!(payload["results"][0]["path"], "Welcome.md");

    // A tool failure is a result with isError, not a protocol error.
    let missing = c.call(
        4,
        "tools/call",
        json!({"name": "get_page", "arguments": {"path": "Nope.md"}}),
    );
    assert!(missing.get("error").is_none(), "{missing}");
    assert_eq!(missing["result"]["isError"], json!(true), "{missing}");

    // Resources and prompts.
    let resources = c.call(5, "resources/list", json!({}));
    assert_eq!(
        resources["result"]["resources"][0]["uri"], "vault:///Welcome.md",
        "{resources}"
    );
    let read = c.call(6, "resources/read", json!({"uri": "vault:///Welcome.md"}));
    assert!(
        read["result"]["contents"][0]["text"]
            .as_str()
            .expect("resource text")
            .contains("Welcome"),
        "{read}"
    );
    let prompts = c.call(7, "prompts/list", json!({}));
    assert_eq!(prompts["result"]["prompts"][0]["name"], "daily_review");
    let prompt = c.call(8, "prompts/get", json!({"name": "daily_review"}));
    assert!(
        prompt["result"]["messages"][0]["content"]["text"]
            .as_str()
            .expect("prompt text")
            .contains("get_agenda"),
        "{prompt}"
    );

    // Closing stdin ends the session cleanly.
    drop(c.stdin);
    let status = c.child.wait().expect("wait");
    assert!(status.success(), "server exited with {status}");
}

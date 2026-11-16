//! Test-only helpers: a tiny hand-rolled HTTP/1.1 mock server and the provider contract tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::{StorageError, StorageProvider, VaultPath, WriteMode};

/// A parsed request.
#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    /// Raw path (still percent-encoded), without the query.
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Lower-cased header names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn q(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A response.
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn value(status: u16, v: &serde_json::Value) -> Self {
        Self::json(status, &v.to_string())
    }

    pub fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body,
        }
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// Decode `a=b&c=d` (form-urlencoded) into pairs.
pub fn query_of(s: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(&format!("http://x.invalid/?{s}"))
        .map(|u| u.query_pairs().into_owned().collect())
        .unwrap_or_default()
}

/// Percent-decode a path component.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

type Handler = dyn Fn(&Req) -> Resp + Send + Sync + 'static;

/// A background HTTP server on `127.0.0.1:0`.
pub struct MockServer {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    pub fn start(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let h = handler.clone();
                std::thread::spawn(move || serve(conn, &*h));
            }
        });
        Self { port, stop }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(conn: TcpStream, handler: &Handler) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(mut writer) = conn.try_clone() else {
        return;
    };
    let mut r = BufReader::new(conn);
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 {
            return;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if r.read_exact(&mut body).is_err() {
        return;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), query_of(q)),
        None => (target.clone(), Vec::new()),
    };
    let req = Req {
        method,
        path,
        query,
        headers,
        body,
    };
    let resp = handler(&req);
    let mut out = format!("HTTP/1.1 {} X\r\n", resp.status).into_bytes();
    for (k, v) in &resp.headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            resp.body.len()
        )
        .as_bytes(),
    );
    if req.method != "HEAD" {
        out.extend_from_slice(&resp.body);
    }
    let _ = writer.write_all(&out);
    let _ = writer.flush();
}

fn p(s: &str) -> VaultPath {
    VaultPath::new(s).unwrap()
}

/// The `StorageProvider` contract every provider must satisfy.
pub fn contract(s: &dyn StorageProvider) {
    // create
    let e1 = s.write(&p("a/b.md"), b"one", WriteMode::CreateNew).unwrap();
    assert_eq!(e1.path, p("a/b.md"));
    assert_eq!(e1.size, 3);
    // CreateNew conflict
    assert!(matches!(
        s.write(&p("a/b.md"), b"zzz", WriteMode::CreateNew),
        Err(StorageError::AlreadyExists(_))
    ));
    assert_eq!(s.read(&p("a/b.md")).unwrap(), b"one");
    // stat
    let st = s.stat(&p("a/b.md")).unwrap().unwrap();
    assert_eq!(st.revision, e1.revision);
    assert!(s.stat(&p("nope.md")).unwrap().is_none());
    // replace with right revision
    let e2 = s
        .write(
            &p("a/b.md"),
            b"two!",
            WriteMode::Replace(e1.revision.clone()),
        )
        .unwrap();
    assert_ne!(e2.revision, e1.revision);
    assert_eq!(e2.size, 4);
    // replace with stale revision: conflict, file untouched
    match s.write(
        &p("a/b.md"),
        b"stale",
        WriteMode::Replace(e1.revision.clone()),
    ) {
        Err(StorageError::Conflict { actual, .. }) => assert_eq!(actual, Some(e2.revision.clone())),
        other => panic!("expected conflict, got {other:?}"),
    }
    assert_eq!(s.read(&p("a/b.md")).unwrap(), b"two!");
    // replace of a missing file is a conflict with no actual revision
    match s.write(
        &p("ghost.md"),
        b"x",
        WriteMode::Replace(e1.revision.clone()),
    ) {
        Err(StorageError::Conflict { actual: None, .. }) => {}
        other => panic!("expected conflict, got {other:?}"),
    }
    // overwrite (create and replace)
    s.write(&p("top.md"), b"t1", WriteMode::Overwrite).unwrap();
    s.write(&p("top.md"), b"t2", WriteMode::Overwrite).unwrap();
    assert_eq!(s.read(&p("top.md")).unwrap(), b"t2");
    // nested
    s.write(&p("x/y/z.md"), b"deep", WriteMode::CreateNew)
        .unwrap();
    let mut files: Vec<String> = s
        .list()
        .unwrap()
        .into_iter()
        .map(|e| e.path.as_str().to_string())
        .collect();
    files.sort();
    assert_eq!(files, ["a/b.md", "top.md", "x/y/z.md"]);
    let mut dirs: Vec<String> = s
        .list_dirs()
        .unwrap()
        .into_iter()
        .map(|d| d.as_str().to_string())
        .collect();
    dirs.sort();
    assert_eq!(dirs, ["a", "x", "x/y"]);
    // read missing
    assert!(matches!(
        s.read(&p("missing.md")),
        Err(StorageError::NotFound(_))
    ));
    // rename
    let moved = s.rename(&p("top.md"), &p("moved/top2.md")).unwrap();
    assert_eq!(moved.path, p("moved/top2.md"));
    assert!(s.stat(&p("top.md")).unwrap().is_none());
    assert_eq!(s.read(&p("moved/top2.md")).unwrap(), b"t2");
    assert!(matches!(
        s.rename(&p("moved/top2.md"), &p("a/b.md")),
        Err(StorageError::AlreadyExists(_))
    ));
    assert!(matches!(
        s.rename(&p("nothing.md"), &p("other.md")),
        Err(StorageError::NotFound(_))
    ));
    // create_dir (idempotent)
    s.create_dir(&p("empty/dir")).unwrap();
    s.create_dir(&p("empty/dir")).unwrap();
    let dirs: Vec<String> = s
        .list_dirs()
        .unwrap()
        .into_iter()
        .map(|d| d.as_str().to_string())
        .collect();
    assert!(dirs.contains(&"empty".to_string()) && dirs.contains(&"empty/dir".to_string()));
    // trash is soft and removes the file from the live listing
    s.trash(&p("a/b.md")).unwrap();
    assert!(s.stat(&p("a/b.md")).unwrap().is_none());
    assert!(
        !s.list()
            .unwrap()
            .iter()
            .any(|e| e.path.as_str() == "a/b.md")
    );
    assert!(matches!(
        s.trash(&p("a/b.md")),
        Err(StorageError::NotFound(_))
    ));
}

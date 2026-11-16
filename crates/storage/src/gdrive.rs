//! Google Drive provider over the Drive v3 REST API with the least-privilege `drive.file` scope.
//!
//! Layout: the app creates one root folder (its name is passed in, typically the vault name) and
//! the vault lives inside it; `VaultPath` segments map to nested Drive folders. With `drive.file`
//! the app can only see files and folders it created, so looking a folder up by name is safe.
//!
//! * **Revision** is the file's `md5Checksum` (content based, stable); `v<version>` is the
//!   fallback when Drive reports no checksum. Drive's own `version` counter also bumps for
//!   metadata-only changes (rename, trash, sharing) which would cause spurious conflicts.
//! * **Conditional writes are not available.** Drive v3 does not document `If-Match`/ETag
//!   preconditions on `files.update` or on the media/multipart upload endpoints, so
//!   `WriteMode::Replace` is *check-then-write*: the current revision is read first and the upload
//!   follows. A concurrent writer can slip in between (a window of one round trip); the sync engine
//!   must treat a later three-way merge as the safety net. `CreateNew` has the same window because
//!   Drive permits duplicate names.
//! * **Duplicates**: Drive allows several files with one name in a folder; the first match wins.
//! * **Trash** sets `trashed=true` (recoverable from the Drive bin for 30 days); the API `DELETE`
//!   is never used. The returned path is a nominal `.selenite/trash/<secs>-<name>` and is not
//!   readable through this provider.
//! * Folder ids are cached; call [`GoogleDrive::forget_cached_folders`] if the user may have
//!   deleted folders behind the app's back.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use reqwest::Url;
use reqwest::blocking::{Client, Response};
use serde::Deserialize;

use crate::oauth::Authenticator;
use crate::util::{http_client, lock, net_err, parse_rfc3339, status_error};
use crate::{
    Entry, Result, Revision, StorageError, StorageProvider, VaultPath, WriteMode, unix_secs,
    vault_path,
};

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FIELDS: &str = "id,name,mimeType,size,modifiedTime,md5Checksum,version";
const DEFAULT_API: &str = "https://www.googleapis.com/drive/v3";
const DEFAULT_UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveFile {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mime_type: String,
    size: Option<String>,
    modified_time: Option<String>,
    md5_checksum: Option<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    next_page_token: Option<String>,
}

fn revision_of(f: &DriveFile) -> Revision {
    Revision(
        f.md5_checksum
            .clone()
            .or_else(|| f.version.as_ref().map(|v| format!("v{v}")))
            .or_else(|| f.modified_time.clone())
            .unwrap_or_default(),
    )
}

fn to_entry(path: &VaultPath, f: &DriveFile) -> Entry {
    Entry {
        path: path.clone(),
        size: f.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        modified: f
            .modified_time
            .as_deref()
            .and_then(parse_rfc3339)
            .unwrap_or(SystemTime::UNIX_EPOCH),
        revision: revision_of(f),
    }
}

/// Escape a value for use inside single quotes in a Drive `q` expression.
fn q_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Google Drive storage rooted at an app-created folder.
pub struct GoogleDrive {
    auth: Arc<Authenticator>,
    client: Client,
    api: String,
    upload: String,
    root_name: String,
    /// Directory path (`""` = root folder) to Drive folder id.
    folders: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for GoogleDrive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleDrive")
            .field("root_name", &self.root_name)
            .finish_non_exhaustive()
    }
}

impl GoogleDrive {
    /// Use the production Drive endpoints. `root_name` is the folder created/used for the vault.
    pub fn new(auth: Arc<Authenticator>, root_name: &str) -> Result<Self> {
        Self::with_base_urls(auth, root_name, DEFAULT_API, DEFAULT_UPLOAD)
    }

    /// Override the REST and upload base URLs (no trailing slash); used by tests.
    pub fn with_base_urls(
        auth: Arc<Authenticator>,
        root_name: &str,
        api: &str,
        upload: &str,
    ) -> Result<Self> {
        Ok(Self {
            auth,
            client: http_client()?,
            api: api.trim_end_matches('/').to_string(),
            upload: upload.trim_end_matches('/').to_string(),
            root_name: root_name.to_string(),
            folders: Mutex::new(HashMap::new()),
        })
    }

    /// Drop the folder-id cache.
    pub fn forget_cached_folders(&self) {
        if let Ok(mut m) = self.folders.lock() {
            m.clear();
        }
    }

    fn url(&self, base: &str, path: &str, params: &[(&str, &str)]) -> Result<Url> {
        let mut u = Url::parse(&format!("{base}{path}"))
            .map_err(|e| StorageError::Internal(format!("bad url: {e}")))?;
        if !params.is_empty() {
            u.query_pairs_mut().extend_pairs(params.iter().copied());
        }
        Ok(u)
    }

    fn send(
        &self,
        build: &dyn Fn(&Client, &str) -> reqwest::blocking::RequestBuilder,
    ) -> Result<Response> {
        self.auth
            .send(&|tok| build(&self.client, tok).bearer_auth(tok))
    }

    fn json<T: for<'de> Deserialize<'de>>(resp: Response, what: &str) -> Result<T> {
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), what));
        }
        resp.json().map_err(|_| {
            StorageError::Internal(format!("remote: unreadable response during {what}"))
        })
    }

    /// Paginated `files.list`.
    fn list_query(&self, q: &str) -> Result<Vec<DriveFile>> {
        let fields = format!("nextPageToken,files({FIELDS})");
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut params = vec![("q", q), ("fields", fields.as_str()), ("pageSize", "1000")];
            if let Some(t) = &token {
                params.push(("pageToken", t));
            }
            let url = self.url(&self.api, "/files", &params)?;
            let resp = self.send(&|c, _| c.get(url.clone()))?;
            let page: FileList = Self::json(resp, "files.list")?;
            out.extend(page.files);
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => return Ok(out),
            }
        }
    }

    fn child_folder(&self, parent: &str, name: &str) -> Result<Option<String>> {
        let q = format!(
            "name='{}' and mimeType='{FOLDER_MIME}' and '{}' in parents and trashed=false",
            q_escape(name),
            q_escape(parent)
        );
        Ok(self.list_query(&q)?.into_iter().next().map(|f| f.id))
    }

    fn make_folder(&self, parent: &str, name: &str) -> Result<String> {
        let url = self.url(&self.api, "/files", &[("fields", FIELDS)])?;
        let body = serde_json::json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent]});
        let resp = self.send(&|c, _| c.post(url.clone()).json(&body))?;
        Ok(Self::json::<DriveFile>(resp, "folder create")?.id)
    }

    fn cached(&self, dir: &str) -> Result<Option<String>> {
        Ok(lock(&self.folders)?.get(dir).cloned())
    }

    fn remember(&self, dir: &str, id: &str) -> Result<()> {
        lock(&self.folders)?.insert(dir.to_string(), id.to_string());
        Ok(())
    }

    /// Folder id of `dir` (`""` is the vault root folder), optionally creating missing folders.
    fn dir_id(&self, dir: &str, create: bool) -> Result<Option<String>> {
        if let Some(id) = self.cached(dir)? {
            return Ok(Some(id));
        }
        let mut current = match self.cached("")? {
            Some(id) => id,
            None => {
                let found = match self.child_folder("root", &self.root_name)? {
                    Some(id) => id,
                    None if create => self.make_folder("root", &self.root_name)?,
                    None => return Ok(None),
                };
                self.remember("", &found)?;
                found
            }
        };
        let mut prefix = String::new();
        for seg in dir.split('/').filter(|s| !s.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            current = match self.cached(&prefix)? {
                Some(id) => id,
                None => {
                    let id = match self.child_folder(&current, seg)? {
                        Some(id) => id,
                        None if create => self.make_folder(&current, seg)?,
                        None => return Ok(None),
                    };
                    self.remember(&prefix, &id)?;
                    id
                }
            };
        }
        Ok(Some(current))
    }

    fn find_file(&self, path: &VaultPath) -> Result<Option<DriveFile>> {
        let Some(parent) = self.dir_id(path.parent().unwrap_or(""), false)? else {
            return Ok(None);
        };
        let q = format!(
            "name='{}' and '{}' in parents and trashed=false and mimeType!='{FOLDER_MIME}'",
            q_escape(path.file_name()),
            q_escape(&parent)
        );
        Ok(self.list_query(&q)?.into_iter().next())
    }

    fn create_file(&self, path: &VaultPath, data: &[u8]) -> Result<DriveFile> {
        let parent = self
            .dir_id(path.parent().unwrap_or(""), true)?
            .ok_or_else(|| StorageError::Internal("could not create parent folder".into()))?;
        let boundary = format!(
            "selenite{:016x}{:016x}",
            rand::random::<u64>(),
            rand::random::<u64>()
        );
        let meta = serde_json::json!({"name": path.file_name(), "parents": [parent]}).to_string();
        let mut body = Vec::with_capacity(data.len() + meta.len() + 256);
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
        let url = self.url(
            &self.upload,
            "/files",
            &[("uploadType", "multipart"), ("fields", FIELDS)],
        )?;
        let ctype = format!("multipart/related; boundary={boundary}");
        let resp = self.send(&|c, _| {
            c.post(url.clone())
                .header("Content-Type", ctype.as_str())
                .body(body.clone())
        })?;
        Self::json(resp, "file create")
    }

    fn update_media(&self, path: &VaultPath, id: &str, data: &[u8]) -> Result<DriveFile> {
        let url = self.url(
            &self.upload,
            &format!("/files/{id}"),
            &[("uploadType", "media"), ("fields", FIELDS)],
        )?;
        let resp = self.send(&|c, _| {
            c.patch(url.clone())
                .header("Content-Type", "application/octet-stream")
                .body(data.to_vec())
        })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(path.clone()));
        }
        Self::json(resp, "file update")
    }

    fn walk(&self) -> Result<(Vec<Entry>, Vec<VaultPath>)> {
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        let Some(root) = self.dir_id("", false)? else {
            return Ok((files, dirs));
        };
        let mut queue = VecDeque::from([(root, String::new())]);
        let mut skipped = 0usize;
        while let Some((id, prefix)) = queue.pop_front() {
            let q = format!("'{}' in parents and trashed=false", q_escape(&id));
            for f in self.list_query(&q)? {
                let full = if prefix.is_empty() {
                    f.name.clone()
                } else {
                    format!("{prefix}/{}", f.name)
                };
                let Ok(vp) = vault_path(&full) else {
                    skipped += 1;
                    continue;
                };
                if f.mime_type == FOLDER_MIME {
                    self.remember(&full, &f.id)?;
                    dirs.push(vp);
                    queue.push_back((f.id, full));
                } else if f.mime_type.starts_with("application/vnd.google-apps.") {
                    skipped += 1; // Google-native documents have no byte content
                } else {
                    files.push(to_entry(&vp, &f));
                }
            }
        }
        if skipped > 0 {
            tracing::warn!(
                skipped,
                "gdrive: skipped entries that are not valid vault files"
            );
        }
        Ok((files, dirs))
    }
}

impl StorageProvider for GoogleDrive {
    fn kind(&self) -> &'static str {
        "gdrive"
    }

    fn list(&self) -> Result<Vec<Entry>> {
        Ok(self.walk()?.0)
    }

    fn list_dirs(&self) -> Result<Vec<VaultPath>> {
        Ok(self.walk()?.1)
    }

    fn stat(&self, path: &VaultPath) -> Result<Option<Entry>> {
        Ok(self.find_file(path)?.map(|f| to_entry(path, &f)))
    }

    fn read(&self, path: &VaultPath) -> Result<Vec<u8>> {
        let f = self
            .find_file(path)?
            .ok_or_else(|| StorageError::NotFound(path.clone()))?;
        let url = self.url(&self.api, &format!("/files/{}", f.id), &[("alt", "media")])?;
        let resp = self.send(&|c, _| c.get(url.clone()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(path.clone()));
        }
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "file download"));
        }
        resp.bytes().map(|b| b.to_vec()).map_err(net_err)
    }

    fn write(&self, path: &VaultPath, data: &[u8], mode: WriteMode) -> Result<Entry> {
        let existing = self.find_file(path)?;
        match (&mode, &existing) {
            (WriteMode::CreateNew, Some(_)) => {
                return Err(StorageError::AlreadyExists(path.clone()));
            }
            (WriteMode::Replace(expected), Some(f)) if &revision_of(f) != expected => {
                return Err(StorageError::Conflict {
                    path: path.clone(),
                    expected: expected.clone(),
                    actual: Some(revision_of(f)),
                });
            }
            (WriteMode::Replace(expected), None) => {
                return Err(StorageError::Conflict {
                    path: path.clone(),
                    expected: expected.clone(),
                    actual: None,
                });
            }
            _ => {}
        }
        let f = match existing {
            Some(f) => self.update_media(path, &f.id, data)?,
            None => self.create_file(path, data)?,
        };
        Ok(to_entry(path, &f))
    }

    fn rename(&self, from: &VaultPath, to: &VaultPath) -> Result<Entry> {
        let f = self
            .find_file(from)?
            .ok_or_else(|| StorageError::NotFound(from.clone()))?;
        if self.find_file(to)?.is_some() {
            return Err(StorageError::AlreadyExists(to.clone()));
        }
        let new_parent = self
            .dir_id(to.parent().unwrap_or(""), true)?
            .ok_or_else(|| StorageError::Internal("could not create parent folder".into()))?;
        let old_parent = self.dir_id(from.parent().unwrap_or(""), false)?;
        let mut params = vec![("fields", FIELDS), ("addParents", new_parent.as_str())];
        if let Some(op) = old_parent.as_deref() {
            params.push(("removeParents", op));
        }
        let url = self.url(&self.api, &format!("/files/{}", f.id), &params)?;
        let body = serde_json::json!({"name": to.file_name()});
        let resp = self.send(&|c, _| c.patch(url.clone()).json(&body))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(from.clone()));
        }
        let updated: DriveFile = Self::json(resp, "file rename")?;
        Ok(to_entry(to, &updated))
    }

    fn trash(&self, path: &VaultPath) -> Result<VaultPath> {
        let f = self
            .find_file(path)?
            .ok_or_else(|| StorageError::NotFound(path.clone()))?;
        let url = self.url(&self.api, &format!("/files/{}", f.id), &[("fields", "id")])?;
        let body = serde_json::json!({"trashed": true});
        let resp = self.send(&|c, _| c.patch(url.clone()).json(&body))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(path.clone()));
        }
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "file trash"));
        }
        vault_path(&format!(
            ".selenite/trash/{}-{}",
            unix_secs(),
            path.file_name()
        ))
    }

    fn create_dir(&self, path: &VaultPath) -> Result<()> {
        self.dir_id(path.as_str(), true)?
            .map(|_| ())
            .ok_or_else(|| StorageError::Internal("could not create folder".into()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::oauth::{MemoryTokenStore, OAuthConfig, TokenStore, Tokens};
    use crate::testutil::{MockServer, Req, Resp, contract, percent_decode};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    struct F {
        id: String,
        name: String,
        parent: String,
        mime: String,
        data: Vec<u8>,
        version: u64,
        trashed: bool,
    }

    #[derive(Default)]
    struct State {
        files: Vec<F>,
        next: u64,
        deletes: usize,
    }

    fn file_json(f: &F) -> serde_json::Value {
        let mut v = serde_json::json!({
            "id": f.id, "name": f.name, "mimeType": f.mime,
            "modifiedTime": "2026-01-02T03:04:05.000Z", "version": f.version.to_string(),
        });
        if f.mime != FOLDER_MIME {
            let mut h = DefaultHasher::new();
            f.data.hash(&mut h);
            v["size"] = f.data.len().to_string().into();
            v["md5Checksum"] = format!("{:016x}", h.finish()).into();
        }
        v
    }

    fn unquote(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            match c {
                '\\' => out.extend(it.next()),
                '\'' => break,
                c => out.push(c),
            }
        }
        out
    }

    fn split_multipart(body: &[u8], boundary: &str) -> Vec<Vec<u8>> {
        let delim = format!("--{boundary}").into_bytes();
        let mut parts = Vec::new();
        let mut i = 0;
        let find = |hay: &[u8], needle: &[u8], from: usize| {
            hay[from..]
                .windows(needle.len())
                .position(|w| w == needle)
                .map(|p| p + from)
        };
        while let Some(s) = find(body, &delim, i) {
            let after = s + delim.len();
            if body[after..].starts_with(b"--") {
                break;
            }
            let start = after + 2; // CRLF
            let Some(e) = find(body, &delim, start) else {
                break;
            };
            let part = &body[start..e - 2]; // strip CRLF before next delimiter
            let split = find(part, b"\r\n\r\n", 0).unwrap();
            parts.push(part[split + 4..].to_vec());
            i = e;
        }
        parts
    }

    fn handle(state: &Mutex<State>, req: &Req) -> Resp {
        if req.header("authorization") != Some("Bearer good") {
            return Resp::status(401);
        }
        let mut st = state.lock().unwrap();
        let path = req.path.as_str();
        let new_id = |st: &mut State| {
            st.next += 1;
            format!("id{}", st.next)
        };
        match (req.method.as_str(), path) {
            ("GET", "/drive/v3/files") => {
                let q = req.q("q").unwrap_or("");
                let parent = q.find("' in parents").map(|i| {
                    let s = q[..i].rfind('\'').unwrap();
                    q[s + 1..i].to_string()
                });
                let name = q.strip_prefix("name='").map(unquote);
                let mime_eq = q
                    .find("mimeType='")
                    .filter(|i| *i == 0 || &q[i - 1..*i] != "!")
                    .map(|i| unquote(&q[i + 10..]));
                let mime_ne = q.find("mimeType!='").map(|i| unquote(&q[i + 11..]));
                let hits: Vec<&F> = st
                    .files
                    .iter()
                    .filter(|f| !f.trashed)
                    .filter(|f| parent.as_deref().is_none_or(|p| f.parent == p))
                    .filter(|f| name.as_deref().is_none_or(|n| f.name == n))
                    .filter(|f| mime_eq.as_deref().is_none_or(|m| f.mime == m))
                    .filter(|f| mime_ne.as_deref().is_none_or(|m| f.mime != m))
                    .collect();
                // Page size 2 to exercise pagination.
                let off: usize = req.q("pageToken").and_then(|t| t.parse().ok()).unwrap_or(0);
                let page: Vec<_> = hits
                    .iter()
                    .skip(off)
                    .take(2)
                    .map(|f| file_json(f))
                    .collect();
                let mut v = serde_json::json!({"files": page});
                if off + 2 < hits.len() {
                    v["nextPageToken"] = (off + 2).to_string().into();
                }
                Resp::value(200, &v)
            }
            ("POST", "/drive/v3/files") => {
                let b = req.json();
                let id = new_id(&mut st);
                let f = F {
                    id,
                    name: b["name"].as_str().unwrap().into(),
                    parent: b["parents"][0].as_str().unwrap().into(),
                    mime: b["mimeType"].as_str().unwrap().into(),
                    data: vec![],
                    version: 1,
                    trashed: false,
                };
                let j = file_json(&f);
                st.files.push(f);
                Resp::value(200, &j)
            }
            ("POST", "/upload/drive/v3/files") => {
                let ct = req.header("content-type").unwrap();
                let boundary = ct.split("boundary=").nth(1).unwrap();
                let parts = split_multipart(&req.body, boundary);
                let meta: serde_json::Value = serde_json::from_slice(&parts[0]).unwrap();
                let id = new_id(&mut st);
                let f = F {
                    id,
                    name: meta["name"].as_str().unwrap().into(),
                    parent: meta["parents"][0].as_str().unwrap().into(),
                    mime: "application/octet-stream".into(),
                    data: parts[1].clone(),
                    version: 1,
                    trashed: false,
                };
                let j = file_json(&f);
                st.files.push(f);
                Resp::value(200, &j)
            }
            (m, p) => {
                let (upload, rest) = match p.strip_prefix("/upload/drive/v3/files/") {
                    Some(r) => (true, r),
                    None => match p.strip_prefix("/drive/v3/files/") {
                        Some(r) => (false, r),
                        None => return Resp::status(404),
                    },
                };
                if m == "DELETE" {
                    st.deletes += 1;
                    return Resp::status(204);
                }
                let id = percent_decode(rest);
                let new_parent = req.q("addParents").map(str::to_string);
                let Some(f) = st.files.iter_mut().find(|f| f.id == id) else {
                    return Resp::status(404);
                };
                match (m, upload) {
                    ("GET", false) if req.q("alt") == Some("media") => {
                        Resp::bytes(200, f.data.clone())
                    }
                    ("PATCH", true) => {
                        f.data = req.body.clone();
                        f.version += 1;
                        Resp::value(200, &file_json(f))
                    }
                    ("PATCH", false) => {
                        let b = req.json();
                        if let Some(n) = b["name"].as_str() {
                            f.name = n.into();
                        }
                        if let Some(t) = b["trashed"].as_bool() {
                            f.trashed = t;
                        }
                        if let Some(np) = new_parent {
                            f.parent = np;
                        }
                        f.version += 1;
                        Resp::value(200, &file_json(f))
                    }
                    _ => Resp::status(405),
                }
            }
        }
    }

    struct Fixture {
        _token: MockServer,
        _api: MockServer,
        state: Arc<Mutex<State>>,
        drive: GoogleDrive,
        token_hits: Arc<Mutex<usize>>,
    }

    fn fixture(initial_token: &str) -> Fixture {
        let token_hits = Arc::new(Mutex::new(0usize));
        let th = token_hits.clone();
        let token = MockServer::start(move |_| {
            *th.lock().unwrap() += 1;
            Resp::json(200, r#"{"access_token":"good","expires_in":3600}"#)
        });
        let state = Arc::new(Mutex::new(State::default()));
        let st = state.clone();
        let api = MockServer::start(move |req| handle(&st, req));
        let mut cfg = OAuthConfig::google_drive("cid", "sec");
        cfg.token_url = format!("{}/token", token.url());
        let store = Arc::new(MemoryTokenStore::new());
        let tokens = Tokens {
            access_token: initial_token.into(),
            refresh_token: "r1".into(),
            expires_at: i64::MAX / 2,
        };
        let auth = Arc::new(Authenticator::with_tokens(cfg, store, tokens).unwrap());
        let drive = GoogleDrive::with_base_urls(
            auth,
            "My Vault",
            &format!("{}/drive/v3", api.url()),
            &format!("{}/upload/drive/v3", api.url()),
        )
        .unwrap();
        Fixture {
            _token: token,
            _api: api,
            state,
            drive,
            token_hits,
        }
    }

    #[test]
    fn contract_tests() {
        let fx = fixture("good");
        contract(&fx.drive);
        let st = fx.state.lock().unwrap();
        assert_eq!(st.deletes, 0, "DELETE must never be used");
        // The trashed file is still present (soft delete) and the vault lives under one root.
        assert!(st.files.iter().any(|f| f.name == "b.md" && f.trashed));
        assert_eq!(st.files.iter().filter(|f| f.name == "My Vault").count(), 1);
        assert_eq!(*fx.token_hits.lock().unwrap(), 0);
    }

    #[test]
    fn refreshes_once_on_401() {
        let fx = fixture("stale");
        contract(&fx.drive);
        assert_eq!(*fx.token_hits.lock().unwrap(), 1);
    }

    #[test]
    fn quote_in_names_is_escaped() {
        let fx = fixture("good");
        let path = VaultPath::new("it's/o'k.md").unwrap();
        fx.drive.write(&path, b"x", WriteMode::CreateNew).unwrap();
        assert_eq!(fx.drive.read(&path).unwrap(), b"x");
    }

    #[test]
    fn list_on_fresh_account_is_empty_and_creates_nothing() {
        let fx = fixture("good");
        assert!(fx.drive.list().unwrap().is_empty());
        assert!(fx.state.lock().unwrap().files.is_empty());
    }

    #[test]
    fn offline_is_reported_as_offline() {
        let dead = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let store = Arc::new(MemoryTokenStore::new());
        let tokens = Tokens {
            access_token: "good".into(),
            refresh_token: "r1".into(),
            expires_at: i64::MAX / 2,
        };
        store.save("r1").unwrap();
        let auth = Arc::new(
            Authenticator::with_tokens(OAuthConfig::google_drive("c", "s"), store, tokens).unwrap(),
        );
        let base = format!("http://127.0.0.1:{dead}");
        let d = GoogleDrive::with_base_urls(auth, "v", &base, &base).unwrap();
        match d.stat(&VaultPath::new("a.md").unwrap()) {
            Err(StorageError::Internal(m)) => assert!(m.starts_with("offline:"), "{m}"),
            other => panic!("expected offline, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "live test: needs SELENITE_TEST_GOOGLE_CLIENT_ID/SECRET and an interactive sign-in"]
    fn live_google_drive() {
        let id = std::env::var("SELENITE_TEST_GOOGLE_CLIENT_ID").unwrap();
        let secret = std::env::var("SELENITE_TEST_GOOGLE_CLIENT_SECRET").unwrap();
        let cfg = OAuthConfig::google_drive(&id, &secret);
        let tokens = crate::oauth::authorize(
            &cfg,
            &crate::oauth::default_open_browser,
            std::time::Duration::from_secs(180),
        )
        .unwrap();
        let auth = Arc::new(
            Authenticator::with_tokens(cfg, Arc::new(MemoryTokenStore::new()), tokens).unwrap(),
        );
        let d = GoogleDrive::new(auth, "selenite-live-test").unwrap();
        d.create_dir(&VaultPath::new("live").unwrap()).unwrap();
        let p = VaultPath::new("live/hello.md").unwrap();
        d.write(&p, b"hello", WriteMode::Overwrite).unwrap();
        assert!(d.list().unwrap().iter().any(|e| e.path == p));
        assert_eq!(d.read(&p).unwrap(), b"hello");
        d.trash(&p).unwrap();
    }
}

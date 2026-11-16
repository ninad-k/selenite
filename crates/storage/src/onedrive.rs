//! OneDrive provider over Microsoft Graph, confined to the app's own folder
//! (`/me/drive/special/approot`, scope `Files.ReadWrite.AppFolder`, usually
//! `OneDrive/Apps/<app name>`). `VaultPath`s are addressed relative to that folder using Graph's
//! path syntax (`approot:/<path>:/content`).
//!
//! * **Revision** is the item's `eTag`.
//! * **Conditional writes**: `Replace` sends `If-Match: <eTag>` (a `412` becomes `Conflict`) and
//!   additionally pre-checks the current eTag, because Microsoft does not document `If-Match` for
//!   the simple `PUT .../content` call as clearly as for upload sessions; the pre-check narrows the
//!   race to one round trip if the header is ignored. `CreateNew` uses
//!   `@microsoft.graph.conflictBehavior=fail` (`409` becomes `AlreadyExists`).
//! * Files larger than 4 MB go through an upload session (chunks are sent without the bearer
//!   token, as the pre-authenticated `uploadUrl` requires).
//! * **Trash** is `DELETE /me/drive/items/{id}`. Graph does not hard-delete: the item moves to the
//!   OneDrive recycle bin, which is this provider's soft delete (recoverable by the user until the
//!   bin is emptied or retention expires). The returned path is a nominal
//!   `.selenite/trash/<secs>-<name>` and is not readable through this provider.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use reqwest::StatusCode;
use reqwest::Url;
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::Deserialize;

use crate::oauth::Authenticator;
use crate::util::{http_client, net_err, parse_rfc3339, status_error};
use crate::{
    Entry, Result, Revision, StorageError, StorageProvider, VaultPath, WriteMode, unix_secs,
    vault_path,
};

const DEFAULT_BASE: &str = "https://graph.microsoft.com/v1.0";
/// Largest body sent with a single `PUT .../content` (Graph limit is 4 MB).
const SMALL_LIMIT: usize = 4 * 1024 * 1024;
/// Upload-session chunk size; must be a multiple of 320 KiB.
const CHUNK: usize = 32 * 320 * 1024;
const SELECT: &str = "id,name,size,eTag,cTag,lastModifiedDateTime,file,folder";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    id: String,
    #[serde(default)]
    name: String,
    size: Option<u64>,
    e_tag: Option<String>,
    c_tag: Option<String>,
    last_modified_date_time: Option<String>,
    folder: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Children {
    #[serde(default)]
    value: Vec<Item>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSession {
    upload_url: String,
}

fn revision_of(i: &Item) -> Revision {
    Revision(
        i.e_tag
            .clone()
            .or_else(|| i.c_tag.clone())
            .unwrap_or_default(),
    )
}

fn to_entry(path: &VaultPath, i: &Item) -> Entry {
    Entry {
        path: path.clone(),
        size: i.size.unwrap_or(0),
        modified: i
            .last_modified_date_time
            .as_deref()
            .and_then(parse_rfc3339)
            .unwrap_or(SystemTime::UNIX_EPOCH),
        revision: revision_of(i),
    }
}

/// OneDrive storage in the app folder.
pub struct OneDrive {
    auth: Arc<Authenticator>,
    client: Client,
    base: String,
    small_limit: usize,
    chunk: usize,
}

impl std::fmt::Debug for OneDrive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneDrive").finish_non_exhaustive()
    }
}

impl OneDrive {
    /// Use the production Graph endpoint.
    pub fn new(auth: Arc<Authenticator>) -> Result<Self> {
        Self::with_base_url(auth, DEFAULT_BASE)
    }

    /// Override the Graph base URL (no trailing slash), e.g. `https://graph.microsoft.com/v1.0`.
    pub fn with_base_url(auth: Arc<Authenticator>, base: &str) -> Result<Self> {
        Ok(Self {
            auth,
            client: http_client()?,
            base: base.trim_end_matches('/').to_string(),
            small_limit: SMALL_LIMIT,
            chunk: CHUNK,
        })
    }

    /// URL addressing an item by path inside the app folder (`None` = the folder itself), with an
    /// optional trailing action segment such as `content` or `children`.
    fn path_url(&self, path: Option<&str>, action: Option<&str>) -> Result<Url> {
        let mut u = Url::parse(&self.base)
            .map_err(|e| StorageError::Internal(format!("bad base url: {e}")))?;
        {
            let mut segs = u
                .path_segments_mut()
                .map_err(|_| StorageError::Internal("bad base url".into()))?;
            segs.pop_if_empty();
            match path {
                None => {
                    segs.extend(["me", "drive", "special", "approot"]);
                }
                Some(p) => {
                    segs.extend(["me", "drive", "special", "approot:"]);
                    let parts: Vec<&str> = p.split('/').collect();
                    for (i, part) in parts.iter().enumerate() {
                        if i + 1 == parts.len() {
                            segs.push(&format!("{part}:"));
                        } else {
                            segs.push(part);
                        }
                    }
                }
            }
            if let Some(a) = action {
                segs.push(a);
            }
        }
        Ok(u)
    }

    fn id_url(&self, id: &str, action: Option<&str>) -> Result<Url> {
        let mut u = Url::parse(&self.base)
            .map_err(|e| StorageError::Internal(format!("bad base url: {e}")))?;
        {
            let mut segs = u
                .path_segments_mut()
                .map_err(|_| StorageError::Internal("bad base url".into()))?;
            segs.pop_if_empty();
            segs.extend(["me", "drive", "items", id]);
            if let Some(a) = action {
                segs.push(a);
            }
        }
        Ok(u)
    }

    fn send(&self, build: &dyn Fn(&Client) -> RequestBuilder) -> Result<Response> {
        self.auth.send(&|tok| build(&self.client).bearer_auth(tok))
    }

    fn json<T: for<'de> Deserialize<'de>>(resp: Response, what: &str) -> Result<T> {
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), what));
        }
        resp.json().map_err(|_| {
            StorageError::Internal(format!("remote: unreadable response during {what}"))
        })
    }

    fn get_item(&self, path: Option<&str>) -> Result<Option<Item>> {
        let mut url = self.path_url(path, None)?;
        url.query_pairs_mut().append_pair("$select", SELECT);
        let resp = self.send(&|c| c.get(url.clone()))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::json(resp, "item lookup")?))
    }

    fn stat_file(&self, path: &VaultPath) -> Result<Option<Item>> {
        Ok(self
            .get_item(Some(path.as_str()))?
            .filter(|i| i.folder.is_none()))
    }

    fn children(&self, first: Url) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut next = Some(first);
        while let Some(url) = next.take() {
            let resp = self.send(&|c| c.get(url.clone()))?;
            let page: Children = Self::json(resp, "list children")?;
            out.extend(page.value);
            if let Some(link) = page.next_link {
                // Only follow continuation links that stay on the configured Graph endpoint.
                if !link.starts_with(&self.base) {
                    return Err(StorageError::Internal(
                        "remote: unexpected continuation link".into(),
                    ));
                }
                next =
                    Some(Url::parse(&link).map_err(|_| {
                        StorageError::Internal("remote: bad continuation link".into())
                    })?);
            }
        }
        Ok(out)
    }

    fn walk(&self) -> Result<(Vec<Entry>, Vec<VaultPath>)> {
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        let mut skipped = 0usize;
        let mut first = self.path_url(None, Some("children"))?;
        first
            .query_pairs_mut()
            .append_pair("$select", SELECT)
            .append_pair("$top", "200");
        let mut queue = VecDeque::from([(first, String::new())]);
        while let Some((url, prefix)) = queue.pop_front() {
            for i in self.children(url)? {
                let full = if prefix.is_empty() {
                    i.name.clone()
                } else {
                    format!("{prefix}/{}", i.name)
                };
                let Ok(vp) = vault_path(&full) else {
                    skipped += 1;
                    continue;
                };
                if i.folder.is_some() {
                    let mut u = self.id_url(&i.id, Some("children"))?;
                    u.query_pairs_mut()
                        .append_pair("$select", SELECT)
                        .append_pair("$top", "200");
                    queue.push_back((u, full));
                    dirs.push(vp);
                } else {
                    files.push(to_entry(&vp, &i));
                }
            }
        }
        if skipped > 0 {
            tracing::warn!(
                skipped,
                "onedrive: skipped entries that are not valid vault files"
            );
        }
        Ok((files, dirs))
    }

    fn conflict(&self, path: &VaultPath, expected: &Revision) -> Result<StorageError> {
        let actual = self.stat_file(path)?.map(|i| revision_of(&i));
        Ok(StorageError::Conflict {
            path: path.clone(),
            expected: expected.clone(),
            actual,
        })
    }

    fn put_small(&self, path: &VaultPath, data: &[u8], mode: &WriteMode) -> Result<Entry> {
        let mut url = self.path_url(Some(path.as_str()), Some("content"))?;
        url.query_pairs_mut().append_pair("$select", SELECT);
        if matches!(mode, WriteMode::CreateNew) {
            url.query_pairs_mut()
                .append_pair("@microsoft.graph.conflictBehavior", "fail");
        }
        let if_match = match mode {
            WriteMode::Replace(r) => Some(r.0.clone()),
            _ => None,
        };
        let resp = self.send(&|c| {
            let mut rb = c
                .put(url.clone())
                .header("Content-Type", "application/octet-stream")
                .body(data.to_vec());
            if let Some(m) = &if_match {
                rb = rb.header("If-Match", m.as_str());
            }
            rb
        })?;
        self.finish_write(path, mode, resp, "file upload")
    }

    fn finish_write(
        &self,
        path: &VaultPath,
        mode: &WriteMode,
        resp: Response,
        what: &str,
    ) -> Result<Entry> {
        match (resp.status(), mode) {
            (StatusCode::CONFLICT, WriteMode::CreateNew) => {
                Err(StorageError::AlreadyExists(path.clone()))
            }
            (StatusCode::PRECONDITION_FAILED | StatusCode::NOT_FOUND, WriteMode::Replace(r)) => {
                Err(self.conflict(path, r)?)
            }
            _ => {
                let item: Item = Self::json(resp, what)?;
                Ok(to_entry(path, &item))
            }
        }
    }

    fn put_session(&self, path: &VaultPath, data: &[u8], mode: &WriteMode) -> Result<Entry> {
        let url = self.path_url(Some(path.as_str()), Some("createUploadSession"))?;
        let behavior = if matches!(mode, WriteMode::CreateNew) {
            "fail"
        } else {
            "replace"
        };
        let body = serde_json::json!({"item": {"@microsoft.graph.conflictBehavior": behavior}});
        let if_match = match mode {
            WriteMode::Replace(r) => Some(r.0.clone()),
            _ => None,
        };
        let resp = self.send(&|c| {
            let mut rb = c.post(url.clone()).json(&body);
            if let Some(m) = &if_match {
                rb = rb.header("If-Match", m.as_str());
            }
            rb
        })?;
        match (resp.status(), mode) {
            (StatusCode::CONFLICT, WriteMode::CreateNew) => {
                return Err(StorageError::AlreadyExists(path.clone()));
            }
            (StatusCode::PRECONDITION_FAILED | StatusCode::NOT_FOUND, WriteMode::Replace(r)) => {
                return Err(self.conflict(path, r)?);
            }
            _ => {}
        }
        let session: UploadSession = Self::json(resp, "create upload session")?;
        let total = data.len();
        let mut start = 0;
        while start < total {
            let end = (start + self.chunk).min(total);
            // The upload URL is pre-authenticated: never attach the bearer token to it.
            let resp = self
                .client
                .put(&session.upload_url)
                .header(
                    "Content-Range",
                    format!("bytes {}-{}/{total}", start, end - 1),
                )
                .body(data[start..end].to_vec())
                .send()
                .map_err(net_err)?;
            if end == total {
                let item: Item = Self::json(resp, "upload chunk")?;
                return Ok(to_entry(path, &item));
            }
            if resp.status() != StatusCode::ACCEPTED && !resp.status().is_success() {
                return Err(status_error(resp.status(), "upload chunk"));
            }
            start = end;
        }
        Err(StorageError::Internal("upload session ended early".into()))
    }

    fn make_dir(&self, parent: Option<&str>, name: &str) -> Result<()> {
        let url = self.path_url(parent, Some("children"))?;
        let body = serde_json::json!({
            "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail"
        });
        let resp = self.send(&|c| c.post(url.clone()).json(&body))?;
        match resp.status() {
            StatusCode::CONFLICT => Ok(()), // already exists
            s if s.is_success() => Ok(()),
            s => Err(status_error(s, "folder create")),
        }
    }
}

impl StorageProvider for OneDrive {
    fn kind(&self) -> &'static str {
        "onedrive"
    }

    fn list(&self) -> Result<Vec<Entry>> {
        Ok(self.walk()?.0)
    }

    fn list_dirs(&self) -> Result<Vec<VaultPath>> {
        Ok(self.walk()?.1)
    }

    fn stat(&self, path: &VaultPath) -> Result<Option<Entry>> {
        Ok(self.stat_file(path)?.map(|i| to_entry(path, &i)))
    }

    fn read(&self, path: &VaultPath) -> Result<Vec<u8>> {
        // Graph answers with a 302 to a pre-authenticated download URL; reqwest follows it and
        // does not forward the Authorization header to a different host.
        let url = self.path_url(Some(path.as_str()), Some("content"))?;
        let resp = self.send(&|c| c.get(url.clone()))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(path.clone()));
        }
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "file download"));
        }
        resp.bytes().map(|b| b.to_vec()).map_err(net_err)
    }

    fn write(&self, path: &VaultPath, data: &[u8], mode: WriteMode) -> Result<Entry> {
        if let WriteMode::Replace(expected) = &mode {
            // Pre-check (see module docs); the If-Match header below is the enforced guard.
            match self.stat_file(path)? {
                Some(cur) if &revision_of(&cur) == expected => {}
                Some(cur) => {
                    return Err(StorageError::Conflict {
                        path: path.clone(),
                        expected: expected.clone(),
                        actual: Some(revision_of(&cur)),
                    });
                }
                None => {
                    return Err(StorageError::Conflict {
                        path: path.clone(),
                        expected: expected.clone(),
                        actual: None,
                    });
                }
            }
        }
        if data.len() > self.small_limit {
            self.put_session(path, data, &mode)
        } else {
            self.put_small(path, data, &mode)
        }
    }

    fn rename(&self, from: &VaultPath, to: &VaultPath) -> Result<Entry> {
        let item = self
            .stat_file(from)?
            .ok_or_else(|| StorageError::NotFound(from.clone()))?;
        if self.stat_file(to)?.is_some() {
            return Err(StorageError::AlreadyExists(to.clone()));
        }
        let mut body = serde_json::json!({"name": to.file_name()});
        if from.parent() != to.parent() {
            if let Some(dir) = to.parent() {
                self.create_dir(&vault_path(dir)?)?;
            }
            let parent = self
                .get_item(to.parent())?
                .ok_or_else(|| StorageError::Internal("target folder vanished".into()))?;
            body["parentReference"] = serde_json::json!({"id": parent.id});
        }
        body["@microsoft.graph.conflictBehavior"] = "fail".into();
        let mut url = self.id_url(&item.id, None)?;
        url.query_pairs_mut().append_pair("$select", SELECT);
        let resp = self.send(&|c| c.patch(url.clone()).json(&body))?;
        match resp.status() {
            StatusCode::NOT_FOUND => Err(StorageError::NotFound(from.clone())),
            StatusCode::CONFLICT => Err(StorageError::AlreadyExists(to.clone())),
            _ => {
                let moved: Item = Self::json(resp, "file rename")?;
                Ok(to_entry(to, &moved))
            }
        }
    }

    fn trash(&self, path: &VaultPath) -> Result<VaultPath> {
        let item = self
            .stat_file(path)?
            .ok_or_else(|| StorageError::NotFound(path.clone()))?;
        let url = self.id_url(&item.id, None)?;
        let resp = self.send(&|c| c.delete(url.clone()))?;
        match resp.status() {
            StatusCode::NOT_FOUND => return Err(StorageError::NotFound(path.clone())),
            s if !s.is_success() => return Err(status_error(s, "file delete (recycle bin)")),
            _ => {}
        }
        vault_path(&format!(
            ".selenite/trash/{}-{}",
            unix_secs(),
            path.file_name()
        ))
    }

    fn create_dir(&self, path: &VaultPath) -> Result<()> {
        let mut parent: Option<String> = None;
        for seg in path.as_str().split('/') {
            self.make_dir(parent.as_deref(), seg)?;
            parent = Some(match parent {
                Some(p) => format!("{p}/{seg}"),
                None => seg.to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::oauth::{MemoryTokenStore, OAuthConfig, Tokens};
    use crate::testutil::{MockServer, Req, Resp, contract, percent_decode};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct I {
        id: String,
        name: String,
        parent: String, // "root" or an id
        folder: bool,
        data: Vec<u8>,
        etag: u64,
    }

    struct Session {
        path: String,
        buf: Vec<u8>,
        replace: bool,
    }

    #[derive(Default)]
    struct State {
        items: Vec<I>,
        next: u64,
        sessions: HashMap<String, Session>,
        deleted: Vec<String>,
    }

    fn item_json(i: &I) -> serde_json::Value {
        let mut v = serde_json::json!({
            "id": i.id, "name": i.name, "size": i.data.len(),
            "eTag": format!("\"{},{}\"", i.id, i.etag),
            "lastModifiedDateTime": "2026-01-02T03:04:05Z",
        });
        if i.folder {
            v["folder"] = serde_json::json!({});
        } else {
            v["file"] = serde_json::json!({});
        }
        v
    }

    impl State {
        fn find(&self, parent: &str, name: &str) -> Option<usize> {
            self.items
                .iter()
                .position(|i| i.parent == parent && i.name == name)
        }

        fn resolve(&self, path: &str) -> Option<usize> {
            let mut parent = "root".to_string();
            let mut idx = None;
            for seg in path.split('/').filter(|s| !s.is_empty()) {
                let k = self.find(&parent, seg)?;
                parent = self.items[k].id.clone();
                idx = Some(k);
            }
            idx
        }

        fn new_item(&mut self, parent: &str, name: &str, folder: bool, data: Vec<u8>) -> usize {
            self.next += 1;
            self.items.push(I {
                id: format!("it{}", self.next),
                name: name.into(),
                parent: parent.into(),
                folder,
                data,
                etag: 1,
            });
            self.items.len() - 1
        }

        fn ensure_dirs(&mut self, dirs: &[&str]) -> String {
            let mut parent = "root".to_string();
            for d in dirs {
                let k = match self.find(&parent, d) {
                    Some(k) => k,
                    None => self.new_item(&parent, d, true, vec![]),
                };
                parent = self.items[k].id.clone();
            }
            parent
        }

        /// Create or replace a file at `path`, honouring conflict behaviour.
        fn put(
            &mut self,
            path: &str,
            data: Vec<u8>,
            fail_if_exists: bool,
        ) -> std::result::Result<usize, u16> {
            let segs: Vec<&str> = path.split('/').collect();
            let (name, dirs) = segs.split_last().unwrap();
            let parent = self.ensure_dirs(dirs);
            match self.find(&parent, name) {
                Some(_) if fail_if_exists => Err(409),
                Some(k) => {
                    self.items[k].data = data;
                    self.items[k].etag += 1;
                    Ok(k)
                }
                None => Ok(self.new_item(&parent, name, false, data)),
            }
        }
    }

    fn list_page(st: &State, parent: &str, req: &Req, host: &str, route: &str) -> Resp {
        let kids: Vec<&I> = st.items.iter().filter(|i| i.parent == parent).collect();
        let off: usize = req.q("skip").and_then(|s| s.parse().ok()).unwrap_or(0);
        let page: Vec<_> = kids
            .iter()
            .skip(off)
            .take(2)
            .map(|i| item_json(i))
            .collect();
        let mut v = serde_json::json!({"value": page});
        if off + 2 < kids.len() {
            v["@odata.nextLink"] = format!("http://{host}/v1.0{route}?skip={}", off + 2).into();
        }
        Resp::value(200, &v)
    }

    fn handle(state: &Mutex<State>, req: &Req) -> Resp {
        let host = req.header("host").unwrap_or("").to_string();
        let mut st = state.lock().unwrap();
        // Pre-authenticated endpoints (the upload URL must NOT carry the bearer token).
        if let Some(sid) = req.path.strip_prefix("/upload/") {
            if req.header("authorization").is_some() {
                return Resp::status(400);
            }
            let range = req.header("content-range").unwrap();
            let (span, total) = range.trim_start_matches("bytes ").split_once('/').unwrap();
            let (a, b) = span.split_once('-').unwrap();
            let (a, b, total): (usize, usize, usize) = (
                a.parse().unwrap(),
                b.parse().unwrap(),
                total.parse().unwrap(),
            );
            let s = st.sessions.get_mut(sid).unwrap();
            assert_eq!(a, s.buf.len(), "chunks must arrive in order");
            s.buf.extend_from_slice(&req.body);
            if b + 1 < total {
                return Resp::json(202, r#"{"nextExpectedRanges":[]}"#);
            }
            let (path, data, _replace) = (s.path.clone(), s.buf.clone(), s.replace);
            let k = st.put(&path, data, false).unwrap();
            return Resp::value(201, &item_json(&st.items[k]));
        }
        if let Some(id) = req.path.strip_prefix("/dl/") {
            return match st.items.iter().find(|i| i.id == id) {
                Some(i) => Resp::bytes(200, i.data.clone()),
                _ => Resp::status(400),
            };
        }
        if req.header("authorization") != Some("Bearer good") {
            return Resp::status(401);
        }
        let Some(p) = req.path.strip_prefix("/v1.0") else {
            return Resp::status(404);
        };
        let m = req.method.as_str();
        if p == "/me/drive/special/approot/children" {
            return match m {
                "GET" => list_page(&st, "root", req, &host, p),
                "POST" => create_folder(&mut st, "root", req),
                _ => Resp::status(405),
            };
        }
        if let Some(rest) = p.strip_prefix("/me/drive/items/") {
            let (id, action) = match rest.split_once('/') {
                Some((i, a)) => (i, a),
                None => (rest, ""),
            };
            if action == "children" {
                return match m {
                    "GET" => list_page(&st, id, req, &host, p),
                    "POST" => create_folder(&mut st, id, req),
                    _ => Resp::status(405),
                };
            }
            let Some(k) = st.items.iter().position(|i| i.id == id) else {
                return Resp::status(404);
            };
            return match m {
                "DELETE" => {
                    let it = st.items.remove(k);
                    st.deleted.push(it.name.clone());
                    // keep it around so tests can see it went to the "bin"
                    st.items.push(I {
                        parent: "recycle-bin".into(),
                        ..it
                    });
                    Resp::status(204)
                }
                "PATCH" => {
                    let b = req.json();
                    let new_name = b["name"].as_str().unwrap().to_string();
                    let new_parent = b["parentReference"]["id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| st.items[k].parent.clone());
                    if st.find(&new_parent, &new_name).is_some() {
                        return Resp::status(409);
                    }
                    let it = &mut st.items[k];
                    it.name = new_name;
                    it.parent = new_parent;
                    it.etag += 1;
                    Resp::value(200, &item_json(it))
                }
                _ => Resp::status(405),
            };
        }
        if p == "/me/drive/special/approot" && m == "GET" {
            return Resp::value(
                200,
                &serde_json::json!({"id": "root", "name": "approot", "folder": {}}),
            );
        }
        let Some(rest) = p.strip_prefix("/me/drive/special/approot:") else {
            return Resp::status(404);
        };
        let colon = rest.rfind(':').unwrap();
        let path = percent_decode(rest[..colon].trim_start_matches('/'));
        let action = &rest[colon + 1..];
        match (m, action) {
            ("GET", "") => match st.resolve(&path) {
                Some(k) => Resp::value(200, &item_json(&st.items[k])),
                None => Resp::status(404),
            },
            ("GET", "/content") => match st.resolve(&path) {
                Some(k) => Resp::status(302)
                    .header("Location", &format!("http://{host}/dl/{}", st.items[k].id)),
                None => Resp::status(404),
            },
            ("POST", "/children") => {
                let parent = match st.resolve(&path) {
                    Some(k) => st.items[k].id.clone(),
                    None => return Resp::status(404),
                };
                create_folder(&mut st, &parent, req)
            }
            ("PUT", "/content") => {
                let existing = st.resolve(&path);
                if let (Some(k), Some(im)) = (existing, req.header("if-match")) {
                    if format!("\"{},{}\"", st.items[k].id, st.items[k].etag) != im {
                        return Resp::status(412);
                    }
                }
                if existing.is_none() && req.header("if-match").is_some() {
                    return Resp::status(404);
                }
                let fail = req.q("@microsoft.graph.conflictBehavior") == Some("fail");
                match st.put(&path, req.body.clone(), fail) {
                    Ok(k) => Resp::value(201, &item_json(&st.items[k])),
                    Err(code) => Resp::json(code, r#"{"error":{"code":"nameAlreadyExists"}}"#),
                }
            }
            ("POST", "/createUploadSession") => {
                let existing = st.resolve(&path);
                let b = req.json();
                let replace = b["item"]["@microsoft.graph.conflictBehavior"] == "replace";
                if existing.is_some() && !replace {
                    return Resp::status(409);
                }
                if let (Some(k), Some(im)) = (existing, req.header("if-match")) {
                    if format!("\"{},{}\"", st.items[k].id, st.items[k].etag) != im {
                        return Resp::status(412);
                    }
                }
                st.next += 1;
                let sid = format!("s{}", st.next);
                st.sessions.insert(
                    sid.clone(),
                    Session {
                        path,
                        buf: vec![],
                        replace,
                    },
                );
                Resp::value(
                    200,
                    &serde_json::json!({"uploadUrl": format!("http://{host}/upload/{sid}")}),
                )
            }
            _ => Resp::status(405),
        }
    }

    fn create_folder(st: &mut State, parent: &str, req: &Req) -> Resp {
        let b = req.json();
        let name = b["name"].as_str().unwrap();
        if st.find(parent, name).is_some() {
            return Resp::status(409);
        }
        let k = st.new_item(parent, name, true, vec![]);
        Resp::value(201, &item_json(&st.items[k]))
    }

    struct Fixture {
        _token: MockServer,
        _api: MockServer,
        state: Arc<Mutex<State>>,
        drive: OneDrive,
        token_hits: Arc<Mutex<usize>>,
    }

    fn fixture(initial_token: &str) -> Fixture {
        let token_hits = Arc::new(Mutex::new(0usize));
        let th = token_hits.clone();
        let token = MockServer::start(move |_| {
            *th.lock().unwrap() += 1;
            Resp::json(
                200,
                r#"{"access_token":"good","refresh_token":"r2","expires_in":3600}"#,
            )
        });
        let state = Arc::new(Mutex::new(State::default()));
        let st = state.clone();
        let api = MockServer::start(move |req| handle(&st, req));
        let mut cfg = OAuthConfig::onedrive("cid");
        cfg.token_url = format!("{}/token", token.url());
        let store = Arc::new(MemoryTokenStore::new());
        let tokens = Tokens {
            access_token: initial_token.into(),
            refresh_token: "r1".into(),
            expires_at: i64::MAX / 2,
        };
        let auth = Arc::new(Authenticator::with_tokens(cfg, store, tokens).unwrap());
        let drive = OneDrive::with_base_url(auth, &format!("{}/v1.0", api.url())).unwrap();
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
        // Trash went through DELETE (recycle bin), and the item is not in the live tree.
        assert_eq!(st.deleted, ["b.md"]);
        assert_eq!(*fx.token_hits.lock().unwrap(), 0);
    }

    #[test]
    fn refreshes_once_on_401() {
        let fx = fixture("stale");
        contract(&fx.drive);
        assert_eq!(*fx.token_hits.lock().unwrap(), 1);
    }

    #[test]
    fn large_files_use_upload_sessions() {
        let fx = fixture("good");
        let drive = OneDrive {
            small_limit: 10,
            chunk: 10,
            ..fx.drive
        };
        let p = VaultPath::new("big/file.bin").unwrap();
        let data: Vec<u8> = (0..25u8).collect();
        let e = drive.write(&p, &data, WriteMode::CreateNew).unwrap();
        assert_eq!(e.size, 25);
        assert_eq!(drive.read(&p).unwrap(), data);
        assert!(matches!(
            drive.write(&p, &data, WriteMode::CreateNew),
            Err(StorageError::AlreadyExists(_))
        ));
        let data2: Vec<u8> = (100..130u8).collect();
        let e2 = drive
            .write(&p, &data2, WriteMode::Replace(e.revision.clone()))
            .unwrap();
        assert_eq!(drive.read(&p).unwrap(), data2);
        assert!(matches!(
            drive.write(&p, &data, WriteMode::Replace(e.revision)),
            Err(StorageError::Conflict { .. })
        ));
        assert_eq!(drive.stat(&p).unwrap().unwrap().revision, e2.revision);
    }

    #[test]
    fn if_match_is_enforced_server_side_too() {
        // Bypass the pre-check to prove the header alone yields a Conflict.
        let fx = fixture("good");
        let p = VaultPath::new("n.md").unwrap();
        let e = fx.drive.write(&p, b"1", WriteMode::CreateNew).unwrap();
        fx.drive.write(&p, b"2", WriteMode::Overwrite).unwrap();
        let r = fx
            .drive
            .put_small(&p, b"3", &WriteMode::Replace(e.revision))
            .unwrap_err();
        assert!(matches!(r, StorageError::Conflict { .. }), "{r:?}");
        assert_eq!(fx.drive.read(&p).unwrap(), b"2");
    }

    #[test]
    fn special_characters_in_names() {
        let fx = fixture("good");
        let p = VaultPath::new("my notes/a b#c%d.md").unwrap();
        fx.drive.write(&p, b"x", WriteMode::CreateNew).unwrap();
        assert_eq!(fx.drive.read(&p).unwrap(), b"x");
        assert!(fx.drive.stat(&p).unwrap().is_some());
    }

    #[test]
    fn offline_is_reported_as_offline() {
        let dead = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let tokens = Tokens {
            access_token: "good".into(),
            refresh_token: "r1".into(),
            expires_at: i64::MAX / 2,
        };
        let auth = Arc::new(
            Authenticator::with_tokens(
                OAuthConfig::onedrive("c"),
                Arc::new(MemoryTokenStore::new()),
                tokens,
            )
            .unwrap(),
        );
        let d = OneDrive::with_base_url(auth, &format!("http://127.0.0.1:{dead}/v1.0")).unwrap();
        match d.stat(&VaultPath::new("a.md").unwrap()) {
            Err(StorageError::Internal(m)) => assert!(m.starts_with("offline:"), "{m}"),
            other => panic!("expected offline, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "live test: needs SELENITE_TEST_ONEDRIVE_CLIENT_ID and an interactive sign-in"]
    fn live_onedrive() {
        let id = std::env::var("SELENITE_TEST_ONEDRIVE_CLIENT_ID").unwrap();
        let cfg = OAuthConfig::onedrive(&id);
        let tokens = crate::oauth::authorize(
            &cfg,
            &crate::oauth::default_open_browser,
            std::time::Duration::from_secs(180),
        )
        .unwrap();
        let auth = Arc::new(
            Authenticator::with_tokens(cfg, Arc::new(MemoryTokenStore::new()), tokens).unwrap(),
        );
        let d = OneDrive::new(auth).unwrap();
        let dir = VaultPath::new("selenite-live-test").unwrap();
        d.create_dir(&dir).unwrap();
        let p = VaultPath::new("selenite-live-test/hello.md").unwrap();
        d.write(&p, b"hello", WriteMode::Overwrite).unwrap();
        assert!(d.list().unwrap().iter().any(|e| e.path == p));
        assert_eq!(d.read(&p).unwrap(), b"hello");
        d.trash(&p).unwrap();
    }
}

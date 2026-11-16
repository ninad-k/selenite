//! The three-way sync engine.

use std::collections::{BTreeMap, BTreeSet};

use selenite_core::VaultPath;
use selenite_storage::{Entry, StorageError, StorageProvider, WriteMode, content_hash};

use crate::SyncError;
use crate::manifest::{ConflictState, Manifest, ManifestEntry, is_synced, sanitize_id};
use crate::queue::{OfflineQueue, is_offline};

type R<T> = Result<T, StorageError>;

/// A path whose two versions diverged, and where the other side's version was saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictInfo {
    /// The conflicted path. Its local content was left untouched.
    pub path: String,
    /// The conflict copy holding the remote version (exists on both sides).
    pub copy_path: String,
    /// The conflict copy holding the local version (exists on both sides).
    pub local_copy_path: String,
}

/// What one [`sync`] run did. Paths only, never content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Paths uploaded to the remote.
    pub pushed: Vec<String>,
    /// Paths downloaded to the local side.
    pub pulled: Vec<String>,
    /// New conflicts found in this run.
    pub conflicts: Vec<ConflictInfo>,
    /// Paths moved to local trash because the remote deleted them.
    pub trashed_local: Vec<String>,
    /// Paths moved to remote trash because the local side deleted them.
    pub trashed_remote: Vec<String>,
    /// Per-path failures `(path, message)`; they do not abort the run.
    pub errors: Vec<(String, String)>,
    /// Number of paths left untouched because of an unresolved conflict.
    pub skipped: usize,
    /// Paths with an unresolved conflict (new or older).
    pub unresolved: Vec<String>,
    /// True if the run stopped because the remote was unreachable.
    pub offline: bool,
}

impl SyncReport {
    /// True if the run changed nothing and hit no error (unresolved conflicts do not count).
    pub fn is_noop(&self) -> bool {
        self.pushed.is_empty()
            && self.pulled.is_empty()
            && self.conflicts.is_empty()
            && self.trashed_local.is_empty()
            && self.trashed_remote.is_empty()
            && self.errors.is_empty()
            && !self.offline
    }
}

fn is_race(e: &StorageError) -> bool {
    matches!(
        e,
        StorageError::Conflict { .. } | StorageError::AlreadyExists(_) | StorageError::NotFound(_)
    )
}

struct Probe {
    changed: bool,
    bytes: Option<Vec<u8>>,
    hash: Option<String>,
}

/// Has the file changed since the base? Revision first (cheap), confirmed by content hash.
fn probe(
    p: &dyn StorageProvider,
    path: &VaultPath,
    e: &Entry,
    base_rev: &str,
    base_hash: &str,
) -> R<Probe> {
    if e.revision.0 == base_rev {
        return Ok(Probe {
            changed: false,
            bytes: None,
            hash: None,
        });
    }
    let bytes = p.read(path)?;
    let hash = content_hash(&bytes);
    Ok(Probe {
        changed: hash != base_hash,
        bytes: Some(bytes),
        hash: Some(hash),
    })
}

fn read_hash(p: &dyn StorageProvider, path: &VaultPath) -> R<(Vec<u8>, String)> {
    let bytes = p.read(path)?;
    let hash = content_hash(&bytes);
    Ok((bytes, hash))
}

fn bytes_of(pr: Probe, p: &dyn StorageProvider, path: &VaultPath) -> R<(Vec<u8>, String)> {
    match (pr.bytes, pr.hash) {
        (Some(b), Some(h)) => Ok((b, h)),
        _ => read_hash(p, path),
    }
}

fn dirty_local(l: Option<&Entry>, m: Option<&ManifestEntry>) -> bool {
    match (l, m) {
        (Some(l), Some(m)) => l.revision.0 != m.local_rev,
        (None, None) => false,
        _ => true,
    }
}

struct Engine<'a> {
    local: &'a dyn StorageProvider,
    remote: &'a dyn StorageProvider,
    tag: String,
    now: i64,
    manifest: Manifest,
    report: SyncReport,
    followups: Vec<VaultPath>,
}

impl Engine<'_> {
    fn record(&mut self, path: &VaultPath, local_rev: &str, remote_rev: &str, hash: String) {
        self.manifest.entries.insert(
            path.to_string(),
            ManifestEntry {
                hash,
                local_rev: local_rev.to_string(),
                remote_rev: remote_rev.to_string(),
                synced_at: self.now,
                conflict: None,
            },
        );
    }

    fn push(
        &mut self,
        path: &VaultPath,
        l: &Entry,
        bytes: Vec<u8>,
        hash: String,
        mode: WriteMode,
    ) -> R<()> {
        let re = self.remote.write(path, &bytes, mode)?;
        self.record(path, &l.revision.0, &re.revision.0, hash);
        self.report.pushed.push(path.to_string());
        Ok(())
    }

    fn pull(
        &mut self,
        path: &VaultPath,
        r: &Entry,
        bytes: Vec<u8>,
        hash: String,
        mode: WriteMode,
    ) -> R<()> {
        let le = self.local.write(path, &bytes, mode)?;
        self.record(path, &le.revision.0, &r.revision.0, hash);
        self.report.pulled.push(path.to_string());
        Ok(())
    }

    fn copy_name(&self, path: &VaultPath, tag: &str, avoid: Option<&VaultPath>) -> R<VaultPath> {
        let name = path.file_name();
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        let stamp = chrono::DateTime::from_timestamp(self.now, 0)
            .unwrap_or_default()
            .format("%Y-%m-%d %H%M");
        let dir = path.parent().map(|d| format!("{d}/")).unwrap_or_default();
        for n in 1..1000 {
            let suffix = if n == 1 {
                String::new()
            } else {
                format!(" {n}")
            };
            let raw = format!("{dir}{stem} (conflict {tag} {stamp}{suffix}){ext}");
            let candidate = VaultPath::new(&raw)
                .map_err(|e| StorageError::InvalidPath(format!("{raw}: {e}")))?;
            if avoid != Some(&candidate)
                && self.local.stat(&candidate)?.is_none()
                && self.remote.stat(&candidate)?.is_none()
            {
                return Ok(candidate);
            }
        }
        Err(StorageError::Internal(
            "no free conflict copy name".to_string(),
        ))
    }

    /// Keep the local file, save the remote version as a conflict copy, remember the conflict.
    #[allow(clippy::too_many_arguments)]
    fn conflict(
        &mut self,
        path: &VaultPath,
        l: &Entry,
        r: &Entry,
        local_bytes: Vec<u8>,
        local_hash: String,
        remote_bytes: Vec<u8>,
        remote_hash: String,
        base: Option<&ManifestEntry>,
    ) -> R<()> {
        let tag = self.tag.clone();
        let copy = self.copy_name(path, &tag, None)?;
        let local_copy = self.copy_name(path, "local", Some(&copy))?;
        self.local
            .write(&copy, &remote_bytes, WriteMode::CreateNew)?;
        self.followups.push(copy.clone());
        // The remote never sees the local version otherwise: share it as a copy too.
        self.local
            .write(&local_copy, &local_bytes, WriteMode::CreateNew)?;
        self.followups.push(local_copy.clone());
        let state = ConflictState {
            local_hash: local_hash.clone(),
            remote_hash,
            local_rev: l.revision.0.clone(),
            remote_rev: r.revision.0.clone(),
        };
        let entry = match base {
            Some(b) => ManifestEntry {
                conflict: Some(state),
                ..b.clone()
            },
            None => ManifestEntry {
                hash: local_hash,
                local_rev: l.revision.0.clone(),
                remote_rev: r.revision.0.clone(),
                synced_at: self.now,
                conflict: Some(state),
            },
        };
        self.manifest.entries.insert(path.to_string(), entry);
        tracing::info!(path = %path, copy = %copy, "sync conflict, kept both versions");
        self.report.unresolved.push(path.to_string());
        self.report.skipped += 1;
        self.report.conflicts.push(ConflictInfo {
            path: path.to_string(),
            copy_path: copy.to_string(),
            local_copy_path: local_copy.to_string(),
        });
        Ok(())
    }

    fn process(&mut self, path: &VaultPath, l: &Option<Entry>, r: &Option<Entry>) -> R<()> {
        let m = self.manifest.entries.get(path.as_str()).cloned();
        match (l, r, m) {
            (None, None, m) => {
                if m.is_some() {
                    self.manifest.entries.remove(path.as_str());
                }
                Ok(())
            }
            (Some(l), None, None) => {
                let (b, h) = read_hash(self.local, path)?;
                self.push(path, l, b, h, WriteMode::CreateNew)
            }
            (None, Some(r), None) => {
                let (b, h) = read_hash(self.remote, path)?;
                self.pull(path, r, b, h, WriteMode::CreateNew)
            }
            (Some(l), Some(r), None) => {
                let (lb, lh) = read_hash(self.local, path)?;
                let (rb, rh) = read_hash(self.remote, path)?;
                if lh == rh {
                    self.record(path, &l.revision.0, &r.revision.0, lh);
                    Ok(())
                } else {
                    self.conflict(path, l, r, lb, lh, rb, rh, None)
                }
            }
            // Deleted on the remote.
            (Some(l), None, Some(m)) => {
                let pr = probe(self.local, path, l, &m.local_rev, &m.hash)?;
                if !pr.changed && m.conflict.is_none() {
                    let cur = self.local.stat(path)?;
                    if cur.as_ref().map(|e| &e.revision) != Some(&l.revision) {
                        return Err(StorageError::Conflict {
                            path: path.clone(),
                            expected: l.revision.clone(),
                            actual: cur.map(|e| e.revision),
                        });
                    }
                    let t = self.local.trash(path)?;
                    self.manifest.entries.remove(path.as_str());
                    self.report.trashed_local.push(path.to_string());
                    self.followups.push(t);
                    Ok(())
                } else {
                    // Modified here, deleted there: keep the modified version.
                    let (b, h) = bytes_of(pr, self.local, path)?;
                    self.push(path, l, b, h, WriteMode::CreateNew)
                }
            }
            // Deleted locally.
            (None, Some(r), Some(m)) => {
                let pr = probe(self.remote, path, r, &m.remote_rev, &m.hash)?;
                if !pr.changed && m.conflict.is_none() {
                    let cur = self.remote.stat(path)?;
                    if cur.as_ref().map(|e| &e.revision) != Some(&r.revision) {
                        return Err(StorageError::Conflict {
                            path: path.clone(),
                            expected: r.revision.clone(),
                            actual: cur.map(|e| e.revision),
                        });
                    }
                    let t = self.remote.trash(path)?;
                    self.manifest.entries.remove(path.as_str());
                    self.report.trashed_remote.push(path.to_string());
                    self.followups.push(t);
                    Ok(())
                } else {
                    let (b, h) = bytes_of(pr, self.remote, path)?;
                    self.pull(path, r, b, h, WriteMode::CreateNew)
                }
            }
            (Some(l), Some(r), Some(m)) => {
                if let Some(c) = &m.conflict {
                    return self.process_conflicted(path, l, r, &m, c);
                }
                let pl = probe(self.local, path, l, &m.local_rev, &m.hash)?;
                let pr = probe(self.remote, path, r, &m.remote_rev, &m.hash)?;
                match (pl.changed, pr.changed) {
                    (false, false) => {
                        if l.revision.0 != m.local_rev || r.revision.0 != m.remote_rev {
                            self.record(path, &l.revision.0, &r.revision.0, m.hash.clone());
                        }
                        Ok(())
                    }
                    (true, false) => {
                        let (b, h) = bytes_of(pl, self.local, path)?;
                        self.push(path, l, b, h, WriteMode::Replace(r.revision.clone()))
                    }
                    (false, true) => {
                        let (b, h) = bytes_of(pr, self.remote, path)?;
                        self.pull(path, r, b, h, WriteMode::Replace(l.revision.clone()))
                    }
                    (true, true) => {
                        let (lb, lh) = bytes_of(pl, self.local, path)?;
                        let (rb, rh) = bytes_of(pr, self.remote, path)?;
                        if lh == rh {
                            self.record(path, &l.revision.0, &r.revision.0, lh);
                            Ok(())
                        } else {
                            self.conflict(path, l, r, lb, lh, rb, rh, Some(&m))
                        }
                    }
                }
            }
        }
    }

    fn process_conflicted(
        &mut self,
        path: &VaultPath,
        l: &Entry,
        r: &Entry,
        m: &ManifestEntry,
        c: &ConflictState,
    ) -> R<()> {
        let pending = |s: &mut Self| {
            s.report.unresolved.push(path.to_string());
            s.report.skipped += 1;
        };
        if l.revision.0 == c.local_rev && r.revision.0 == c.remote_rev {
            pending(self);
            return Ok(());
        }
        let (lb, lh) = read_hash(self.local, path)?;
        let (rb, rh) = read_hash(self.remote, path)?;
        if lh == rh {
            self.record(path, &l.revision.0, &r.revision.0, lh);
        } else if rh == c.remote_hash {
            if lh == c.local_hash {
                // Nothing changed except revisions (touch).
                let mut e = m.clone();
                e.conflict = Some(ConflictState {
                    local_rev: l.revision.0.clone(),
                    remote_rev: r.revision.0.clone(),
                    ..c.clone()
                });
                self.manifest.entries.insert(path.to_string(), e);
                pending(self);
            } else {
                // The user edited the local file after the conflict: that resolves it. The
                // remote version it replaces is preserved in the conflict copy.
                self.push(path, l, lb, lh, WriteMode::Replace(r.revision.clone()))?;
            }
        } else {
            // Remote changed again: never overwrite either side, save another copy.
            self.conflict(path, l, r, lb, lh, rb, rh, Some(m))?;
        }
        Ok(())
    }

    fn stat_both(&self, path: &VaultPath) -> R<(Option<Entry>, Option<Entry>)> {
        Ok((self.local.stat(path)?, self.remote.stat(path)?))
    }

    /// Process one path, retrying on races. Returns true if the remote is offline.
    fn run(&mut self, path: &VaultPath, mut l: Option<Entry>, mut r: Option<Entry>) -> bool {
        for attempt in 0..3 {
            match self.process(path, &l, &r) {
                Ok(()) => return false,
                Err(e) if is_offline(&e) => return true,
                Err(e) if is_race(&e) && attempt < 2 => match self.stat_both(path) {
                    Ok((nl, nr)) => {
                        l = nl;
                        r = nr;
                    }
                    Err(e2) => return self.fail(path, &e2),
                },
                Err(e) => return self.fail(path, &e),
            }
        }
        false
    }

    fn fail(&mut self, path: &VaultPath, e: &StorageError) -> bool {
        if is_offline(e) {
            return true;
        }
        tracing::warn!(path = %path, error = %e, "sync failed for path");
        self.report.errors.push((path.to_string(), e.to_string()));
        false
    }
}

fn valid_id(remote_id: &str) -> Result<(), SyncError> {
    // Loading the manifest validates the id; this is only for the conflict-copy tag.
    if sanitize_id(remote_id).is_empty() {
        return Err(SyncError::InvalidRemoteId(remote_id.to_string()));
    }
    Ok(())
}

fn save_offline(
    local: &dyn StorageProvider,
    now: i64,
    pending: Vec<String>,
) -> Result<(), SyncError> {
    let prev = OfflineQueue::load(local)?;
    OfflineQueue {
        since: if prev.since > 0 && !prev.pending.is_empty() {
            prev.since
        } else {
            now
        },
        last_attempt: now,
        pending,
    }
    .save(local)
}

/// Two-way sync of `local` and `remote`, using the manifest for `remote_id` as the common base.
///
/// Per path: unchanged on both sides is skipped; a one-sided change is pushed or pulled
/// (conditionally on the revision just seen); a change on both sides with different content keeps
/// the local file, saves the remote version as `<stem> (conflict <remote-id> <date time>).<ext>`
/// on both sides and holds the path until resolved. Deletions propagate as `trash`, and a delete
/// never beats an edit. Per-path failures land in [`SyncReport::errors`]; an unreachable remote
/// records the dirty paths in the offline queue and returns a report with `offline` set.
/// Only manifest, queue or local listing failures return `Err`.
pub fn sync(
    local: &dyn StorageProvider,
    remote: &dyn StorageProvider,
    remote_id: &str,
    now: i64,
) -> Result<SyncReport, SyncError> {
    valid_id(remote_id)?;
    let manifest = Manifest::load(local, remote_id)?;
    let local_list = local.list()?;
    let remote_list = match remote.list() {
        Ok(v) => v,
        Err(e) if is_offline(&e) => {
            let pending = local_list
                .iter()
                .filter(|e| is_synced(e.path.as_str()))
                .filter(|e| dirty_local(Some(e), manifest.entries.get(e.path.as_str())))
                .map(|e| e.path.to_string())
                .chain(
                    manifest
                        .entries
                        .keys()
                        .filter(|k| !local_list.iter().any(|e| e.path.as_str() == k.as_str()))
                        .cloned(),
                )
                .collect();
            save_offline(local, now, pending)?;
            return Ok(SyncReport {
                offline: true,
                ..SyncReport::default()
            });
        }
        Err(e) => return Err(SyncError::Remote(e.to_string())),
    };

    let lmap: BTreeMap<String, Entry> = local_list
        .into_iter()
        .filter(|e| is_synced(e.path.as_str()))
        .map(|e| (e.path.to_string(), e))
        .collect();
    let rmap: BTreeMap<String, Entry> = remote_list
        .into_iter()
        .filter(|e| is_synced(e.path.as_str()))
        .map(|e| (e.path.to_string(), e))
        .collect();
    let mut keys: BTreeSet<String> = lmap.keys().chain(rmap.keys()).cloned().collect();
    keys.extend(manifest.entries.keys().filter(|k| is_synced(k)).cloned());

    let mut eng = Engine {
        local,
        remote,
        tag: sanitize_id(remote_id),
        now,
        manifest,
        report: SyncReport::default(),
        followups: Vec::new(),
    };

    let keys: Vec<String> = keys.into_iter().collect();
    let mut pending: Option<Vec<String>> = None;
    for (i, key) in keys.iter().enumerate() {
        let Ok(path) = VaultPath::new(key) else {
            eng.report
                .errors
                .push((key.clone(), "invalid path".to_string()));
            continue;
        };
        if eng.run(&path, lmap.get(key).cloned(), rmap.get(key).cloned()) {
            pending = Some(
                keys[i..]
                    .iter()
                    .filter(|k| dirty_local(lmap.get(*k), eng.manifest.entries.get(k.as_str())))
                    .cloned()
                    .collect(),
            );
            break;
        }
    }

    if pending.is_none() {
        while let Some(path) = eng.followups.pop() {
            match eng.stat_both(&path) {
                Ok((l, r)) => {
                    if eng.run(&path, l, r) {
                        pending = Some(vec![path.to_string()]);
                        break;
                    }
                }
                Err(e) => {
                    if eng.fail(&path, &e) {
                        pending = Some(vec![path.to_string()]);
                        break;
                    }
                }
            }
        }
    }

    eng.manifest.save(local)?;
    let mut report = eng.report;
    match pending {
        Some(p) => {
            let mut p = p;
            p.extend(eng.followups.iter().map(|v| v.to_string()));
            save_offline(local, now, p)?;
            report.offline = true;
        }
        None => OfflineQueue::clear(local)?,
    }
    tracing::info!(
        pushed = report.pushed.len(),
        pulled = report.pulled.len(),
        conflicts = report.conflicts.len(),
        errors = report.errors.len(),
        "sync finished"
    );
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::pending;
    use selenite_storage::{LocalFolder, MemoryStorage, Revision};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    const T0: i64 = 1_700_000_000;

    fn p(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }
    fn put(s: &dyn StorageProvider, path: &str, text: &str) {
        s.write(&p(path), text.as_bytes(), WriteMode::Overwrite)
            .unwrap();
    }
    fn get(s: &dyn StorageProvider, path: &str) -> Option<String> {
        s.stat(&p(path)).unwrap()?;
        Some(String::from_utf8(s.read(&p(path)).unwrap()).unwrap())
    }
    fn texts(s: &dyn StorageProvider) -> Vec<String> {
        s.list()
            .unwrap()
            .iter()
            .map(|e| String::from_utf8(s.read(&e.path).unwrap()).unwrap())
            .collect()
    }
    fn paths(s: &dyn StorageProvider) -> BTreeSet<String> {
        s.list()
            .unwrap()
            .iter()
            .map(|e| e.path.to_string())
            .filter(|x| is_synced(x))
            .collect()
    }
    fn run(l: &dyn StorageProvider, r: &dyn StorageProvider) -> SyncReport {
        sync(l, r, "drive", T0).unwrap()
    }
    fn has(s: &dyn StorageProvider, text: &str) -> bool {
        texts(s).iter().any(|t| t == text)
    }

    #[test]
    fn first_sync_uploads_everything_and_excludes_private_state() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "a.md", "A");
        put(&l, "dir/b.md", "B");
        put(&l, ".selenite/config.yaml", "cfg");
        put(&l, ".obsidian/app.json", "{}");
        put(&l, ".selenite/index.sqlite", "idx");
        put(&l, ".selenite/index.sqlite-wal", "wal");
        put(&l, "x.md.selenite-tmp-1", "tmp");
        put(&l, ".DS_Store", "junk");
        put(&l, "Thumbs.db", "junk");
        let rep = run(&l, &r);
        assert_eq!(rep.pushed.len(), 4, "{rep:?}");
        assert_eq!(
            paths(&r),
            [
                "a.md",
                "dir/b.md",
                ".selenite/config.yaml",
                ".obsidian/app.json"
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        );
        for bad in [
            ".selenite/index.sqlite",
            ".selenite/sync/drive.json",
            ".DS_Store",
        ] {
            assert!(get(&r, bad).is_none(), "{bad} crossed");
        }
        assert!(get(&l, ".selenite/sync/drive.json").is_some());
    }

    #[test]
    fn pull_new_remote_file_and_push_edit() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&r, "n/new.md", "remote new");
        let rep = run(&l, &r);
        assert_eq!(rep.pulled, vec!["n/new.md"]);
        assert_eq!(get(&l, "n/new.md").unwrap(), "remote new");
        put(&l, "n/new.md", "edited locally");
        let rep = run(&l, &r);
        assert_eq!(rep.pushed, vec!["n/new.md"]);
        assert_eq!(get(&r, "n/new.md").unwrap(), "edited locally");
        put(&r, "n/new.md", "edited remotely");
        let rep = run(&l, &r);
        assert_eq!(rep.pulled, vec!["n/new.md"]);
        assert_eq!(get(&l, "n/new.md").unwrap(), "edited remotely");
    }

    #[test]
    fn both_edited_keeps_both_versions_everywhere() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "dir/n.md", "base");
        run(&l, &r);
        put(&l, "dir/n.md", "local edit");
        put(&r, "dir/n.md", "remote edit");
        let rep = run(&l, &r);
        assert_eq!(rep.conflicts.len(), 1);
        let c = &rep.conflicts[0];
        assert_eq!(c.path, "dir/n.md");
        assert!(
            c.copy_path
                .starts_with("dir/n (conflict drive 2023-11-14 2213)")
        );
        assert!(c.copy_path.ends_with(").md"));
        assert_eq!(get(&l, "dir/n.md").unwrap(), "local edit");
        assert_eq!(get(&l, &c.copy_path).unwrap(), "remote edit");
        assert_eq!(get(&r, &c.copy_path).unwrap(), "remote edit");
        for side in [&l as &dyn StorageProvider, &r] {
            assert!(has(side, "local edit") && has(side, "remote edit"));
        }
        // Unresolved conflict is stable: no further copies, nothing overwritten.
        let rep2 = run(&l, &r);
        assert!(rep2.is_noop());
        assert_eq!(rep2.unresolved, vec!["dir/n.md"]);
        assert_eq!(paths(&l).len(), 3);
        // Editing the local file resolves it; the remote version survives in the copy.
        put(&l, "dir/n.md", "merged");
        let rep3 = run(&l, &r);
        assert_eq!(rep3.pushed, vec!["dir/n.md"]);
        assert_eq!(get(&r, "dir/n.md").unwrap(), "merged");
        assert!(has(&r, "remote edit"));
        assert!(run(&l, &r).unresolved.is_empty());
    }

    #[test]
    fn equal_edits_do_not_conflict() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "a.md", "base");
        run(&l, &r);
        put(&l, "a.md", "same");
        put(&r, "a.md", "same");
        let rep = run(&l, &r);
        assert!(rep.is_noop(), "{rep:?}");
        assert_eq!(paths(&l).len(), 1);
    }

    #[test]
    fn touch_without_change_is_not_a_change() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "a.md", "base");
        run(&l, &r);
        put(&l, "a.md", "base");
        put(&r, "a.md", "base");
        assert!(run(&l, &r).is_noop());
    }

    #[test]
    fn local_delete_becomes_remote_trash() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "a.md", "keep me safe");
        run(&l, &r);
        l.trash(&p("a.md")).unwrap();
        let rep = run(&l, &r);
        assert_eq!(rep.trashed_remote, vec!["a.md"]);
        assert!(get(&r, "a.md").is_none());
        assert!(has(&r, "keep me safe"), "remote trash keeps the content");
        assert!(has(&l, "keep me safe"));
        assert_eq!(paths(&l), paths(&r));
        assert!(run(&l, &r).is_noop());
    }

    #[test]
    fn remote_delete_becomes_local_trash() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "a.md", "bye");
        run(&l, &r);
        r.trash(&p("a.md")).unwrap();
        let rep = run(&l, &r);
        assert_eq!(rep.trashed_local, vec!["a.md"]);
        assert!(get(&l, "a.md").is_none() && has(&l, "bye"));
        assert_eq!(paths(&l), paths(&r));
        assert!(run(&l, &r).is_noop());
    }

    #[test]
    fn delete_never_beats_edit() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "a.md", "base");
        put(&l, "b.md", "base");
        run(&l, &r);
        r.trash(&p("a.md")).unwrap();
        put(&l, "a.md", "local edit");
        l.trash(&p("b.md")).unwrap();
        put(&r, "b.md", "remote edit");
        run(&l, &r);
        assert_eq!(get(&r, "a.md").unwrap(), "local edit");
        assert_eq!(get(&l, "b.md").unwrap(), "remote edit");
        assert_eq!(paths(&l), paths(&r));
        assert!(run(&l, &r).is_noop());
    }

    #[test]
    fn second_run_is_a_noop() {
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        put(&l, "a.md", "A");
        put(&r, "b.md", "B");
        run(&l, &r);
        let before = (texts(&l), texts(&r));
        let rep = run(&l, &r);
        assert_eq!(rep, SyncReport::default());
        assert_eq!((texts(&l), texts(&r)), before);
    }

    struct Racy {
        inner: MemoryStorage,
        hook: Mutex<Option<(String, Vec<u8>)>>,
    }
    impl StorageProvider for Racy {
        fn kind(&self) -> &'static str {
            "racy"
        }
        fn list(&self) -> R<Vec<Entry>> {
            self.inner.list()
        }
        fn list_dirs(&self) -> R<Vec<VaultPath>> {
            self.inner.list_dirs()
        }
        fn stat(&self, path: &VaultPath) -> R<Option<Entry>> {
            self.inner.stat(path)
        }
        fn read(&self, path: &VaultPath) -> R<Vec<u8>> {
            self.inner.read(path)
        }
        fn write(&self, path: &VaultPath, data: &[u8], mode: WriteMode) -> R<Entry> {
            if matches!(mode, WriteMode::Replace(_)) {
                let hooked = self.hook.lock().ok().and_then(|mut g| {
                    if g.as_ref().is_some_and(|(hp, _)| hp == path.as_str()) {
                        g.take()
                    } else {
                        None
                    }
                });
                if let Some((_, bytes)) = hooked {
                    self.inner.write(path, &bytes, WriteMode::Overwrite)?;
                }
            }
            self.inner.write(path, data, mode)
        }
        fn rename(&self, from: &VaultPath, to: &VaultPath) -> R<Entry> {
            self.inner.rename(from, to)
        }
        fn trash(&self, path: &VaultPath) -> R<VaultPath> {
            self.inner.trash(path)
        }
        fn create_dir(&self, path: &VaultPath) -> R<()> {
            self.inner.create_dir(path)
        }
    }

    #[test]
    fn raced_replace_becomes_a_conflict_copy() {
        let l = MemoryStorage::new();
        let r = Racy {
            inner: MemoryStorage::new(),
            hook: Mutex::new(None),
        };
        put(&l, "a.md", "base");
        run(&l, &r);
        put(&l, "a.md", "local edit");
        *r.hook.lock().unwrap() = Some(("a.md".into(), b"sneaky remote".to_vec()));
        let rep = run(&l, &r);
        assert!(rep.errors.is_empty(), "{rep:?}");
        assert_eq!(rep.conflicts.len(), 1);
        for side in [&l as &dyn StorageProvider, &r] {
            assert!(has(side, "local edit") && has(side, "sneaky remote"));
        }
    }

    struct Flaky {
        inner: MemoryStorage,
        down: AtomicBool,
    }
    impl Flaky {
        fn gate(&self) -> R<()> {
            if self.down.load(Ordering::SeqCst) {
                Err(StorageError::Io {
                    path: "remote".into(),
                    source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
                })
            } else {
                Ok(())
            }
        }
    }
    impl StorageProvider for Flaky {
        fn kind(&self) -> &'static str {
            "flaky"
        }
        fn list(&self) -> R<Vec<Entry>> {
            self.gate()?;
            self.inner.list()
        }
        fn list_dirs(&self) -> R<Vec<VaultPath>> {
            self.gate()?;
            self.inner.list_dirs()
        }
        fn stat(&self, path: &VaultPath) -> R<Option<Entry>> {
            self.gate()?;
            self.inner.stat(path)
        }
        fn read(&self, path: &VaultPath) -> R<Vec<u8>> {
            self.gate()?;
            self.inner.read(path)
        }
        fn write(&self, path: &VaultPath, data: &[u8], mode: WriteMode) -> R<Entry> {
            self.gate()?;
            self.inner.write(path, data, mode)
        }
        fn rename(&self, from: &VaultPath, to: &VaultPath) -> R<Entry> {
            self.gate()?;
            self.inner.rename(from, to)
        }
        fn trash(&self, path: &VaultPath) -> R<VaultPath> {
            self.gate()?;
            self.inner.trash(path)
        }
        fn create_dir(&self, path: &VaultPath) -> R<()> {
            self.gate()?;
            self.inner.create_dir(path)
        }
    }

    #[test]
    fn offline_is_queued_then_cleared() {
        let l = MemoryStorage::new();
        let r = Flaky {
            inner: MemoryStorage::new(),
            down: AtomicBool::new(false),
        };
        put(&l, "a.md", "A");
        run(&l, &r);
        put(&l, "a.md", "A2");
        put(&l, "new.md", "N");
        r.down.store(true, Ordering::SeqCst);
        let rep = run(&l, &r);
        assert!(rep.offline && rep.errors.is_empty());
        assert_eq!(pending(&l), vec!["a.md".to_string(), "new.md".to_string()]);
        assert_eq!(get(&r.inner, "a.md").unwrap(), "A");
        r.down.store(false, Ordering::SeqCst);
        let rep = run(&l, &r);
        assert!(!rep.offline);
        assert_eq!(rep.pushed.len(), 2);
        assert!(pending(&l).is_empty());
        assert_eq!(get(&r.inner, "a.md").unwrap(), "A2");
    }

    #[test]
    fn offline_listing_failure_leaves_manifest_untouched() {
        let l = MemoryStorage::new();
        let r = Flaky {
            inner: MemoryStorage::new(),
            down: AtomicBool::new(false),
        };
        put(&l, "a.md", "A");
        run(&l, &r);
        put(&l, "a.md", "A2");
        // List works, writes fail: simulate by taking the remote down after listing is impossible
        // here, so check the listing-failure path only and that the manifest is untouched.
        r.down.store(true, Ordering::SeqCst);
        run(&l, &r);
        let m = Manifest::load(&l, "drive").unwrap();
        assert_eq!(m.entries.len(), 1);
    }

    #[test]
    fn local_folder_against_memory() {
        let dir = tempfile::tempdir().unwrap();
        let l = LocalFolder::create(dir.path()).unwrap();
        let r = MemoryStorage::new();
        put(&l, "notes/a.md", "A");
        put(&l, ".selenite/index.sqlite", "cache");
        put(&r, "notes/b.md", "B");
        let rep = run(&l, &r);
        assert_eq!(rep.pushed, vec!["notes/a.md"]);
        assert_eq!(rep.pulled, vec!["notes/b.md"]);
        assert!(dir.path().join("notes/b.md").exists());
        assert!(dir.path().join(".selenite/sync/drive.json").exists());
        assert!(get(&r, ".selenite/index.sqlite").is_none());
        put(&l, "notes/a.md", "A local");
        put(&r, "notes/a.md", "A remote");
        let rep = run(&l, &r);
        assert_eq!(rep.conflicts.len(), 1);
        assert!(has(&l, "A local") && has(&l, "A remote"));
        assert!(has(&r, "A local") && has(&r, "A remote"));
        assert!(run(&l, &r).is_noop());
        let _ = Revision(String::new());
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    fn snapshot(s: &dyn StorageProvider) -> BTreeMap<String, Vec<u8>> {
        s.list()
            .unwrap()
            .iter()
            .filter(|e| is_synced(e.path.as_str()))
            .map(|e| (e.path.to_string(), s.read(&e.path).unwrap()))
            .collect()
    }

    #[test]
    fn randomized_edits_never_lose_data_and_converge() {
        const NAMES: [&str; 6] = ["a.md", "b.md", "dir/c.md", "dir/d.txt", "e", "deep/x/y.md"];
        let (l, r) = (MemoryStorage::new(), MemoryStorage::new());
        let mut rng = Lcg(0x5EED_1234_ABCD);
        let mut counter = 0u64;
        let mut prev_all: BTreeSet<Vec<u8>> = BTreeSet::new();
        for i in 0..200i64 {
            for side in [&l as &dyn StorageProvider, &r] {
                for _ in 0..rng.next() % 4 {
                    let name = NAMES[(rng.next() % NAMES.len() as u64) as usize];
                    let path = p(name);
                    if rng.next() % 4 == 0 {
                        if side.stat(&path).unwrap().is_some() {
                            side.trash(&path).unwrap();
                        }
                    } else {
                        counter += 1;
                        side.write(
                            &path,
                            format!("version {counter}").as_bytes(),
                            WriteMode::Overwrite,
                        )
                        .unwrap();
                    }
                }
            }
            let mut pre: BTreeSet<Vec<u8>> = BTreeSet::new();
            pre.extend(snapshot(&l).into_values());
            pre.extend(snapshot(&r).into_values());
            let required: Vec<&Vec<u8>> = pre.difference(&prev_all).collect();

            let rep = sync(&l, &r, "drive", T0 + i * 60).unwrap();
            assert!(rep.errors.is_empty(), "iter {i}: {:?}", rep.errors);
            let (sl, sr) = (snapshot(&l), snapshot(&r));
            let (vl, vr): (BTreeSet<&Vec<u8>>, BTreeSet<&Vec<u8>>) =
                (sl.values().collect(), sr.values().collect());
            for v in required {
                assert!(vl.contains(v), "iter {i}: local lost a version");
                assert!(vr.contains(v), "iter {i}: remote lost a version");
            }
            assert_eq!(
                sl.keys().collect::<Vec<_>>(),
                sr.keys().collect::<Vec<_>>(),
                "iter {i}: file sets differ"
            );
            for (k, v) in &sl {
                if !rep.unresolved.contains(k) {
                    assert_eq!(Some(v), sr.get(k), "iter {i}: {k} differs");
                }
            }
            let again = sync(&l, &r, "drive", T0 + i * 60).unwrap();
            assert!(
                again.is_noop(),
                "iter {i}: second run not a no-op: {again:?}"
            );
            prev_all = vl.into_iter().chain(vr).cloned().collect();
        }
    }
}

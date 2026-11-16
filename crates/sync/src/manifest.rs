//! The sync manifest and the exclusion rules.

use std::collections::BTreeMap;

use selenite_core::VaultPath;
use selenite_storage::{StorageProvider, WriteMode};
use serde::{Deserialize, Serialize};

use crate::SyncError;

/// Current manifest schema version.
pub const MANIFEST_VERSION: u32 = 1;

/// Recorded when both sides changed a path differently and the conflict is not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictState {
    /// Content hash of the local version at conflict time.
    pub local_hash: String,
    /// Content hash of the remote version at conflict time.
    pub remote_hash: String,
    /// Local revision at conflict time.
    pub local_rev: String,
    /// Remote revision at conflict time.
    pub remote_rev: String,
}

/// What was true of one path at the last successful sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Content hash at last sync.
    pub hash: String,
    /// Local revision at last sync.
    pub local_rev: String,
    /// Remote revision at last sync.
    pub remote_rev: String,
    /// Unix seconds of the last sync of this path.
    pub synced_at: i64,
    /// Set while an unresolved conflict exists for this path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<ConflictState>,
}

/// Per-remote sync state, stored locally at `.selenite/sync/<remote-id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version.
    pub version: u32,
    /// Identifier of the remote this manifest belongs to.
    pub remote_id: String,
    /// Path (as `/`-separated string) to entry.
    pub entries: BTreeMap<String, ManifestEntry>,
}

/// Make a remote id safe to use as a file name.
pub(crate) fn sanitize_id(remote_id: &str) -> String {
    remote_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn manifest_path(remote_id: &str) -> Result<VaultPath, SyncError> {
    let id = sanitize_id(remote_id);
    if id.is_empty() || id == "queue" || id.chars().all(|c| c == '.') {
        return Err(SyncError::InvalidRemoteId(remote_id.to_string()));
    }
    VaultPath::new(&format!(".selenite/sync/{id}.json"))
        .map_err(|_| SyncError::InvalidRemoteId(remote_id.to_string()))
}

impl Manifest {
    /// An empty manifest for `remote_id`.
    pub fn new(remote_id: &str) -> Self {
        Self {
            version: MANIFEST_VERSION,
            remote_id: remote_id.to_string(),
            entries: BTreeMap::new(),
        }
    }

    /// Load the manifest for `remote_id` from `local`, or an empty one if none exists yet.
    ///
    /// A manifest that exists but cannot be parsed is an error: guessing would risk data loss.
    pub fn load(local: &dyn StorageProvider, remote_id: &str) -> Result<Self, SyncError> {
        let path = manifest_path(remote_id)?;
        if local.stat(&path)?.is_none() {
            return Ok(Self::new(remote_id));
        }
        let bytes = local.read(&path)?;
        let m: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| SyncError::State(format!("manifest {path}: {e}")))?;
        if m.version != MANIFEST_VERSION {
            return Err(SyncError::State(format!(
                "manifest {path}: unsupported version {}",
                m.version
            )));
        }
        if m.remote_id != remote_id {
            return Err(SyncError::State(format!(
                "manifest {path}: belongs to another remote"
            )));
        }
        Ok(m)
    }

    /// Persist the manifest through the local provider.
    pub fn save(&self, local: &dyn StorageProvider) -> Result<(), SyncError> {
        let path = manifest_path(&self.remote_id)?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| SyncError::State(format!("manifest serialise: {e}")))?;
        local.write(&path, &bytes, WriteMode::Overwrite)?;
        Ok(())
    }
}

/// Whether a path takes part in sync.
///
/// Excluded: the index cache (`.selenite/index.sqlite*`), sync state (`.selenite/sync/**`),
/// temporary files (`*.selenite-tmp-*`) and OS junk. Everything else is synced, including
/// `.selenite/config.yaml`, `.selenite/audit/**`, `.selenite/trash/**` (trash is user data) and
/// `.obsidian/**`.
pub fn is_synced(path: &str) -> bool {
    if path == ".selenite/sync" || path.starts_with(".selenite/sync/") {
        return false;
    }
    if path.starts_with(".selenite/index.sqlite") {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.contains(".selenite-tmp-") {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    !matches!(lower.as_str(), ".ds_store" | "thumbs.db" | "desktop.ini")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use selenite_storage::MemoryStorage;

    #[test]
    fn exclusion_rules() {
        for p in [
            ".selenite/index.sqlite",
            ".selenite/index.sqlite-wal",
            ".selenite/index.sqlite-shm",
            ".selenite/sync/remote.json",
            ".selenite/sync/queue.json",
            "a/b.md.selenite-tmp-123",
            ".DS_Store",
            "a/.DS_Store",
            "x/Thumbs.db",
            "x/thumbs.db",
            "desktop.ini",
        ] {
            assert!(!is_synced(p), "{p} should be excluded");
        }
        for p in [
            "note.md",
            "a/b/c.png",
            ".selenite/config.yaml",
            ".selenite/audit/2026.log",
            ".selenite/trash/1-x.md",
            ".obsidian/app.json",
            ".syncthing",
        ] {
            assert!(is_synced(p), "{p} should be synced");
        }
    }

    #[test]
    fn manifest_round_trip_and_validation() {
        let s = MemoryStorage::new();
        let mut m = Manifest::load(&s, "drive").unwrap();
        assert!(m.entries.is_empty());
        m.entries.insert(
            "a.md".into(),
            ManifestEntry {
                hash: "h".into(),
                local_rev: "1".into(),
                remote_rev: "2".into(),
                synced_at: 5,
                conflict: None,
            },
        );
        m.save(&s).unwrap();
        assert_eq!(Manifest::load(&s, "drive").unwrap(), m);
        assert!(matches!(
            Manifest::load(&s, ""),
            Err(SyncError::InvalidRemoteId(_))
        ));
        assert!(matches!(
            Manifest::load(&s, "queue"),
            Err(SyncError::InvalidRemoteId(_))
        ));
        let p = VaultPath::new(".selenite/sync/bad.json").unwrap();
        s.write(&p, b"{not json", WriteMode::Overwrite).unwrap();
        assert!(matches!(
            Manifest::load(&s, "bad"),
            Err(SyncError::State(_))
        ));
    }
}

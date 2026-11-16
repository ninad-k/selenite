//! Offline detection and the persisted queue of paths awaiting a reachable remote.

use selenite_core::VaultPath;
use selenite_storage::{StorageError, StorageProvider, WriteMode};
use serde::{Deserialize, Serialize};

use crate::SyncError;

const QUEUE_PATH: &str = ".selenite/sync/queue.json";

/// Whether an error means the remote is unreachable (as opposed to a per-file problem).
///
/// True for I/O errors of kind `ConnectionRefused`, `ConnectionReset`, `ConnectionAborted`,
/// `TimedOut` or `NotConnected`, and for `Internal` errors whose message starts with `offline:`.
pub fn is_offline(err: &StorageError) -> bool {
    use std::io::ErrorKind::{
        ConnectionAborted, ConnectionRefused, ConnectionReset, NotConnected, TimedOut,
    };
    match err {
        StorageError::Io { source, .. } => matches!(
            source.kind(),
            ConnectionRefused | ConnectionReset | ConnectionAborted | TimedOut | NotConnected
        ),
        StorageError::Internal(msg) => msg.starts_with("offline:"),
        _ => false,
    }
}

/// Paths that still need syncing because the last run could not reach the remote.
/// Persisted at `.selenite/sync/queue.json` (local side only, never synced).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineQueue {
    /// Unix seconds of the first failed attempt in the current outage.
    pub since: i64,
    /// Unix seconds of the latest failed attempt.
    pub last_attempt: i64,
    /// Paths with pending local changes.
    pub pending: Vec<String>,
}

fn queue_path() -> Result<VaultPath, SyncError> {
    VaultPath::new(QUEUE_PATH).map_err(|e| SyncError::State(e.to_string()))
}

impl OfflineQueue {
    /// Load the queue; empty if none was ever written.
    pub fn load(local: &dyn StorageProvider) -> Result<Self, SyncError> {
        let path = queue_path()?;
        if local.stat(&path)?.is_none() {
            return Ok(Self::default());
        }
        let bytes = local.read(&path)?;
        serde_json::from_slice(&bytes).map_err(|e| SyncError::State(format!("queue: {e}")))
    }

    /// Persist the queue.
    pub fn save(&self, local: &dyn StorageProvider) -> Result<(), SyncError> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| SyncError::State(format!("queue serialise: {e}")))?;
        local.write(&queue_path()?, &bytes, WriteMode::Overwrite)?;
        Ok(())
    }

    /// Forget all pending work (after a successful run). Writes only if something was pending.
    pub fn clear(local: &dyn StorageProvider) -> Result<(), SyncError> {
        if Self::load(local)?.pending.is_empty() {
            return Ok(());
        }
        Self::default().save(local)
    }
}

/// Paths awaiting sync because the remote was unreachable. Empty if unreadable or none.
pub fn pending(local: &dyn StorageProvider) -> Vec<String> {
    OfflineQueue::load(local)
        .map(|q| q.pending)
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use selenite_storage::MemoryStorage;
    use std::io;

    fn io_err(kind: io::ErrorKind) -> StorageError {
        StorageError::Io {
            path: "x".into(),
            source: io::Error::from(kind),
        }
    }

    #[test]
    fn classifies_offline() {
        assert!(is_offline(&io_err(io::ErrorKind::ConnectionRefused)));
        assert!(is_offline(&io_err(io::ErrorKind::TimedOut)));
        assert!(is_offline(&io_err(io::ErrorKind::NotConnected)));
        assert!(!is_offline(&io_err(io::ErrorKind::PermissionDenied)));
        assert!(is_offline(&StorageError::Internal(
            "offline: no route".into()
        )));
        assert!(!is_offline(&StorageError::Internal("poisoned".into())));
    }

    #[test]
    fn queue_round_trip_and_clear() {
        let s = MemoryStorage::new();
        assert!(pending(&s).is_empty());
        let q = OfflineQueue {
            since: 1,
            last_attempt: 2,
            pending: vec!["a.md".into()],
        };
        q.save(&s).unwrap();
        assert_eq!(pending(&s), vec!["a.md".to_string()]);
        OfflineQueue::clear(&s).unwrap();
        assert!(pending(&s).is_empty());
    }
}

//! The `StorageProvider` abstraction over user-owned storage.
//!
//! Providers: [`LocalFolder`] (a directory on disk) and [`MemoryStorage`] (in-memory, for tests).
//! Cloud drives: [`GoogleDrive`] and [`OneDrive`] (OAuth in [`oauth`]). Nothing in this crate logs file content.

mod gdrive;
mod local;
mod memory;
pub mod oauth;
mod onedrive;
#[cfg(test)]
mod testutil;
mod util;

pub use gdrive::GoogleDrive;
pub use local::LocalFolder;
pub use memory::MemoryStorage;
pub use onedrive::OneDrive;
pub use selenite_core::VaultPath;

use std::time::SystemTime;

/// Opaque version token for a file.
///
/// `LocalFolder` uses `"<mtime_nanos>-<size>"`; cloud providers will use etags.
/// Only equality is meaningful.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Revision(pub String);

/// Metadata of one stored file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Vault-relative path.
    pub path: VaultPath,
    /// Size in bytes.
    pub size: u64,
    /// Last modification time.
    pub modified: SystemTime,
    /// Current version token.
    pub revision: Revision,
}

/// How a write treats an existing file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteMode {
    /// Fail with [`StorageError::AlreadyExists`] if the file is present.
    CreateNew,
    /// Fail with [`StorageError::Conflict`] unless the current revision equals this one.
    Replace(Revision),
    /// Unconditional write; for app-owned files such as config.
    Overwrite,
}

/// Errors returned by storage providers.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The requested file does not exist.
    #[error("not found: {0}")]
    NotFound(VaultPath),
    /// The target already exists.
    #[error("already exists: {0}")]
    AlreadyExists(VaultPath),
    /// The file changed (or vanished) since the caller last saw it.
    #[error("conflict on {path}: expected revision {expected:?}, found {actual:?}")]
    Conflict {
        /// The file in question.
        path: VaultPath,
        /// Revision the caller expected.
        expected: Revision,
        /// Revision actually present, `None` if the file does not exist.
        actual: Option<Revision>,
    },
    /// The path is unusable (for example it resolves outside the storage root).
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// An underlying I/O failure.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// Path (relative where possible) the operation concerned.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// An internal invariant failed (for example a poisoned lock).
    #[error("internal storage error: {0}")]
    Internal(String),
}

/// Result alias for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// User-owned storage. Synchronous by design; async callers use `spawn_blocking`.
pub trait StorageProvider: Send + Sync {
    /// Stable identifier, e.g. `"local"`.
    fn kind(&self) -> &'static str;
    /// All files (not directories) recursively, including hidden ones. Order unspecified.
    fn list(&self) -> Result<Vec<Entry>>;
    /// All directories (relative, excluding root), for the file tree, including empty ones.
    fn list_dirs(&self) -> Result<Vec<VaultPath>>;
    /// Metadata of a file, or `None` if it does not exist.
    fn stat(&self, path: &VaultPath) -> Result<Option<Entry>>;
    /// Read a whole file.
    fn read(&self, path: &VaultPath) -> Result<Vec<u8>>;
    /// Atomic write (temp file in the same directory + rename/persist). Creates parent dirs.
    /// Returns the new [`Entry`].
    fn write(&self, path: &VaultPath, data: &[u8], mode: WriteMode) -> Result<Entry>;
    /// Rename/move; fails with `AlreadyExists` if `to` exists; creates parent dirs.
    fn rename(&self, from: &VaultPath, to: &VaultPath) -> Result<Entry>;
    /// Soft delete: move to `.selenite/trash/<unix-secs>-<filename>`; never hard-deletes.
    /// Returns the trash path.
    fn trash(&self, path: &VaultPath) -> Result<VaultPath>;
    /// Create a directory (and parents); succeeds if it already exists.
    fn create_dir(&self, path: &VaultPath) -> Result<()>;
}

/// Content hash (blake3, lowercase hex) used by the index and sync.
pub fn content_hash(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

pub(crate) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn vault_path(raw: &str) -> Result<VaultPath> {
    VaultPath::new(raw).map_err(|e| StorageError::InvalidPath(format!("{raw}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_hex() {
        let h = content_hash(b"abc");
        assert_eq!(h.len(), 64);
        assert_eq!(h, content_hash(b"abc"));
        assert_ne!(h, content_hash(b"abd"));
    }

    #[test]
    fn trait_is_object_safe() {
        let b: Box<dyn StorageProvider> = Box::new(MemoryStorage::new());
        assert_eq!(b.kind(), "memory");
    }
}

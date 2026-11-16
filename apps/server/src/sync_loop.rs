//! Background sync: every interval, sync each configured remote, then refresh the index.
//!
//! The engine lock is held only for the final refresh; the sync itself works on its own
//! handle to the vault folder. Logs hold remote ids and counts, never paths or content.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use selenite_mcp::VaultMcp;
use selenite_storage::{LocalFolder, StorageProvider};
use selenite_sync::{
    RemoteConfig, RemoteError, RemotesFile, keychain_store, now_secs, open_remote, sync,
};
use tokio::sync::watch;

/// Opens the storage provider for a remote.
pub type OpenRemote<'a> =
    dyn Fn(&RemoteConfig) -> Result<Box<dyn StorageProvider>, RemoteError> + 'a;

/// Result of one pass over all remotes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassOutcome {
    /// Remotes that synced.
    pub synced: usize,
    /// Remotes that could not be opened or failed.
    pub failed: usize,
    /// Whether any remote was reached, so the local files may have changed.
    pub reached_remote: bool,
}

/// The production opener: refresh token from the OS keychain.
pub fn keychain_opener(remote: &RemoteConfig) -> Result<Box<dyn StorageProvider>, RemoteError> {
    open_remote(remote, keychain_store(remote.provider, &remote.id))
}

/// Sync every remote of the vault at `root` once. `warned` remembers remotes that already
/// produced a "cannot open" warning so it is logged once, not every interval.
pub fn sync_pass(root: &Path, open: &OpenRemote<'_>, warned: &mut HashSet<String>) -> PassOutcome {
    let mut out = PassOutcome::default();
    let local = match LocalFolder::open(root) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "sync: cannot open the vault folder");
            return out;
        }
    };
    let file = match RemotesFile::load(&local) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "sync: cannot read the remotes file");
            return out;
        }
    };
    for remote in &file.remotes {
        let provider = match open(remote) {
            Ok(p) => {
                warned.remove(&remote.id);
                p
            }
            Err(e) => {
                out.failed += 1;
                if warned.insert(remote.id.clone()) {
                    tracing::warn!(
                        remote = %remote.id,
                        error = %e,
                        "cannot open remote: sign in on a desktop machine and copy the refresh \
                         token into the keychain, or run without sync (--no-sync)"
                    );
                }
                continue;
            }
        };
        match sync(&local, provider.as_ref(), &remote.id, now_secs()) {
            Ok(report) => {
                out.synced += 1;
                out.reached_remote |= !report.offline;
                tracing::info!(
                    remote = %remote.id,
                    pushed = report.pushed.len(),
                    pulled = report.pulled.len(),
                    conflicts = report.conflicts.len(),
                    errors = report.errors.len(),
                    offline = report.offline,
                    "sync finished"
                );
            }
            Err(e) => {
                out.failed += 1;
                tracing::warn!(remote = %remote.id, error = %e, "sync failed");
            }
        }
    }
    out
}

/// Run [`sync_pass`] now and then every `interval` until `stop` flips to `true`.
pub async fn run_loop(
    root: PathBuf,
    vault: VaultMcp,
    interval: Duration,
    mut stop: watch::Receiver<bool>,
) {
    let mut warned = HashSet::new();
    loop {
        let r = root.clone();
        let handed = std::mem::take(&mut warned);
        let joined = tokio::task::spawn_blocking(move || {
            let mut w = handed;
            let outcome = sync_pass(&r, &keychain_opener, &mut w);
            (outcome, w)
        })
        .await;
        match joined {
            Ok((outcome, w)) => {
                warned = w;
                if outcome.reached_remote {
                    match vault.refresh_index().await {
                        Ok((added, updated, removed)) => {
                            tracing::info!(added, updated, removed, "index refreshed after sync");
                        }
                        Err(e) => tracing::error!(error = %e, "index refresh failed"),
                    }
                }
            }
            Err(_) => tracing::error!("sync task failed"),
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use selenite_storage::MemoryStorage;
    use selenite_sync::Provider;

    fn vault_with_remote() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tmp");
        let local = LocalFolder::create(dir.path()).expect("vault");
        let file = RemotesFile {
            remotes: vec![RemoteConfig {
                id: "gdrive-1".into(),
                provider: Provider::GoogleDrive,
                folder: "Selenite".into(),
                account_hint: None,
            }],
        };
        file.save(&local).expect("save remotes");
        dir
    }

    #[test]
    fn unopenable_remote_warns_once_and_does_not_fail_the_pass() {
        let dir = vault_with_remote();
        let mut warned = HashSet::new();
        let open = |r: &RemoteConfig| Err(RemoteError::NotSignedIn(r.id.clone()));
        let a = sync_pass(dir.path(), &open, &mut warned);
        assert_eq!(a.failed, 1);
        assert!(!a.reached_remote);
        assert_eq!(warned.len(), 1);
        let b = sync_pass(dir.path(), &open, &mut warned);
        assert_eq!(b.failed, 1);
        assert_eq!(warned.len(), 1);
    }

    #[test]
    fn reachable_remote_syncs() {
        let dir = vault_with_remote();
        let mut warned = HashSet::new();
        let open = |_: &RemoteConfig| -> Result<Box<dyn StorageProvider>, RemoteError> {
            Ok(Box::new(MemoryStorage::new()))
        };
        let out = sync_pass(dir.path(), &open, &mut warned);
        assert_eq!(out.synced, 1);
        assert!(out.reached_remote);
    }
}

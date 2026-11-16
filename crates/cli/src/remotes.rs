//! `selenite remote ...` and `selenite sync`: cloud remotes for a vault.
//!
//! Remote configuration lives in `.selenite/remotes.yaml` (no secrets); refresh tokens live in
//! the OS keychain. Nothing here prints tokens, client ids or file content, only paths and counts.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use selenite_index::Engine;
use selenite_storage::oauth::{OAuthConfig, TokenStore};
use selenite_storage::{LocalFolder, StorageProvider};
use selenite_sync::remote::SIGN_IN_TIMEOUT;
use selenite_sync::{
    Provider, RemoteConfig, RemoteError, RemotesFile, ReportSummary, now_secs, pending, sign_in,
    sync,
};
use serde::Serialize;

/// Exit code when every remote was unreachable (changes are queued).
pub const EXIT_OFFLINE: u8 = 3;

/// Opens the storage provider for a configured remote.
pub type OpenRemote<'a> =
    dyn Fn(&RemoteConfig) -> Result<Box<dyn StorageProvider>, RemoteError> + 'a;
/// Gives the token store for `(provider, remote id)`.
pub type StoreFor<'a> = dyn Fn(Provider, &str) -> Arc<dyn TokenStore> + 'a;

fn print_json<T: Serialize + ?Sized>(out: &mut dyn Write, value: &T) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
    Ok(())
}

/// `remote add`: sign in through the browser, store the token, record the remote.
#[allow(clippy::too_many_arguments)]
pub fn remote_add(
    root: &Path,
    provider: Provider,
    folder: Option<String>,
    client: Result<OAuthConfig, RemoteError>,
    store_for: &StoreFor<'_>,
    open_browser: &dyn Fn(&str) -> Result<(), String>,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let local = LocalFolder::open(root)?;
    let mut file = RemotesFile::load(&local)?;
    let id = file.next_id(provider);
    let folder = folder
        .map(|f| f.trim().to_owned())
        .filter(|f| !f.is_empty())
        .or_else(|| {
            local
                .root()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "vault".to_owned());
    if client.is_ok() {
        eprintln!("Waiting for sign-in in your browser…");
    }
    let store = store_for(provider, &id);
    sign_in(
        provider,
        client,
        Arc::clone(&store),
        open_browser,
        SIGN_IN_TIMEOUT,
    )?;
    let remote = RemoteConfig {
        id: id.clone(),
        provider,
        folder,
        account_hint: None,
    };
    file.remotes.push(remote.clone());
    if let Err(e) = file.save(&local) {
        // Do not leave a token behind for a remote that was never recorded.
        let _ = store.clear();
        return Err(e.into());
    }
    if json {
        print_json(out, &remote)?;
    } else {
        writeln!(
            out,
            "Added {} remote {id} (folder \"{}\"). Run `selenite sync` to sync.",
            provider.label(),
            remote.folder
        )?;
    }
    Ok(())
}

/// `remote list`.
pub fn remote_list(root: &Path, json: bool, out: &mut dyn Write) -> Result<()> {
    let file = RemotesFile::load(&LocalFolder::open(root)?)?;
    if json {
        return print_json(out, &file.remotes);
    }
    if file.remotes.is_empty() {
        writeln!(
            out,
            "No remotes. Add one with `selenite remote add google-drive`."
        )?;
    }
    for r in &file.remotes {
        writeln!(
            out,
            "{}  {}  folder \"{}\"",
            r.id,
            r.provider.slug(),
            r.folder
        )?;
    }
    Ok(())
}

/// `remote remove`: forget the configuration and the stored token. Remote files are kept.
pub fn remote_remove(
    root: &Path,
    id: &str,
    store_for: &StoreFor<'_>,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let local = LocalFolder::open(root)?;
    let mut file = RemotesFile::load(&local)?;
    let Some(removed) = file.remove(id) else {
        bail!("no remote with id {id:?} (see `selenite remote list`)");
    };
    file.save(&local)?;
    store_for(removed.provider, &removed.id).clear()?;
    if json {
        print_json(out, &removed)?;
    } else {
        writeln!(
            out,
            "Removed remote {id}. Files already in the cloud were not touched."
        )?;
    }
    Ok(())
}

#[derive(Serialize)]
struct RemoteRun {
    remote: String,
    #[serde(flatten)]
    report: Option<ReportSummary>,
    error: Option<String>,
    pending: usize,
}

/// `sync`: run every remote (or one) and refresh the index. Returns the process exit code:
/// 0 success, 1 error, 3 offline (changes queued).
pub fn sync_cmd(
    root: &Path,
    only: Option<&str>,
    json: bool,
    open: &OpenRemote<'_>,
    out: &mut dyn Write,
) -> Result<u8> {
    let local = LocalFolder::open(root)?;
    let file = RemotesFile::load(&local)?;
    let remotes: Vec<&RemoteConfig> = match only {
        Some(id) => match file.get(id) {
            Some(r) => vec![r],
            None => bail!("no remote with id {id:?} (see `selenite remote list`)"),
        },
        None => file.remotes.iter().collect(),
    };
    if remotes.is_empty() {
        if json {
            print_json(out, &Vec::<RemoteRun>::new())?;
        } else {
            writeln!(
                out,
                "No remotes configured. Add one with `selenite remote add google-drive` or `selenite remote add onedrive`."
            )?;
        }
        return Ok(0);
    }
    let mut runs = Vec::new();
    for r in remotes {
        let result = open(r).map_err(|e| e.to_string()).and_then(|remote| {
            sync(&local, remote.as_ref(), &r.id, now_secs()).map_err(|e| e.to_string())
        });
        let (report, error) = match result {
            Ok(rep) => (Some(ReportSummary::from(&rep)), None),
            Err(e) => (None, Some(e)),
        };
        runs.push(RemoteRun {
            remote: r.id.clone(),
            report,
            error,
            pending: pending(&local).len(),
        });
    }
    if runs
        .iter()
        .any(|r| r.report.as_ref().is_some_and(|s| !s.offline))
    {
        Engine::open_local(root)?.refresh()?;
    }
    let failed = runs
        .iter()
        .any(|r| r.error.is_some() || r.report.as_ref().is_some_and(|s| !s.errors.is_empty()));
    let offline = runs
        .iter()
        .any(|r| r.report.as_ref().is_some_and(|s| s.offline));
    if json {
        print_json(out, &runs)?;
    } else {
        for run in &runs {
            write_run(out, run)?;
        }
    }
    Ok(if failed {
        1
    } else if offline {
        EXIT_OFFLINE
    } else {
        0
    })
}

fn write_run(out: &mut dyn Write, run: &RemoteRun) -> Result<()> {
    if let Some(err) = &run.error {
        writeln!(out, "{}: error: {err}", run.remote)?;
        return Ok(());
    }
    let Some(s) = &run.report else {
        return Ok(());
    };
    if s.offline {
        writeln!(
            out,
            "{}: offline, {} changes queued; run sync again when connected",
            run.remote, run.pending
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "{}: pushed {}, pulled {}, conflicts {}, trashed {}, errors {}",
        run.remote,
        s.pushed.len(),
        s.pulled.len(),
        s.conflicts.len(),
        s.trashed.len(),
        s.errors.len()
    )?;
    for c in &s.conflicts {
        writeln!(
            out,
            "  conflict: {} (other version kept as {}, yours as {})",
            c.path, c.copy_path, c.local_copy_path
        )?;
    }
    for e in &s.errors {
        writeln!(out, "  error: {e}")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use selenite_core::VaultPath;
    use selenite_storage::oauth::MemoryTokenStore;
    use selenite_storage::{MemoryStorage, WriteMode};
    use selenite_sync::remote::{CompiledIds, client_config_with};
    use std::cell::Cell;

    fn vault() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("My Vault");
        Engine::init_local(&root).unwrap();
        (tmp, root)
    }

    fn remote(id: &str) -> RemoteConfig {
        RemoteConfig {
            id: id.into(),
            provider: Provider::GoogleDrive,
            folder: "F".into(),
            account_hint: None,
        }
    }

    #[test]
    fn sync_without_remotes_explains_how_to_add_one() {
        let (_t, root) = vault();
        let mut out = Vec::new();
        let never: &OpenRemote<'_> = &|_| panic!("no remote should be opened");
        let code = sync_cmd(&root, None, false, never, &mut out).unwrap();
        assert_eq!(code, 0);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("selenite remote add google-drive")
        );
    }

    #[test]
    fn remote_add_without_client_id_errors_before_opening_a_browser() {
        let (_t, root) = vault();
        let opened = Cell::new(false);
        let open = |_: &str| {
            opened.set(true);
            Ok(())
        };
        let none = |_: &str| None;
        let store_for: &StoreFor<'_> = &|_, _| Arc::new(MemoryTokenStore::new());
        let err = remote_add(
            &root,
            Provider::GoogleDrive,
            None,
            client_config_with(Provider::GoogleDrive, &none, CompiledIds::default()),
            store_for,
            &open,
            false,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Google Drive sign-in is not configured for this build (see docs/oauth-setup.md)"
        );
        assert!(!opened.get());
        let file = RemotesFile::load(&LocalFolder::open(&root).unwrap()).unwrap();
        assert!(file.remotes.is_empty());
    }

    #[test]
    fn list_and_remove_round_trip() {
        let (_t, root) = vault();
        let local = LocalFolder::open(&root).unwrap();
        RemotesFile {
            remotes: vec![remote("gdrive-1")],
        }
        .save(&local)
        .unwrap();
        let mut out = Vec::new();
        remote_list(&root, false, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("gdrive-1"));
        let cleared = Arc::new(MemoryTokenStore::new());
        cleared.save("tok").unwrap();
        let c2 = Arc::clone(&cleared);
        let store_for: &StoreFor<'_> = &move |_, _| c2.clone();
        remote_remove(&root, "gdrive-1", store_for, false, &mut Vec::new()).unwrap();
        assert!(cleared.load().unwrap().is_none());
        assert!(RemotesFile::load(&local).unwrap().remotes.is_empty());
        assert!(remote_remove(&root, "gdrive-1", store_for, false, &mut Vec::new()).is_err());
    }

    #[test]
    fn sync_pulls_remote_files_and_refreshes_the_index() {
        let (_t, root) = vault();
        let local = LocalFolder::open(&root).unwrap();
        RemotesFile {
            remotes: vec![remote("gdrive-1")],
        }
        .save(&local)
        .unwrap();
        let mem = Arc::new(MemoryStorage::new());
        mem.write(
            &VaultPath::new("Remote note.md").unwrap(),
            b"# Remote note\n\nzebrafish\n",
            WriteMode::CreateNew,
        )
        .unwrap();
        struct Shared(Arc<MemoryStorage>);
        impl StorageProvider for Shared {
            fn kind(&self) -> &'static str {
                "memory"
            }
            fn list(&self) -> selenite_storage::Result<Vec<selenite_storage::Entry>> {
                self.0.list()
            }
            fn list_dirs(&self) -> selenite_storage::Result<Vec<VaultPath>> {
                self.0.list_dirs()
            }
            fn stat(
                &self,
                p: &VaultPath,
            ) -> selenite_storage::Result<Option<selenite_storage::Entry>> {
                self.0.stat(p)
            }
            fn read(&self, p: &VaultPath) -> selenite_storage::Result<Vec<u8>> {
                self.0.read(p)
            }
            fn write(
                &self,
                p: &VaultPath,
                d: &[u8],
                m: WriteMode,
            ) -> selenite_storage::Result<selenite_storage::Entry> {
                self.0.write(p, d, m)
            }
            fn rename(
                &self,
                a: &VaultPath,
                b: &VaultPath,
            ) -> selenite_storage::Result<selenite_storage::Entry> {
                self.0.rename(a, b)
            }
            fn trash(&self, p: &VaultPath) -> selenite_storage::Result<VaultPath> {
                self.0.trash(p)
            }
            fn create_dir(&self, p: &VaultPath) -> selenite_storage::Result<()> {
                self.0.create_dir(p)
            }
        }
        let m2 = Arc::clone(&mem);
        let open: &OpenRemote<'_> = &move |_| Ok(Box::new(Shared(Arc::clone(&m2))));
        let mut out = Vec::new();
        let code = sync_cmd(&root, Some("gdrive-1"), false, open, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("pulled 1"), "{text}");
        let engine = Engine::open_local(&root).unwrap();
        assert!(!engine.search("zebrafish", 5).unwrap().is_empty());
        assert!(sync_cmd(&root, Some("nope"), false, open, &mut Vec::new()).is_err());
    }
}

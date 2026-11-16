//! Remote configuration shared by the CLI and the desktop app.
//!
//! A vault lists its remotes in `.selenite/remotes.yaml`. That file is synced and holds **no
//! secrets**: only an id, the provider, the remote folder name and an optional account hint. The
//! refresh token lives in the OS keychain (service `<APP_NAME>-<provider>`, account = remote id).
//! OAuth client ids come from the environment or from the build, never from the vault, and are
//! never printed or logged.

use std::sync::Arc;
use std::time::Duration;

use selenite_core::{APP_NAME, VaultPath};
use selenite_storage::oauth::{
    Authenticator, KeyringTokenStore, OAuthConfig, TokenStore, authorize,
};
use selenite_storage::{GoogleDrive, OneDrive, StorageError, StorageProvider, WriteMode};
use serde::{Deserialize, Serialize};

use crate::{ConflictInfo, SyncError, SyncReport};

const REMOTES_PATH: &str = ".selenite/remotes.yaml";

/// How long an interactive sign-in may take before it is abandoned.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// Errors from remote configuration and sign-in.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The build has no OAuth client id for the provider.
    #[error("{0} sign-in is not configured for this build (see docs/oauth-setup.md)")]
    NotConfigured(&'static str),
    /// No refresh token is stored for the remote.
    #[error("not signed in to remote {0}; remove it and add it again")]
    NotSignedIn(String),
    /// Storage, keychain or network failure.
    #[error("{0}")]
    Storage(#[from] StorageError),
    /// `remotes.yaml` could not be parsed or written.
    #[error("remotes file error: {0}")]
    State(String),
    /// The sign-in could not start (for example the browser did not open).
    #[error("sign-in failed: {0}")]
    SignIn(String),
}

/// A supported cloud storage provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    /// Google Drive (`drive.file` scope).
    #[serde(rename = "google-drive")]
    GoogleDrive,
    /// OneDrive (app folder scope).
    #[serde(rename = "onedrive")]
    OneDrive,
}

impl Provider {
    /// Stable identifier used in `remotes.yaml` and the keychain service name.
    pub fn slug(self) -> &'static str {
        match self {
            Self::GoogleDrive => "google-drive",
            Self::OneDrive => "onedrive",
        }
    }

    /// Name shown to people.
    pub fn label(self) -> &'static str {
        match self {
            Self::GoogleDrive => "Google Drive",
            Self::OneDrive => "OneDrive",
        }
    }

    /// Parse a slug.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "google-drive" => Some(Self::GoogleDrive),
            "onedrive" => Some(Self::OneDrive),
            _ => None,
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::GoogleDrive => "gdrive",
            Self::OneDrive => "onedrive",
        }
    }
}

/// One configured remote. Contains no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Stable id, e.g. `gdrive-1`; also names the sync manifest and the keychain account.
    pub id: String,
    /// Provider.
    pub provider: Provider,
    /// Name of the remote root folder (Google Drive; OneDrive always uses its app folder).
    pub folder: String,
    /// Optional label to tell accounts apart (never a token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_hint: Option<String>,
}

/// The content of `.selenite/remotes.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotesFile {
    /// Configured remotes in the order they were added.
    #[serde(default)]
    pub remotes: Vec<RemoteConfig>,
}

fn remotes_path() -> Result<VaultPath, RemoteError> {
    VaultPath::new(REMOTES_PATH).map_err(|e| RemoteError::State(e.to_string()))
}

impl RemotesFile {
    /// Load the file; empty if the vault has none yet.
    pub fn load(local: &dyn StorageProvider) -> Result<Self, RemoteError> {
        let path = remotes_path()?;
        if local.stat(&path)?.is_none() {
            return Ok(Self::default());
        }
        let bytes = local.read(&path)?;
        serde_norway::from_slice(&bytes).map_err(|e| RemoteError::State(e.to_string()))
    }

    /// Write the file (atomic, unconditional: the app owns it).
    pub fn save(&self, local: &dyn StorageProvider) -> Result<(), RemoteError> {
        let text = serde_norway::to_string(self).map_err(|e| RemoteError::State(e.to_string()))?;
        local.write(&remotes_path()?, text.as_bytes(), WriteMode::Overwrite)?;
        Ok(())
    }

    /// The remote with this id.
    pub fn get(&self, id: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.id == id)
    }

    /// The first unused id for a provider: `gdrive-1`, `gdrive-2`, ...
    pub fn next_id(&self, provider: Provider) -> String {
        (1..)
            .map(|n| format!("{}-{n}", provider.id_prefix()))
            .find(|id| self.get(id).is_none())
            .unwrap_or_else(|| format!("{}-1", provider.id_prefix()))
    }

    /// Remove a remote's configuration; returns it if it existed.
    pub fn remove(&mut self, id: &str) -> Option<RemoteConfig> {
        let i = self.remotes.iter().position(|r| r.id == id)?;
        Some(self.remotes.remove(i))
    }
}

// ---------------------------------------------------------------------------------------------
// Client ids
// ---------------------------------------------------------------------------------------------

/// OAuth client ids baked in at build time (`option_env!`, for release builds).
#[derive(Debug, Clone, Copy, Default)]
pub struct CompiledIds {
    /// Google client id.
    pub google_id: Option<&'static str>,
    /// Google (desktop, non-confidential) client secret.
    pub google_secret: Option<&'static str>,
    /// Microsoft application id.
    pub onedrive_id: Option<&'static str>,
}

fn compiled() -> CompiledIds {
    CompiledIds {
        google_id: option_env!("SELENITE_GOOGLE_CLIENT_ID"),
        google_secret: option_env!("SELENITE_GOOGLE_CLIENT_SECRET"),
        onedrive_id: option_env!("SELENITE_ONEDRIVE_CLIENT_ID"),
    }
}

/// First non-empty of: runtime `name`, runtime `TEST` name, compile-time value.
fn pick(
    lookup: &dyn Fn(&str) -> Option<String>,
    name: &str,
    compiled: Option<&str>,
) -> Option<String> {
    let test = name.replacen("SELENITE_", "SELENITE_TEST_", 1);
    [lookup(name), lookup(&test), compiled.map(str::to_owned)]
        .into_iter()
        .flatten()
        .map(|v| v.trim().to_owned())
        .find(|v| !v.is_empty())
}

/// OAuth configuration for a provider from the process environment, then the build.
pub fn client_config(provider: Provider) -> Result<OAuthConfig, RemoteError> {
    client_config_with(provider, &|k| std::env::var(k).ok(), compiled())
}

/// [`client_config`] with an explicit environment lookup and compiled-in ids (for tests).
pub fn client_config_with(
    provider: Provider,
    lookup: &dyn Fn(&str) -> Option<String>,
    compiled: CompiledIds,
) -> Result<OAuthConfig, RemoteError> {
    match provider {
        Provider::GoogleDrive => {
            let id = pick(lookup, "SELENITE_GOOGLE_CLIENT_ID", compiled.google_id);
            let secret = pick(
                lookup,
                "SELENITE_GOOGLE_CLIENT_SECRET",
                compiled.google_secret,
            );
            match (id, secret) {
                (Some(id), Some(secret)) => Ok(OAuthConfig::google_drive(&id, &secret)),
                _ => Err(RemoteError::NotConfigured("Google Drive")),
            }
        }
        Provider::OneDrive => {
            match pick(lookup, "SELENITE_ONEDRIVE_CLIENT_ID", compiled.onedrive_id) {
                Some(id) => Ok(OAuthConfig::onedrive(&id)),
                None => Err(RemoteError::NotConfigured("OneDrive")),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tokens, sign-in, opening a remote
// ---------------------------------------------------------------------------------------------

/// The OS-keychain store for one remote's refresh token.
pub fn keychain_store(provider: Provider, remote_id: &str) -> Arc<dyn TokenStore> {
    Arc::new(KeyringTokenStore::new(
        &format!("{APP_NAME}-{}", provider.slug()),
        remote_id,
    ))
}

/// Run the browser sign-in for `cfg` and store the refresh token in `store`.
///
/// The client configuration is checked first, so a build without client ids fails before any
/// browser opens.
pub fn sign_in(
    provider: Provider,
    client: Result<OAuthConfig, RemoteError>,
    store: Arc<dyn TokenStore>,
    open_browser: &dyn Fn(&str) -> Result<(), String>,
    timeout: Duration,
) -> Result<(), RemoteError> {
    let cfg = client?;
    let tokens = authorize(&cfg, open_browser, timeout)?;
    Authenticator::with_tokens(cfg, store, tokens)?;
    tracing::info!(provider = provider.slug(), "signed in");
    Ok(())
}

/// Build the storage provider for a configured remote using the stored refresh token.
pub fn open_remote(
    remote: &RemoteConfig,
    store: Arc<dyn TokenStore>,
) -> Result<Box<dyn StorageProvider>, RemoteError> {
    open_remote_with(remote, store, client_config(remote.provider)?)
}

/// [`open_remote`] with an explicit OAuth configuration.
pub fn open_remote_with(
    remote: &RemoteConfig,
    store: Arc<dyn TokenStore>,
    cfg: OAuthConfig,
) -> Result<Box<dyn StorageProvider>, RemoteError> {
    if store.load()?.is_none() {
        return Err(RemoteError::NotSignedIn(remote.id.clone()));
    }
    let auth = Arc::new(Authenticator::new(cfg, store));
    Ok(match remote.provider {
        Provider::GoogleDrive => Box::new(GoogleDrive::new(auth, &remote.folder)?),
        Provider::OneDrive => Box::new(OneDrive::new(auth)?),
    })
}

/// Current Unix time in seconds.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

// ---------------------------------------------------------------------------------------------
// Report summary
// ---------------------------------------------------------------------------------------------

/// A conflict and the copies that keep both versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictSummary {
    /// The conflicted path (local content untouched).
    pub path: String,
    /// Copy holding the remote version.
    pub copy_path: String,
    /// Copy holding the local version.
    pub local_copy_path: String,
}

impl From<&ConflictInfo> for ConflictSummary {
    fn from(c: &ConflictInfo) -> Self {
        Self {
            path: c.path.clone(),
            copy_path: c.copy_path.clone(),
            local_copy_path: c.local_copy_path.clone(),
        }
    }
}

/// Serialisable view of a [`SyncReport`]. Paths and messages only, never content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReportSummary {
    /// Paths uploaded.
    pub pushed: Vec<String>,
    /// Paths downloaded.
    pub pulled: Vec<String>,
    /// New conflicts with their copy paths.
    pub conflicts: Vec<ConflictSummary>,
    /// Paths moved to trash on either side.
    pub trashed: Vec<String>,
    /// Per-path failures as `path: message`.
    pub errors: Vec<String>,
    /// Paths with an unresolved conflict (new or older).
    pub unresolved: Vec<String>,
    /// The remote was unreachable.
    pub offline: bool,
}

impl From<&SyncReport> for ReportSummary {
    fn from(r: &SyncReport) -> Self {
        Self {
            pushed: r.pushed.clone(),
            pulled: r.pulled.clone(),
            conflicts: r.conflicts.iter().map(ConflictSummary::from).collect(),
            trashed: r
                .trashed_local
                .iter()
                .chain(&r.trashed_remote)
                .cloned()
                .collect(),
            errors: r.errors.iter().map(|(p, m)| format!("{p}: {m}")).collect(),
            unresolved: r.unresolved.clone(),
            offline: r.offline,
        }
    }
}

impl From<SyncError> for RemoteError {
    fn from(e: SyncError) -> Self {
        Self::State(e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use selenite_storage::MemoryStorage;
    use selenite_storage::oauth::MemoryTokenStore;
    use std::collections::HashMap;

    fn sample() -> RemotesFile {
        RemotesFile {
            remotes: vec![
                RemoteConfig {
                    id: "gdrive-1".into(),
                    provider: Provider::GoogleDrive,
                    folder: "Notes".into(),
                    account_hint: Some("me@example.com".into()),
                },
                RemoteConfig {
                    id: "onedrive-1".into(),
                    provider: Provider::OneDrive,
                    folder: "Notes".into(),
                    account_hint: None,
                },
            ],
        }
    }

    #[test]
    fn remotes_file_round_trips_and_has_no_secrets() {
        let s = MemoryStorage::new();
        assert_eq!(RemotesFile::load(&s).unwrap(), RemotesFile::default());
        let f = sample();
        f.save(&s).unwrap();
        assert_eq!(RemotesFile::load(&s).unwrap(), f);
        let text = String::from_utf8(
            s.read(&VaultPath::new(".selenite/remotes.yaml").unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(text.contains("provider: google-drive"), "{text}");
        assert!(!text.contains("token"));
    }

    #[test]
    fn ids_are_unique_and_remove_works() {
        let mut f = sample();
        assert_eq!(f.next_id(Provider::GoogleDrive), "gdrive-2");
        assert_eq!(f.next_id(Provider::OneDrive), "onedrive-2");
        assert!(f.remove("gdrive-1").is_some());
        assert!(f.remove("gdrive-1").is_none());
        assert_eq!(f.next_id(Provider::GoogleDrive), "gdrive-1");
    }

    #[test]
    fn client_ids_resolve_in_order() {
        let env: HashMap<&str, &str> = HashMap::from([
            ("SELENITE_TEST_GOOGLE_CLIENT_ID", "test-id"),
            ("SELENITE_TEST_GOOGLE_CLIENT_SECRET", "test-secret"),
            ("SELENITE_GOOGLE_CLIENT_SECRET", "real-secret"),
        ]);
        let lookup = |k: &str| env.get(k).map(|v| (*v).to_string());
        let cfg =
            client_config_with(Provider::GoogleDrive, &lookup, CompiledIds::default()).unwrap();
        assert_eq!(cfg.client_id, "test-id");
        assert_eq!(cfg.client_secret.as_deref(), Some("real-secret"));
        // Compile-time ids are the last resort.
        let none = |_: &str| None;
        let built = CompiledIds {
            onedrive_id: Some("built"),
            ..CompiledIds::default()
        };
        let cfg = client_config_with(Provider::OneDrive, &none, built).unwrap();
        assert_eq!(cfg.client_id, "built");
    }

    #[test]
    fn missing_client_id_is_a_clear_error_and_opens_no_browser() {
        let none = |_: &str| None;
        let err =
            client_config_with(Provider::GoogleDrive, &none, CompiledIds::default()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Google Drive sign-in is not configured for this build (see docs/oauth-setup.md)"
        );
        let opened = std::cell::Cell::new(false);
        let open = |_: &str| {
            opened.set(true);
            Ok(())
        };
        let res = sign_in(
            Provider::OneDrive,
            client_config_with(Provider::OneDrive, &none, CompiledIds::default()),
            Arc::new(MemoryTokenStore::new()),
            &open,
            Duration::from_millis(10),
        );
        assert!(matches!(res, Err(RemoteError::NotConfigured("OneDrive"))));
        assert!(!opened.get());
    }

    #[test]
    fn open_remote_requires_a_stored_token() {
        let r = &sample().remotes[1];
        let res = open_remote_with(
            r,
            Arc::new(MemoryTokenStore::new()),
            OAuthConfig::onedrive("x"),
        );
        assert!(matches!(res, Err(RemoteError::NotSignedIn(_))));
    }

    #[test]
    fn summary_flattens_report() {
        let rep = SyncReport {
            pushed: vec!["a.md".into()],
            trashed_local: vec!["b.md".into()],
            trashed_remote: vec!["c.md".into()],
            errors: vec![("d.md".into(), "boom".into())],
            offline: true,
            ..SyncReport::default()
        };
        let s = ReportSummary::from(&rep);
        assert_eq!(s.trashed, vec!["b.md", "c.md"]);
        assert_eq!(s.errors, vec!["d.md: boom"]);
        assert!(s.offline);
    }
}

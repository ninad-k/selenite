//! Shared application state: the open engine, the settings file location and the reminder
//! notifier.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};

use selenite_index::Engine;
use selenite_plugins::PluginHost;
use selenite_reminders::{MemoryNotifier, Notifier};
use selenite_storage::StorageProvider;
use selenite_sync::{ConflictSummary, RemoteConfig, keychain_store};
use serde::Serialize;

use crate::commands::CmdError;

/// What the UI shows about OS reminder registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemindersStatus {
    /// Notifier backend (`windows-toast`, `memory`, ...).
    pub kind: String,
    /// Number of reminder occurrences registered by the last successful re-plan.
    pub scheduled: usize,
    /// The last notifier failure, cleared by the next success.
    pub last_error: Option<String>,
}

/// Opens configured remotes and forgets their sign-in. Tests replace it with an in-memory fake.
pub trait RemoteFactory: Send + Sync {
    /// Build the storage provider for a configured remote.
    ///
    /// # Errors
    ///
    /// Returns a message (never a token) when the remote cannot be opened.
    fn open(&self, remote: &RemoteConfig) -> Result<Box<dyn StorageProvider>, String>;

    /// Delete the stored sign-in (refresh token) of a remote.
    ///
    /// # Errors
    ///
    /// Returns a message when the keychain refuses.
    fn forget(&self, remote: &RemoteConfig) -> Result<(), String>;
}

/// The real factory: OAuth through the OS keychain.
#[derive(Debug, Default)]
pub struct KeychainRemotes;

impl RemoteFactory for KeychainRemotes {
    fn open(&self, remote: &RemoteConfig) -> Result<Box<dyn StorageProvider>, String> {
        selenite_sync::open_remote(remote, keychain_store(remote.provider, &remote.id))
            .map_err(|e| e.to_string())
    }

    fn forget(&self, remote: &RemoteConfig) -> Result<(), String> {
        keychain_store(remote.provider, &remote.id)
            .clear()
            .map_err(|e| e.to_string())
    }
}

/// Outcome of a sync run as the UI sees it. Paths and counts only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncSummary {
    /// Remotes that were synced.
    pub remotes: usize,
    /// Files uploaded.
    pub pushed: usize,
    /// Files downloaded.
    pub pulled: usize,
    /// Files moved to trash on either side.
    pub trashed: usize,
    /// New conflicts with the copies that keep both versions.
    pub conflicts: Vec<ConflictSummary>,
    /// Paths with an unresolved conflict (new or older).
    pub unresolved: Vec<String>,
    /// Failures: per-path and per-remote messages.
    pub errors: Vec<String>,
    /// At least one remote was unreachable.
    pub offline: bool,
}

/// Mutable sync bookkeeping kept in memory for the running app.
#[derive(Debug, Default)]
pub struct SyncBook {
    /// Unix seconds when the last run finished.
    pub last_run: Option<i64>,
    /// Result of the last run.
    pub last_report: Option<SyncSummary>,
    /// Last whole-run failure, cleared by the next success.
    pub error: Option<String>,
    /// Unix seconds of the last successful sync per remote id.
    pub per_remote: BTreeMap<String, i64>,
}

/// State managed by Tauri and passed to the command layer.
pub struct AppState {
    /// The open vault engine, if any.
    pub engine: Mutex<Option<Engine>>,
    /// Where the remembered-vault settings JSON lives.
    pub settings_path: PathBuf,
    /// OS notification adapter that receives every re-plan.
    pub notifier: Box<dyn Notifier>,
    /// Outcome of the last re-plan.
    pub reminders: Mutex<RemindersStatus>,
    /// Opens remotes; replaced in tests.
    pub remotes: Box<dyn RemoteFactory>,
    /// Sync bookkeeping.
    pub sync: Mutex<SyncBook>,
    /// True while a sync run is in flight (runs never overlap).
    pub sync_running: AtomicBool,
    /// Serves calls from sandboxed plugins (permission checks, rate limits, audit).
    pub plugins: Mutex<PluginHost>,
}

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("settings_path", &self.settings_path)
            .field("notifier", &self.notifier.kind())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Create state with no vault open and an in-memory notifier (tests, headless use).
    #[must_use]
    pub fn new(settings_path: PathBuf) -> Self {
        Self::with_notifier(settings_path, Box::new(MemoryNotifier::new()))
    }

    /// Create state with no vault open and the given notifier.
    #[must_use]
    pub fn with_notifier(settings_path: PathBuf, notifier: Box<dyn Notifier>) -> Self {
        let status = RemindersStatus {
            kind: notifier.kind().to_owned(),
            scheduled: 0,
            last_error: None,
        };
        Self {
            engine: Mutex::new(None),
            settings_path,
            notifier,
            reminders: Mutex::new(status),
            remotes: Box::new(KeychainRemotes),
            sync: Mutex::new(SyncBook::default()),
            sync_running: AtomicBool::new(false),
            plugins: Mutex::new(PluginHost::new()),
        }
    }

    /// Replace the remote factory (tests inject in-memory remotes).
    #[must_use]
    pub fn with_remotes(mut self, remotes: Box<dyn RemoteFactory>) -> Self {
        self.remotes = remotes;
        self
    }

    /// Lock the sync bookkeeping, recovering from a poisoned lock.
    pub fn lock_sync(&self) -> MutexGuard<'_, SyncBook> {
        self.sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lock the engine slot, recovering from a poisoned lock.
    pub fn lock(&self) -> MutexGuard<'_, Option<Engine>> {
        self.engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lock the reminder status, recovering from a poisoned lock.
    pub fn lock_reminders(&self) -> MutexGuard<'_, RemindersStatus> {
        self.reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Run `f` against the open engine, or fail with [`CmdError::NoVault`].
    ///
    /// # Errors
    ///
    /// Returns [`CmdError::NoVault`] when no vault is open, or whatever `f` returns.
    pub fn with_engine<T>(
        &self,
        f: impl FnOnce(&mut Engine) -> Result<T, CmdError>,
    ) -> Result<T, CmdError> {
        let mut guard = self.lock();
        let engine = guard.as_mut().ok_or(CmdError::NoVault)?;
        f(engine)
    }
}

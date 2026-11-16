//! Shared application state: the open engine, the settings file location and the reminder
//! notifier.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use selenite_index::Engine;
use selenite_reminders::{MemoryNotifier, Notifier};
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
        }
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

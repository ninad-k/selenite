//! Shared application state: the open engine and the settings file location.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use selenite_index::Engine;

use crate::commands::CmdError;

/// State managed by Tauri and passed to the command layer.
#[derive(Debug)]
pub struct AppState {
    /// The open vault engine, if any.
    pub engine: Mutex<Option<Engine>>,
    /// Where the remembered-vault settings JSON lives.
    pub settings_path: PathBuf,
}

impl AppState {
    /// Create state with no vault open.
    #[must_use]
    pub fn new(settings_path: PathBuf) -> Self {
        Self {
            engine: Mutex::new(None),
            settings_path,
        }
    }

    /// Lock the engine slot, recovering from a poisoned lock.
    pub fn lock(&self) -> MutexGuard<'_, Option<Engine>> {
        self.engine
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

//! Plugin API v1.
//!
//! Plugins are small JavaScript files in `<vault>/.selenite/plugins/<id>/` that run only inside a
//! sandboxed iframe in the desktop UI and reach the vault through a versioned postMessage RPC.
//! The iframe never receives Tauri IPC; every call arrives in [`PluginHost::call`], which checks
//! the plugin's manifest permissions, rate-limits it and audit-logs writes. See
//! `docs/plugins.md` for the API and the security model.

mod host;
mod manifest;
mod store;

pub use host::{Limits, METHODS, MethodSpec, PluginHost};
pub use manifest::{MANIFEST_FILE, MAX_ID_LEN, Manifest, Permission, is_valid_id};
pub use store::{
    ENABLED_FILE, LoadedPlugin, MAX_SOURCE_BYTES, PLUGINS_DIR, PluginInfo, discover, load_enabled,
    set_enabled,
};

/// Version of the RPC protocol spoken between the UI and the sandboxed iframes.
pub const API_VERSION: u32 = 1;

/// Errors of the plugin layer. Messages never contain page text.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// `manifest.json` (or the script it names) is not acceptable.
    #[error("invalid plugin: {0}")]
    InvalidManifest(String),
    /// No such plugin.
    #[error("plugin not found: {0}")]
    NotFound(String),
    /// The plugin is installed but the user has not enabled it.
    #[error("plugin is not enabled: {0}")]
    Disabled(String),
    /// The plugin's files changed after the user enabled it; it must be enabled again.
    #[error("plugin changed since it was enabled: {0}")]
    Changed(String),
    /// The method is not part of the plugin API.
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    /// The manifest does not grant the permission the method needs.
    #[error("plugin `{plugin}` may not call {method}: the `{needed}` permission is required")]
    PermissionDenied {
        /// Calling plugin.
        plugin: String,
        /// Method it tried.
        method: String,
        /// Permission it lacks.
        needed: &'static str,
    },
    /// Too many calls in the current window.
    #[error("plugin `{0}` is calling too fast; slow down")]
    RateLimited(String),
    /// The call's arguments were rejected.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// The engine refused the operation.
    #[error("{message}")]
    Engine {
        /// Coarse category for the audit log.
        category: &'static str,
        /// What went wrong (no page content).
        message: String,
    },
    /// Reading or writing plugin files failed.
    #[error("plugin storage error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests;

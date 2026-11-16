//! Desktop shell: a thin Tauri host. All business logic lives in the
//! workspace crates; this crate only exposes it to the web UI.

use serde::Serialize;

/// Application identity shown by the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    /// Display name of the application.
    pub name: String,
    /// Version of the desktop shell.
    pub version: String,
}

/// Build the [`AppInfo`] value from core constants and crate metadata.
#[must_use]
pub fn current_app_info() -> AppInfo {
    AppInfo {
        name: selenite_core::APP_NAME.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Tauri command returning the application name and version.
#[tauri::command]
fn app_info() -> AppInfo {
    current_app_info()
}

/// Start the desktop application and block until the window closes.
///
/// # Errors
///
/// Returns a [`tauri::Error`] if the runtime fails to build or run.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<(), tauri::Error> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_info])
        .run(tauri::generate_context!())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_uses_core_name() {
        let info = current_app_info();
        assert_eq!(info.name, selenite_core::APP_NAME);
        assert!(!info.version.is_empty());
    }
}

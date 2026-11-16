//! Desktop shell: a thin Tauri host. All business logic lives in the
//! workspace crates; this crate only exposes it to the web UI.

pub mod commands;
pub mod state;

use selenite_index::TreeNode;
use tauri::{Manager, State};

use commands::{
    AppInfo, BacklinkDto, CmdError, OutLinkDto, PageContent, SaveResult, SearchHitDto, TagDto,
    VaultInfo,
};
use state::AppState;

/// Build the [`AppInfo`] value from core constants and crate metadata.
#[must_use]
pub fn current_app_info() -> AppInfo {
    commands::app_info()
}

fn s<T>(r: Result<T, CmdError>) -> Result<T, String> {
    r.map_err(|e| e.to_string())
}

#[tauri::command]
fn app_info() -> AppInfo {
    commands::app_info()
}

#[tauri::command]
fn open_vault(state: State<'_, AppState>, path: String) -> Result<VaultInfo, String> {
    s(commands::open_vault(&state, &path))
}

#[tauri::command]
fn create_vault(state: State<'_, AppState>, path: String) -> Result<VaultInfo, String> {
    s(commands::create_vault(&state, &path))
}

#[tauri::command]
fn last_vault(state: State<'_, AppState>) -> Option<String> {
    commands::last_vault(&state)
}

#[tauri::command]
fn vault_info(state: State<'_, AppState>) -> Result<VaultInfo, String> {
    s(commands::vault_info(&state))
}

#[tauri::command]
fn tree(state: State<'_, AppState>) -> Result<TreeNode, String> {
    s(commands::tree(&state))
}

#[tauri::command]
fn read_page(state: State<'_, AppState>, path: String) -> Result<PageContent, String> {
    s(commands::read_page(&state, &path))
}

#[tauri::command]
fn save_page(
    state: State<'_, AppState>,
    path: String,
    text: String,
    revision: String,
) -> Result<SaveResult, String> {
    s(commands::save_page(&state, &path, &text, &revision))
}

#[tauri::command]
fn create_page(
    state: State<'_, AppState>,
    title: String,
    folder: Option<String>,
) -> Result<String, String> {
    s(commands::create_page(&state, &title, folder.as_deref()))
}

#[tauri::command]
fn create_page_at(state: State<'_, AppState>, path: String) -> Result<String, String> {
    s(commands::create_page_at(&state, &path))
}

#[tauri::command]
fn resolve(state: State<'_, AppState>, target: String) -> Result<Option<String>, String> {
    s(commands::resolve(&state, &target))
}

#[tauri::command]
fn search(
    state: State<'_, AppState>,
    query: String,
    limit: usize,
) -> Result<Vec<SearchHitDto>, String> {
    s(commands::search(&state, &query, limit))
}

#[tauri::command]
fn backlinks(state: State<'_, AppState>, path: String) -> Result<Vec<BacklinkDto>, String> {
    s(commands::backlinks(&state, &path))
}

#[tauri::command]
fn outgoing(state: State<'_, AppState>, path: String) -> Result<Vec<OutLinkDto>, String> {
    s(commands::outgoing(&state, &path))
}

#[tauri::command]
fn tags(state: State<'_, AppState>) -> Result<Vec<TagDto>, String> {
    s(commands::tags(&state))
}

#[tauri::command]
fn daily_note(state: State<'_, AppState>) -> Result<String, String> {
    s(commands::daily_note(&state))
}

#[tauri::command]
fn refresh(state: State<'_, AppState>) -> Result<(), String> {
    s(commands::refresh(&state))
}

/// Start the desktop application and block until the window closes.
///
/// # Errors
///
/// Returns a [`tauri::Error`] if the runtime fails to build or run.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<(), tauri::Error> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_config_dir()?;
            let state = AppState::new(dir.join("settings.json"));
            commands::open_last_vault(&state);
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            open_vault,
            create_vault,
            last_vault,
            vault_info,
            tree,
            read_page,
            save_page,
            create_page,
            create_page_at,
            resolve,
            search,
            backlinks,
            outgoing,
            tags,
            daily_note,
            refresh
        ])
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

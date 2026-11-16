//! Desktop shell: a thin Tauri host. All business logic lives in the
//! workspace crates; this crate only exposes it to the web UI.

pub mod commands;
pub mod state;

use selenite_index::TreeNode;
use tauri::{Manager, State};

use commands::{
    AgendaDto, AppInfo, BacklinkDto, CmdError, OutLinkDto, PageContent, ProjectDto, QuickAddResult,
    SaveResult, SearchHitDto, TagDto, TaskDetail, TaskDto, VaultInfo, ViewDto,
};
use state::{AppState, RemindersStatus};

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

#[tauri::command]
fn set_mode(state: State<'_, AppState>, mode: String) -> Result<VaultInfo, String> {
    s(commands::set_mode(&state, &mode))
}

#[tauri::command]
fn tasks_query(state: State<'_, AppState>, expr: String) -> Result<Vec<TaskDto>, String> {
    s(commands::tasks_query(&state, &expr))
}

#[tauri::command]
fn task_quick_add(state: State<'_, AppState>, text: String) -> Result<QuickAddResult, String> {
    s(commands::task_quick_add(&state, &text))
}

#[tauri::command]
fn task_complete(state: State<'_, AppState>, path: String) -> Result<TaskDto, String> {
    s(commands::task_complete(&state, &path))
}

#[tauri::command]
fn task_reopen(state: State<'_, AppState>, path: String) -> Result<TaskDto, String> {
    s(commands::task_reopen(&state, &path))
}

#[tauri::command]
fn task_archive(state: State<'_, AppState>, path: String) -> Result<(), String> {
    s(commands::task_archive(&state, &path))
}

#[tauri::command]
fn task_update(
    state: State<'_, AppState>,
    path: String,
    patch: serde_json::Value,
) -> Result<TaskDto, String> {
    s(commands::task_update(&state, &path, &patch))
}

#[tauri::command]
fn task_get(state: State<'_, AppState>, path: String) -> Result<TaskDetail, String> {
    s(commands::task_get(&state, &path))
}

#[tauri::command]
fn task_projects(state: State<'_, AppState>) -> Result<Vec<ProjectDto>, String> {
    s(commands::task_projects(&state))
}

#[tauri::command]
fn task_labels(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    s(commands::task_labels(&state))
}

#[tauri::command]
fn task_agenda(state: State<'_, AppState>, days: u32) -> Result<AgendaDto, String> {
    s(commands::task_agenda(&state, days))
}

#[tauri::command]
fn task_views(state: State<'_, AppState>) -> Result<Vec<ViewDto>, String> {
    s(commands::task_views(&state))
}

#[tauri::command]
fn task_view_save(
    state: State<'_, AppState>,
    name: String,
    filter: String,
    layout: String,
) -> Result<Vec<ViewDto>, String> {
    s(commands::task_view_save(&state, &name, &filter, &layout))
}

#[tauri::command]
fn reminders_status(state: State<'_, AppState>) -> RemindersStatus {
    commands::reminders_status(&state)
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
            let notifier = selenite_reminders::default_notifier(&app.config().identifier);
            let state = AppState::with_notifier(dir.join("settings.json"), notifier);
            // Opening the remembered vault re-plans reminders (the startup re-plan).
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
            refresh,
            set_mode,
            tasks_query,
            task_quick_add,
            task_complete,
            task_reopen,
            task_archive,
            task_update,
            task_get,
            task_projects,
            task_labels,
            task_agenda,
            task_views,
            task_view_save,
            reminders_status
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

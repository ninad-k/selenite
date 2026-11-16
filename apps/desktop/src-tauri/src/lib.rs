//! Desktop shell: a thin Tauri host. All business logic lives in the
//! workspace crates; this crate only exposes it to the web UI.

pub mod commands;
pub mod state;

use selenite_index::{BlockPage, Citation, DatabaseInfo, TreeNode, ViewResult};
use selenite_plugins::PluginInfo;
use tauri::{Emitter, Manager, State};

use commands::{
    AgendaDto, AppInfo, AutoSync, BacklinkDto, BlockTypeDto, CmdError, OutLinkDto, PageContent,
    PageNode, PluginSource, ProjectDto, QuickAddResult, RemoteDto, SaveResult, SearchHitDto,
    SyncStatusDto, TagDto, TaskDetail, TaskDto, VaultInfo, ViewDto,
};
use state::{AppState, RemindersStatus, SyncSummary};

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
fn ask(
    state: State<'_, AppState>,
    question: String,
    limit: usize,
) -> Result<Vec<Citation>, String> {
    s(commands::ask(&state, &question, limit))
}

#[tauri::command]
fn plugin_list(state: State<'_, AppState>) -> Result<Vec<PluginInfo>, String> {
    s(commands::plugin_list(&state))
}

#[tauri::command]
fn plugin_set_enabled(state: State<'_, AppState>, id: String, enabled: bool) -> Result<(), String> {
    s(commands::plugin_set_enabled(&state, &id, enabled))
}

#[tauri::command]
fn plugin_load(state: State<'_, AppState>, id: String) -> Result<PluginSource, String> {
    s(commands::plugin_load(&state, &id))
}

#[tauri::command]
fn plugin_call(
    state: State<'_, AppState>,
    plugin_id: String,
    method: String,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    s(commands::plugin_call(&state, &plugin_id, &method, &args))
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

#[tauri::command]
fn block_types() -> Vec<BlockTypeDto> {
    commands::block_types()
}

#[tauri::command]
fn asset_base() -> &'static str {
    commands::ASSET_BASE
}

#[tauri::command]
fn asset_url(
    state: State<'_, AppState>,
    page: String,
    target: String,
) -> Result<Option<String>, String> {
    s(commands::asset_url(&state, &page, &target))
}

#[tauri::command]
fn page_blocks(state: State<'_, AppState>, path: String) -> Result<BlockPage, String> {
    s(commands::page_blocks(&state, &path))
}

#[tauri::command]
fn edit_block(
    state: State<'_, AppState>,
    path: String,
    edit: serde_json::Value,
    revision: String,
) -> Result<SaveResult, String> {
    s(commands::edit_block(&state, &path, &edit, &revision))
}

#[tauri::command]
fn workspace_pages(state: State<'_, AppState>) -> Result<Vec<PageNode>, String> {
    s(commands::workspace_pages(&state))
}

#[tauri::command]
fn create_subpage(
    state: State<'_, AppState>,
    parent: Option<String>,
    title: String,
) -> Result<String, String> {
    s(commands::create_subpage(&state, parent.as_deref(), &title))
}

#[tauri::command]
fn set_page_meta(
    state: State<'_, AppState>,
    path: String,
    icon: Option<String>,
    cover: Option<String>,
) -> Result<SaveResult, String> {
    s(commands::set_page_meta(
        &state,
        &path,
        icon.as_deref(),
        cover.as_deref(),
    ))
}

#[tauri::command]
fn databases(state: State<'_, AppState>) -> Result<Vec<DatabaseInfo>, String> {
    s(commands::databases(&state))
}

#[tauri::command]
fn database_schema(
    state: State<'_, AppState>,
    folder: String,
) -> Result<serde_json::Value, String> {
    s(commands::database_schema(&state, &folder))
}

#[tauri::command]
fn database_view(
    state: State<'_, AppState>,
    folder: String,
    view: Option<String>,
    query: Option<serde_json::Value>,
) -> Result<ViewResult, String> {
    s(commands::database_view(
        &state,
        &folder,
        view.as_deref(),
        query.as_ref(),
    ))
}

#[tauri::command]
fn database_create(
    state: State<'_, AppState>,
    folder: String,
    properties: Vec<serde_json::Value>,
) -> Result<(), String> {
    s(commands::database_create(&state, &folder, &properties))
}

#[tauri::command]
fn database_schema_save(
    state: State<'_, AppState>,
    folder: String,
    schema: serde_json::Value,
) -> Result<(), String> {
    s(commands::database_schema_save(&state, &folder, &schema))
}

#[tauri::command]
fn row_create(
    state: State<'_, AppState>,
    folder: String,
    title: String,
    values: std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<String, String> {
    s(commands::row_create(&state, &folder, &title, &values))
}

#[tauri::command]
fn row_set(
    state: State<'_, AppState>,
    path: String,
    property: String,
    value: serde_json::Value,
) -> Result<(), String> {
    s(commands::row_set(&state, &path, &property, &value))
}

/// Tell the UI that sync state (and possibly vault files) changed.
fn notify_sync(app: &tauri::AppHandle) {
    let _ = app.emit("sync-changed", ());
}

#[tauri::command]
fn remotes_list(state: State<'_, AppState>) -> Result<Vec<RemoteDto>, String> {
    s(commands::remotes_list(&state))
}

/// Runs the browser sign-in on a worker thread so the UI stays responsive.
#[tauri::command]
async fn remote_connect(
    app: tauri::AppHandle,
    provider: String,
    folder: Option<String>,
) -> Result<RemoteDto, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        commands::remote_connect(&state, &provider, folder.as_deref()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn remote_disconnect(state: State<'_, AppState>, id: String) -> Result<(), String> {
    s(commands::remote_disconnect(&state, &id))
}

/// Syncs one remote (or all) on a worker thread; the engine lock is only taken for the refresh.
#[tauri::command]
async fn sync_now(app: tauri::AppHandle, id: Option<String>) -> Result<SyncSummary, String> {
    let handle = app.clone();
    let out = tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        commands::sync_now(&state, id.as_deref()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;
    notify_sync(&app);
    out
}

#[tauri::command]
fn sync_status(state: State<'_, AppState>) -> SyncStatusDto {
    commands::sync_status(&state)
}

/// Automatic sync trigger (`open` or `focus`); returns whether a run happened.
#[tauri::command]
async fn sync_auto(app: tauri::AppHandle, reason: String) -> Result<bool, String> {
    let handle = app.clone();
    let ran = tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        commands::sync_auto(&state, AutoSync::parse(&reason))
    })
    .await
    .map_err(|e| e.to_string())?;
    if ran {
        notify_sync(&app);
    }
    Ok(ran)
}

/// Background timer: sync every five minutes while the app runs.
fn start_sync_timer(app: &tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(
                commands::AUTO_SYNC_INTERVAL_SECS,
            ));
            let ran = {
                let state = handle.state::<AppState>();
                commands::sync_auto(&state, AutoSync::Tick)
            };
            if ran {
                notify_sync(&handle);
            }
        }
    });
}

fn asset_response(app: &tauri::AppHandle, request_path: &str) -> tauri::http::Response<Vec<u8>> {
    let builder = tauri::http::Response::builder().header("Access-Control-Allow-Origin", "*");
    let state = app.state::<AppState>();
    let result = match commands::read_asset(&state, request_path) {
        Ok((bytes, ct)) => builder
            .status(200)
            .header("Content-Type", ct)
            .header("X-Content-Type-Options", "nosniff")
            .body(bytes),
        Err(commands::AssetError(code)) => builder.status(code).body(Vec::new()),
    };
    result.unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
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
        .register_uri_scheme_protocol("vault", |ctx, request| {
            asset_response(ctx.app_handle(), request.uri().path())
        })
        .setup(|app| {
            let dir = app.path().app_config_dir()?;
            let notifier = selenite_reminders::default_notifier(&app.config().identifier);
            let state = AppState::with_notifier(dir.join("settings.json"), notifier);
            // Opening the remembered vault re-plans reminders (the startup re-plan).
            commands::open_last_vault(&state);
            app.manage(state);
            start_sync_timer(app.handle());
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
            ask,
            plugin_list,
            plugin_set_enabled,
            plugin_load,
            plugin_call,
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
            reminders_status,
            block_types,
            asset_base,
            asset_url,
            page_blocks,
            edit_block,
            workspace_pages,
            create_subpage,
            set_page_meta,
            databases,
            database_schema,
            database_view,
            database_create,
            database_schema_save,
            row_create,
            row_set,
            remotes_list,
            remote_connect,
            remote_disconnect,
            sync_now,
            sync_status,
            sync_auto
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

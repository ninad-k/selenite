//! Tauri-free command layer. Every function takes `&AppState` so it can be unit-tested; the
//! `#[tauri::command]` wrappers in `lib.rs` only map errors to strings.
//!
//! Errors never carry page contents.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use selenite_core::database::{PropertyDef, ViewQuery};
use selenite_core::{Due, Mode, PathError, Priority, ReminderSpec, TaskFields, VaultPath};
use selenite_index::{
    Agenda, BlockEdit, BlockPage, Citation, DatabaseInfo, Engine, EngineError, TaskItem, TreeNode,
    ViewResult,
};
use selenite_plugins::{Permission, PluginError, PluginInfo};
use selenite_reminders::PlannerInput;
use selenite_storage::{Revision, StorageError};
use selenite_vault::{DatabaseSchema, Document, PropertyValue, SavedView};
use serde::{Deserialize, Serialize};

use std::sync::atomic::{AtomicBool, Ordering};

use selenite_storage::LocalFolder;
use selenite_storage::oauth::{OAuthConfig, TokenStore, default_open_browser};
use selenite_sync::remote::SIGN_IN_TIMEOUT;
use selenite_sync::{
    Provider, RemoteConfig, RemoteError, RemotesFile, ReportSummary, client_config, keychain_store,
    now_secs, pending, sign_in,
};

use crate::state::{AppState, RemindersStatus, SyncSummary};

/// Errors surfaced to the UI as plain strings.
#[derive(Debug, thiserror::Error)]
pub enum CmdError {
    /// No vault is open.
    #[error("no vault is open")]
    NoVault,
    /// The page changed on disk since it was read.
    #[error("CONFLICT: the page changed on disk")]
    Conflict,
    /// A path from the UI was invalid.
    #[error("invalid path: {0}")]
    Path(#[from] PathError),
    /// The engine failed.
    #[error("{0}")]
    Engine(EngineError),
    /// The settings file could not be read or written.
    #[error("settings error: {0}")]
    Settings(String),
    /// A task request was invalid.
    #[error("{0}")]
    Task(String),
    /// A cloud remote or sync request failed.
    #[error("{0}")]
    Remote(String),
    /// A plugin request failed.
    #[error("{0}")]
    Plugin(String),
}

impl From<EngineError> for CmdError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Storage(StorageError::Conflict { .. }) => Self::Conflict,
            other => Self::Engine(other),
        }
    }
}

/// Application identity shown by the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    /// Display name of the application.
    pub name: String,
    /// Version of the desktop shell.
    pub version: String,
}

/// Summary of the open vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultInfo {
    /// Absolute root folder.
    pub root: String,
    /// Display name of the vault (folder name).
    pub name: String,
    /// Stable mode id.
    pub mode_id: String,
    /// Display label of the mode.
    pub mode_label: String,
    /// Number of indexed pages.
    pub page_count: u64,
    /// All modes in display order with their configured labels.
    pub modes: Vec<ModeDto>,
}

/// A mode id and its display label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModeDto {
    /// Stable id (`vault`, `workspace`, `tasks`).
    pub id: String,
    /// Configured display label.
    pub label: String,
}

/// A page's text and revision token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PageContent {
    /// Full file text.
    pub text: String,
    /// Opaque revision to pass back on save.
    pub revision: String,
}

/// Result of saving a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaveResult {
    /// New revision token.
    pub revision: String,
}

/// A search result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHitDto {
    /// Page path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Snippet; matches wrapped in U+E000 / U+E001.
    pub snippet: String,
}

/// A page linking to the current page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BacklinkDto {
    /// Source page path.
    pub source: String,
    /// Source page title.
    pub source_title: String,
    /// 1-based line of the link.
    pub line: u32,
    /// The line's text.
    pub context: String,
}

/// A link leaving the current page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutLinkDto {
    /// Link target as written.
    pub target: String,
    /// Resolved page path, if any.
    pub resolved: Option<String>,
    /// 1-based line of the link.
    pub line: u32,
}

/// A tag and its page count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagDto {
    /// Tag text.
    pub tag: String,
    /// Number of pages using it.
    pub count: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Settings {
    last_vault: Option<String>,
}

fn read_settings(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_settings(path: &Path, settings: &Settings) -> Result<(), CmdError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CmdError::Settings(e.to_string()))?;
    }
    let text =
        serde_json::to_string_pretty(settings).map_err(|e| CmdError::Settings(e.to_string()))?;
    std::fs::write(path, text).map_err(|e| CmdError::Settings(e.to_string()))
}

fn vp(raw: &str) -> Result<VaultPath, CmdError> {
    Ok(VaultPath::new(raw)?)
}

/// Application name and version.
#[must_use]
pub fn app_info() -> AppInfo {
    AppInfo {
        name: selenite_core::APP_NAME.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

fn install(state: &AppState, engine: Engine, root: &str) {
    *state.lock() = Some(engine);
    let settings = Settings {
        last_vault: Some(root.to_owned()),
    };
    if let Err(err) = write_settings(&state.settings_path, &settings) {
        tracing::warn!(%err, "could not remember vault");
    }
    replan_reminders(state);
}

/// Open an existing folder as the current vault and remember it.
///
/// # Errors
///
/// Fails if the folder cannot be opened or indexed.
pub fn open_vault(state: &AppState, path: &str) -> Result<VaultInfo, CmdError> {
    let engine = Engine::open_local(Path::new(path))?;
    install(state, engine, path);
    vault_info(state)
}

/// Create a new vault in a folder and open it.
///
/// # Errors
///
/// Fails if the folder is already a vault or cannot be written.
pub fn create_vault(state: &AppState, path: &str) -> Result<VaultInfo, CmdError> {
    let engine = Engine::init_local(Path::new(path))?;
    install(state, engine, path);
    vault_info(state)
}

/// The remembered vault path, if any.
#[must_use]
pub fn last_vault(state: &AppState) -> Option<String> {
    read_settings(&state.settings_path).last_vault
}

/// Open the remembered vault if there is one; returns whether it opened.
pub fn open_last_vault(state: &AppState) -> bool {
    match last_vault(state) {
        Some(p) => match open_vault(state, &p) {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!(%err, "could not reopen last vault");
                false
            }
        },
        None => false,
    }
}

/// Describe the open vault.
///
/// # Errors
///
/// Fails when no vault is open.
pub fn vault_info(state: &AppState) -> Result<VaultInfo, CmdError> {
    state.with_engine(|e| {
        let root: PathBuf = e.root().map(Path::to_path_buf).unwrap_or_default();
        let name = root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mode = e.config().mode;
        Ok(VaultInfo {
            root: root.display().to_string(),
            name,
            mode_id: mode.id().to_owned(),
            mode_label: e.config().mode_names.label(mode).to_owned(),
            page_count: e.index().page_count().map_err(EngineError::from)?,
            modes: Mode::ALL
                .iter()
                .map(|m| ModeDto {
                    id: m.id().to_owned(),
                    label: e.config().mode_names.label(*m).to_owned(),
                })
                .collect(),
        })
    })
}

/// The visible file tree.
///
/// # Errors
///
/// Fails when no vault is open or storage fails.
pub fn tree(state: &AppState) -> Result<TreeNode, CmdError> {
    state.with_engine(|e| Ok(e.tree()?))
}

/// Read a page.
///
/// # Errors
///
/// Fails when the path is invalid or unreadable.
pub fn read_page(state: &AppState, path: &str) -> Result<PageContent, CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| {
        let (text, rev) = e.read_page(&p)?;
        Ok(PageContent {
            text,
            revision: rev.0,
        })
    })
}

/// Save a page, guarded by the revision the caller last saw.
///
/// # Errors
///
/// A stale revision yields [`CmdError::Conflict`] (message starts with `CONFLICT:`).
pub fn save_page(
    state: &AppState,
    path: &str,
    text: &str,
    revision: &str,
) -> Result<SaveResult, CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| {
        let entry = e.save_page(&p, text, Some(Revision(revision.to_owned())))?;
        Ok(SaveResult {
            revision: entry.revision.0,
        })
    })
}

/// Create a page from a title in an optional folder; returns its path.
///
/// # Errors
///
/// Fails if the title is empty or the page exists.
pub fn create_page(
    state: &AppState,
    title: &str,
    folder: Option<&str>,
) -> Result<String, CmdError> {
    let title = title.trim();
    let folder = folder.map(str::trim).filter(|f| !f.is_empty());
    state.with_engine(|e| {
        let body = format!("# {title}\n\n");
        Ok(e.create_titled_page(title, folder, &body)?.to_string())
    })
}

/// Create the page an unresolved wikilink points to: `<target>.md` with a title heading.
///
/// # Errors
///
/// Fails if the target is not a valid path or the page exists.
pub fn create_page_at(state: &AppState, path: &str) -> Result<String, CmdError> {
    let raw = path.trim();
    let with_ext = if raw.to_lowercase().ends_with(".md") {
        raw.to_owned()
    } else {
        format!("{raw}.md")
    };
    let p = vp(&with_ext)?;
    let title = p.stem().to_owned();
    state.with_engine(|e| {
        e.create_page(&p, &format!("# {title}\n"))?;
        Ok(p.to_string())
    })
}

/// Resolve a wikilink target to a page path.
///
/// # Errors
///
/// Fails when no vault is open or the index fails.
pub fn resolve(state: &AppState, target: &str) -> Result<Option<String>, CmdError> {
    state.with_engine(|e| Ok(e.resolve(target)?.map(|p| p.to_string())))
}

/// Full-text search.
///
/// # Errors
///
/// Fails when no vault is open or the query fails.
pub fn search(state: &AppState, query: &str, limit: usize) -> Result<Vec<SearchHitDto>, CmdError> {
    state.with_engine(|e| {
        Ok(e.search(query, limit)?
            .into_iter()
            .map(|h| SearchHitDto {
                path: h.path.to_string(),
                title: h.title,
                snippet: h.snippet,
            })
            .collect())
    })
}

/// Passages that answer `question`, with citations. Retrieval only: no model is called.
///
/// # Errors
///
/// Fails when no vault is open or the index fails.
pub fn ask(state: &AppState, question: &str, limit: usize) -> Result<Vec<Citation>, CmdError> {
    state.with_engine(|e| Ok(e.ask(question, limit.clamp(1, 25))?))
}

/// What the UI needs to start a plugin in its sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginSource {
    /// Plugin id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The plugin script (only returned for an enabled, unchanged plugin).
    pub source: String,
    /// Granted permissions.
    pub permissions: Vec<Permission>,
}

impl From<PluginError> for CmdError {
    fn from(e: PluginError) -> Self {
        Self::Plugin(e.to_string())
    }
}

/// Plugins found in the open vault and whether each is enabled.
///
/// # Errors
///
/// Fails when no vault is open or storage cannot be listed.
pub fn plugin_list(state: &AppState) -> Result<Vec<PluginInfo>, CmdError> {
    state.with_engine(|e| Ok(selenite_plugins::discover(e)?))
}

/// Enable or disable a plugin. Only the UI's own confirm flow calls this; there is no plugin
/// method that reaches it.
///
/// # Errors
///
/// Fails when no vault is open, or the plugin is missing or invalid.
pub fn plugin_set_enabled(state: &AppState, id: &str, enabled: bool) -> Result<(), CmdError> {
    state.with_engine(|e| Ok(selenite_plugins::set_enabled(e, id, enabled)?))
}

/// The script of an enabled plugin, for the sandboxed iframe.
///
/// # Errors
///
/// Fails when the plugin is not enabled, changed since it was enabled, or is invalid.
pub fn plugin_load(state: &AppState, id: &str) -> Result<PluginSource, CmdError> {
    state.with_engine(|e| {
        let p = selenite_plugins::load_enabled(e, id)?;
        Ok(PluginSource {
            id: p.manifest.id,
            name: p.manifest.name,
            source: p.source,
            permissions: p.manifest.permissions,
        })
    })
}

/// Serve one call from a sandboxed plugin. `plugin_id` is chosen by the UI from the iframe the
/// message came from; permissions, rate limits and auditing are enforced here.
///
/// # Errors
///
/// Fails when no vault is open or the host refuses or fails the call.
pub fn plugin_call(
    state: &AppState,
    plugin_id: &str,
    method: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, CmdError> {
    state.with_engine(|e| {
        let mut host = state
            .plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(host.call(e, plugin_id, method, args)?)
    })
}

/// Pages linking to `path`.
///
/// # Errors
///
/// Fails when the path is invalid or the index fails.
pub fn backlinks(state: &AppState, path: &str) -> Result<Vec<BacklinkDto>, CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| {
        Ok(e.backlinks(&p)?
            .into_iter()
            .map(|b| BacklinkDto {
                source: b.source.to_string(),
                source_title: b.source_title,
                line: b.line,
                context: b.context,
            })
            .collect())
    })
}

/// Links leaving `path`.
///
/// # Errors
///
/// Fails when the path is invalid or the index fails.
pub fn outgoing(state: &AppState, path: &str) -> Result<Vec<OutLinkDto>, CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| {
        Ok(e.outgoing(&p)?
            .into_iter()
            .map(|o| OutLinkDto {
                target: o.target,
                resolved: o.resolved.map(|r| r.to_string()),
                line: o.line,
            })
            .collect())
    })
}

/// All tags with counts.
///
/// # Errors
///
/// Fails when no vault is open or the index fails.
pub fn tags(state: &AppState) -> Result<Vec<TagDto>, CmdError> {
    state.with_engine(|e| {
        Ok(e.tags()?
            .into_iter()
            .map(|t| TagDto {
                tag: t.tag,
                count: t.count,
            })
            .collect())
    })
}

/// Today's daily note path (created if missing).
///
/// # Errors
///
/// Fails when the note cannot be created.
pub fn daily_note(state: &AppState) -> Result<String, CmdError> {
    daily_note_for(state, chrono::Local::now().date_naive())
}

/// Daily note for a given date (created if missing).
///
/// # Errors
///
/// Fails when the note cannot be created.
pub fn daily_note_for(state: &AppState, date: chrono::NaiveDate) -> Result<String, CmdError> {
    state.with_engine(|e| Ok(e.daily_note(date)?.to_string()))
}

/// Re-scan files changed outside the app.
///
/// # Errors
///
/// Fails when no vault is open or indexing fails.
pub fn refresh(state: &AppState) -> Result<(), CmdError> {
    state.with_engine(|e| {
        e.refresh()?;
        Ok(())
    })?;
    replan_reminders(state);
    Ok(())
}

/// Switch the interface mode. Only the mode setting changes; no data is touched.
///
/// # Errors
///
/// Fails on an unknown mode id, when no vault is open, or when the config cannot be written.
pub fn set_mode(state: &AppState, mode: &str) -> Result<VaultInfo, CmdError> {
    let mode = Mode::ALL
        .iter()
        .copied()
        .find(|m| m.id() == mode)
        .ok_or_else(|| CmdError::Task(format!("unknown mode `{mode}`")))?;
    state.with_engine(|e| {
        let mut config = e.config().clone();
        config.mode = mode;
        e.set_config(config)?;
        Ok(())
    })?;
    vault_info(state)
}

// ---------------------------------------------------------------- tasks

/// A task as the UI sees it (flattened, with defaults filled in).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskDto {
    /// Vault path of the task file.
    pub path: String,
    /// Title.
    pub title: String,
    /// Completed.
    pub done: bool,
    /// Priority 1 (highest) to 4 (none).
    pub priority: u8,
    /// `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM`.
    pub due: Option<String>,
    /// Whether the task repeats.
    pub recurring: bool,
    /// Recurrence rule text, if any.
    pub recurrence: Option<String>,
    /// Reminders in their text form (`-15m`, `2026-11-20T08:45`).
    pub reminders: Vec<String>,
    /// Project path such as `Home/Garden`.
    pub project: Option<String>,
    /// Section within the project.
    pub section: Option<String>,
    /// Labels.
    pub labels: Vec<String>,
    /// When it was completed.
    pub completed_at: Option<String>,
}

fn task_dto(path: &str, f: &TaskFields) -> TaskDto {
    TaskDto {
        path: path.to_owned(),
        title: f.title.clone(),
        done: f.done,
        priority: f.priority.as_u8(),
        due: f.due.map(|d| d.to_string()),
        recurring: f.recurrence.is_some(),
        recurrence: f.recurrence.as_ref().map(ToString::to_string),
        reminders: f.reminders.iter().map(ToString::to_string).collect(),
        project: f.project.clone(),
        section: f.section.clone(),
        labels: f.labels.clone(),
        completed_at: f.completed_at.map(|t| t.to_string()),
    }
}

fn item_dto(t: &TaskItem) -> TaskDto {
    task_dto(t.path.as_str(), &t.fields)
}

/// A task and its Markdown body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskDetail {
    /// The task fields.
    pub fields: TaskDto,
    /// Body text below the front matter.
    pub body: String,
}

/// Result of quick-adding a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuickAddResult {
    /// Path of the new task file.
    pub path: String,
    /// Parser warnings to show next to the quick-add bar.
    pub warnings: Vec<String>,
}

/// One day of the agenda.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgendaDayDto {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Open tasks on that day.
    pub tasks: Vec<TaskDto>,
}

/// Overdue tasks plus the next days.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgendaDto {
    /// Open tasks due before the first day.
    pub overdue: Vec<TaskDto>,
    /// One entry per day.
    pub days: Vec<AgendaDayDto>,
}

/// A project and its open-task count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectDto {
    /// Project path.
    pub project: String,
    /// Open tasks directly in it.
    pub open: usize,
}

/// A saved task view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewDto {
    /// Display name.
    pub name: String,
    /// Filter expression.
    pub filter: String,
    /// `list` or `board`.
    pub layout: String,
}

fn local_now() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

fn local_today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// Map a built-in view name to its filter expression; anything else is a raw expression.
fn view_expr(expr: &str) -> String {
    match expr.trim().to_lowercase().as_str() {
        "inbox" => "inbox & open".to_owned(),
        "today" => "today & open".to_owned(),
        "upcoming" => "next 7 days & open".to_owned(),
        "completed" => "done".to_owned(),
        _ => expr.to_owned(),
    }
}

/// List tasks for a built-in view name (`inbox`, `today`, `upcoming`, `completed`) or a raw
/// filter expression.
///
/// # Errors
///
/// Fails when the filter does not parse or no vault is open.
pub fn tasks_query(state: &AppState, expr: &str) -> Result<Vec<TaskDto>, CmdError> {
    let expr = view_expr(expr);
    let today = local_today();
    state.with_engine(|e| Ok(e.query_tasks(&expr, today)?.iter().map(item_dto).collect()))
}

/// Create a task from one line of natural language.
///
/// # Errors
///
/// Fails when the line has no title or the file cannot be written.
pub fn task_quick_add(state: &AppState, text: &str) -> Result<QuickAddResult, CmdError> {
    let res = state.with_engine(|e| {
        let (path, warnings) = e.quick_add(text, local_now())?;
        Ok(QuickAddResult {
            path: path.to_string(),
            warnings,
        })
    })?;
    replan_reminders(state);
    Ok(res)
}

/// Complete a task (recurring tasks advance to their next date and stay open).
///
/// # Errors
///
/// Fails when the path is not a task or the write conflicts.
pub fn task_complete(state: &AppState, path: &str) -> Result<TaskDto, CmdError> {
    let p = vp(path)?;
    let dto = state.with_engine(|e| {
        let f = e.complete_task(&p, local_now())?;
        Ok(task_dto(path, &f))
    })?;
    replan_reminders(state);
    Ok(dto)
}

/// Reopen a completed task.
///
/// # Errors
///
/// Fails when the path is not a task or the write conflicts.
pub fn task_reopen(state: &AppState, path: &str) -> Result<TaskDto, CmdError> {
    let p = vp(path)?;
    let dto = state.with_engine(|e| {
        let f = e.reopen_task(&p)?;
        Ok(task_dto(path, &f))
    })?;
    replan_reminders(state);
    Ok(dto)
}

/// Archive a task (moved to the vault trash).
///
/// # Errors
///
/// Fails when the path is not a task.
pub fn task_archive(state: &AppState, path: &str) -> Result<(), CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| {
        e.archive_task(&p)?;
        Ok(())
    })?;
    replan_reminders(state);
    Ok(())
}

/// A validated task edit. `Some(None)` clears an optional field.
#[derive(Debug, Default)]
struct TaskPatch {
    title: Option<String>,
    priority: Option<Priority>,
    due: Option<Option<Due>>,
    project: Option<Option<String>>,
    section: Option<Option<String>>,
    labels: Option<Vec<String>>,
    reminders: Option<Vec<ReminderSpec>>,
}

fn bad(msg: impl Into<String>) -> CmdError {
    CmdError::Task(msg.into())
}

fn opt_text(v: &serde_json::Value, key: &str) -> Result<Option<Option<String>>, CmdError> {
    match v.get(key) {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            Ok(Some((!t.is_empty()).then(|| t.to_owned())))
        }
        Some(_) => Err(bad(format!("`{key}` must be text or null"))),
    }
}

fn text_list(v: &serde_json::Value, key: &str) -> Result<Option<Vec<String>>, CmdError> {
    let Some(raw) = v.get(key) else {
        return Ok(None);
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| bad(format!("`{key}` must be a list of text")))?;
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        let s = x
            .as_str()
            .ok_or_else(|| bad(format!("`{key}` must be a list of text")))?
            .trim();
        if !s.is_empty() {
            out.push(s.to_owned());
        }
    }
    Ok(Some(out))
}

fn parse_patch(v: &serde_json::Value) -> Result<TaskPatch, CmdError> {
    if !v.is_object() {
        return Err(bad("the task patch must be an object"));
    }
    let mut p = TaskPatch::default();
    if let Some(t) = v.get("title") {
        let t = t
            .as_str()
            .ok_or_else(|| bad("`title` must be text"))?
            .trim();
        if t.is_empty() {
            return Err(bad("a task needs a title"));
        }
        p.title = Some(t.to_owned());
    }
    if let Some(n) = v.get("priority") {
        let n = n
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .and_then(Priority::from_u8)
            .ok_or_else(|| bad("`priority` must be 1 to 4"))?;
        p.priority = Some(n);
    }
    match v.get("due") {
        None => {}
        Some(serde_json::Value::Null) => p.due = Some(None),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => p.due = Some(None),
        Some(serde_json::Value::String(s)) => {
            p.due = Some(Some(
                s.parse::<Due>().map_err(|e| bad(format!("due: {e}")))?,
            ));
        }
        Some(_) => return Err(bad("`due` must be text or null")),
    }
    p.project = opt_text(v, "project")?;
    p.section = opt_text(v, "section")?;
    p.labels = text_list(v, "labels")?;
    if let Some(items) = text_list(v, "reminders")? {
        let mut specs = Vec::with_capacity(items.len());
        for s in &items {
            specs.push(
                s.parse::<ReminderSpec>()
                    .map_err(|e| bad(format!("reminder: {e}")))?,
            );
        }
        p.reminders = Some(specs);
    }
    Ok(p)
}

/// Edit task fields. `patch` is a JSON object with optional `title`, `priority` (1-4), `due`
/// (`YYYY-MM-DD`, `YYYY-MM-DDTHH:MM` or null), `project`, `section` (text or null), `labels`
/// and `reminders` (lists of text).
///
/// # Errors
///
/// Fails on an invalid patch (nothing is written), a non-task path or a write conflict.
pub fn task_update(
    state: &AppState,
    path: &str,
    patch: &serde_json::Value,
) -> Result<TaskDto, CmdError> {
    let p = vp(path)?;
    let patch = parse_patch(patch)?;
    let dto = state.with_engine(|e| {
        let f = e.update_task(&p, |f| {
            if let Some(v) = patch.title {
                f.title = v;
            }
            if let Some(v) = patch.priority {
                f.priority = v;
            }
            if let Some(v) = patch.due {
                f.due = v;
            }
            if let Some(v) = patch.project {
                f.project = v;
            }
            if let Some(v) = patch.section {
                f.section = v;
            }
            if let Some(v) = patch.labels {
                f.labels = v;
            }
            if let Some(v) = patch.reminders {
                f.reminders = v;
            }
        })?;
        Ok(task_dto(path, &f))
    })?;
    replan_reminders(state);
    Ok(dto)
}

/// A task's fields and body.
///
/// # Errors
///
/// Fails when the path is not a task.
pub fn task_get(state: &AppState, path: &str) -> Result<TaskDetail, CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| {
        let fields = e.task(&p)?;
        let (text, _) = e.read_page(&p)?;
        Ok(TaskDetail {
            fields: task_dto(path, &fields),
            body: Document::parse(&text).body().to_owned(),
        })
    })
}

/// Projects with open counts.
///
/// # Errors
///
/// Fails when no vault is open.
pub fn task_projects(state: &AppState) -> Result<Vec<ProjectDto>, CmdError> {
    state.with_engine(|e| {
        Ok(e.projects()?
            .into_iter()
            .map(|p| ProjectDto {
                project: p.project,
                open: p.open,
            })
            .collect())
    })
}

/// Labels in use.
///
/// # Errors
///
/// Fails when no vault is open.
pub fn task_labels(state: &AppState) -> Result<Vec<String>, CmdError> {
    state.with_engine(|e| Ok(e.labels()?))
}

fn agenda_dto(a: &Agenda) -> AgendaDto {
    AgendaDto {
        overdue: a.overdue.iter().map(item_dto).collect(),
        days: a
            .days
            .iter()
            .map(|d| AgendaDayDto {
                date: d.date.format("%Y-%m-%d").to_string(),
                tasks: d.tasks.iter().map(item_dto).collect(),
            })
            .collect(),
    }
}

/// Open tasks per day for `days` days from today, plus overdue tasks.
///
/// # Errors
///
/// Fails when no vault is open.
pub fn task_agenda(state: &AppState, days: u32) -> Result<AgendaDto, CmdError> {
    let today = local_today();
    state.with_engine(|e| Ok(agenda_dto(&e.agenda(today, days)?)))
}

/// Saved task views (empty until a Tasks database exists; reading never creates one).
///
/// # Errors
///
/// Fails when no vault is open or the schema cannot be read.
pub fn task_views(state: &AppState) -> Result<Vec<ViewDto>, CmdError> {
    state.with_engine(|e| {
        if e.task_databases().is_empty() {
            return Ok(Vec::new());
        }
        Ok(e.saved_views()?
            .into_iter()
            .map(|v| ViewDto {
                name: v.name,
                filter: v.filter,
                layout: v.layout,
            })
            .collect())
    })
}

/// Save (or replace by name) a task view and return the updated list.
///
/// # Errors
///
/// Fails when the name is empty, the filter does not parse or the schema cannot be written.
pub fn task_view_save(
    state: &AppState,
    name: &str,
    filter: &str,
    layout: &str,
) -> Result<Vec<ViewDto>, CmdError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(bad("a saved view needs a name"));
    }
    let layout = if layout == "board" { "board" } else { "list" };
    let today = local_today();
    state.with_engine(|e| {
        e.save_view(
            SavedView {
                name: name.to_owned(),
                filter: filter.trim().to_owned(),
                layout: layout.to_owned(),
            },
            today,
        )?;
        Ok(())
    })?;
    task_views(state)
}

// ---------------------------------------------------------------- reminders

/// Re-plan the seven-day reminder window from every task and register it with the notifier.
/// A notifier failure is recorded in the status and never returned: the user's command must
/// not fail because the OS refused a notification. Only counts are logged.
pub fn replan_reminders(state: &AppState) {
    replan_reminders_at(state, local_now());
}

/// [`replan_reminders`] at an explicit local time.
pub fn replan_reminders_at(state: &AppState, now: NaiveDateTime) {
    let tasks = {
        let guard = state.lock();
        let Some(engine) = guard.as_ref() else {
            return;
        };
        match engine.all_tasks() {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!("could not read tasks for reminders");
                state.lock_reminders().last_error = Some(err.to_string());
                return;
            }
        }
    };
    let keys: Vec<String> = tasks.iter().map(|t| t.path.to_string()).collect();
    let input = tasks.iter().zip(&keys).map(|(t, k)| PlannerInput {
        task_key: k,
        fields: &t.fields,
    });
    let result = selenite_reminders::replan(state.notifier.as_ref(), input, now);
    let mut status = state.lock_reminders();
    match result {
        Ok(planned) => {
            status.scheduled = planned.len();
            status.last_error = None;
        }
        Err(err) => {
            tracing::warn!("reminder registration failed");
            status.last_error = Some(err.to_string());
        }
    }
}

/// Current reminder registration status.
#[must_use]
pub fn reminders_status(state: &AppState) -> RemindersStatus {
    state.lock_reminders().clone()
}

// ---------------------------------------------------------------- workspace

/// Base URL the UI uses for vault assets (served by the `vault` URI scheme). Windows webviews
/// map custom schemes to `http://<scheme>.localhost/`.
pub const ASSET_BASE: &str = if cfg!(windows) {
    "http://vault.localhost/"
} else {
    "vault://localhost/"
};

/// Base URL of the `plugin` URI scheme that serves sandboxed plugin frames.
pub const PLUGIN_BASE: &str = if cfg!(windows) {
    "http://plugin.localhost/"
} else {
    "plugin://localhost/"
};

/// The script that defines the `selenite` global inside a plugin frame.
const PLUGIN_BOOTSTRAP: &str = include_str!("plugin_bootstrap.js");

/// Keep text from ending its `<script>` element or opening an HTML comment.
fn script_safe(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    let lower = js.to_ascii_lowercase();
    let b = js.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if lower[i..].starts_with("</script") {
            out.push_str(r"<\/");
            i += 2;
        } else if js[i..].starts_with("<!--") {
            out.push_str(r"<\!--");
            i += 4;
        } else {
            let ch = js[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Build the HTML document of an enabled, unchanged plugin for the `plugin` scheme.
/// `request_path` is `/<plugin-id>/`; anything else (or a plugin that is disabled, changed or
/// invalid) yields `None`, which the scheme handler turns into a 404.
pub fn plugin_frame(state: &AppState, request_path: &str) -> Option<String> {
    let id = request_path
        .strip_prefix('/')
        .and_then(|p| p.strip_suffix('/'))
        .filter(|id| selenite_plugins::is_valid_id(id))?;
    let loaded = state
        .with_engine(|e| Ok(selenite_plugins::load_enabled(e, id)?))
        .ok()?;
    Some(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head><body><script>{}</script><script>{}</script></body></html>",
        script_safe(PLUGIN_BOOTSTRAP),
        script_safe(&loaded.source)
    ))
}

/// One node of the Workspace page tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PageNode {
    /// Display title.
    pub name: String,
    /// Page path (`page`), database folder (`database`, `tasks`) or folder path (`folder`).
    pub path: String,
    /// `page`, `database`, `tasks` or `folder`.
    pub kind: String,
    /// Page icon from front matter.
    pub icon: Option<String>,
    /// Sub-pages and databases.
    pub children: Vec<PageNode>,
}

/// A block type offered by the slash menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockTypeDto {
    /// Template id.
    pub kind: String,
    /// Menu label.
    pub label: String,
    /// Short description.
    pub hint: String,
    /// Markdown skeleton.
    pub markdown: String,
}

const BLOCK_TYPES: &[(&str, &str, &str)] = &[
    ("text", "Text", "Plain paragraph"),
    ("h1", "Heading 1", "Large section title"),
    ("h2", "Heading 2", "Medium section title"),
    ("h3", "Heading 3", "Small section title"),
    ("bullet", "Bulleted list", "Unordered list"),
    ("numbered", "Numbered list", "Ordered list"),
    ("todo", "To-do", "Checkbox item"),
    ("toggle", "Toggle", "Collapsible section"),
    ("quote", "Quote", "Quoted text"),
    ("callout", "Callout", "Highlighted note"),
    ("code", "Code", "Code block"),
    ("divider", "Divider", "Horizontal rule"),
    ("image", "Image", "Image from a path or URL"),
    ("file", "File", "Link to a file"),
    ("embed", "Embed", "Embed a page or file"),
    ("table", "Table", "Simple table"),
];

/// Block types for the slash menu, with their Markdown skeletons.
#[must_use]
pub fn block_types() -> Vec<BlockTypeDto> {
    BLOCK_TYPES
        .iter()
        .map(|(kind, label, hint)| BlockTypeDto {
            kind: (*kind).to_owned(),
            label: (*label).to_owned(),
            hint: (*hint).to_owned(),
            markdown: selenite_vault::block_template(kind)
                .unwrap_or_default()
                .to_owned(),
        })
        .collect()
}

/// Split a page into rendered blocks.
///
/// # Errors
///
/// Fails when the path is invalid or the page cannot be read.
pub fn page_blocks(state: &AppState, path: &str) -> Result<BlockPage, CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| Ok(e.page_blocks(&p, ASSET_BASE)?))
}

/// Apply one block edit (`{"op": "replace"|"insert"|"delete"|"move"|"toggle_task", ...}`),
/// conditional on the revision the UI rendered.
///
/// # Errors
///
/// A stale revision yields [`CmdError::Conflict`]; a malformed edit is an error.
pub fn edit_block(
    state: &AppState,
    path: &str,
    edit: &serde_json::Value,
    revision: &str,
) -> Result<SaveResult, CmdError> {
    let p = vp(path)?;
    let edit: BlockEdit = serde_json::from_value(edit.clone())
        .map_err(|e| CmdError::Task(format!("bad edit: {e}")))?;
    state.with_engine(|e| {
        let rev = e.edit_block(&p, &edit, Revision(revision.to_owned()))?;
        Ok(SaveResult { revision: rev.0 })
    })
}

fn page_meta(e: &Engine, path: &str) -> (Option<String>, String) {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md")
        .to_owned();
    let Ok(p) = VaultPath::new(path) else {
        return (None, stem);
    };
    let Ok((text, _)) = e.read_page(&p) else {
        return (None, stem);
    };
    let doc = Document::parse(&text);
    let icon = doc
        .property("icon")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty());
    (icon, doc.title().unwrap_or(stem))
}

fn build_nodes(
    e: &Engine,
    dir: &TreeNode,
    dbs: &std::collections::HashMap<String, String>,
) -> Vec<PageNode> {
    let mut nodes = Vec::new();
    for file in dir
        .children
        .iter()
        .filter(|c| !c.is_dir && c.name.to_lowercase().ends_with(".md"))
    {
        let stem = &file.name[..file.name.len() - 3];
        let sub = dir
            .children
            .iter()
            .find(|d| d.is_dir && d.name == stem && !dbs.contains_key(&d.path));
        let (icon, title) = page_meta(e, &file.path);
        nodes.push(PageNode {
            name: title,
            path: file.path.clone(),
            kind: "page".into(),
            icon,
            children: sub.map(|d| build_nodes(e, d, dbs)).unwrap_or_default(),
        });
    }
    for d in dir.children.iter().filter(|c| c.is_dir) {
        if let Some(kind) = dbs.get(&d.path) {
            nodes.push(PageNode {
                name: d.name.clone(),
                path: d.path.clone(),
                kind: if kind == "tasks" { "tasks" } else { "database" }.into(),
                icon: None,
                children: Vec::new(),
            });
            continue;
        }
        let has_page = dir
            .children
            .iter()
            .any(|f| !f.is_dir && f.name == format!("{}.md", d.name));
        if has_page {
            continue;
        }
        let children = build_nodes(e, d, dbs);
        if !children.is_empty() {
            nodes.push(PageNode {
                name: d.name.clone(),
                path: d.path.clone(),
                kind: "folder".into(),
                icon: None,
                children,
            });
        }
    }
    nodes.sort_by_key(|n| n.name.to_lowercase());
    nodes
}

/// The nested page tree for the Workspace sidebar. A page `A.md` with a sibling folder `A/`
/// has the pages in `A/` as children; database folders appear as database (or task list)
/// nodes; hidden folders are excluded.
///
/// # Errors
///
/// Fails when no vault is open or storage fails.
pub fn workspace_pages(state: &AppState) -> Result<Vec<PageNode>, CmdError> {
    state.with_engine(|e| {
        let tree = e.tree()?;
        let dbs = e
            .databases()?
            .into_iter()
            .map(|d| (d.folder, d.kind))
            .collect();
        Ok(build_nodes(e, &tree, &dbs))
    })
}

/// Create a sub-page `Parent/Title.md` (folder `Parent/` next to `Parent.md`), or a top-level
/// page when `parent` is `None`. Returns the new page path.
///
/// # Errors
///
/// Fails if the title is empty, the page exists or the parent path is invalid.
pub fn create_subpage(
    state: &AppState,
    parent: Option<&str>,
    title: &str,
) -> Result<String, CmdError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(CmdError::Task("a page needs a title".into()));
    }
    let folder = match parent.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            let pp = vp(p)?;
            Some(if pp.is_markdown() {
                p.replace('\\', "/")[..p.len() - 3].to_owned()
            } else {
                pp.to_string()
            })
        }
        None => None,
    };
    state.with_engine(|e| {
        Ok(
            e.create_titled_page(title, folder.as_deref(), &format!("# {title}\n"))?
                .to_string(),
        )
    })
}

/// Set (`Some(text)`), clear (`Some("")`) or keep (`None`) the `icon` and `cover` front matter of a page. The body is
/// untouched and the write is conditional on the revision read inside this call.
///
/// # Errors
///
/// Fails when the page cannot be read, has invalid front matter, or changed meanwhile.
pub fn set_page_meta(
    state: &AppState,
    path: &str,
    icon: Option<&str>,
    cover: Option<&str>,
) -> Result<SaveResult, CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| {
        let (text, rev) = e.read_page(&p)?;
        let mut doc = Document::parse(&text);
        if doc.front_matter_error().is_some() {
            return Err(CmdError::Task(
                "the page has invalid front matter; fix it by hand first".into(),
            ));
        }
        for (key, value) in [("icon", icon), ("cover", cover)] {
            match value.map(str::trim) {
                None => {}
                Some("") => {
                    doc.remove_property(key);
                }
                Some(v) => doc.set_property(key, PropertyValue::Text(v.to_owned())),
            }
        }
        let entry = e.save_page(&p, &doc.to_text(), Some(rev))?;
        Ok(SaveResult {
            revision: entry.revision.0,
        })
    })
}

// ---------------------------------------------------------------- assets

/// Why an asset request failed (an HTTP status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetError(pub u16);

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Validate the path of an asset request (`/folder/pic.png`, percent-encoded) as a visible
/// vault path: no `..`, no absolute or drive paths, nothing hidden such as `.selenite`.
///
/// # Errors
///
/// Returns [`AssetError`] 400 for malformed or escaping paths and 403 for hidden ones.
pub fn asset_vault_path(request_path: &str) -> Result<VaultPath, AssetError> {
    let decoded = percent_decode(request_path.strip_prefix('/').unwrap_or(request_path))
        .ok_or(AssetError(400))?;
    if decoded.contains('\0') {
        return Err(AssetError(400));
    }
    let p = VaultPath::new(&decoded).map_err(|_| AssetError(400))?;
    if p.is_hidden() {
        return Err(AssetError(403));
    }
    Ok(p)
}

/// Content type for an asset by file extension.
#[must_use]
pub fn asset_content_type(path: &VaultPath) -> &'static str {
    match path.extension().as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("avif") => "image/avif",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Read an asset for the `vault` URI scheme. Only files inside the open vault root are served.
///
/// # Errors
///
/// 400/403 for bad paths, 404 when the file or vault is missing.
pub fn read_asset(
    state: &AppState,
    request_path: &str,
) -> Result<(Vec<u8>, &'static str), AssetError> {
    let p = asset_vault_path(request_path)?;
    let ct = asset_content_type(&p);
    state
        .with_engine(|e| {
            e.storage()
                .read(&p)
                .map_err(|err| CmdError::Engine(EngineError::from(err)))
        })
        .map(|bytes| (bytes, ct))
        .map_err(|_| AssetError(404))
}

/// Resolve an image reference (for example a gallery cover value) written relative to `page`
/// to an asset URL; `None` when the file does not exist.
///
/// # Errors
///
/// Fails when no vault is open or storage fails.
pub fn asset_url(state: &AppState, page: &str, target: &str) -> Result<Option<String>, CmdError> {
    let t = target.trim();
    if t.starts_with("https://") || t.starts_with("http://") {
        return Ok(Some(t.to_owned()));
    }
    if t.is_empty() {
        return Ok(None);
    }
    let dir = page.rsplit_once('/').map(|(d, _)| d);
    state.with_engine(|e| {
        let mut candidates = Vec::new();
        if let Some(d) = dir {
            candidates.push(format!("{d}/{t}"));
        }
        candidates.push(t.to_owned());
        for c in candidates {
            if let Ok(p) = VaultPath::new(&c) {
                if !p.is_hidden() && e.storage().stat(&p).map_err(EngineError::from)?.is_some() {
                    return Ok(Some(encode_asset(ASSET_BASE, p.as_str())));
                }
            }
        }
        let name = t.rsplit('/').next().unwrap_or(t).to_lowercase();
        let mut found: Vec<VaultPath> = e
            .storage()
            .list()
            .map_err(EngineError::from)?
            .into_iter()
            .map(|x| x.path)
            .filter(|p| !p.is_hidden() && p.file_name().to_lowercase() == name)
            .collect();
        found.sort_by_key(|p| p.as_str().len());
        Ok(found.first().map(|p| encode_asset(ASSET_BASE, p.as_str())))
    })
}

fn encode_asset(base: &str, p: &str) -> String {
    let mut out = String::from(base);
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------- databases

/// Every database folder (tables and task lists).
///
/// # Errors
///
/// Fails when no vault is open.
pub fn databases(state: &AppState) -> Result<Vec<DatabaseInfo>, CmdError> {
    state.with_engine(|e| Ok(e.databases()?))
}

/// A database's schema as JSON (`kind`, `properties`, `table_views`, ...).
///
/// # Errors
///
/// Fails when the folder is not a database.
pub fn database_schema(state: &AppState, folder: &str) -> Result<serde_json::Value, CmdError> {
    state.with_engine(|e| {
        let schema = e.database_schema(folder)?;
        serde_json::to_value(schema).map_err(|err| CmdError::Task(err.to_string()))
    })
}

/// Apply a saved view (by name, default first) or an ad-hoc `ViewQuery` JSON to a database.
///
/// # Errors
///
/// Fails on a bad query, unknown view or non-table database.
pub fn database_view(
    state: &AppState,
    folder: &str,
    view: Option<&str>,
    query: Option<&serde_json::Value>,
) -> Result<ViewResult, CmdError> {
    let adhoc: Option<ViewQuery> = match query {
        Some(q) if !q.is_null() => Some(
            serde_json::from_value(q.clone())
                .map_err(|e| CmdError::Task(format!("bad query: {e}")))?,
        ),
        _ => None,
    };
    state.with_engine(|e| Ok(e.query_database_view(folder, view, adhoc, local_now())?))
}

/// Create a table database in `folder` with a `Name` title property plus `properties`
/// (`PropertyDef` JSON objects such as `{"name": "Status", "type": "select", "options": []}`).
///
/// # Errors
///
/// Fails on malformed or invalid properties, or when the folder is already a database.
pub fn database_create(
    state: &AppState,
    folder: &str,
    properties: &[serde_json::Value],
) -> Result<(), CmdError> {
    let defs: Vec<PropertyDef> = properties
        .iter()
        .map(|p| {
            serde_json::from_value(p.clone())
                .map_err(|e| CmdError::Task(format!("bad property: {e}")))
        })
        .collect::<Result<_, _>>()?;
    state.with_engine(|e| Ok(e.create_database(folder, defs)?))
}

/// Replace a database schema (properties and views); validated before writing.
///
/// # Errors
///
/// Fails on a malformed or invalid schema.
pub fn database_schema_save(
    state: &AppState,
    folder: &str,
    schema: &serde_json::Value,
) -> Result<(), CmdError> {
    let schema: DatabaseSchema = serde_json::from_value(schema.clone())
        .map_err(|e| CmdError::Task(format!("bad schema: {e}")))?;
    state.with_engine(|e| Ok(e.save_database_schema(folder, &schema, None)?))
}

/// Add a row to a database; returns its path.
///
/// # Errors
///
/// Fails when the title is empty or the folder is not a table database.
pub fn row_create(
    state: &AppState,
    folder: &str,
    title: &str,
    values: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<String, CmdError> {
    state.with_engine(|e| {
        Ok(e.create_row(folder, title, values, local_now())?
            .to_string())
    })
}

/// Set (or clear with `null`) one property of a row.
///
/// # Errors
///
/// Fails for computed or unknown properties.
pub fn row_set(
    state: &AppState,
    path: &str,
    property: &str,
    value: &serde_json::Value,
) -> Result<(), CmdError> {
    let p = vp(path)?;
    state.with_engine(|e| Ok(e.set_row_property(&p, property, value)?))
}

// ---------------------------------------------------------------- cloud sync

fn remote_err(e: impl std::fmt::Display) -> CmdError {
    CmdError::Remote(e.to_string())
}

/// A configured remote as the UI shows it. Contains no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteDto {
    /// Remote id, e.g. `gdrive-1`.
    pub id: String,
    /// Provider slug (`google-drive`, `onedrive`).
    pub provider: String,
    /// Provider display name.
    pub provider_label: String,
    /// Remote folder name.
    pub folder: String,
    /// Optional account label.
    pub account_hint: Option<String>,
    /// Unix seconds of the last successful sync in this session.
    pub last_sync: Option<i64>,
}

/// Sync state for the status bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncStatusDto {
    /// A run is in flight.
    pub running: bool,
    /// Number of configured remotes.
    pub remotes: usize,
    /// Unix seconds when the last run finished.
    pub last_run: Option<i64>,
    /// Counts and paths of the last run.
    pub last_report: Option<SyncSummary>,
    /// Paths still waiting for an unreachable remote.
    pub pending: usize,
    /// Last whole-run failure.
    pub error: Option<String>,
}

/// What triggered an automatic sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSync {
    /// A vault was opened.
    Open,
    /// The five-minute timer fired.
    Tick,
    /// The window regained focus.
    Focus,
}

impl AutoSync {
    /// Parse the UI's reason string; unknown values count as a focus event (the most cautious).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "open" => Self::Open,
            "tick" => Self::Tick,
            _ => Self::Focus,
        }
    }

    /// Minimum age of the previous run before this trigger syncs again.
    const fn min_age_secs(self) -> i64 {
        match self {
            Self::Open | Self::Tick => 0,
            Self::Focus => 60,
        }
    }
}

/// Interval of the background sync timer.
pub const AUTO_SYNC_INTERVAL_SECS: u64 = 300;

fn vault_local(state: &AppState) -> Result<LocalFolder, CmdError> {
    let root = state.with_engine(|e| Ok(e.root().map(Path::to_path_buf)))?;
    let root = root.ok_or(CmdError::NoVault)?;
    LocalFolder::open(root).map_err(remote_err)
}

fn load_remotes(local: &LocalFolder) -> Result<RemotesFile, CmdError> {
    RemotesFile::load(local).map_err(remote_err)
}

fn remote_dto(state: &AppState, r: &RemoteConfig) -> RemoteDto {
    RemoteDto {
        id: r.id.clone(),
        provider: r.provider.slug().to_owned(),
        provider_label: r.provider.label().to_owned(),
        folder: r.folder.clone(),
        account_hint: r.account_hint.clone(),
        last_sync: state.lock_sync().per_remote.get(&r.id).copied(),
    }
}

/// The vault's configured remotes.
///
/// # Errors
///
/// Fails when no vault is open or `remotes.yaml` is unreadable.
pub fn remotes_list(state: &AppState) -> Result<Vec<RemoteDto>, CmdError> {
    let file = load_remotes(&vault_local(state)?)?;
    Ok(file.remotes.iter().map(|r| remote_dto(state, r)).collect())
}

/// Sign in to a provider in the browser and record the remote. Blocks until the sign-in ends
/// (up to five minutes); call it from a background thread.
///
/// # Errors
///
/// Fails when the build has no client id, the sign-in is abandoned, or the file cannot be saved.
pub fn remote_connect(
    state: &AppState,
    provider: &str,
    folder: Option<&str>,
) -> Result<RemoteDto, CmdError> {
    let provider = Provider::parse(provider)
        .ok_or_else(|| remote_err(format!("unknown provider {provider}")))?;
    remote_connect_with(
        state,
        provider,
        folder,
        client_config(provider),
        &|p, id| keychain_store(p, id),
        &default_open_browser,
    )
}

/// [`remote_connect`] with explicit sign-in plumbing (tests).
///
/// # Errors
///
/// Same as [`remote_connect`].
pub fn remote_connect_with(
    state: &AppState,
    provider: Provider,
    folder: Option<&str>,
    client: Result<OAuthConfig, RemoteError>,
    store_for: &dyn Fn(Provider, &str) -> std::sync::Arc<dyn TokenStore>,
    open_browser: &dyn Fn(&str) -> Result<(), String>,
) -> Result<RemoteDto, CmdError> {
    let local = vault_local(state)?;
    let mut file = load_remotes(&local)?;
    let id = file.next_id(provider);
    let folder = folder
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            local
                .root()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "vault".to_owned());
    let store = store_for(provider, &id);
    sign_in(
        provider,
        client,
        std::sync::Arc::clone(&store),
        open_browser,
        SIGN_IN_TIMEOUT,
    )
    .map_err(remote_err)?;
    let remote = RemoteConfig {
        id,
        provider,
        folder,
        account_hint: None,
    };
    file.remotes.push(remote.clone());
    if let Err(e) = file.save(&local) {
        let _ = store.clear();
        return Err(remote_err(e));
    }
    Ok(remote_dto(state, &remote))
}

/// Forget a remote and its stored sign-in. Files already in the cloud are never touched.
///
/// # Errors
///
/// Fails on an unknown id or when the file or keychain cannot be updated.
pub fn remote_disconnect(state: &AppState, id: &str) -> Result<(), CmdError> {
    let local = vault_local(state)?;
    let mut file = load_remotes(&local)?;
    let removed = file
        .remove(id)
        .ok_or_else(|| remote_err(format!("no remote named {id}")))?;
    file.save(&local).map_err(remote_err)?;
    state.lock_sync().per_remote.remove(id);
    state.remotes.forget(&removed).map_err(remote_err)
}

struct RunGuard<'a>(&'a AtomicBool);

impl<'a> RunGuard<'a> {
    fn acquire(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Sync one remote (`Some(id)`) or all of them (`None`) and refresh the index. Blocks on network
/// I/O without holding the engine lock; call it from a background thread.
///
/// # Errors
///
/// Fails when no vault is open, the id is unknown, or another sync is already running.
/// Per-remote failures are reported in the summary, not as errors.
pub fn sync_now(state: &AppState, id: Option<&str>) -> Result<SyncSummary, CmdError> {
    sync_now_at(state, id, now_secs())
}

/// [`sync_now`] at an explicit time.
///
/// # Errors
///
/// Same as [`sync_now`].
pub fn sync_now_at(state: &AppState, id: Option<&str>, now: i64) -> Result<SyncSummary, CmdError> {
    let local = vault_local(state)?;
    let file = load_remotes(&local)?;
    let remotes: Vec<RemoteConfig> = match id {
        Some(id) => vec![
            file.get(id)
                .cloned()
                .ok_or_else(|| remote_err(format!("no remote named {id}")))?,
        ],
        None => file.remotes.clone(),
    };
    let _guard = RunGuard::acquire(&state.sync_running)
        .ok_or_else(|| remote_err("a sync is already running"))?;
    let mut summary = SyncSummary::default();
    let mut synced = Vec::new();
    for r in &remotes {
        summary.remotes += 1;
        let run = state.remotes.open(r).and_then(|remote| {
            selenite_sync::sync(&local, remote.as_ref(), &r.id, now).map_err(|e| e.to_string())
        });
        match run {
            Ok(report) => {
                let s = ReportSummary::from(&report);
                summary.pushed += s.pushed.len();
                summary.pulled += s.pulled.len();
                summary.trashed += s.trashed.len();
                summary.conflicts.extend(s.conflicts);
                for p in s.unresolved {
                    if !summary.unresolved.contains(&p) {
                        summary.unresolved.push(p);
                    }
                }
                summary
                    .errors
                    .extend(s.errors.into_iter().map(|e| format!("{}: {e}", r.id)));
                summary.offline |= s.offline;
                if !s.offline {
                    synced.push(r.id.clone());
                }
            }
            Err(e) => summary.errors.push(format!("{}: {e}", r.id)),
        }
    }
    if !synced.is_empty() {
        // Pull the changes into the index, then re-plan reminders (ADR-005).
        if let Err(e) = refresh(state) {
            summary.errors.push(format!("index refresh: {e}"));
        }
    }
    tracing::info!(
        remotes = summary.remotes,
        pushed = summary.pushed,
        pulled = summary.pulled,
        errors = summary.errors.len(),
        "sync finished"
    );
    let mut book = state.lock_sync();
    book.last_run = Some(now);
    book.error = summary.errors.first().cloned();
    for rid in synced {
        book.per_remote.insert(rid, now);
    }
    book.last_report = Some(summary.clone());
    Ok(summary)
}

/// Sync automatically if there are remotes, nothing is running and the last run is old enough
/// for this trigger. Returns whether a run happened; failures are kept in [`sync_status`].
pub fn sync_auto(state: &AppState, reason: AutoSync) -> bool {
    sync_auto_at(state, reason, now_secs())
}

/// [`sync_auto`] at an explicit time.
pub fn sync_auto_at(state: &AppState, reason: AutoSync, now: i64) -> bool {
    let Ok(local) = vault_local(state) else {
        return false;
    };
    match RemotesFile::load(&local) {
        Ok(f) if !f.remotes.is_empty() => {}
        _ => return false,
    }
    if state.sync_running.load(Ordering::Acquire) {
        return false;
    }
    let recent = state
        .lock_sync()
        .last_run
        .is_some_and(|t| now - t < reason.min_age_secs());
    if recent {
        return false;
    }
    match sync_now_at(state, None, now) {
        Ok(_) => true,
        Err(err) => {
            // Includes losing a race with another run; only the message is kept.
            tracing::warn!(%err, "automatic sync did not run");
            let mut book = state.lock_sync();
            if !matches!(err, CmdError::Remote(ref m) if m.contains("already running")) {
                book.error = Some(err.to_string());
            }
            false
        }
    }
}

/// Current sync state. Works without a vault (everything empty).
#[must_use]
pub fn sync_status(state: &AppState) -> SyncStatusDto {
    let (remotes, pending_count) = match vault_local(state) {
        Ok(local) => (
            RemotesFile::load(&local).map_or(0, |f| f.remotes.len()),
            pending(&local).len(),
        ),
        Err(_) => (0, 0),
    };
    let book = state.lock_sync();
    SyncStatusDto {
        running: state.sync_running.load(Ordering::Acquire),
        remotes,
        last_run: book.last_run,
        last_report: book.last_report.clone(),
        pending: pending_count,
        error: book.error.clone(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn fixture() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().join("cfg").join("settings.json"));
        let vault = dir.path().join("vault");
        create_vault(&state, vault.to_str().unwrap()).unwrap();
        (dir, state)
    }

    fn names(nodes: &[PageNode]) -> Vec<String> {
        nodes.iter().map(|n| n.name.clone()).collect()
    }

    // ---- cloud sync

    use selenite_core::VaultPath as Vp;
    use selenite_storage::{Entry, MemoryStorage, StorageProvider, WriteMode};
    use std::sync::{Arc, Mutex};

    struct Shared(Arc<MemoryStorage>);

    impl StorageProvider for Shared {
        fn kind(&self) -> &'static str {
            "memory"
        }
        fn list(&self) -> selenite_storage::Result<Vec<Entry>> {
            self.0.list()
        }
        fn list_dirs(&self) -> selenite_storage::Result<Vec<Vp>> {
            self.0.list_dirs()
        }
        fn stat(&self, p: &Vp) -> selenite_storage::Result<Option<Entry>> {
            self.0.stat(p)
        }
        fn read(&self, p: &Vp) -> selenite_storage::Result<Vec<u8>> {
            self.0.read(p)
        }
        fn write(&self, p: &Vp, d: &[u8], m: WriteMode) -> selenite_storage::Result<Entry> {
            self.0.write(p, d, m)
        }
        fn rename(&self, a: &Vp, b: &Vp) -> selenite_storage::Result<Entry> {
            self.0.rename(a, b)
        }
        fn trash(&self, p: &Vp) -> selenite_storage::Result<Vp> {
            self.0.trash(p)
        }
        fn create_dir(&self, p: &Vp) -> selenite_storage::Result<()> {
            self.0.create_dir(p)
        }
    }

    struct FakeRemotes {
        mem: Arc<MemoryStorage>,
        forgotten: Arc<Mutex<Vec<String>>>,
    }

    impl crate::state::RemoteFactory for FakeRemotes {
        fn open(&self, _r: &RemoteConfig) -> Result<Box<dyn StorageProvider>, String> {
            Ok(Box::new(Shared(Arc::clone(&self.mem))))
        }
        fn forget(&self, r: &RemoteConfig) -> Result<(), String> {
            self.forgotten.lock().unwrap().push(r.id.clone());
            Ok(())
        }
    }

    type SyncFixture = (
        tempfile::TempDir,
        AppState,
        Arc<MemoryStorage>,
        Arc<Mutex<Vec<String>>>,
    );

    fn sync_fixture() -> SyncFixture {
        let dir = tempfile::tempdir().unwrap();
        let mem = Arc::new(MemoryStorage::new());
        let forgotten = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new(dir.path().join("cfg").join("settings.json")).with_remotes(
            Box::new(FakeRemotes {
                mem: Arc::clone(&mem),
                forgotten: Arc::clone(&forgotten),
            }),
        );
        let vault = dir.path().join("vault");
        create_vault(&state, vault.to_str().unwrap()).unwrap();
        (dir, state, mem, forgotten)
    }

    fn add_remote(state: &AppState) {
        let local = vault_local(state).unwrap();
        let mut f = RemotesFile::load(&local).unwrap();
        f.remotes.push(RemoteConfig {
            id: "gdrive-1".into(),
            provider: Provider::GoogleDrive,
            folder: "Notes".into(),
            account_hint: None,
        });
        f.save(&local).unwrap();
    }

    #[test]
    fn sync_status_default_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().join("settings.json"));
        let s = sync_status(&state);
        assert!(!s.running);
        assert_eq!(s.remotes, 0);
        assert!(s.last_run.is_none() && s.last_report.is_none() && s.error.is_none());
        assert_eq!(s.pending, 0);
        // With a vault but no remotes nothing runs, even automatically.
        let (_d, state, _m, _f) = sync_fixture();
        assert!(remotes_list(&state).unwrap().is_empty());
        assert!(!sync_auto(&state, AutoSync::Open));
        assert!(sync_now(&state, None).unwrap().remotes == 0);
    }

    #[test]
    fn remotes_file_handling_and_disconnect() {
        let (_d, state, _m, forgotten) = sync_fixture();
        add_remote(&state);
        let list = remotes_list(&state).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].provider, "google-drive");
        assert_eq!(list[0].provider_label, "Google Drive");
        assert!(list[0].last_sync.is_none());
        assert!(sync_now(&state, Some("nope")).is_err());
        remote_disconnect(&state, "gdrive-1").unwrap();
        assert!(remotes_list(&state).unwrap().is_empty());
        assert_eq!(forgotten.lock().unwrap().as_slice(), ["gdrive-1"]);
        assert!(remote_disconnect(&state, "gdrive-1").is_err());
    }

    #[test]
    fn connect_without_client_id_fails_before_opening_a_browser() {
        let (_d, state, _m, _f) = sync_fixture();
        let opened = std::cell::Cell::new(false);
        let open = |_: &str| {
            opened.set(true);
            Ok(())
        };
        let none = |_: &str| None;
        let err = remote_connect_with(
            &state,
            Provider::OneDrive,
            None,
            selenite_sync::remote::client_config_with(
                Provider::OneDrive,
                &none,
                selenite_sync::remote::CompiledIds::default(),
            ),
            &|_, _| Arc::new(selenite_storage::oauth::MemoryTokenStore::new()),
            &open,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not configured for this build"));
        assert!(!opened.get());
        assert!(remotes_list(&state).unwrap().is_empty());
    }

    #[test]
    fn sync_now_round_trip_pulls_indexes_and_pushes() {
        let (_d, state, mem, _f) = sync_fixture();
        add_remote(&state);
        mem.write(
            &Vp::new("Remote note.md").unwrap(),
            b"# Remote note\n\nquokka\n",
            WriteMode::CreateNew,
        )
        .unwrap();
        let local_page = create_page(&state, "Local note", None).unwrap();
        assert!(search(&state, "quokka", 5).unwrap().is_empty());

        let s = sync_now_at(&state, None, 1_000).unwrap();
        assert_eq!(s.remotes, 1);
        assert_eq!(s.pulled, 1, "{s:?}");
        assert!(s.pushed >= 1, "{s:?}");
        assert!(s.errors.is_empty() && !s.offline, "{s:?}");
        // Engine::refresh ran: the remote-only file is searchable.
        assert_eq!(search(&state, "quokka", 5).unwrap().len(), 1);
        // The local page reached the remote.
        assert!(mem.stat(&Vp::new(&local_page).unwrap()).unwrap().is_some());

        let st = sync_status(&state);
        assert_eq!(st.last_run, Some(1_000));
        assert_eq!(st.last_report.as_ref().map(|r| r.pulled), Some(1));
        assert_eq!(st.pending, 0);
        assert!(st.error.is_none());
        assert_eq!(remotes_list(&state).unwrap()[0].last_sync, Some(1_000));

        // Auto-sync respects the one-minute focus gap but not the open trigger.
        assert!(!sync_auto_at(&state, AutoSync::Focus, 1_030));
        assert!(sync_auto_at(&state, AutoSync::Focus, 1_061));
        assert!(sync_auto_at(&state, AutoSync::Tick, 1_062));
    }

    #[test]
    fn sync_runs_never_overlap() {
        let (_d, state, _m, _f) = sync_fixture();
        add_remote(&state);
        let _held = RunGuard::acquire(&state.sync_running).unwrap();
        assert!(
            sync_now(&state, None)
                .unwrap_err()
                .to_string()
                .contains("already running")
        );
        assert!(!sync_auto(&state, AutoSync::Open));
        assert!(sync_status(&state).running);
    }

    #[test]
    fn workspace_pages_nest_subpages_and_list_databases() {
        let (_d, state) = fixture();
        let a = create_subpage(&state, None, "Alpha").unwrap();
        assert_eq!(a, "Alpha.md");
        let b = create_subpage(&state, Some(&a), "Beta").unwrap();
        assert_eq!(b, "Alpha/Beta.md");
        let c = create_subpage(&state, Some(&b), "Gamma").unwrap();
        assert_eq!(c, "Alpha/Beta/Gamma.md");
        database_create(&state, "Projects", &[]).unwrap();
        state
            .with_engine(|e| {
                let mut s = selenite_vault::DatabaseSchema::tasks();
                s.version = 1;
                e.storage()
                    .write(
                        &VaultPath::new("Chores/_schema.yaml").unwrap(),
                        s.to_yaml().unwrap().as_bytes(),
                        selenite_storage::WriteMode::CreateNew,
                    )
                    .map_err(EngineError::from)?;
                Ok(())
            })
            .unwrap();
        refresh(&state).unwrap();
        let tree = workspace_pages(&state).unwrap();
        let alpha = tree.iter().find(|n| n.name == "Alpha").unwrap();
        assert_eq!(alpha.kind, "page");
        assert_eq!(names(&alpha.children), vec!["Beta"]);
        assert_eq!(names(&alpha.children[0].children), vec!["Gamma"]);
        // The Alpha folder is not listed a second time.
        assert_eq!(tree.iter().filter(|n| n.path == "Alpha").count(), 0);
        let proj = tree.iter().find(|n| n.name == "Projects").unwrap();
        assert_eq!(proj.kind, "database");
        let chores = tree.iter().find(|n| n.name == "Chores").unwrap();
        assert_eq!(chores.kind, "tasks");
        // Hidden folders never show.
        assert!(!tree.iter().any(|n| n.path.starts_with('.')));
    }

    #[test]
    fn create_subpage_rejects_empty_title_and_duplicates() {
        let (_d, state) = fixture();
        assert!(create_subpage(&state, None, "  ").is_err());
        create_subpage(&state, None, "Dup").unwrap();
        assert!(create_subpage(&state, None, "Dup").is_err());
    }

    #[test]
    fn set_page_meta_keeps_body_bytes() {
        let (_d, state) = fixture();
        let a = create_page(&state, "Meta", None).unwrap();
        let body = "# Meta\n\nHello  world\t!\n\n- a\n- b\n\n```\ncode\n```\n";
        let rd = read_page(&state, &a).unwrap();
        save_page(&state, &a, body, &rd.revision).unwrap();
        set_page_meta(&state, &a, Some("🌙"), Some("cover.png")).unwrap();
        let after = read_page(&state, &a).unwrap().text;
        assert!(after.ends_with(body), "{after:?}");
        assert!(after.contains("icon:") && after.contains("cover:"));
        let blocks = page_blocks(&state, &a).unwrap();
        assert_eq!(blocks.icon.as_deref(), Some("🌙"));
        set_page_meta(&state, &a, Some(""), Some("")).unwrap();
        let cleared = read_page(&state, &a).unwrap().text;
        assert!(cleared.ends_with(body));
        assert!(!cleared.contains("icon:"));
    }

    #[test]
    fn edit_block_replaces_and_conflicts_on_stale_revision() {
        let (_d, state) = fixture();
        let a = create_page(&state, "Blocks", None).unwrap();
        let rd = read_page(&state, &a).unwrap();
        save_page(&state, &a, "# Blocks\n\nOne\n\nTwo\n", &rd.revision).unwrap();
        let page = page_blocks(&state, &a).unwrap();
        assert_eq!(page.blocks.len(), 3);
        let edit = serde_json::json!({"op": "replace", "block": "1", "markdown": "Uno"});
        let saved = edit_block(&state, &a, &edit, &page.revision).unwrap();
        assert_ne!(saved.revision, page.revision);
        let again = edit_block(&state, &a, &edit, &page.revision).unwrap_err();
        assert!(again.to_string().starts_with("CONFLICT:"), "{again}");
        let mv = serde_json::json!({"op": "move", "block": "1", "to": 2});
        let moved = edit_block(&state, &a, &mv, &saved.revision).unwrap();
        let text = read_page(&state, &a).unwrap();
        assert_eq!(text.revision, moved.revision);
        assert!(text.text.find("Two").unwrap() < text.text.find("Uno").unwrap());
        assert!(
            edit_block(
                &state,
                &a,
                &serde_json::json!({"op": "nope"}),
                &moved.revision
            )
            .is_err()
        );
    }

    #[test]
    fn asset_paths_reject_escapes_and_hidden() {
        assert_eq!(asset_vault_path("/a/b.png").unwrap().as_str(), "a/b.png");
        assert_eq!(
            asset_vault_path("/My%20Pics/x.png").unwrap().as_str(),
            "My Pics/x.png"
        );
        assert_eq!(asset_vault_path("/../secret.txt"), Err(AssetError(400)));
        assert_eq!(asset_vault_path("/a/%2e%2e/b.png"), Err(AssetError(400)));
        assert_eq!(asset_vault_path("/a/..%2Fb.png"), Err(AssetError(400)));
        assert_eq!(asset_vault_path("/C:/Windows/x.png"), Err(AssetError(400)));
        assert_eq!(asset_vault_path("//etc/passwd"), Err(AssetError(400)));
        assert_eq!(
            asset_vault_path("/.selenite/index.sqlite"),
            Err(AssetError(403))
        );
        assert_eq!(asset_vault_path("/a/.hidden/x.png"), Err(AssetError(403)));
        assert_eq!(asset_vault_path("/%zz"), Err(AssetError(400)));
        assert_eq!(asset_vault_path("/"), Err(AssetError(400)));
        assert_eq!(
            asset_content_type(&VaultPath::new("a.PNG").unwrap()),
            "image/png"
        );
        assert_eq!(
            asset_content_type(&VaultPath::new("a.bin").unwrap()),
            "application/octet-stream"
        );
    }

    #[test]
    fn read_asset_serves_vault_files_only() {
        let (dir, state) = fixture();
        let root = dir.path().join("vault");
        std::fs::write(root.join("pic.png"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.path().join("outside.png"), [9u8]).unwrap();
        let (bytes, ct) = read_asset(&state, "/pic.png").unwrap();
        assert_eq!((bytes, ct), (vec![1, 2, 3], "image/png"));
        assert_eq!(read_asset(&state, "/missing.png"), Err(AssetError(404)));
        assert_eq!(read_asset(&state, "/../outside.png"), Err(AssetError(400)));
        assert_eq!(
            read_asset(&state, "/.selenite/config.json"),
            Err(AssetError(403))
        );
        let img = create_page(&state, "Img", None).unwrap();
        let rd = read_page(&state, &img).unwrap();
        save_page(&state, &img, "# Img\n\n![[pic.png]]\n", &rd.revision).unwrap();
        let page = page_blocks(&state, &img).unwrap();
        let html = &page.blocks[1].html;
        assert!(
            html.contains(&format!("src=\"{ASSET_BASE}pic.png\"")),
            "{html}"
        );
        assert_eq!(
            asset_url(&state, &img, "pic.png").unwrap().as_deref(),
            Some(format!("{ASSET_BASE}pic.png").as_str())
        );
    }

    #[test]
    fn database_round_trip_with_rows_and_group_by() {
        let (_d, state) = fixture();
        let props = vec![
            serde_json::json!({"name": "Status", "type": "select",
                "options": [{"name": "Todo"}, {"name": "Doing"}, {"name": "Done"}]}),
            serde_json::json!({"name": "Price", "type": "number"}),
            serde_json::json!({"name": "Qty", "type": "number"}),
            serde_json::json!({"name": "Total", "type": "formula", "expression": "prop(\"Price\") * prop(\"Qty\")"}),
        ];
        database_create(&state, "Projects", &props).unwrap();
        assert!(database_create(&state, "Projects", &props).is_err());
        let dbs = databases(&state).unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].kind, "table");
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("Status".to_owned(), serde_json::json!("Todo"));
        vals.insert("Price".to_owned(), serde_json::json!(4));
        vals.insert("Qty".to_owned(), serde_json::json!(3));
        let p1 = row_create(&state, "Projects", "Alpha", &vals).unwrap();
        assert_eq!(p1, "Projects/Alpha.md");
        let p2 = row_create(&state, "Projects", "Beta", &vals).unwrap();
        row_create(
            &state,
            "Projects",
            "Gamma",
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        row_set(&state, &p2, "Status", &serde_json::json!("Doing")).unwrap();
        assert!(row_set(&state, &p2, "Total", &serde_json::json!(1)).is_err());
        let view = database_view(&state, "Projects", None, None).unwrap();
        assert_eq!(view.rows.len(), 3);
        let alpha = view.rows.iter().find(|r| r.title == "Alpha").unwrap();
        assert_eq!(alpha.values["Total"], selenite_core::Value::Number(12.0));
        let q = serde_json::json!({"group_by": "Status"});
        let grouped = database_view(&state, "Projects", None, Some(&q)).unwrap();
        let groups = grouped.groups.unwrap();
        let labels: Vec<_> = groups.iter().map(|g| g.label.as_str()).collect();
        assert!(
            labels.contains(&"Todo") && labels.contains(&"Doing"),
            "{labels:?}"
        );
        let filtered = serde_json::json!({"filter": {"op": "and", "rules": [
            {"property": "Status", "op": "is", "value": "Doing"}]}});
        let only = database_view(&state, "Projects", None, Some(&filtered)).unwrap();
        assert_eq!(only.rows.len(), 1);
        assert_eq!(only.rows[0].title, "Beta");
        // Save a view into the schema and read it back.
        let mut schema = database_schema(&state, "Projects").unwrap();
        schema["table_views"] = serde_json::json!([
            {"name": "Table", "layout": "table"},
            {"name": "Doing", "layout": "board", "group_by": "Status",
             "filter": {"op": "and", "rules": [
                {"property": "Status", "op": "is", "value": "Doing"}]}}
        ]);
        database_schema_save(&state, "Projects", &schema).unwrap();
        let doing = database_view(&state, "Projects", Some("Doing"), None).unwrap();
        assert_eq!(doing.views.len(), 2);
        assert_eq!(doing.rows.len(), 1);
        assert!(database_schema_save(&state, "Projects", &serde_json::json!({"kind": 5})).is_err());
    }

    #[test]
    fn block_types_cover_all_templates() {
        let types = block_types();
        assert_eq!(types.len(), 16);
        assert!(
            types
                .iter()
                .any(|t| t.kind == "callout" && t.markdown.contains("[!note]"))
        );
    }

    #[test]
    fn create_vault_reports_info_and_is_remembered() {
        let (dir, state) = fixture();
        let info = vault_info(&state).unwrap();
        assert_eq!(info.name, "vault");
        assert_eq!(info.mode_id, "vault");
        assert!(info.page_count >= 1);
        assert!(last_vault(&state).unwrap().ends_with("vault"));
        let again = AppState::new(dir.path().join("cfg").join("settings.json"));
        assert!(open_last_vault(&again));
        assert!(vault_info(&again).is_ok());
    }

    #[test]
    fn no_vault_is_an_error() {
        let state = AppState::new(PathBuf::from("unused.json"));
        assert!(matches!(tree(&state), Err(CmdError::NoVault)));
    }

    #[test]
    fn linked_pages_give_backlinks_and_outgoing() {
        let (_d, state) = fixture();
        let a = create_page(&state, "Alpha", None).unwrap();
        let b = create_page(&state, "Beta", Some("Projects")).unwrap();
        assert_eq!(b, "Projects/Beta.md");
        let rd = read_page(&state, &a).unwrap();
        save_page(&state, &a, "# Alpha\n\nSee [[Beta]].\n", &rd.revision).unwrap();
        let back = backlinks(&state, &b).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].source, a);
        let out = outgoing(&state, &a).unwrap();
        assert_eq!(out[0].resolved.as_deref(), Some(b.as_str()));
        assert_eq!(
            resolve(&state, "Beta").unwrap().as_deref(),
            Some(b.as_str())
        );
    }

    #[test]
    fn stale_revision_is_a_conflict() {
        let (_d, state) = fixture();
        let a = create_page(&state, "Gamma", None).unwrap();
        let rd = read_page(&state, &a).unwrap();
        let saved = save_page(&state, &a, "# Gamma\n\nfirst\n", &rd.revision).unwrap();
        assert_ne!(saved.revision, rd.revision);
        let err = save_page(&state, &a, "# Gamma\n\nsecond\n", &rd.revision).unwrap_err();
        assert!(err.to_string().starts_with("CONFLICT:"), "{err}");
    }

    #[test]
    fn search_finds_text_with_highlight_markers() {
        let (_d, state) = fixture();
        let a = create_page(&state, "Notes", None).unwrap();
        let rd = read_page(&state, &a).unwrap();
        save_page(&state, &a, "# Notes\n\nquokka habitat\n", &rd.revision).unwrap();
        let hits = search(&state, "quokka", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, a);
        assert!(hits[0].snippet.contains('\u{E000}'));
    }

    #[test]
    fn unresolved_link_page_is_created_at_target() {
        let (_d, state) = fixture();
        assert_eq!(resolve(&state, "Missing").unwrap(), None);
        let p = create_page_at(&state, "Missing").unwrap();
        assert_eq!(p, "Missing.md");
        assert_eq!(read_page(&state, &p).unwrap().text, "# Missing\n");
        assert_eq!(
            resolve(&state, "Missing").unwrap().as_deref(),
            Some("Missing.md")
        );
        assert!(create_page_at(&state, "../escape").is_err());
    }

    #[test]
    fn daily_note_is_created_once() {
        let (_d, state) = fixture();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        let p = daily_note_for(&state, date).unwrap();
        assert_eq!(p, "Daily/2026-11-16.md");
        assert_eq!(daily_note_for(&state, date).unwrap(), p);
        assert!(daily_note(&state).unwrap().starts_with("Daily/"));
        assert!(
            tree(&state)
                .unwrap()
                .children
                .iter()
                .any(|c| c.name == "Daily")
        );
        refresh(&state).unwrap();
        assert!(tags(&state).is_ok());
    }

    // ------------------------------------------------------------ tasks and reminders

    use selenite_reminders::{Notifier, Occurrence, ReminderError};

    #[test]
    fn set_mode_switches_without_touching_pages() {
        let (_d, state) = fixture();
        let before = vault_info(&state).unwrap();
        assert_eq!(before.modes.len(), 3);
        assert_eq!(before.modes[2].id, "tasks");
        let info = set_mode(&state, "tasks").unwrap();
        assert_eq!(info.mode_id, "tasks");
        assert_eq!(info.mode_label, "Tasks");
        assert_eq!(info.page_count, before.page_count);
        assert_eq!(vault_info(&state).unwrap().mode_id, "tasks");
        assert!(set_mode(&state, "nonsense").is_err());
        assert_eq!(set_mode(&state, "vault").unwrap().mode_id, "vault");
    }

    #[test]
    fn timed_task_is_scheduled_once_and_completing_removes_it() {
        let (_d, state) = fixture();
        assert_eq!(state.notifier.scheduled_ids().unwrap().len(), 0);
        let res = task_quick_add(&state, "call bank tomorrow at 15:00").unwrap();
        assert!(res.path.ends_with(".md"));
        assert_eq!(state.notifier.scheduled_ids().unwrap().len(), 1);
        let status = reminders_status(&state);
        assert_eq!(status.scheduled, 1);
        assert_eq!(status.kind, "memory");
        assert!(status.last_error.is_none());
        let done = task_complete(&state, &res.path).unwrap();
        assert!(done.done);
        assert_eq!(state.notifier.scheduled_ids().unwrap().len(), 0);
        assert_eq!(reminders_status(&state).scheduled, 0);
        task_reopen(&state, &res.path).unwrap();
        assert_eq!(state.notifier.scheduled_ids().unwrap().len(), 1);
        task_archive(&state, &res.path).unwrap();
        assert_eq!(state.notifier.scheduled_ids().unwrap().len(), 0);
    }

    #[test]
    fn builtin_views_update_and_agenda() {
        let (_d, state) = fixture();
        task_quick_add(&state, "buy milk today").unwrap();
        task_quick_add(&state, "file taxes p1 #money").unwrap();
        let today = tasks_query(&state, "today").unwrap();
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].title, "buy milk");
        assert!(tasks_query(&state, "completed").unwrap().is_empty());
        assert_eq!(tasks_query(&state, "p1").unwrap().len(), 1);
        assert!(tasks_query(&state, "((").is_err());

        let path = today[0].path.clone();
        let patch = serde_json::json!({
            "title": "buy oat milk", "priority": 2, "due": "2026-12-01T09:30",
            "project": "Home", "section": "Shopping", "labels": ["errand", "food"],
            "reminders": ["-15m", "2026-11-30T08:45"]
        });
        let t = task_update(&state, &path, &patch).unwrap();
        assert_eq!(t.title, "buy oat milk");
        assert_eq!(t.priority, 2);
        assert_eq!(t.due.as_deref(), Some("2026-12-01T09:30"));
        assert_eq!(t.reminders, vec!["-15m", "2026-11-30T08:45"]);
        let got = task_get(&state, &path).unwrap();
        assert_eq!(got.fields.project.as_deref(), Some("Home"));
        assert_eq!(got.fields.section.as_deref(), Some("Shopping"));
        assert_eq!(got.fields.labels, vec!["errand", "food"]);
        let cleared = task_update(
            &state,
            &path,
            &serde_json::json!({"due": null, "project": null, "section": ""}),
        )
        .unwrap();
        assert!(cleared.due.is_none() && cleared.project.is_none() && cleared.section.is_none());

        // An invalid patch writes nothing.
        assert!(task_update(&state, &path, &serde_json::json!({"priority": 9})).is_err());
        assert!(task_update(&state, &path, &serde_json::json!({"due": "soon"})).is_err());
        assert!(task_update(&state, &path, &serde_json::json!({"title": " "})).is_err());
        assert_eq!(
            task_get(&state, &path).unwrap().fields.title,
            "buy oat milk"
        );

        assert!(
            task_projects(&state)
                .unwrap()
                .iter()
                .all(|p| p.project != "Home")
        );
        assert!(task_labels(&state).unwrap().contains(&"errand".to_owned()));
        assert_eq!(task_agenda(&state, 7).unwrap().days.len(), 7);
    }

    #[test]
    fn saved_views_round_trip_and_reading_creates_nothing() {
        let (_d, state) = fixture();
        assert!(task_views(&state).unwrap().is_empty());
        assert!(
            tree(&state)
                .unwrap()
                .children
                .iter()
                .all(|c| c.name != "Tasks")
        );
        task_quick_add(&state, "x").unwrap();
        let views = task_view_save(&state, "Urgent", "p1 & open", "board").unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].layout, "board");
        assert!(task_view_save(&state, "Bad", "((", "list").is_err());
        assert!(task_view_save(&state, " ", "p1", "list").is_err());
        assert_eq!(task_views(&state).unwrap()[0].name, "Urgent");
    }

    struct FailingNotifier;

    impl Notifier for FailingNotifier {
        fn kind(&self) -> &'static str {
            "failing"
        }
        fn replace_all(&self, _: &[Occurrence]) -> Result<usize, ReminderError> {
            Err(ReminderError::Notify("denied".into()))
        }
        fn scheduled_ids(&self) -> Result<Vec<String>, ReminderError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn notifier_failure_is_recorded_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::with_notifier(dir.path().join("s.json"), Box::new(FailingNotifier));
        create_vault(&state, dir.path().join("v").to_str().unwrap()).unwrap();
        let res = task_quick_add(&state, "call bank tomorrow at 15:00");
        assert!(res.is_ok());
        let status = reminders_status(&state);
        assert_eq!(status.kind, "failing");
        assert!(status.last_error.as_deref().unwrap().contains("denied"));
        refresh(&state).unwrap();
    }

    // ---- ask and plugins

    fn put_plugin(state: &AppState, id: &str, perms: &str) {
        let engine_path = |rel: &str| selenite_core::VaultPath::new(rel).unwrap();
        state
            .with_engine(|e| {
                let manifest =
                    format!(r#"{{"id":"{id}","name":"T","version":"1","permissions":[{perms}]}}"#);
                e.storage()
                    .write(
                        &engine_path(&format!(".selenite/plugins/{id}/manifest.json")),
                        manifest.as_bytes(),
                        selenite_storage::WriteMode::Overwrite,
                    )
                    .unwrap();
                e.storage()
                    .write(
                        &engine_path(&format!(".selenite/plugins/{id}/main.js")),
                        b"// plugin",
                        selenite_storage::WriteMode::Overwrite,
                    )
                    .unwrap();
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn plugin_frames_are_only_served_for_enabled_unchanged_plugins() {
        let (_d, state) = fixture();
        put_plugin(&state, "reader", r#""read""#);
        // Disabled, malformed and unknown requests are all refused.
        assert!(plugin_frame(&state, "/reader/").is_none());
        plugin_set_enabled(&state, "reader", true).unwrap();
        for bad in [
            "",
            "/",
            "/reader",
            "reader/",
            "/../reader/",
            "/reader/x",
            "//",
            "/nope/",
        ] {
            assert!(plugin_frame(&state, bad).is_none(), "{bad}");
        }
        let html = plugin_frame(&state, "/reader/").unwrap();
        assert!(html.contains("// plugin") && html.contains("selenite"));
        assert!(!html.contains("srcdoc"));
        // A changed file no longer matches its enabled hash.
        state
            .with_engine(|e| {
                e.storage()
                    .write(
                        &selenite_core::VaultPath::new(".selenite/plugins/reader/main.js").unwrap(),
                        b"// changed",
                        selenite_storage::WriteMode::Overwrite,
                    )
                    .unwrap();
                Ok(())
            })
            .unwrap();
        assert!(plugin_frame(&state, "/reader/").is_none());
    }

    #[test]
    fn script_safe_neutralises_script_end_and_comment_openers() {
        let out = script_safe("a</SCRIPT>b<!--c é");
        assert!(!out.to_ascii_lowercase().contains("</script"));
        assert!(!out.contains("<!--"));
        assert!(out.ends_with("é"));
    }

    #[test]
    fn ask_returns_citations_for_the_open_vault() {
        let (_d, state) = fixture();
        create_page(&state, "Berlin", None).unwrap();
        state
            .with_engine(|e| {
                let p = selenite_core::VaultPath::new("Berlin.md").unwrap();
                let (_, rev) = e.read_page(&p)?;
                e.save_page(
                    &p,
                    "# Berlin

Checkout at the hotel is at noon.
",
                    Some(rev),
                )?;
                Ok(())
            })
            .unwrap();
        let cites = ask(&state, "When is hotel checkout?", 5).unwrap();
        assert_eq!(cites[0].path, "Berlin.md");
        assert_eq!((cites[0].start_line, cites[0].end_line), (3, 3));
        let empty = AppState::new(std::path::PathBuf::from("unused.json"));
        assert!(matches!(ask(&empty, "x", 3), Err(CmdError::NoVault)));
    }

    #[test]
    fn plugin_calls_are_permission_checked_and_audited() {
        let (_d, state) = fixture();
        put_plugin(&state, "reader", r#""read""#);
        assert!(!plugin_list(&state).unwrap()[0].enabled);
        let args = serde_json::json!({"title": "Nope", "markdown": "x"});
        // Disabled: nothing runs and no source is handed out.
        assert!(plugin_load(&state, "reader").is_err());
        assert!(plugin_call(&state, "reader", "vault.createPage", &args).is_err());

        plugin_set_enabled(&state, "reader", true).unwrap();
        assert_eq!(plugin_load(&state, "reader").unwrap().source, "// plugin");
        let err = plugin_call(&state, "reader", "vault.createPage", &args).unwrap_err();
        assert!(err.to_string().contains("`write` permission"), "{err}");
        assert!(resolve(&state, "Nope").unwrap().is_none());

        put_plugin(&state, "writer", r#""write""#);
        plugin_set_enabled(&state, "writer", true).unwrap();
        plugin_call(&state, "writer", "vault.createPage", &args).unwrap();
        assert!(resolve(&state, "Nope").unwrap().is_some());
        let log = state
            .with_engine(|e| {
                Ok(selenite_index::audit::AuditLog::new()
                    .entries(e, 10)
                    .unwrap())
            })
            .unwrap();
        let transports: Vec<_> = log.iter().map(|a| a.transport.clone().unwrap()).collect();
        assert_eq!(transports, vec!["plugin:reader", "plugin:writer"]);
    }
}

//! Tauri-free command layer. Every function takes `&AppState` so it can be unit-tested; the
//! `#[tauri::command]` wrappers in `lib.rs` only map errors to strings.
//!
//! Errors never carry page contents.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use selenite_core::{Due, Mode, PathError, Priority, ReminderSpec, TaskFields, VaultPath};
use selenite_index::{Agenda, Engine, EngineError, TaskItem, TreeNode};
use selenite_reminders::PlannerInput;
use selenite_storage::{Revision, StorageError};
use selenite_vault::{Document, SavedView};
use serde::{Deserialize, Serialize};

use crate::state::{AppState, RemindersStatus};

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
}

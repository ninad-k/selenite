//! The MCP server: tools, resources and prompts over one vault [`Engine`].
//!
//! # Concurrency
//!
//! [`Engine`] owns a `rusqlite::Connection`, which is `Send` but not `Sync`, and every engine
//! call is blocking file and SQLite I/O. So the engine lives in an `Arc<std::sync::Mutex<_>>`
//! and each tool call runs inside `tokio::task::spawn_blocking`, taking the lock for the whole
//! call. That keeps the async runtime free, serialises vault access (one writer at a time, so
//! a tool's read-modify-write and its audit append are never interleaved with another tool),
//! and avoids holding a lock across an `.await`. A poisoned lock is recovered rather than
//! propagated: the engine's state is on disk, not in memory.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{Local, NaiveDate, NaiveDateTime};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, GetPromptRequestParams, GetPromptResponse, GetPromptResult,
        Implementation, ListPromptsResult, ListResourcesResult, PaginatedRequestParams, Prompt,
        PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
        Resource, ResourceContents, Role, ServerCapabilities, ServerConfig,
    },
    schemars::JsonSchema,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use selenite_core::database::{PropertyDef, ViewDef, ViewQuery};
use selenite_core::filter::{BuiltinView, Filter, builtin};
use selenite_core::{APP_NAME, Due, PathError, Priority, ReminderSpec, TaskFields, VaultPath};
use selenite_index::{Engine, EngineError, HIGHLIGHT_END, HIGHLIGHT_START, TaskItem};
use selenite_storage::{Revision, StorageError};
use selenite_vault::replace_block;
use selenite_vault::{
    BlockRef, DatabaseKind, Document, SCHEMA_FILE, VaultError, append_blocks, layout, parse_blocks,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::audit::{AuditEntry, AuditLog, args_digest};

const INSTRUCTIONS: &str = "\
A vault is a folder of Markdown pages (with optional YAML front matter) linked by [[wikilinks]] \
and #tags. Tasks are pages too: each task is a Markdown file in a Tasks database folder, with its \
fields (title, priority p1-p4, due, project, section, labels, reminders) in front matter. \
Use `search` and `get_page` to read, or `ask_vault` for a natural-language question (it returns citable passages; answer from them only and cite [path#Lstart-Lend]); every page read returns a `revision`. Pass that revision \
back when you write (`append_blocks`, `update_block`) so a concurrent edit is reported as a \
conflict instead of being overwritten. Pages are edited by block (paragraph, list, heading, ...) \
-- call `list_blocks` to see them and address a block by index or by its `^anchor`. \
Nothing can be deleted through this server: `archive_page` and `archive_task` move items to the \
vault trash. Every write is recorded in a local audit log (without page content).";

/// Returned with every `ask_vault` result.
const ASK_INSTRUCTIONS: &str =
    "Answer using only these passages and cite them as [path#Lstart-Lend]";

/// Public message and audit category of a failed tool call.
struct ToolFail {
    message: String,
    category: &'static str,
}

impl ToolFail {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            category: "invalid_arguments",
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            category: "conflict",
        }
    }
}

impl From<EngineError> for ToolFail {
    fn from(e: EngineError) -> Self {
        let category = match &e {
            EngineError::Storage(StorageError::Conflict { .. }) => "conflict",
            EngineError::Storage(StorageError::NotFound(_)) => "not_found",
            EngineError::Storage(StorageError::AlreadyExists(_)) => "already_exists",
            EngineError::Path(_)
            | EngineError::NotAPage(_)
            | EngineError::NotUtf8(_)
            | EngineError::Database(_) => "invalid_arguments",
            EngineError::Vault(VaultError::Block(_)) => "invalid_arguments",
            EngineError::Task(_) => "task_error",
            _ => "engine_error",
        };
        let message = if category == "conflict" {
            format!("conflict: {e}. Re-read the page and retry.")
        } else {
            e.to_string()
        };
        Self { message, category }
    }
}

impl From<VaultError> for ToolFail {
    fn from(e: VaultError) -> Self {
        Self::invalid(e.to_string())
    }
}

impl From<PathError> for ToolFail {
    fn from(e: PathError) -> Self {
        Self::invalid(format!("invalid path: {e}"))
    }
}

impl From<StorageError> for ToolFail {
    fn from(e: StorageError) -> Self {
        ToolFail::from(EngineError::from(e))
    }
}

type ToolResult<T> = Result<T, ToolFail>;

/// What a write tool touched, for the audit log.
#[derive(Default)]
struct Trace {
    targets: Vec<String>,
    before: Option<String>,
    after: Option<String>,
}

struct Inner {
    engine: Mutex<Engine>,
    audit: AuditLog,
}

/// The vault MCP server. Cheap to clone; clones share one engine.
#[derive(Clone)]
pub struct VaultMcp {
    inner: Arc<Inner>,
    transport: &'static str,
    tool_router: ToolRouter<VaultMcp>,
}

impl std::fmt::Debug for VaultMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultMcp").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Parameter types (doc comments become the JSON-schema descriptions the model sees).
// ---------------------------------------------------------------------------------------------

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Parameters of `search`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SearchParams {
    /// Full-text query. Supports `tag:name`, `path:folder`, quoted phrases and `-exclusion`.
    pub query: String,
    /// Maximum number of results (default 20, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Parameters of `ask_vault`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AskParams {
    /// The question in plain language, for example `Where do we stay in Berlin?`.
    pub question: String,
    /// Maximum number of passages (default 8, at most 25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Parameters naming one page.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PathParams {
    /// Vault-relative page path such as `Projects/Plan.md` (`.md` may be left out).
    pub path: String,
}

/// Parameters of `create_page`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreatePageParams {
    /// Page title; the file name is derived from it. Give either `title` or `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Exact vault-relative path to create (for example `Notes/Idea.md`). Give either `title`
    /// or `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Folder for a page created by `title` (default: the vault root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Markdown content of the page (may include YAML front matter).
    pub markdown: String,
}

/// Parameters of `append_blocks`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppendBlocksParams {
    /// Vault-relative page path.
    pub path: String,
    /// Markdown to add after the page's last block (one blank line is inserted between).
    pub markdown: String,
    /// Revision from `get_page`. When given and the page has changed since, nothing is written
    /// and a conflict error is returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Parameters of `update_block`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct UpdateBlockParams {
    /// Vault-relative page path.
    pub path: String,
    /// The block to replace: an anchor such as `^intro`, or a block index such as `3`
    /// (see `list_blocks`).
    pub block: String,
    /// New Markdown for the block. An existing anchor is kept if the new text has none.
    pub markdown: String,
    /// Revision from `get_page` or `list_blocks`. When given and the page has changed since,
    /// nothing is written and a conflict error is returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Parameters of `list_tasks`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListTasksParams {
    /// A built-in view (`inbox`, `today`, `upcoming`, `completed`, `all`) or a filter
    /// expression such as `p1 & #Home & !done`, `overdue | today`, `@errand`, `next 7 days`.
    /// Default: `open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Maximum number of tasks returned (default 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Parameters of `create_task`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateTaskParams {
    /// One line of natural language, for example `Buy milk tomorrow 5pm p2 #Home @errand`.
    /// Give either `text` or `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Task title (structured form). Give either `text` or `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Priority `p1` (highest) to `p4` (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Due date `YYYY-MM-DD`, or date and time `YYYY-MM-DDTHH:MM`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
    /// Project path such as `Home` or `Home/Garden`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Section within the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// Reminders: `YYYY-MM-DDTHH:MM` or a lead time before the due moment such as `-15m`,
    /// `-1h`, `-2d`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminders: Option<Vec<String>>,
}

/// Parameters of `update_task`. Omitted fields are left unchanged.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct UpdateTaskParams {
    /// Vault-relative path of the task.
    pub path: String,
    /// New title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Priority `p1` (highest) to `p4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Due date `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM`; `null` clears the due date.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub due: Option<Option<String>>,
    /// Project path; `null` moves the task to the Inbox.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub project: Option<Option<String>>,
    /// Section; `null` clears it.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub section: Option<Option<String>>,
    /// Replacement list of labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
}

/// Parameters of `set_reminder`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SetReminderParams {
    /// Vault-relative path of the task.
    pub path: String,
    /// The task's complete reminder list (replaces the old one; empty clears it). Each entry is
    /// `YYYY-MM-DDTHH:MM` or a lead time before the due moment such as `-15m`, `-1h`, `-2d`.
    pub reminders: Vec<String>,
}

/// Parameters of `get_agenda`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AgendaParams {
    /// Number of days starting today (default 7, at most 366).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
}

/// Parameters of `query_database`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct QueryDatabaseParams {
    /// Database folder. Omit to query all tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Tasks databases only: filter expression, same syntax as `list_tasks` (`p1 & !done`,
    /// `@errand | overdue`). Default: `open`. Not used for table databases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Table databases only: name of a saved view (default: the database's first view).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    /// Table databases only: an ad-hoc query that replaces the view's own query. An object with
    /// `filter` (a group `{"op": "and", "rules": [...]}` whose rules are `{"property", "op",
    /// "value"}` conditions or nested groups), `sorts` (`[{"property", "descending"}]`) and
    /// `group_by` (a select property name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub query: Option<Value>,
}

/// Parameters naming one database folder (empty for the vault root).
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct FolderParams {
    /// Vault-relative database folder such as `Projects/Books`.
    pub folder: String,
}

/// Parameters of `create_database`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateDatabaseParams {
    /// Vault-relative folder to turn into a table database. It must not already be a database.
    pub folder: String,
    /// Properties after the `Name` title column. Each is an object with `name` and `type`:
    /// `text`, `number` (optional `format`), `select` or `multi_select` (`options`: `[{"name":
    /// "Todo"}]`), `date`, `checkbox`, `url`, `relation` (`database`: folder of the target),
    /// `rollup` (`relation`, `property`, `function`: count, sum, average, min, max, ...),
    /// `formula` (`expression`: `prop("Price") * prop("Qty")`), `created_time`, `updated_time`.
    #[serde(default)]
    #[schemars(with = "Vec<serde_json::Value>")]
    pub properties: Vec<Value>,
}

/// Parameters of `update_database_schema`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct UpdateDatabaseSchemaParams {
    /// Vault-relative folder of a table database.
    pub folder: String,
    /// Replacement property list, same shape as in `create_database`. Omit to keep the current
    /// properties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Vec<serde_json::Value>>")]
    pub properties: Option<Vec<Value>>,
    /// Replacement saved views. Each is an object with `name`, `layout` (table, board, list,
    /// calendar, gallery, timeline), optional `group_by`, `filter`, `sorts`, `visible` (property
    /// names), `date_property`, `end_date_property`, `cover_property`. Omit to keep the views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Vec<serde_json::Value>>")]
    pub views: Option<Vec<Value>>,
    /// Revision from `get_database_schema`. When given and the schema has changed since, nothing
    /// is written and a conflict error is returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Parameters of `create_row`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateRowParams {
    /// Vault-relative folder of a table database.
    pub folder: String,
    /// Row title; also the row's file name.
    pub title: String,
    /// Property values by name, for example `{"Status": "Done", "Price": 9.5, "Due":
    /// "2030-01-15"}`. Dates are `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM`; lists are JSON arrays.
    /// Computed properties (formula, rollup, created or updated time) are ignored.
    #[serde(default)]
    #[schemars(with = "std::collections::BTreeMap<String, serde_json::Value>")]
    pub values: BTreeMap<String, Value>,
}

/// Parameters of `set_row_property`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SetRowPropertyParams {
    /// Vault-relative path of the row (a page in a table database).
    pub path: String,
    /// Property name.
    pub property: String,
    /// The new value as JSON. `null` clears the property. Computed properties cannot be set.
    #[schemars(with = "serde_json::Value")]
    pub value: Value,
}

/// Parameters of `daily_note`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DailyNoteParams {
    /// Date `YYYY-MM-DD` (default: today). The note is created if it does not exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn now() -> NaiveDateTime {
    Local::now().naive_local()
}

/// A database folder as the engine expects it: trimmed, no slashes at the ends.
fn folder_name(raw: &str) -> String {
    raw.trim().trim_matches('/').to_string()
}

/// Vault path (as text) of a database folder's schema file, used as the audit target.
fn schema_target(folder: &str) -> String {
    if folder.is_empty() {
        SCHEMA_FILE.to_string()
    } else {
        format!("{folder}/{SCHEMA_FILE}")
    }
}

/// Decode a list of JSON objects into typed definitions, naming the first bad item.
fn parse_items<T: DeserializeOwned>(what: &str, items: &[Value]) -> ToolResult<Vec<T>> {
    items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::from_value::<T>(v.clone())
                .map_err(|e| ToolFail::invalid(format!("invalid {what} {}: {e}", i + 1)))
        })
        .collect()
}

fn to_json<T: Serialize>(value: &T) -> ToolResult<Value> {
    serde_json::to_value(value).map_err(|e| ToolFail {
        message: format!("could not encode the result: {e}"),
        category: "engine_error",
    })
}

fn page_path(raw: &str) -> ToolResult<VaultPath> {
    let raw = raw.trim();
    let mut p = VaultPath::new(raw)?;
    if p.extension().is_none() {
        p = VaultPath::new(&format!("{raw}.md"))?;
    }
    Ok(p)
}

fn rev_of(eng: &Engine, p: &VaultPath) -> Option<String> {
    eng.storage()
        .stat(p)
        .ok()
        .flatten()
        .map(|entry| entry.revision.0)
}

fn check_revision(expected: Option<&str>, actual: &str, path: &VaultPath) -> ToolResult<()> {
    match expected {
        Some(r) if r != actual => Err(ToolFail::conflict(format!(
            "conflict: {path} changed since revision {r} (now {actual}). Re-read it with \
             get_page and retry."
        ))),
        _ => Ok(()),
    }
}

fn parse_priority(s: &str) -> ToolResult<Priority> {
    let t = s.trim();
    t.parse::<Priority>()
        .or_else(|_| format!("p{t}").parse::<Priority>())
        .map_err(|_| ToolFail::invalid(format!("invalid priority `{s}` (use p1, p2, p3 or p4)")))
}

fn parse_due(s: &str) -> ToolResult<Due> {
    s.trim().parse::<Due>().map_err(|_| {
        ToolFail::invalid(format!(
            "invalid due `{s}` (use YYYY-MM-DD or YYYY-MM-DDTHH:MM)"
        ))
    })
}

fn parse_reminders(items: &[String]) -> ToolResult<Vec<ReminderSpec>> {
    items
        .iter()
        .map(|s| {
            s.parse::<ReminderSpec>().map_err(|_| {
                ToolFail::invalid(format!(
                    "invalid reminder `{s}` (use YYYY-MM-DDTHH:MM or a lead time like -15m)"
                ))
            })
        })
        .collect()
}

fn task_json(path: &str, fields: &TaskFields) -> Value {
    let mut v = serde_json::to_value(fields).unwrap_or_else(|_| json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("path".into(), json!(path));
        obj.insert("done".into(), json!(fields.done));
        obj.insert("priority".into(), json!(fields.priority.to_string()));
    }
    v
}

fn item_json(t: &TaskItem) -> Value {
    task_json(t.path.as_str(), &t.fields)
}

fn clean_snippet(s: &str) -> String {
    s.replace([HIGHLIGHT_START, HIGHLIGHT_END], "**")
}

fn ok_json(value: &Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn err_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn to_result(r: ToolResult<Value>) -> CallToolResult {
    match r {
        Ok(v) => ok_json(&v),
        Err(f) => err_result(f.message),
    }
}

pub(crate) fn uri_for(path: &VaultPath) -> String {
    let mut out = String::from("vault:///");
    for b in path.as_str().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn path_from_uri(uri: &str) -> Option<VaultPath> {
    let rest = uri.strip_prefix("vault:///")?;
    let bytes = rest.as_bytes();
    let mut raw = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = rest.get(i + 1..i + 3)?;
            raw.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            raw.push(bytes[i]);
            i += 1;
        }
    }
    VaultPath::new(&String::from_utf8(raw).ok()?).ok()
}

fn builtin_or_filter(expr: &str, today: NaiveDate) -> ToolResult<Filter> {
    let view = match expr.trim().to_lowercase().as_str() {
        "inbox" => Some(BuiltinView::Inbox),
        "today" => Some(BuiltinView::Today),
        "upcoming" => Some(BuiltinView::Upcoming),
        "completed" => Some(BuiltinView::Completed),
        "all" => Some(BuiltinView::All),
        _ => None,
    };
    match view {
        Some(v) => Ok(builtin(v, today)),
        None => Filter::parse(expr, today).map_err(|e| ToolFail::invalid(format!("filter: {e}"))),
    }
}

impl VaultMcp {
    /// Serve `engine`. Audit entries go into the engine's vault.
    pub fn new(engine: Engine) -> Self {
        Self {
            inner: Arc::new(Inner {
                engine: Mutex::new(engine),
                audit: AuditLog::new(),
            }),
            transport: "stdio",
            tool_router: Self::tool_router(),
        }
    }

    /// A handle to the same engine whose audit entries record `transport` (`"stdio"` is the
    /// default, the HTTP server uses `"http"`).
    #[must_use]
    pub fn with_transport(mut self, transport: &'static str) -> Self {
        self.transport = transport;
        self
    }

    /// Bring the index up to date with files changed outside the engine (for example by sync).
    /// Holds the engine lock only for the refresh itself. Returns `(added, updated, removed)`
    /// page counts.
    pub async fn refresh_index(&self) -> Result<(usize, usize, usize), String> {
        self.blocking(|eng, _| {
            eng.refresh()
                .map(|s| (s.added, s.updated, s.removed))
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|f| f.message)?
    }

    /// The newest `limit` audit entries, oldest first (used by tests and the CLI).
    pub async fn audit_entries(&self, limit: usize) -> Result<Vec<AuditEntry>, String> {
        self.blocking(move |eng, audit| audit.entries(eng, limit))
            .await
            .map_err(|f| f.message)?
    }

    /// Definitions of all registered tools.
    pub fn tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    /// Names of all registered tools.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    /// Run `f` with the engine locked, on the blocking thread pool.
    async fn blocking<T, F>(&self, f: F) -> ToolResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Engine, &AuditLog) -> T + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.engine.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut guard, &inner.audit)
        })
        .await
        .map_err(|_| ToolFail {
            message: "internal error: the tool task failed".into(),
            category: "engine_error",
        })
    }

    /// Run a read-only tool.
    async fn read<F>(&self, f: F) -> CallToolResult
    where
        F: FnOnce(&mut Engine) -> ToolResult<Value> + Send + 'static,
    {
        match self.blocking(move |eng, _| f(eng)).await {
            Ok(r) => to_result(r),
            Err(f) => err_result(f.message),
        }
    }

    /// Run a write tool and append one audit entry describing it.
    async fn write<F>(&self, tool: &'static str, args: &impl Serialize, f: F) -> CallToolResult
    where
        F: FnOnce(&mut Engine, &mut Trace) -> ToolResult<Value> + Send + 'static,
    {
        let digest = args_digest(&serde_json::to_value(args).unwrap_or(Value::Null));
        let transport = self.transport;
        let outcome = self
            .blocking(move |eng, audit| {
                let mut trace = Trace::default();
                let result = f(eng, &mut trace);
                let entry = AuditEntry {
                    ts: Local::now().to_rfc3339(),
                    tool: tool.to_string(),
                    targets: trace.targets,
                    ok: result.is_ok(),
                    error: result.as_ref().err().map(|e| e.category.to_string()),
                    revision_before: trace.before,
                    revision_after: trace.after,
                    args_digest: digest,
                    transport: Some(transport.to_string()),
                };
                let logged = audit.record(eng, &entry);
                (result, logged)
            })
            .await;
        match outcome {
            Ok((result, logged)) => {
                if let Err(e) = &logged {
                    tracing::error!(tool, error = %e, "audit log write failed");
                }
                match result {
                    Ok(mut value) => {
                        if logged.is_err()
                            && let Some(obj) = value.as_object_mut()
                        {
                            obj.insert("audit_error".into(), json!(true));
                        }
                        ok_json(&value)
                    }
                    Err(f) => err_result(f.message),
                }
            }
            Err(f) => err_result(f.message),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

#[tool_router]
impl VaultMcp {
    /// Full-text search over pages.
    #[tool(
        description = "Full-text search over the vault's pages. Returns path, title and a snippet (matches wrapped in **). Supports tag:name, path:folder, \"phrases\" and -exclusions.",
        annotations(read_only_hint = true)
    )]
    pub async fn search(&self, Parameters(p): Parameters<SearchParams>) -> CallToolResult {
        self.read(move |eng| {
            let limit = p.limit.unwrap_or(20).clamp(1, 100);
            let hits = eng.search(&p.query, limit)?;
            Ok(json!({
                "results": hits.iter().map(|h| json!({
                    "path": h.path.as_str(),
                    "title": h.title,
                    "snippet": clean_snippet(&h.snippet),
                })).collect::<Vec<_>>()
            }))
        })
        .await
    }

    /// Ranked, citable passages for a natural-language question.
    #[tool(
        description = "Find passages in the vault that answer a natural-language question. Returns ranked citations (path, title, heading, start_line, end_line, excerpt, score) and instructions. No answer is generated here: compose it yourself from the excerpts only and cite each claim as [path#Lstart-Lend].",
        annotations(read_only_hint = true)
    )]
    pub async fn ask_vault(&self, Parameters(p): Parameters<AskParams>) -> CallToolResult {
        self.read(move |eng| {
            let limit = p.limit.unwrap_or(8).clamp(1, 25);
            let citations = eng.ask(&p.question, limit)?;
            Ok(json!({
                "question": p.question,
                "citations": to_json(&citations)?,
                "instructions": ASK_INSTRUCTIONS,
            }))
        })
        .await
    }

    /// Read one page.
    #[tool(
        description = "Read a page: its full text (including front matter), current revision, backlinks and outgoing links. Pass the revision back when writing.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_page(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        self.read(move |eng| {
            let path = page_path(&p.path)?;
            let (text, rev) = eng.read_page(&path)?;
            let backlinks = eng.backlinks(&path)?;
            let outgoing = eng.outgoing(&path)?;
            Ok(json!({
                "path": path.as_str(),
                "text": text,
                "revision": rev.0,
                "backlinks": backlinks.iter().map(|b| json!({
                    "source": b.source.as_str(),
                    "title": b.source_title,
                    "line": b.line,
                    "context": b.context,
                })).collect::<Vec<_>>(),
                "outgoing": outgoing.iter().map(|o| json!({
                    "target": o.target,
                    "resolved": o.resolved.as_ref().map(|r| r.as_str()),
                    "line": o.line,
                    "embed": o.embed,
                })).collect::<Vec<_>>(),
            }))
        })
        .await
    }

    /// List the blocks of a page.
    #[tool(
        description = "List a page's top-level Markdown blocks (index, kind, anchor, text) plus its revision, so a block can be replaced with update_block.",
        annotations(read_only_hint = true)
    )]
    pub async fn list_blocks(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        self.read(move |eng| {
            let path = page_path(&p.path)?;
            let (text, rev) = eng.read_page(&path)?;
            let doc = Document::parse(&text);
            let blocks = parse_blocks(doc.body());
            Ok(json!({
                "path": path.as_str(),
                "revision": rev.0,
                "blocks": blocks.iter().map(|b| json!({
                    "index": b.index,
                    "kind": b.kind.name(),
                    "anchor": b.anchor,
                    "text": b.text,
                })).collect::<Vec<_>>(),
            }))
        })
        .await
    }

    /// Create a page.
    #[tool(
        description = "Create a new page (fails if it already exists). Give a title (file name derived, optional folder) or an exact path, plus the Markdown content.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn create_page(&self, Parameters(p): Parameters<CreatePageParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("create_page", &args, move |eng, trace| {
            let path = match (&p.title, &p.path) {
                (Some(_), Some(_)) => {
                    return Err(ToolFail::invalid("give either `title` or `path`, not both"));
                }
                (None, None) => return Err(ToolFail::invalid("give `title` or `path`")),
                (Some(title), None) => layout::page_path_for_title(title, p.folder.as_deref())?,
                (None, Some(path)) => page_path(path)?,
            };
            trace.targets.push(path.as_str().to_string());
            let entry = eng.create_page(&path, &p.markdown)?;
            trace.after = Some(entry.revision.0.clone());
            Ok(json!({"path": path.as_str(), "revision": entry.revision.0}))
        })
        .await
    }

    /// Append blocks to a page.
    #[tool(
        description = "Append Markdown after the last block of a page (one blank line between). Optionally pass the revision from get_page; a stale revision returns a conflict error and writes nothing.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn append_blocks(
        &self,
        Parameters(p): Parameters<AppendBlocksParams>,
    ) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("append_blocks", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            let (text, rev) = eng.read_page(&path)?;
            trace.before = Some(rev.0.clone());
            check_revision(p.revision.as_deref(), &rev.0, &path)?;
            let mut doc = Document::parse(&text);
            let body = append_blocks(doc.body(), &p.markdown);
            doc.set_body(&body);
            let entry = eng.save_page(&path, &doc.to_text(), Some(rev))?;
            trace.after = Some(entry.revision.0.clone());
            Ok(json!({"path": path.as_str(), "revision": entry.revision.0}))
        })
        .await
    }

    /// Replace one block of a page.
    #[tool(
        description = "Replace one top-level block of a page, addressed by anchor (\"^intro\") or index (\"3\"; see list_blocks). Everything else in the page is left byte-identical. Optionally pass the revision; a stale revision returns a conflict error and writes nothing.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn update_block(
        &self,
        Parameters(p): Parameters<UpdateBlockParams>,
    ) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("update_block", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            let block = BlockRef::parse(&p.block)?;
            let (text, rev) = eng.read_page(&path)?;
            trace.before = Some(rev.0.clone());
            check_revision(p.revision.as_deref(), &rev.0, &path)?;
            let mut doc = Document::parse(&text);
            let body = replace_block(doc.body(), &block, &p.markdown)?;
            doc.set_body(&body);
            let entry = eng.save_page(&path, &doc.to_text(), Some(rev))?;
            trace.after = Some(entry.revision.0.clone());
            Ok(json!({"path": path.as_str(), "revision": entry.revision.0}))
        })
        .await
    }

    /// List tasks.
    #[tool(
        description = "List tasks. `filter` is a built-in view (inbox, today, upcoming, completed, all) or a filter expression such as `p1 & #Home`, `overdue | today`, `@errand & !done`, `next 7 days`. Default: open tasks.",
        annotations(read_only_hint = true)
    )]
    pub async fn list_tasks(&self, Parameters(p): Parameters<ListTasksParams>) -> CallToolResult {
        self.read(move |eng| {
            let today = Local::now().date_naive();
            let filter = builtin_or_filter(p.filter.as_deref().unwrap_or("open"), today)?;
            let tasks = eng.list_tasks(&filter, today)?;
            let total = tasks.len();
            let limit = p.limit.unwrap_or(200).clamp(1, 1000);
            Ok(json!({
                "total": total,
                "tasks": tasks.iter().take(limit).map(item_json).collect::<Vec<_>>(),
            }))
        })
        .await
    }

    /// Create a task.
    #[tool(
        description = "Create a task. Either `text` (one line of natural language: \"Pay rent every month p1 #Home\") or structured fields (title, priority, due, project, section, labels, reminders).",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn create_task(&self, Parameters(p): Parameters<CreateTaskParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("create_task", &args, move |eng, trace| {
            let (path, fields, warnings) = match (&p.text, &p.title) {
                (Some(_), Some(_)) => {
                    return Err(ToolFail::invalid("give either `text` or `title`, not both"));
                }
                (None, None) => return Err(ToolFail::invalid("give `text` or `title`")),
                (Some(text), None) => {
                    let (path, warnings) = eng.quick_add(text, now())?;
                    let fields = eng.task(&path)?;
                    (path, fields, warnings)
                }
                (None, Some(title)) => {
                    let mut fields = TaskFields::new(title.trim());
                    if let Some(s) = &p.priority {
                        fields.priority = parse_priority(s)?;
                    }
                    if let Some(s) = &p.due {
                        fields.due = Some(parse_due(s)?);
                    }
                    fields.project = p.project.clone().filter(|s| !s.trim().is_empty());
                    fields.section = p.section.clone().filter(|s| !s.trim().is_empty());
                    fields.labels = p.labels.clone().unwrap_or_default();
                    if let Some(r) = &p.reminders {
                        fields.reminders = parse_reminders(r)?;
                    }
                    let path = eng.create_task(&fields, "")?;
                    (path, fields, Vec::new())
                }
            };
            trace.targets.push(path.as_str().to_string());
            trace.after = rev_of(eng, &path);
            Ok(json!({
                "path": path.as_str(),
                "task": task_json(path.as_str(), &fields),
                "warnings": warnings,
            }))
        })
        .await
    }

    /// Update task fields.
    #[tool(
        description = "Change fields of a task (title, priority, due, project, section, labels). Omitted fields are unchanged; `null` clears due, project or section.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn update_task(&self, Parameters(p): Parameters<UpdateTaskParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("update_task", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            let priority = p.priority.as_deref().map(parse_priority).transpose()?;
            let due = match &p.due {
                None => None,
                Some(None) => Some(None),
                Some(Some(s)) => Some(Some(parse_due(s)?)),
            };
            if let Some(t) = &p.title
                && t.trim().is_empty()
            {
                return Err(ToolFail::invalid("title must not be empty"));
            }
            trace.before = rev_of(eng, &path);
            let fields = eng.update_task(&path, |f| {
                if let Some(t) = &p.title {
                    f.title = t.trim().to_string();
                }
                if let Some(pr) = priority {
                    f.priority = pr;
                }
                if let Some(d) = due {
                    f.due = d;
                }
                if let Some(pj) = &p.project {
                    f.project = pj.clone().filter(|s| !s.trim().is_empty());
                }
                if let Some(s) = &p.section {
                    f.section = s.clone().filter(|s| !s.trim().is_empty());
                }
                if let Some(l) = &p.labels {
                    f.labels = l.clone();
                }
            })?;
            trace.after = rev_of(eng, &path);
            Ok(json!({"path": path.as_str(), "task": task_json(path.as_str(), &fields)}))
        })
        .await
    }

    /// Complete a task.
    #[tool(
        description = "Mark a task done. A recurring task advances to its next due date instead and stays open.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn complete_task(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("complete_task", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            trace.before = rev_of(eng, &path);
            let fields = eng.complete_task(&path, now())?;
            trace.after = rev_of(eng, &path);
            Ok(json!({"path": path.as_str(), "task": task_json(path.as_str(), &fields)}))
        })
        .await
    }

    /// Reopen a task.
    #[tool(
        description = "Reopen a completed task.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn reopen_task(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("reopen_task", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            trace.before = rev_of(eng, &path);
            let fields = eng.reopen_task(&path)?;
            trace.after = rev_of(eng, &path);
            Ok(json!({"path": path.as_str(), "task": task_json(path.as_str(), &fields)}))
        })
        .await
    }

    /// Archive a task.
    #[tool(
        description = "Archive a task: move it to the vault trash (it is not deleted and can be restored by the user).",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn archive_task(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("archive_task", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            trace.before = rev_of(eng, &path);
            let trashed = eng.archive_task(&path)?;
            Ok(json!({"path": path.as_str(), "archived_to": trashed.as_str()}))
        })
        .await
    }

    /// Replace a task's reminders.
    #[tool(
        description = "Set a task's reminders, replacing the whole list. Each reminder is an absolute local time `YYYY-MM-DDTHH:MM` or a lead time before the due moment (`-15m`, `-1h`, `-2d`). An empty list clears them.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn set_reminder(
        &self,
        Parameters(p): Parameters<SetReminderParams>,
    ) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("set_reminder", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            let reminders = parse_reminders(&p.reminders)?;
            trace.before = rev_of(eng, &path);
            let fields = eng.update_task(&path, |f| f.reminders = reminders)?;
            trace.after = rev_of(eng, &path);
            Ok(json!({"path": path.as_str(), "task": task_json(path.as_str(), &fields)}))
        })
        .await
    }

    /// The agenda.
    #[tool(
        description = "Open tasks per day for the next N days starting today (default 7), plus overdue tasks. Recurring tasks appear on each occurrence.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_agenda(&self, Parameters(p): Parameters<AgendaParams>) -> CallToolResult {
        self.read(move |eng| {
            let today = Local::now().date_naive();
            let agenda = eng.agenda(today, p.days.unwrap_or(7))?;
            Ok(json!({
                "from": today.to_string(),
                "overdue": agenda.overdue.iter().map(item_json).collect::<Vec<_>>(),
                "days": agenda.days.iter().map(|d| json!({
                    "date": d.date.to_string(),
                    "tasks": d.tasks.iter().map(item_json).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }))
        })
        .await
    }

    /// Query a database.
    #[tool(
        description = "Query a database. Table database: give `folder` and optionally `view` (a saved view's name) or `query` (ad-hoc {filter, sorts, group_by}). Returns the matching rows with every property value (formulas and rollups computed), sorted, and `groups` when the query groups. Tasks database, or no folder for all tasks: `filter` with the list_tasks syntax (default `open`).",
        annotations(read_only_hint = true)
    )]
    pub async fn query_database(
        &self,
        Parameters(p): Parameters<QueryDatabaseParams>,
    ) -> CallToolResult {
        self.read(move |eng| {
            let folder = p.folder.as_deref().map(folder_name);
            let at = now();
            let adhoc: Option<ViewQuery> = match &p.query {
                None => None,
                Some(q) => Some(
                    serde_json::from_value(q.clone())
                        .map_err(|e| ToolFail::invalid(format!("query: {e}")))?,
                ),
            };
            let kind = match &folder {
                None => None,
                Some(f) => match eng.database_schema(f) {
                    Ok(schema) => Some(schema.kind),
                    Err(EngineError::Storage(StorageError::NotFound(_))) => {
                        return Err(ToolFail::invalid(format!(
                            "`{f}` is not a database (no {SCHEMA_FILE} in that folder)"
                        )));
                    }
                    Err(e) => return Err(e.into()),
                },
            };
            if let (Some(DatabaseKind::Table), Some(f)) = (kind, folder.as_deref()) {
                if p.filter.is_some() {
                    return Err(ToolFail::invalid(
                        "a table database is queried with `view` and `query`, not `filter`",
                    ));
                }
                let result = eng.query_database_view(f, p.view.as_deref(), adhoc, at)?;
                return to_json(&result);
            }
            if p.view.is_some() || adhoc.is_some() {
                return Err(ToolFail::invalid(
                    "`view` and `query` apply to table databases; give the `folder` of one",
                ));
            }
            if let Some(f) = folder.as_deref()
                && !eng.task_databases().iter().any(|d| d == f)
            {
                return Err(ToolFail::invalid(format!("`{f}` is not a Tasks database")));
            }
            let today = at.date();
            let filter = p.filter.as_deref().unwrap_or("open");
            let tasks: Vec<TaskItem> = eng
                .query_tasks(filter, today)?
                .into_iter()
                .filter(|t| {
                    folder
                        .as_deref()
                        .is_none_or(|f| t.path.parent().unwrap_or("") == f)
                })
                .collect();
            Ok(json!({
                "total": tasks.len(),
                "rows": tasks.iter().map(item_json).collect::<Vec<_>>(),
            }))
        })
        .await
    }

    /// List databases.
    #[tool(
        description = "List the databases in the vault: table databases and Tasks databases, each with its folder, kind and row count.",
        annotations(read_only_hint = true)
    )]
    pub async fn list_databases(&self) -> CallToolResult {
        self.read(|eng| {
            let databases = eng.databases()?;
            Ok(json!({ "databases": to_json(&databases)? }))
        })
        .await
    }

    /// Get a database schema.
    #[tool(
        description = "Get a database's schema: kind, properties (typed columns, in order), views and the schema revision to pass to update_database_schema.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_database_schema(
        &self,
        Parameters(p): Parameters<FolderParams>,
    ) -> CallToolResult {
        self.read(move |eng| {
            let folder = folder_name(&p.folder);
            let schema = eng.database_schema(&folder)?;
            let path = VaultPath::new(&schema_target(&folder))?;
            let (kind, properties, views) = match schema.kind {
                DatabaseKind::Table => (
                    "table",
                    to_json(&schema.properties)?,
                    to_json(&schema.table_views)?,
                ),
                DatabaseKind::Tasks => ("tasks", json!([]), to_json(&schema.views)?),
            };
            Ok(json!({
                "folder": folder,
                "kind": kind,
                "revision": rev_of(eng, &path),
                "properties": properties,
                "views": views,
            }))
        })
        .await
    }

    /// Create a table database.
    #[tool(
        description = "Create a table database (Workspace mode) in a new folder: a `Name` title column, the given properties and a default Table view. Fails if the folder is already a database. Validated; nothing is written when the definition is invalid.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn create_database(
        &self,
        Parameters(p): Parameters<CreateDatabaseParams>,
    ) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("create_database", &args, move |eng, trace| {
            let folder = folder_name(&p.folder);
            let target = schema_target(&folder);
            trace.targets.push(target.clone());
            let properties: Vec<PropertyDef> = parse_items("property", &p.properties)?;
            eng.create_database(&folder, properties)?;
            let path = VaultPath::new(&target)?;
            let revision = rev_of(eng, &path);
            trace.after = revision.clone();
            Ok(json!({"folder": folder, "path": target, "revision": revision}))
        })
        .await
    }

    /// Update a table database schema.
    #[tool(
        description = "Replace a table database's properties and/or views. Given lists replace the current ones; omitted parts are kept. The result is validated (duplicate or blank names, formula errors and cycles, rollups without a relation, views naming unknown properties); an invalid schema is a tool error and nothing is written. Pass the revision from get_database_schema to detect concurrent edits.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn update_database_schema(
        &self,
        Parameters(p): Parameters<UpdateDatabaseSchemaParams>,
    ) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("update_database_schema", &args, move |eng, trace| {
            let folder = folder_name(&p.folder);
            let target = schema_target(&folder);
            trace.targets.push(target.clone());
            let path = VaultPath::new(&target)?;
            let mut schema = eng.database_schema(&folder)?;
            if schema.kind != DatabaseKind::Table {
                return Err(ToolFail::invalid(format!(
                    "`{folder}` is a Tasks database; only table databases have properties and views"
                )));
            }
            trace.before = rev_of(eng, &path);
            if let Some(items) = &p.properties {
                schema.properties = parse_items("property", items)?;
            }
            if let Some(items) = &p.views {
                schema.table_views = parse_items::<ViewDef>("view", items)?;
            }
            eng.save_database_schema(&folder, &schema, p.revision.clone().map(Revision))?;
            let revision = rev_of(eng, &path);
            trace.after = revision.clone();
            Ok(json!({
                "folder": folder,
                "path": target,
                "revision": revision,
                "properties": to_json(&schema.properties)?,
                "views": to_json(&schema.table_views)?,
            }))
        })
        .await
    }

    /// Create a row.
    #[tool(
        description = "Add a row to a table database: a new Markdown file whose front matter holds the given property values. Fails if the title is empty. The row's path is returned; to delete a row use archive_page.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn create_row(&self, Parameters(p): Parameters<CreateRowParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("create_row", &args, move |eng, trace| {
            let folder = folder_name(&p.folder);
            let path = eng.create_row(&folder, &p.title, &p.values, now())?;
            trace.targets.push(path.as_str().to_string());
            let revision = rev_of(eng, &path);
            trace.after = revision.clone();
            Ok(json!({"path": path.as_str(), "revision": revision}))
        })
        .await
    }

    /// Set a row property.
    #[tool(
        description = "Set one property of a row (table database). Only the front matter changes; the body is untouched. `value: null` clears the property. Computed properties cannot be set.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn set_row_property(
        &self,
        Parameters(p): Parameters<SetRowPropertyParams>,
    ) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("set_row_property", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            trace.before = rev_of(eng, &path);
            eng.set_row_property(&path, &p.property, &p.value)?;
            let revision = rev_of(eng, &path);
            trace.after = revision.clone();
            Ok(json!({"path": path.as_str(), "property": p.property, "revision": revision}))
        })
        .await
    }

    /// Daily note.
    #[tool(
        description = "Get the daily note for a date (default today), creating it if it does not exist. Returns its path, text and revision.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn daily_note(&self, Parameters(p): Parameters<DailyNoteParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        let date = match p.date.as_deref() {
            None => Local::now().date_naive(),
            Some(s) => match NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
                Ok(d) => d,
                Err(_) => return err_result(format!("invalid date `{s}` (use YYYY-MM-DD)")),
            },
        };
        // Creating the note is a write (audited); opening an existing one is a read.
        let exists = self
            .blocking(move |eng, _| {
                layout::daily_note_path(eng.config(), date)
                    .ok()
                    .map(|path| eng.storage().stat(&path).ok().flatten().is_some())
            })
            .await
            .ok()
            .flatten();
        let work = move |eng: &mut Engine, trace: Option<&mut Trace>| -> ToolResult<Value> {
            let path = eng.daily_note(date)?;
            let (text, rev) = eng.read_page(&path)?;
            if let Some(t) = trace {
                t.targets.push(path.as_str().to_string());
                t.after = Some(rev.0.clone());
            }
            Ok(json!({
                "path": path.as_str(),
                "date": date.to_string(),
                "text": text,
                "revision": rev.0,
            }))
        };
        if exists == Some(true) {
            self.read(move |eng| work(eng, None)).await
        } else {
            self.write("daily_note", &args, move |eng, trace| {
                work(eng, Some(trace))
            })
            .await
        }
    }

    /// Tags.
    #[tool(
        description = "List all tags in the vault with the number of pages carrying each.",
        annotations(read_only_hint = true)
    )]
    pub async fn list_tags(&self) -> CallToolResult {
        self.read(|eng| {
            let tags = eng.tags()?;
            Ok(json!({
                "tags": tags.iter().map(|t| json!({"tag": t.tag, "count": t.count})).collect::<Vec<_>>()
            }))
        })
        .await
    }

    /// File tree.
    #[tool(
        description = "The vault's visible folder and file tree.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_tree(&self) -> CallToolResult {
        self.read(|eng| {
            let tree = eng.tree()?;
            serde_json::to_value(tree)
                .map_err(|e| ToolFail::invalid(e.to_string()))
                .map(|t| json!({"tree": t}))
        })
        .await
    }

    /// Archive a page.
    #[tool(
        description = "Archive a page: move it to the vault trash (it is not deleted and can be restored by the user). This server has no delete tool.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub async fn archive_page(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        let args = serde_json::to_value(&p).unwrap_or(Value::Null);
        self.write("archive_page", &args, move |eng, trace| {
            let path = page_path(&p.path)?;
            trace.targets.push(path.as_str().to_string());
            trace.before = rev_of(eng, &path);
            let trashed = eng.trash_page(&path)?;
            Ok(json!({"path": path.as_str(), "archived_to": trashed.as_str()}))
        })
        .await
    }
}

// ---------------------------------------------------------------------------------------------
// Resources and prompts
// ---------------------------------------------------------------------------------------------

const DAILY_REVIEW_PROMPT: &str = "\
Please run a short daily review of my vault.\n\
1. Call `get_agenda` (days: 1) to see today's tasks and anything overdue.\n\
2. Call `daily_note` to open today's note and read what I wrote so far.\n\
3. Summarise: what is overdue, what is due today (highest priority first), and anything in the \
note that looks like an unplanned commitment.\n\
4. Suggest a realistic order for the day and name tasks that should move to another date.\n\
Do not change anything until I agree; if I do, use `update_task` or `append_blocks`.";

const ANSWER_WITH_CITATIONS_PROMPT: &str = "\
Answer my question using only what my vault says.\n\
1. Call `ask_vault` with my question (rephrase it or call it again with different wording if the \
first passages do not cover it; use `get_page` to read more around a promising passage).\n\
2. Write the answer from those passages only. Cite every claim as [path#Lstart-Lend] using the \
citation's path, start_line and end_line, for example [Travel/Berlin Trip.md#L11-L11].\n\
3. If the passages do not contain the answer, say so plainly instead of guessing.\n\
Do not change anything in the vault.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for VaultMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new(APP_NAME, env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let pages = self
            .blocking(|eng, _| eng.index().pages())
            .await
            .map_err(|f| ErrorData::internal_error(f.message, None))?
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let resources = pages
            .iter()
            .map(|p| {
                Resource::new(uri_for(&p.path), p.path.as_str())
                    .with_title(&p.title)
                    .with_mime_type("text/markdown")
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let path = path_from_uri(&uri).ok_or_else(|| {
            ErrorData::invalid_params(format!("not a vault resource URI: {uri}"), None)
        })?;
        let read = self
            .blocking(move |eng, _| eng.read_page(&path))
            .await
            .map_err(|f| ErrorData::internal_error(f.message, None))?;
        match read {
            Ok((text, _)) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("text/markdown"),
            ])
            .into()),
            Err(e) => Err(ErrorData::resource_not_found(e.to_string(), None)),
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![
            Prompt::new(
                "daily_review",
                Some("Review today's agenda and daily note and propose a plan for the day."),
                None,
            ),
            Prompt::new(
                "answer_with_citations",
                Some(
                    "Answer a question from the vault only, citing passages as [path#Lstart-Lend].",
                ),
                None,
            ),
        ]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        match request.name.as_str() {
            "daily_review" => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                Role::User,
                DAILY_REVIEW_PROMPT,
            )])
            .with_description("Daily review of agenda and daily note")
            .into()),
            "answer_with_citations" => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                Role::User,
                ANSWER_WITH_CITATIONS_PROMPT,
            )])
            .with_description("Answer from the vault with citations")
            .into()),
            other => Err(ErrorData::invalid_params(
                format!("unknown prompt `{other}`"),
                None,
            )),
        }
    }
}

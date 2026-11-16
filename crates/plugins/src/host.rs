//! The plugin host: the only door between plugin code and the vault.
//!
//! A plugin runs in a sandboxed iframe with no access to Tauri IPC; the UI forwards its
//! postMessage requests to [`PluginHost::call`], which re-checks everything in Rust: the plugin
//! must exist and be enabled with unchanged files, the method must be in [`METHODS`] (there is no
//! delete, rename, settings or network method), the manifest must grant the method's
//! permission, and the call must be within the rate limit. Writes are audit-logged exactly like
//! MCP writes, with transport `plugin:<id>`.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::Local;
use selenite_core::VaultPath;
use selenite_index::audit::{AuditEntry, AuditLog, args_digest};
use selenite_index::{Engine, EngineError, HIGHLIGHT_END, HIGHLIGHT_START};
use selenite_storage::StorageError;
use selenite_vault::{Document, append_blocks, layout};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::PluginError;
use crate::manifest::Permission;
use crate::store::load_enabled;

/// Largest Markdown payload accepted from a plugin.
pub const MAX_MARKDOWN_BYTES: usize = 256 * 1024;

/// One RPC method of the plugin API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    /// Method name as sent by the plugin.
    pub name: &'static str,
    /// Permission the manifest must grant; `None` means any enabled plugin.
    pub permission: Option<Permission>,
    /// Whether the method changes the vault (audit-logged, stricter rate limit).
    pub writes: bool,
}

/// Every method of API v1. This table is the whole attack surface: anything else is refused.
pub const METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "vault.search",
        permission: Some(Permission::Read),
        writes: false,
    },
    MethodSpec {
        name: "vault.read",
        permission: Some(Permission::Read),
        writes: false,
    },
    MethodSpec {
        name: "vault.backlinks",
        permission: Some(Permission::Read),
        writes: false,
    },
    MethodSpec {
        name: "vault.createPage",
        permission: Some(Permission::Write),
        writes: true,
    },
    MethodSpec {
        name: "vault.appendBlocks",
        permission: Some(Permission::Write),
        writes: true,
    },
    MethodSpec {
        name: "ui.registerCommand",
        permission: Some(Permission::Commands),
        writes: false,
    },
    MethodSpec {
        name: "ui.notify",
        permission: None,
        writes: false,
    },
];

/// Rate limits per plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Calls of any kind allowed per window.
    pub calls: usize,
    /// Write calls allowed per window.
    pub writes: usize,
    /// Window length.
    pub window: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            calls: 120,
            writes: 20,
            window: Duration::from_secs(60),
        }
    }
}

#[derive(Default)]
struct Window {
    calls: VecDeque<Instant>,
    writes: VecDeque<Instant>,
}

fn prune(q: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while q
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= window)
    {
        q.pop_front();
    }
}

/// Serves plugin calls. Holds only rate-limit counters; plugin state lives in the vault.
pub struct PluginHost {
    limits: Limits,
    windows: HashMap<String, Window>,
    audit: AuditLog,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHost")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct Trace {
    targets: Vec<String>,
    before: Option<String>,
    after: Option<String>,
}

fn category(e: &PluginError) -> &'static str {
    match e {
        PluginError::PermissionDenied { .. } => "permission_denied",
        PluginError::RateLimited(_) => "rate_limited",
        PluginError::InvalidArgs(_) => "invalid_arguments",
        PluginError::Engine { category, .. } => category,
        _ => "plugin_error",
    }
}

fn engine_err(e: EngineError) -> PluginError {
    let category = match &e {
        EngineError::Storage(StorageError::Conflict { .. }) => "conflict",
        EngineError::Storage(StorageError::NotFound(_)) => "not_found",
        EngineError::Storage(StorageError::AlreadyExists(_)) => "already_exists",
        EngineError::Path(_) | EngineError::NotAPage(_) | EngineError::NotUtf8(_) => {
            "invalid_arguments"
        }
        _ => "engine_error",
    };
    PluginError::Engine {
        category,
        message: e.to_string(),
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, PluginError> {
    serde_json::from_value(args.clone()).map_err(|e| PluginError::InvalidArgs(e.to_string()))
}

fn invalid(msg: &str) -> PluginError {
    PluginError::InvalidArgs(msg.to_string())
}

fn page_path(raw: &str) -> Result<VaultPath, PluginError> {
    let raw = raw.trim();
    let make = |s: &str| VaultPath::new(s).map_err(|e| invalid(&format!("invalid path: {e}")));
    let p = make(raw)?;
    if p.extension().is_none() {
        make(&format!("{raw}.md"))
    } else {
        Ok(p)
    }
}

fn check_markdown(s: &str) -> Result<(), PluginError> {
    if s.len() > MAX_MARKDOWN_BYTES {
        return Err(invalid("markdown is larger than 256 KiB"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    q: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePageArgs {
    title: String,
    markdown: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendArgs {
    path: String,
    markdown: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    id: String,
    title: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyArgs {
    text: String,
}

impl PluginHost {
    /// A host with the default [`Limits`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    /// A host with custom limits (tests).
    #[must_use]
    pub fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            windows: HashMap::new(),
            audit: AuditLog::new(),
        }
    }

    /// Serve one plugin call.
    ///
    /// `plugin_id` must come from the host side (the UI maps each iframe to its plugin); a
    /// plugin never names itself. Returns the JSON result for the plugin.
    ///
    /// # Errors
    ///
    /// [`PluginError::Disabled`] / [`PluginError::Changed`] / [`PluginError::NotFound`] for a
    /// plugin that may not run, [`PluginError::UnknownMethod`] for a method outside [`METHODS`],
    /// [`PluginError::PermissionDenied`], [`PluginError::RateLimited`],
    /// [`PluginError::InvalidArgs`], or an engine failure. Errors never contain page text.
    pub fn call(
        &mut self,
        engine: &mut Engine,
        plugin_id: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, PluginError> {
        let plugin = load_enabled(engine, plugin_id)?;
        let spec = METHODS
            .iter()
            .find(|m| m.name == method)
            .ok_or_else(|| PluginError::UnknownMethod(method.to_string()))?;
        let mut trace = Trace::default();

        let gate = self.gate(plugin_id, spec, &plugin.manifest.permissions);
        let result = match gate {
            Err(e) => Err(e),
            Ok(()) => self.dispatch(engine, spec, args, &mut trace),
        };
        if spec.writes {
            let entry = AuditEntry {
                ts: Local::now().to_rfc3339(),
                tool: spec.name.to_string(),
                targets: trace.targets,
                ok: result.is_ok(),
                error: result.as_ref().err().map(|e| category(e).to_string()),
                revision_before: trace.before,
                revision_after: trace.after,
                args_digest: args_digest(args),
                transport: Some(format!("plugin:{plugin_id}")),
            };
            if let Err(e) = self.audit.record(engine, &entry) {
                tracing::error!(plugin = plugin_id, error = %e, "plugin audit log write failed");
            }
        }
        result
    }

    /// Permission and rate-limit checks.
    fn gate(
        &mut self,
        plugin_id: &str,
        spec: &MethodSpec,
        granted: &[Permission],
    ) -> Result<(), PluginError> {
        if let Some(needed) = spec.permission
            && !granted.contains(&needed)
        {
            return Err(PluginError::PermissionDenied {
                plugin: plugin_id.to_string(),
                method: spec.name.to_string(),
                needed: needed.as_str(),
            });
        }
        let now = Instant::now();
        let limits = self.limits;
        let w = self.windows.entry(plugin_id.to_string()).or_default();
        prune(&mut w.calls, now, limits.window);
        prune(&mut w.writes, now, limits.window);
        if w.calls.len() >= limits.calls || (spec.writes && w.writes.len() >= limits.writes) {
            return Err(PluginError::RateLimited(plugin_id.to_string()));
        }
        w.calls.push_back(now);
        if spec.writes {
            w.writes.push_back(now);
        }
        Ok(())
    }

    fn dispatch(
        &self,
        engine: &mut Engine,
        spec: &MethodSpec,
        args: &Value,
        trace: &mut Trace,
    ) -> Result<Value, PluginError> {
        match spec.name {
            "vault.search" => {
                let a: SearchArgs = parse_args(args)?;
                if a.q.len() > 500 {
                    return Err(invalid("query is longer than 500 bytes"));
                }
                let hits = engine
                    .search(&a.q, a.limit.unwrap_or(20).clamp(1, 50))
                    .map_err(engine_err)?;
                Ok(json!({ "results": hits.iter().map(|h| json!({
                    "path": h.path.as_str(),
                    "title": h.title,
                    "snippet": h.snippet.replace([HIGHLIGHT_START, HIGHLIGHT_END], ""),
                })).collect::<Vec<_>>() }))
            }
            "vault.read" => {
                let a: PathArgs = parse_args(args)?;
                let path = page_path(&a.path)?;
                let (text, rev) = engine.read_page(&path).map_err(engine_err)?;
                Ok(json!({ "path": path.as_str(), "text": text, "revision": rev.0 }))
            }
            "vault.backlinks" => {
                let a: PathArgs = parse_args(args)?;
                let path = page_path(&a.path)?;
                let links = engine.backlinks(&path).map_err(engine_err)?;
                Ok(json!({ "backlinks": links.iter().map(|b| json!({
                    "source": b.source.as_str(),
                    "title": b.source_title,
                    "line": b.line,
                    "context": b.context,
                })).collect::<Vec<_>>() }))
            }
            "vault.createPage" => {
                let a: CreatePageArgs = parse_args(args)?;
                check_markdown(&a.markdown)?;
                if a.title.trim().is_empty() || a.title.chars().count() > 200 {
                    return Err(invalid("title must be 1 to 200 characters"));
                }
                let path = layout::page_path_for_title(&a.title, None)
                    .map_err(|e| invalid(&format!("invalid title: {e}")))?;
                trace.targets.push(path.as_str().to_string());
                let entry = engine.create_page(&path, &a.markdown).map_err(engine_err)?;
                trace.after = Some(entry.revision.0.clone());
                Ok(json!({ "path": path.as_str(), "revision": entry.revision.0 }))
            }
            "vault.appendBlocks" => {
                let a: AppendArgs = parse_args(args)?;
                check_markdown(&a.markdown)?;
                let path = page_path(&a.path)?;
                trace.targets.push(path.as_str().to_string());
                let (text, rev) = engine.read_page(&path).map_err(engine_err)?;
                trace.before = Some(rev.0.clone());
                let mut doc = Document::parse(&text);
                let body = append_blocks(doc.body(), &a.markdown);
                doc.set_body(&body);
                let entry = engine
                    .save_page(&path, &doc.to_text(), Some(rev))
                    .map_err(engine_err)?;
                trace.after = Some(entry.revision.0.clone());
                Ok(json!({ "path": path.as_str(), "revision": entry.revision.0 }))
            }
            "ui.registerCommand" => {
                let a: CommandArgs = parse_args(args)?;
                let id_ok = !a.id.is_empty()
                    && a.id.len() <= 64
                    && a.id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
                if !id_ok {
                    return Err(invalid("command id must be 1 to 64 letters, digits, - _ ."));
                }
                if a.title.trim().is_empty() || a.title.chars().count() > 80 {
                    return Err(invalid("command title must be 1 to 80 characters"));
                }
                Ok(json!({ "id": a.id, "title": a.title.trim() }))
            }
            "ui.notify" => {
                let a: NotifyArgs = parse_args(args)?;
                if a.text.chars().count() > 500 {
                    return Err(invalid("notification is longer than 500 characters"));
                }
                Ok(json!({ "text": a.text }))
            }
            other => Err(PluginError::UnknownMethod(other.to_string())),
        }
    }
}

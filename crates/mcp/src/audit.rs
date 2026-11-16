//! Append-only audit log of MCP writes.
//!
//! Every write tool call appends one JSON line to `.selenite/audit/mcp-YYYY-MM.jsonl` inside the
//! vault. The log goes through the engine's [`StorageProvider`](selenite_storage::StorageProvider)
//! (not a raw file handle) so it works for any provider. A write is "read the month file, append
//! a line, write it back with `WriteMode::Replace(revision)`" (or `CreateNew` for the first line
//! of a month). That is atomic per call and never clobbers a concurrent change: on a conflict
//! the append is retried against the fresh file. Month files stay small (one short line per
//! write), so rewriting them is cheap.
//!
//! The log never contains page or task content or titles: only the tool name, target paths
//! (the vault's file names), success, a coarse error category, revisions and a digest of the
//! canonical arguments. Read-only tool calls are not logged.

use chrono::{DateTime, Datelike, Local};
use selenite_core::VaultPath;
use selenite_index::Engine;
use selenite_storage::{StorageError, WriteMode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Folder inside the vault that holds audit files.
pub const AUDIT_DIR: &str = ".selenite/audit";

/// One audit line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// RFC 3339 local timestamp.
    pub ts: String,
    /// Tool name, for example `append_blocks`.
    pub tool: String,
    /// Vault paths the call targeted (empty when unknown, for example a failed `create_page`).
    pub targets: Vec<String>,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Coarse error category (`conflict`, `not_found`, ...) when the call failed; never a message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Revision of the (first) target before the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_before: Option<String>,
    /// Revision of the (first) target after the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_after: Option<String>,
    /// blake3 hex digest of the canonical JSON of the call's arguments.
    pub args_digest: String,
}

/// The audit log. Stateless: each call writes into the vault of the engine it is given.
#[derive(Debug, Clone, Default)]
pub struct AuditLog;

/// Rewrite `value` with every object's keys sorted, so equal arguments digest equally.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Digest (blake3 hex) of the canonical JSON form of `args`.
pub fn args_digest(args: &Value) -> String {
    let text = serde_json::to_string(&canonical(args)).unwrap_or_default();
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn file_for(ts: &DateTime<Local>) -> Result<VaultPath, selenite_core::PathError> {
    VaultPath::new(&format!(
        "{AUDIT_DIR}/mcp-{:04}-{:02}.jsonl",
        ts.year(),
        ts.month()
    ))
}

impl AuditLog {
    /// A new audit log.
    pub fn new() -> Self {
        Self
    }

    /// Append `entry` to the current month's file in the engine's vault.
    pub fn record(&self, engine: &Engine, entry: &AuditEntry) -> Result<(), String> {
        let now = DateTime::parse_from_rfc3339(&entry.ts)
            .map(|d| d.with_timezone(&Local))
            .unwrap_or_else(|_| Local::now());
        let path = file_for(&now).map_err(|e| e.to_string())?;
        let mut line = serde_json::to_string(entry).map_err(|e| e.to_string())?;
        line.push('\n');
        let storage = engine.storage();
        for _ in 0..5 {
            let result = match storage.stat(&path).map_err(|e| e.to_string())? {
                None => storage.write(&path, line.as_bytes(), WriteMode::CreateNew),
                Some(existing) => {
                    let mut data = storage.read(&path).map_err(|e| e.to_string())?;
                    if !data.is_empty() && !data.ends_with(b"\n") {
                        data.push(b'\n');
                    }
                    data.extend_from_slice(line.as_bytes());
                    storage.write(&path, &data, WriteMode::Replace(existing.revision))
                }
            };
            match result {
                Ok(_) => return Ok(()),
                // Someone else touched the file between our read and write: try again.
                Err(StorageError::Conflict { .. } | StorageError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("audit log is busy; entry not written".into())
    }

    /// The newest `limit` entries across all month files, oldest first.
    pub fn entries(&self, engine: &Engine, limit: usize) -> Result<Vec<AuditEntry>, String> {
        let storage = engine.storage();
        let mut files: Vec<VaultPath> = storage
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|e| e.path)
            .filter(|p| p.as_str().starts_with(AUDIT_DIR) && p.as_str().ends_with(".jsonl"))
            .collect();
        files.sort();
        let mut all = Vec::new();
        for file in files {
            let bytes = storage.read(&file).map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&bytes);
            all.extend(
                text.lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok()),
            );
        }
        let skip = all.len().saturating_sub(limit);
        Ok(all.split_off(skip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn digest_ignores_key_order() {
        let a = json!({"a": 1, "b": {"x": [1, 2], "y": null}});
        let b = json!({"b": {"y": null, "x": [1, 2]}, "a": 1});
        assert_eq!(args_digest(&a), args_digest(&b));
        assert_ne!(args_digest(&a), args_digest(&json!({"a": 2})));
        assert_eq!(args_digest(&a).len(), 64);
    }
}

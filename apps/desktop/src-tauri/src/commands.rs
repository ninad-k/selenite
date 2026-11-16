//! Tauri-free command layer. Every function takes `&AppState` so it can be unit-tested; the
//! `#[tauri::command]` wrappers in `lib.rs` only map errors to strings.
//!
//! Errors never carry page contents.

use std::path::{Path, PathBuf};

use selenite_core::{PathError, VaultPath};
use selenite_index::{Engine, EngineError, TreeNode};
use selenite_storage::{Revision, StorageError};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

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
    })
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
}

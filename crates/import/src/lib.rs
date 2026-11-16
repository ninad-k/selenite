//! Importers that bring notes and tasks from other apps into a vault.
//!
//! Three sources are supported: an Obsidian vault folder, a Notion "Markdown & CSV" export
//! and Todoist CSV exports. Each importer writes only through the [`Engine`] and its storage
//! provider (atomic writes), never overwrites an existing file, and treats everything it reads
//! as data. Content is never logged; only paths and counts.

mod notion;
mod obsidian;
mod sink;
mod todoist;

use std::io;

use chrono::NaiveDateTime;
use selenite_core::PathError;
use selenite_index::EngineError;
use selenite_storage::StorageError;
use selenite_vault::VaultError;

pub use notion::import_notion;
pub use obsidian::import_obsidian;
pub use todoist::import_todoist;

/// Errors produced by importers.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// Reading the source failed.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// Path concerned.
        path: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The archive could not be read.
    #[error("invalid archive: {0}")]
    Zip(String),
    /// A CSV file could not be read.
    #[error("invalid CSV: {0}")]
    Csv(String),
    /// The engine failed.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// Storage failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A destination path was invalid.
    #[error("invalid path: {0}")]
    Path(#[from] PathError),
    /// A vault file could not be built.
    #[error(transparent)]
    Vault(#[from] VaultError),
    /// The source or options are unusable.
    #[error("{0}")]
    Invalid(String),
}

/// Result alias for importers.
pub type Result<T> = std::result::Result<T, ImportError>;

pub(crate) fn io_err(path: impl Into<String>, source: io::Error) -> ImportError {
    ImportError::Io {
        path: path.into(),
        source,
    }
}

/// Options shared by all importers.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Vault folder to import into; default `Imported/<Source> <YYYY-MM-DD>`.
    pub into: Option<String>,
    /// Report what would happen without writing anything.
    pub dry_run: bool,
    /// The current time (injected so results are reproducible).
    pub now: NaiveDateTime,
}

impl ImportOptions {
    /// Default options at `now`.
    pub fn new(now: NaiveDateTime) -> Self {
        Self {
            into: None,
            dry_run: false,
            now,
        }
    }

    pub(crate) fn target(&self, source: &str) -> String {
        match &self.into {
            Some(f) if !f.trim_matches('/').is_empty() => {
                f.trim().trim_matches(['/', '\\']).replace('\\', "/")
            }
            _ => format!("Imported/{source} {}", self.now.format("%Y-%m-%d")),
        }
    }
}

/// What an import did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ImportReport {
    /// Vault folder the content went into.
    pub target: String,
    /// True when nothing was written.
    pub dry_run: bool,
    /// Markdown pages written.
    pub pages: usize,
    /// Database rows written.
    pub rows: usize,
    /// Tasks written.
    pub tasks: usize,
    /// Non-Markdown files (images, PDFs, ...) written.
    pub attachments: usize,
    /// Items not imported: `(path, reason)`.
    pub skipped: Vec<(String, String)>,
}

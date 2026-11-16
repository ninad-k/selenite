//! The [`Engine`]: one code path for every vault operation, shared by CLI, desktop and MCP.
//!
//! It owns a [`StorageProvider`] (the source of truth) and an [`Index`] (a cache), and keeps the
//! index in step with every write it performs.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use selenite_core::{APP_DIR, APP_NAME, PathError, VaultPath};
use selenite_storage::{
    Entry, LocalFolder, Revision, StorageError, StorageProvider, WriteMode, content_hash,
};
use selenite_vault::config::CONFIG_PATH;
use selenite_vault::{Document, VaultConfig, VaultError, extract_links, layout};
use serde::Serialize;

use crate::db::{
    Backlink, Index, LinkRecord, OpenStatus, OutLink, PageRecord, SearchHit, TagCount,
};
use crate::{INDEX_FILE, IndexError};

/// Errors produced by engine operations.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Storage failed (includes write conflicts).
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The index failed.
    #[error(transparent)]
    Index(#[from] IndexError),
    /// A vault file could not be parsed or serialised.
    #[error(transparent)]
    Vault(#[from] VaultError),
    /// A path was invalid.
    #[error("invalid path: {0}")]
    Path(#[from] PathError),
    /// The path is not a Markdown page.
    #[error("not a page: {0}")]
    NotAPage(VaultPath),
    /// The file is not valid UTF-8 text.
    #[error("not UTF-8 text: {0}")]
    NotUtf8(VaultPath),
    /// The folder is already a vault (on init) or is not usable as one.
    #[error("{0}")]
    NotAVault(String),
}

/// Result alias for engine operations.
pub type Result<T> = std::result::Result<T, EngineError>;

/// Counts from an incremental index refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RefreshStats {
    /// Pages newly indexed.
    pub added: usize,
    /// Pages re-indexed because their content changed.
    pub updated: usize,
    /// Pages dropped because their file disappeared.
    pub removed: usize,
}

/// A node of the vault file tree (hidden entries excluded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeNode {
    /// Display name (file or folder name).
    pub name: String,
    /// Vault-relative path; empty for the root.
    pub path: String,
    /// True for folders.
    pub is_dir: bool,
    /// Children, folders first, then files, each sorted case-insensitively.
    pub children: Vec<TreeNode>,
}

/// The vault engine.
pub struct Engine {
    storage: Box<dyn StorageProvider>,
    index: Index,
    config: VaultConfig,
    root: Option<PathBuf>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("storage", &self.storage.kind())
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn config_path() -> Result<VaultPath> {
    Ok(VaultPath::new(CONFIG_PATH)?)
}

fn millis(e: &Entry) -> i64 {
    e.modified
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Build the index record for one page.
pub fn page_record(path: &VaultPath, text: &str, entry: &Entry) -> PageRecord {
    let doc = Document::parse(text);
    let body = doc.body();
    let lines: Vec<&str> = body.lines().collect();
    let links = extract_links(body)
        .into_iter()
        .map(|l| {
            let context = lines
                .get(l.line.saturating_sub(1) as usize)
                .map(|s| truncate(s.trim(), 240))
                .unwrap_or_default();
            LinkRecord {
                target: l.target,
                heading: l.heading,
                alias: l.alias,
                embed: l.embed,
                line: l.line,
                context,
            }
        })
        .collect();
    let properties = doc
        .front_matter()
        .iter()
        .filter_map(|(k, v)| serde_json::to_string(v).ok().map(|j| (k.to_string(), j)))
        .collect();
    PageRecord {
        path: path.clone(),
        title: doc.title().unwrap_or_else(|| path.stem().to_string()),
        hash: content_hash(text.as_bytes()),
        revision: entry.revision.0.clone(),
        size: entry.size,
        modified_ms: millis(entry),
        body: body.to_string(),
        aliases: doc.aliases(),
        tags: doc.tags(),
        links,
        properties,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

impl Engine {
    /// Create a new vault in `root` (created if missing) and open it.
    ///
    /// Writes `.selenite/config.yaml`, an `.obsidian/` marker so the folder opens in Obsidian,
    /// a welcome page and the daily-notes folder. Fails if the folder is already a vault.
    pub fn init_local(root: &Path) -> Result<Self> {
        let storage = LocalFolder::create(root)?;
        if storage.stat(&config_path()?)?.is_some() {
            return Err(EngineError::NotAVault(format!(
                "{} is already a {APP_NAME} vault",
                root.display()
            )));
        }
        let config = VaultConfig::default();
        storage.write(
            &config_path()?,
            config.to_yaml()?.as_bytes(),
            WriteMode::CreateNew,
        )?;
        for (p, content) in layout::obsidian_marker_files() {
            let p = VaultPath::new(p)?;
            if storage.stat(&p)?.is_none() {
                storage.write(&p, content.as_bytes(), WriteMode::CreateNew)?;
            }
        }
        let welcome = VaultPath::new(layout::WELCOME_PAGE)?;
        if storage.stat(&welcome)?.is_none() {
            storage.write(
                &welcome,
                layout::welcome_text(APP_NAME).as_bytes(),
                WriteMode::CreateNew,
            )?;
        }
        storage.create_dir(&VaultPath::new(&config.daily_folder)?)?;
        drop(storage);
        Self::open_local(root)
    }

    /// Open an existing folder as a vault. A folder without `.selenite/config.yaml` (for
    /// example an existing Markdown vault) gets a default config; nothing else is changed.
    /// The index is opened, rebuilt if missing or outdated, otherwise refreshed incrementally.
    pub fn open_local(root: &Path) -> Result<Self> {
        let storage = LocalFolder::open(root)?;
        let root_path = storage.root().to_path_buf();
        let index_path = root_path.join(APP_DIR).join(INDEX_FILE);
        let (index, status) = Index::open(&index_path)?;
        let mut engine = Self::with_parts(Box::new(storage), index)?;
        engine.root = Some(root_path);
        match status {
            OpenStatus::Created => {
                engine.rebuild_index()?;
            }
            OpenStatus::Ready => {
                engine.refresh()?;
            }
        }
        Ok(engine)
    }

    /// Build an engine from any provider and index. Loads (or creates) the vault config.
    /// The caller decides whether to [`rebuild_index`](Self::rebuild_index).
    pub fn with_parts(storage: Box<dyn StorageProvider>, index: Index) -> Result<Self> {
        let cp = config_path()?;
        let config = match storage.stat(&cp)? {
            Some(_) => {
                let bytes = storage.read(&cp)?;
                let text =
                    String::from_utf8(bytes).map_err(|_| EngineError::NotUtf8(cp.clone()))?;
                VaultConfig::from_yaml(&text)?
            }
            None => {
                let config = VaultConfig::default();
                storage.write(&cp, config.to_yaml()?.as_bytes(), WriteMode::CreateNew)?;
                config
            }
        };
        Ok(Self {
            storage,
            index,
            config,
            root: None,
        })
    }

    /// The vault configuration.
    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// Replace and persist the vault configuration.
    pub fn set_config(&mut self, config: VaultConfig) -> Result<()> {
        self.storage.write(
            &config_path()?,
            config.to_yaml()?.as_bytes(),
            WriteMode::Overwrite,
        )?;
        self.config = config;
        Ok(())
    }

    /// Root folder for local vaults.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The underlying index (read-only queries).
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// The underlying storage provider.
    pub fn storage(&self) -> &dyn StorageProvider {
        self.storage.as_ref()
    }

    fn page_entries(&self) -> Result<Vec<Entry>> {
        Ok(self
            .storage
            .list()?
            .into_iter()
            .filter(|e| layout::is_page(&e.path))
            .collect())
    }

    fn read_text(&self, path: &VaultPath) -> Result<String> {
        let bytes = self.storage.read(path)?;
        String::from_utf8(bytes).map_err(|_| EngineError::NotUtf8(path.clone()))
    }

    /// Drop and rebuild the whole index from the files. Returns the number of pages indexed.
    pub fn rebuild_index(&mut self) -> Result<usize> {
        let entries = self.page_entries()?;
        let mut records = Vec::with_capacity(entries.len());
        for e in &entries {
            match self.read_text(&e.path) {
                Ok(text) => records.push(page_record(&e.path, &text, e)),
                Err(EngineError::NotUtf8(p)) => {
                    tracing::warn!(path = %p, "skipping non-UTF-8 page");
                }
                Err(err) => return Err(err),
            }
        }
        self.index.clear()?;
        let n = self.index.upsert_many(records.iter())?;
        tracing::info!(pages = n, "index rebuilt");
        Ok(n)
    }

    /// Bring the index up to date with files changed outside the engine.
    pub fn refresh(&mut self) -> Result<RefreshStats> {
        let mut states = self.index.page_states()?;
        let mut stats = RefreshStats::default();
        let mut records = Vec::new();
        for e in self.page_entries()? {
            match states.remove(&e.path) {
                Some((_, rev)) if rev == e.revision.0 => {}
                Some((hash, _)) => {
                    let Ok(text) = self.read_text(&e.path) else {
                        continue;
                    };
                    if content_hash(text.as_bytes()) == hash {
                        self.index
                            .touch_revision(&e.path, &e.revision.0, millis(&e))?;
                    } else {
                        records.push(page_record(&e.path, &text, &e));
                        stats.updated += 1;
                    }
                }
                None => {
                    let Ok(text) = self.read_text(&e.path) else {
                        continue;
                    };
                    records.push(page_record(&e.path, &text, &e));
                    stats.added += 1;
                }
            }
        }
        self.index.upsert_many(records.iter())?;
        for gone in states.keys() {
            self.index.remove(gone)?;
            stats.removed += 1;
        }
        Ok(stats)
    }

    fn require_page(path: &VaultPath) -> Result<()> {
        if layout::is_page(path) {
            Ok(())
        } else {
            Err(EngineError::NotAPage(path.clone()))
        }
    }

    /// Read a page's text and current revision.
    pub fn read_page(&self, path: &VaultPath) -> Result<(String, Revision)> {
        Self::require_page(path)?;
        let entry = self
            .storage
            .stat(path)?
            .ok_or_else(|| StorageError::NotFound(path.clone()))?;
        Ok((self.read_text(path)?, entry.revision))
    }

    fn reindex(&mut self, path: &VaultPath, text: &str, entry: &Entry) -> Result<()> {
        self.index.upsert(&page_record(path, text, entry))?;
        Ok(())
    }

    /// Create a new page; fails if the file exists.
    pub fn create_page(&mut self, path: &VaultPath, text: &str) -> Result<Entry> {
        Self::require_page(path)?;
        let entry = self
            .storage
            .write(path, text.as_bytes(), WriteMode::CreateNew)?;
        self.reindex(path, text, &entry)?;
        Ok(entry)
    }

    /// Create a page from a title (file name derived from it) in an optional folder, with
    /// `body` as content. Returns its path.
    pub fn create_titled_page(
        &mut self,
        title: &str,
        folder: Option<&str>,
        body: &str,
    ) -> Result<VaultPath> {
        let path = layout::page_path_for_title(title, folder)?;
        self.create_page(&path, body)?;
        Ok(path)
    }

    /// Save a page. `expected` is the revision the caller last read; the save fails with a
    /// conflict if the file changed since. `None` means the file must not exist yet.
    pub fn save_page(
        &mut self,
        path: &VaultPath,
        text: &str,
        expected: Option<Revision>,
    ) -> Result<Entry> {
        Self::require_page(path)?;
        let mode = match expected {
            Some(r) => WriteMode::Replace(r),
            None => WriteMode::CreateNew,
        };
        let entry = self.storage.write(path, text.as_bytes(), mode)?;
        self.reindex(path, text, &entry)?;
        Ok(entry)
    }

    /// Rename or move a page. Links pointing at the old name are not rewritten (yet).
    pub fn rename_page(&mut self, from: &VaultPath, to: &VaultPath) -> Result<Entry> {
        Self::require_page(from)?;
        Self::require_page(to)?;
        let entry = self.storage.rename(from, to)?;
        self.index.remove(from)?;
        let text = self.read_text(to)?;
        self.reindex(to, &text, &entry)?;
        Ok(entry)
    }

    /// Move a page to the vault trash (never a hard delete). Returns the trash path.
    pub fn trash_page(&mut self, path: &VaultPath) -> Result<VaultPath> {
        Self::require_page(path)?;
        let trashed = self.storage.trash(path)?;
        self.index.remove(path)?;
        Ok(trashed)
    }

    /// Create a folder.
    pub fn create_folder(&mut self, path: &VaultPath) -> Result<()> {
        self.storage.create_dir(path)?;
        Ok(())
    }

    /// Path of the daily note for `date`, creating the note if it does not exist.
    pub fn daily_note(&mut self, date: chrono::NaiveDate) -> Result<VaultPath> {
        let path = layout::daily_note_path(&self.config, date)?;
        if self.storage.stat(&path)?.is_none() {
            let text = format!("# {}\n\n", date.format("%A, %-d %B %Y"));
            self.create_page(&path, &text)?;
        }
        Ok(path)
    }

    /// Full-text search (see [`crate::SearchQuery`]).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        Ok(self.index.search(query, limit)?)
    }

    /// Pages linking to `path`.
    pub fn backlinks(&self, path: &VaultPath) -> Result<Vec<Backlink>> {
        Ok(self.index.backlinks(path)?)
    }

    /// Outgoing links of `path` with their resolution.
    pub fn outgoing(&self, path: &VaultPath) -> Result<Vec<OutLink>> {
        Ok(self.index.outgoing(path)?)
    }

    /// Resolve a link target to a page path.
    pub fn resolve(&self, target: &str) -> Result<Option<VaultPath>> {
        Ok(self.index.resolve(target)?)
    }

    /// All tags with counts.
    pub fn tags(&self) -> Result<Vec<TagCount>> {
        Ok(self.index.tags()?)
    }

    /// The visible file tree (hidden folders and files excluded; all non-hidden files shown).
    pub fn tree(&self) -> Result<TreeNode> {
        let mut root = TreeNode {
            name: String::new(),
            path: String::new(),
            is_dir: true,
            children: Vec::new(),
        };
        let dirs = self.storage.list_dirs()?;
        let files = self.storage.list()?;
        for d in dirs.iter().filter(|d| !d.is_hidden()) {
            insert(&mut root, d.as_str(), true);
        }
        for f in files.iter().filter(|f| !f.path.is_hidden()) {
            insert(&mut root, f.path.as_str(), false);
        }
        sort_tree(&mut root);
        Ok(root)
    }
}

fn insert(root: &mut TreeNode, path: &str, is_dir: bool) {
    let segs: Vec<&str> = path.split('/').collect();
    let mut node = root;
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        let child_path = segs[..=i].join("/");
        let pos = node.children.iter().position(|c| c.name == *seg);
        let idx = match pos {
            Some(p) => p,
            None => {
                node.children.push(TreeNode {
                    name: (*seg).to_string(),
                    path: child_path,
                    is_dir: !last || is_dir,
                    children: Vec::new(),
                });
                node.children.len() - 1
            }
        };
        node = &mut node.children[idx];
    }
}

fn sort_tree(node: &mut TreeNode) {
    node.children.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for c in &mut node.children {
        sort_tree(c);
    }
}

//! The SQLite-backed [`Index`].

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use selenite_core::VaultPath;
use selenite_vault::normalize_target;

use crate::passages::Passage;
use crate::query::SearchQuery;
use crate::schema::{PASSAGE_ROWID_STRIDE, SCHEMA};
use crate::{INDEX_VERSION, IndexError, Result};

/// Marker inserted before a highlighted match in [`SearchHit::snippet`].
pub const HIGHLIGHT_START: char = '\u{E000}';
/// Marker inserted after a highlighted match in [`SearchHit::snippet`].
pub const HIGHLIGHT_END: char = '\u{E001}';

/// Everything the index stores about one page. Built by the engine from a parsed file.
#[derive(Debug, Clone, PartialEq)]
pub struct PageRecord {
    /// Vault-relative path of the page.
    pub path: VaultPath,
    /// Display title (front matter title, first H1, or file stem).
    pub title: String,
    /// Content hash of the file bytes.
    pub hash: String,
    /// Storage revision the record was built from.
    pub revision: String,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time in milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// Body text for full-text search.
    pub body: String,
    /// Alternative names the page can be linked by.
    pub aliases: Vec<String>,
    /// Tags without the leading `#`.
    pub tags: Vec<String>,
    /// Outgoing links.
    pub links: Vec<LinkRecord>,
    /// Front matter properties as `(key, JSON value)`.
    pub properties: Vec<(String, String)>,
    /// Task data, if the page is a row of a Tasks database.
    pub task: Option<TaskRecord>,
    /// Citable passages of the body; line numbers are 1-based lines of the whole file.
    pub passages: Vec<Passage>,
}

/// One full-text match in the passage table, before re-ranking.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PassageCandidate {
    pub path: VaultPath,
    pub title: String,
    pub heading: Option<String>,
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
    /// FTS5 bm25 (negative; lower is better).
    pub bm25: f64,
}

/// Task columns stored for a task row. The full fields are kept as JSON so the index does not
/// depend on the task model's internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// Whether the task is completed.
    pub done: bool,
    /// Due date/time as `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM` (sortable).
    pub due: Option<String>,
    /// Project path, `None` for the Inbox.
    pub project: Option<String>,
    /// Full task fields as JSON.
    pub fields_json: String,
}

/// A task row read back from the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    /// Path of the task file.
    pub path: VaultPath,
    /// Full task fields as JSON.
    pub fields_json: String,
}

/// One outgoing link stored in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRecord {
    /// Raw link target as written (without heading or alias).
    pub target: String,
    /// Heading fragment, if any.
    pub heading: Option<String>,
    /// Display alias, if any.
    pub alias: Option<String>,
    /// True for embeds (`![[...]]`).
    pub embed: bool,
    /// 1-based line number in the body.
    pub line: u32,
    /// The line of text containing the link, for backlink context.
    pub context: String,
}

/// A page in a folder with its front matter properties as `(key, JSON value)`.
pub type FolderRow = (PageSummary, Vec<(String, String)>);

/// Short description of an indexed page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSummary {
    /// Vault-relative path.
    pub path: VaultPath,
    /// Display title.
    pub title: String,
    /// Last-modified time in milliseconds since the Unix epoch.
    pub modified_ms: i64,
}

/// A page linking to the page being inspected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlink {
    /// The linking page.
    pub source: VaultPath,
    /// Title of the linking page.
    pub source_title: String,
    /// 1-based line of the link in the source body.
    pub line: u32,
    /// The line of text containing the link.
    pub context: String,
    /// True if the link is an embed.
    pub embed: bool,
}

/// An outgoing link with its resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutLink {
    /// Raw target text.
    pub target: String,
    /// The page the link resolves to, if it exists.
    pub resolved: Option<VaultPath>,
    /// 1-based line in the body.
    pub line: u32,
    /// True if the link is an embed.
    pub embed: bool,
}

/// A link whose target page does not exist (yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedLink {
    /// Raw target text.
    pub target: String,
    /// Page containing the link.
    pub source: VaultPath,
}

/// A tag and how many pages carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCount {
    /// Tag as first written (original case).
    pub tag: String,
    /// Number of pages with the tag.
    pub count: u32,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Matching page.
    pub path: VaultPath,
    /// Page title.
    pub title: String,
    /// Excerpt with matches wrapped in [`HIGHLIGHT_START`] / [`HIGHLIGHT_END`].
    pub snippet: String,
    /// Relevance; lower is better (FTS5 bm25). Zero for filter-only queries.
    pub score: f64,
}

/// What happened when an on-disk index was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenStatus {
    /// The index existed with the current schema version.
    Ready,
    /// The index was created empty (new, outdated or unreadable); callers must repopulate it.
    Created,
}

/// The disposable SQLite cache.
pub struct Index {
    conn: Connection,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").finish_non_exhaustive()
    }
}

fn path_norm(p: &VaultPath) -> String {
    normalize_target(p.as_str())
}

fn leaf(norm: &str) -> &str {
    norm.rsplit('/').next().unwrap_or(norm)
}

fn to_path(s: String) -> rusqlite::Result<VaultPath> {
    VaultPath::new(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

impl Index {
    /// Open (or create) the index file at `path`. An outdated or unreadable file is deleted and
    /// recreated, since the index is only a cache; [`OpenStatus::Created`] tells the caller to
    /// repopulate it.
    pub fn open(path: &Path) -> Result<(Self, OpenStatus)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| IndexError::Io(e.to_string()))?;
        }
        if path.exists() {
            match Self::try_open_existing(path) {
                Ok(Some(index)) => return Ok((index, OpenStatus::Ready)),
                Ok(None) | Err(_) => {
                    tracing::info!("index outdated or unreadable; recreating");
                    Self::delete_files(path)?;
                }
            }
        }
        let conn = Connection::open(path)?;
        let index = Self::init(conn)?;
        Ok((index, OpenStatus::Created))
    }

    /// Create a fresh in-memory index (tests, benchmarks).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn try_open_existing(path: &Path) -> Result<Option<Self>> {
        let conn = Connection::open(path)?;
        let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != INDEX_VERSION {
            return Ok(None);
        }
        Self::configure(&conn)?;
        Ok(Some(Self { conn }))
    }

    fn delete_files(path: &Path) -> Result<()> {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let p = std::path::PathBuf::from(p);
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| IndexError::Io(e.to_string()))?;
            }
        }
        Ok(())
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        Ok(())
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL is not available for in-memory databases; ignore the reported mode.
        let _mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        Self::configure(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", INDEX_VERSION)?;
        Ok(Self { conn })
    }

    /// Remove every row (used by a full rebuild).
    pub fn clear(&mut self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM pages; DELETE FROM fts; DELETE FROM passages;")?;
        Ok(())
    }

    /// Insert or replace pages in one transaction.
    pub fn upsert_many<'a>(
        &mut self,
        records: impl IntoIterator<Item = &'a PageRecord>,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        for rec in records {
            upsert_in(&tx, rec)?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Insert or replace one page.
    pub fn upsert(&mut self, record: &PageRecord) -> Result<()> {
        self.upsert_many(std::iter::once(record)).map(|_| ())
    }

    /// Remove a page. Returns false if it was not indexed.
    pub fn remove(&mut self, path: &VaultPath) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let removed = remove_in(&tx, path)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Update only the stored revision of a page whose content hash did not change.
    pub fn touch_revision(
        &mut self,
        path: &VaultPath,
        revision: &str,
        modified_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE pages SET revision = ?2, modified_ms = ?3 WHERE path = ?1",
            params![path.as_str(), revision, modified_ms],
        )?;
        Ok(())
    }

    /// Map of indexed path to `(hash, revision)`, for incremental updates.
    pub fn page_states(&self) -> Result<HashMap<VaultPath, (String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, hash, revision FROM pages")?;
        let rows = stmt.query_map([], |r| Ok((to_path(r.get(0)?)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Number of indexed pages.
    pub fn page_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Summary of one page, if indexed.
    pub fn page(&self, path: &VaultPath) -> Result<Option<PageSummary>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, title, modified_ms FROM pages WHERE path = ?1",
                [path.as_str()],
                |r| {
                    Ok(PageSummary {
                        path: to_path(r.get(0)?)?,
                        title: r.get(1)?,
                        modified_ms: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// All pages ordered by path.
    pub fn pages(&self) -> Result<Vec<PageSummary>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, title, modified_ms FROM pages ORDER BY path")?;
        let rows = stmt.query_map([], |r| {
            Ok(PageSummary {
                path: to_path(r.get(0)?)?,
                title: r.get(1)?,
                modified_ms: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Resolve a link target (`Note`, `folder/Note`, `Note.md` or an alias) to a page.
    ///
    /// Order: exact path; path suffix (shortest path wins); file name (shortest path wins);
    /// alias (shortest path wins). Matching is case-insensitive.
    pub fn resolve(&self, target: &str) -> Result<Option<VaultPath>> {
        resolve_norm(&self.conn, &normalize_target(target))
    }

    /// Pages linking to `path`, ordered by source path then line.
    pub fn backlinks(&self, path: &VaultPath) -> Result<Vec<Backlink>> {
        let Some((page_id, stem)) = self
            .conn
            .query_row(
                "SELECT id, stem_norm FROM pages WHERE path = ?1",
                [path.as_str()],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            return Ok(Vec::new());
        };
        let mut leaves = vec![stem];
        {
            let mut stmt = self
                .conn
                .prepare("SELECT alias_norm FROM aliases WHERE page_id = ?1")?;
            for a in stmt.query_map([page_id], |r| r.get::<_, String>(0))? {
                let a = a?;
                leaves.push(leaf(&a).to_string());
            }
        }
        leaves.sort();
        leaves.dedup();

        let mut cache: HashMap<String, Option<VaultPath>> = HashMap::new();
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT l.target_norm, p.path, p.title, l.line, l.context, l.embed
             FROM links l JOIN pages p ON p.id = l.source_id
             WHERE l.leaf_norm = ?1",
        )?;
        for leaf_name in &leaves {
            let rows = stmt.query_map([leaf_name], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    to_path(r.get(1)?)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            })?;
            for row in rows {
                let (target_norm, source, source_title, line, context, embed) = row?;
                let resolved = match cache.get(&target_norm) {
                    Some(r) => r.clone(),
                    None => {
                        let r = resolve_norm(&self.conn, &target_norm)?;
                        cache.insert(target_norm.clone(), r.clone());
                        r
                    }
                };
                if resolved.as_ref() == Some(path) {
                    out.push(Backlink {
                        source,
                        source_title,
                        line,
                        context,
                        embed,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.source.cmp(&b.source).then(a.line.cmp(&b.line)));
        out.dedup();
        Ok(out)
    }

    /// Outgoing links of a page, in body order.
    pub fn outgoing(&self, path: &VaultPath) -> Result<Vec<OutLink>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.target, l.target_norm, l.line, l.embed FROM links l
             JOIN pages p ON p.id = l.source_id WHERE p.path = ?1 ORDER BY l.id",
        )?;
        let rows: Vec<(String, String, u32, bool)> = stmt
            .query_map([path.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(target, norm, line, embed)| {
                Ok(OutLink {
                    resolved: resolve_norm(&self.conn, &norm)?,
                    target,
                    line,
                    embed,
                })
            })
            .collect()
    }

    /// Every link whose target does not resolve to an existing page.
    pub fn unresolved(&self) -> Result<Vec<UnresolvedLink>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.target, l.target_norm, p.path FROM links l
             JOIN pages p ON p.id = l.source_id ORDER BY p.path, l.id",
        )?;
        let rows: Vec<(String, String, VaultPath)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, to_path(r.get(2)?)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut cache: HashMap<String, bool> = HashMap::new();
        let mut out = Vec::new();
        for (target, norm, source) in rows {
            let exists = match cache.get(&norm) {
                Some(&e) => e,
                None => {
                    let e = resolve_norm(&self.conn, &norm)?.is_some();
                    cache.insert(norm.clone(), e);
                    e
                }
            };
            if !exists {
                out.push(UnresolvedLink { target, source });
            }
        }
        Ok(out)
    }

    /// All tags with page counts, ordered by tag.
    pub fn tags(&self) -> Result<Vec<TagCount>> {
        let mut stmt = self.conn.prepare(
            "SELECT MIN(tag), COUNT(DISTINCT page_id) FROM tags GROUP BY tag_norm ORDER BY tag_norm",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TagCount {
                tag: r.get(0)?,
                count: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Front matter properties of a page as `(key, JSON value)`.
    pub fn properties(&self, path: &VaultPath) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT pr.key, pr.value_json FROM properties pr JOIN pages p ON p.id = pr.page_id
             WHERE p.path = ?1 ORDER BY pr.rowid",
        )?;
        let rows = stmt.query_map([path.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Pages directly inside `folder` (not nested deeper) with their front matter properties
    /// as `(key, JSON value)`, ordered by path. `folder` is vault-relative; empty for the root.
    pub fn folder_rows(&self, folder: &str) -> Result<Vec<FolderRow>> {
        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.path, p.title, p.modified_ms FROM pages p
             WHERE substr(p.path, 1, length(?1)) = ?1
               AND instr(substr(p.path, length(?1) + 1), '/') = 0
             ORDER BY p.path",
        )?;
        let pages: Vec<(i64, PageSummary)> = stmt
            .query_map([&prefix], |r| {
                Ok((
                    r.get(0)?,
                    PageSummary {
                        path: to_path(r.get(1)?)?,
                        title: r.get(2)?,
                        modified_ms: r.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut props = self.conn.prepare_cached(
            "SELECT key, value_json FROM properties WHERE page_id = ?1 ORDER BY rowid",
        )?;
        let mut out = Vec::with_capacity(pages.len());
        for (id, summary) in pages {
            let kv: Vec<(String, String)> = props
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            out.push((summary, kv));
        }
        Ok(out)
    }

    /// Read a value from the index metadata table.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Store a value in the index metadata table.
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)              ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// All task rows, optionally only open ones, ordered by due (undated last) then path.
    pub fn task_rows(&self, open_only: bool) -> Result<Vec<TaskRow>> {
        let sql = format!(
            "SELECT p.path, t.fields FROM tasks t JOIN pages p ON p.id = t.page_id {}              ORDER BY t.due IS NULL, t.due, p.path",
            if open_only { "WHERE t.done = 0" } else { "" }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(TaskRow {
                path: to_path(r.get(0)?)?,
                fields_json: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Passages matching the FTS5 expression `expr` (already quoted by the caller), best bm25
    /// first, with the heading column weighted above the text. At most `limit` rows.
    pub(crate) fn passage_candidates(
        &self,
        expr: &str,
        limit: usize,
    ) -> Result<Vec<PassageCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.path, p.title, passages.heading, passages.text, passages.start_line,
                    passages.end_line, bm25(passages, 0.0, 5.0, 1.0, 0.0, 0.0) AS score
             FROM passages JOIN pages p ON p.id = passages.page_id
             WHERE passages MATCH ?1
             ORDER BY score, p.path, passages.start_line
             LIMIT CAST(?2 AS INTEGER)",
        )?;
        let rows = stmt.query_map(params![expr, limit.to_string()], |r| {
            let heading: String = r.get(2)?;
            Ok(PassageCandidate {
                path: to_path(r.get(0)?)?,
                title: r.get(1)?,
                heading: Some(heading).filter(|h| !h.is_empty()),
                text: r.get(3)?,
                start_line: r.get(4)?,
                end_line: r.get(5)?,
                bm25: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Run a search (see [`SearchQuery`] for syntax). Results are best-first.
    pub fn search(&self, input: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let q = SearchQuery::parse(input);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::new();
        let mut args: Vec<String> = Vec::new();
        let fts = q.fts_expression();
        if let Some(expr) = &fts {
            sql.push_str(&format!(
                "SELECT p.path, p.title, snippet(fts, 1, '{HIGHLIGHT_START}', '{HIGHLIGHT_END}', '…', 16), \
                 bm25(fts, 8.0, 1.0) AS score \
                 FROM fts JOIN pages p ON p.id = fts.rowid WHERE fts MATCH ?1"
            ));
            args.push(expr.clone());
        } else {
            sql.push_str("SELECT p.path, p.title, '', 0.0 AS score FROM pages p WHERE 1 = 1");
        }
        for tag in &q.tags {
            args.push(tag.clone());
            let n = args.len();
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM tags t WHERE t.page_id = p.id AND \
                 (t.tag_norm = ?{n} OR substr(t.tag_norm, 1, length(?{n}) + 1) = ?{n} || '/'))"
            ));
        }
        for path in &q.paths {
            args.push(path.clone());
            let n = args.len();
            sql.push_str(&format!(" AND instr(p.path_norm, ?{n}) > 0"));
        }
        if fts.is_some() {
            sql.push_str(" ORDER BY score");
        } else {
            sql.push_str(" ORDER BY p.path");
        }
        args.push(limit.to_string());
        sql.push_str(&format!(" LIMIT CAST(?{} AS INTEGER)", args.len()));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok(SearchHit {
                path: to_path(r.get(0)?)?,
                title: r.get(1)?,
                snippet: r.get(2)?,
                score: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn remove_in(tx: &rusqlite::Transaction<'_>, path: &VaultPath) -> Result<bool> {
    let id: Option<i64> = tx
        .query_row(
            "SELECT id FROM pages WHERE path = ?1",
            [path.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(id) = id else { return Ok(false) };
    tx.execute("DELETE FROM fts WHERE rowid = ?1", [id])?;
    tx.execute(
        "DELETE FROM passages WHERE rowid >= ?1 AND rowid < ?2",
        params![id * PASSAGE_ROWID_STRIDE, (id + 1) * PASSAGE_ROWID_STRIDE],
    )?;
    tx.execute("DELETE FROM pages WHERE id = ?1", [id])?;
    Ok(true)
}

fn upsert_in(tx: &rusqlite::Transaction<'_>, rec: &PageRecord) -> Result<()> {
    remove_in(tx, &rec.path)?;
    let pnorm = path_norm(&rec.path);
    let stem = leaf(&pnorm).to_string();
    tx.execute(
        "INSERT INTO pages (path, path_norm, stem_norm, title, hash, revision, size, modified_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            rec.path.as_str(),
            pnorm,
            stem,
            rec.title,
            rec.hash,
            rec.revision,
            i64::try_from(rec.size).unwrap_or(i64::MAX),
            rec.modified_ms
        ],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO fts (rowid, title, body) VALUES (?1, ?2, ?3)",
        params![id, rec.title, rec.body],
    )?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO passages (rowid, page_id, heading, text, start_line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let max = usize::try_from(PASSAGE_ROWID_STRIDE).unwrap_or(usize::MAX);
        for (n, p) in rec.passages.iter().take(max).enumerate() {
            stmt.execute(params![
                id * PASSAGE_ROWID_STRIDE + i64::try_from(n).unwrap_or(0),
                id,
                p.heading.as_deref().unwrap_or(""),
                p.text,
                p.start_line,
                p.end_line
            ])?;
        }
    }
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO aliases (page_id, alias, alias_norm) VALUES (?1, ?2, ?3)",
        )?;
        for a in &rec.aliases {
            let n = normalize_target(a);
            if !n.is_empty() {
                stmt.execute(params![id, a, n])?;
            }
        }
    }
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO links (source_id, target, target_norm, leaf_norm, heading, alias, embed, line, context)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for l in &rec.links {
            let n = normalize_target(&l.target);
            if n.is_empty() {
                continue;
            }
            stmt.execute(params![
                id,
                l.target,
                n,
                leaf(&n),
                l.heading,
                l.alias,
                l.embed,
                l.line,
                l.context
            ])?;
        }
    }
    {
        let mut stmt =
            tx.prepare_cached("INSERT INTO tags (page_id, tag, tag_norm) VALUES (?1, ?2, ?3)")?;
        for t in &rec.tags {
            stmt.execute(params![id, t, t.to_lowercase()])?;
        }
    }
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO properties (page_id, key, value_json) VALUES (?1, ?2, ?3)",
        )?;
        for (k, v) in &rec.properties {
            stmt.execute(params![id, k, v])?;
        }
    }
    if let Some(t) = &rec.task {
        tx.execute(
            "INSERT INTO tasks (page_id, done, due, project, fields) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, t.done, t.due, t.project, t.fields_json],
        )?;
    }
    Ok(())
}

fn resolve_norm(conn: &Connection, target: &str) -> Result<Option<VaultPath>> {
    if target.is_empty() {
        return Ok(None);
    }
    let found: Option<String> = conn
        .query_row(
            "SELECT path FROM pages WHERE path_norm = ?1",
            [target],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(p) = found {
        return Ok(Some(to_path(p)?));
    }
    let lf = leaf(target);
    let found: Option<String> = if target.contains('/') {
        conn.query_row(
            "SELECT path FROM pages WHERE stem_norm = ?1 AND substr(path_norm, -length(?2) - 1) = '/' || ?2
             ORDER BY length(path), path LIMIT 1",
            params![lf, target],
            |r| r.get(0),
        )
        .optional()?
    } else {
        conn.query_row(
            "SELECT path FROM pages WHERE stem_norm = ?1 ORDER BY length(path), path LIMIT 1",
            [target],
            |r| r.get(0),
        )
        .optional()?
    };
    if let Some(p) = found {
        return Ok(Some(to_path(p)?));
    }
    let found: Option<String> = conn
        .query_row(
            "SELECT p.path FROM aliases a JOIN pages p ON p.id = a.page_id WHERE a.alias_norm = ?1
             ORDER BY length(p.path), p.path LIMIT 1",
            [target],
            |r| r.get(0),
        )
        .optional()?;
    found.map(to_path).transpose().map_err(IndexError::from)
}

//! SQLite schema for the index. Bump [`crate::INDEX_VERSION`] with every change.

/// Rowids of the `passages` table are `page_id * PASSAGE_ROWID_STRIDE + n`; a page has fewer
/// passages than this.
pub(crate) const PASSAGE_ROWID_STRIDE: i64 = 100_000;

/// Full schema, applied to an empty database.
pub(crate) const SCHEMA: &str = r#"
CREATE TABLE pages (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    path_norm   TEXT NOT NULL,
    stem_norm   TEXT NOT NULL,
    title       TEXT NOT NULL,
    hash        TEXT NOT NULL,
    revision    TEXT NOT NULL,
    size        INTEGER NOT NULL,
    modified_ms INTEGER NOT NULL
);
CREATE INDEX pages_stem ON pages(stem_norm);
CREATE INDEX pages_path_norm ON pages(path_norm);

CREATE TABLE aliases (
    page_id    INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    alias      TEXT NOT NULL,
    alias_norm TEXT NOT NULL
);
CREATE INDEX aliases_norm ON aliases(alias_norm);
CREATE INDEX aliases_page ON aliases(page_id);

CREATE TABLE links (
    id          INTEGER PRIMARY KEY,
    source_id   INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    target      TEXT NOT NULL,
    target_norm TEXT NOT NULL,
    leaf_norm   TEXT NOT NULL,
    heading     TEXT,
    alias       TEXT,
    embed       INTEGER NOT NULL,
    line        INTEGER NOT NULL,
    context     TEXT NOT NULL
);
CREATE INDEX links_leaf ON links(leaf_norm);
CREATE INDEX links_source ON links(source_id);

CREATE TABLE tags (
    page_id  INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    tag      TEXT NOT NULL,
    tag_norm TEXT NOT NULL
);
CREATE INDEX tags_norm ON tags(tag_norm);
CREATE INDEX tags_page ON tags(page_id);

CREATE TABLE properties (
    page_id    INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value_json TEXT NOT NULL
);
CREATE INDEX properties_page ON properties(page_id);
CREATE INDEX properties_key ON properties(key);

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE tasks (
    page_id    INTEGER PRIMARY KEY REFERENCES pages(id) ON DELETE CASCADE,
    done       INTEGER NOT NULL,
    due        TEXT,
    project    TEXT,
    fields     TEXT NOT NULL
);
CREATE INDEX tasks_due ON tasks(done, due);

CREATE VIRTUAL TABLE fts USING fts5(
    title,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- One row per citable passage. rowid = page_id * PASSAGE_ROWID_STRIDE + sequence number, so a
-- page's passages are removed with a rowid range delete instead of a table scan.
CREATE VIRTUAL TABLE passages USING fts5(
    page_id UNINDEXED,
    heading,
    text,
    start_line UNINDEXED,
    end_line UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2'
);
"#;

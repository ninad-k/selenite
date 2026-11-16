//! Table databases on the [`Engine`]: a folder with `_schema.yaml` (kind `table`) plus one
//! Markdown file per row, properties in front matter. Computed values (formulas, rollups,
//! created/updated time) are derived at query time and never written to files.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use selenite_core::VaultPath;
use selenite_core::database::{
    DatabaseDef, Formula, Layout, PropertyDef, PropertyType, RowGroup, RowValues, Value, ViewDef,
    ViewQuery, group_rows, matches, rollup, sort_rows,
};
use selenite_storage::{StorageError, WriteMode};
use selenite_vault::{DatabaseKind, DatabaseSchema, Document, PropertyValue, SCHEMA_FILE, layout};
use serde::Serialize;

use crate::engine::{Engine, EngineError, Result};

/// A database folder found in the vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseInfo {
    /// Vault-relative folder.
    pub folder: String,
    /// `table` or `tasks`.
    pub kind: String,
    /// Number of row files.
    pub rows: usize,
}

/// One row with every property value (stored and computed).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    /// Path of the row file.
    pub path: VaultPath,
    /// Row title.
    pub title: String,
    /// Values by property name.
    pub values: BTreeMap<String, Value>,
}

impl RowValues for Row {
    fn value(&self, property: &str) -> Value {
        self.values.get(property).cloned().unwrap_or(Value::Null)
    }
}

/// A group of rows in a view result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewGroup {
    /// Group label.
    pub label: String,
    /// Indices into [`ViewResult::rows`].
    pub rows: Vec<usize>,
}

/// The result of querying a database view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewResult {
    /// Database folder.
    pub folder: String,
    /// Property definitions.
    pub properties: Vec<PropertyDef>,
    /// All saved views of the database.
    pub views: Vec<ViewDef>,
    /// The view that was applied.
    pub view: ViewDef,
    /// Matching rows, sorted.
    pub rows: Vec<Row>,
    /// Groups when the view groups by a property (board columns, grouped tables).
    pub groups: Option<Vec<ViewGroup>>,
}

/// Convert a JSON value (from the index or a client) to a database value.
pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n.as_f64().map_or(Value::Null, Value::Number),
        serde_json::Value::String(s) => {
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                Value::Date(d)
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
                Value::DateTime(dt)
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                Value::DateTime(dt)
            } else {
                Value::Text(s.clone())
            }
        }
        serde_json::Value::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(_) => Value::Text(j.to_string()),
    }
}

/// Convert a database value to a front matter value for writing.
fn value_to_property(v: &Value, kind: &PropertyType) -> PropertyValue {
    match v {
        Value::Null => PropertyValue::Null,
        Value::Bool(b) => PropertyValue::Bool(*b),
        Value::Number(n) => PropertyValue::Number(*n),
        Value::Text(t) => match kind {
            PropertyType::Relation { .. } => PropertyValue::Text(format!("[[{t}]]")),
            _ => PropertyValue::Text(t.clone()),
        },
        Value::Date(d) => PropertyValue::Text(d.format("%Y-%m-%d").to_string()),
        Value::DateTime(dt) => PropertyValue::Text(dt.format("%Y-%m-%dT%H:%M").to_string()),
        Value::List(items) => {
            PropertyValue::List(items.iter().map(|i| value_to_property(i, kind)).collect())
        }
    }
}

fn millis_to_datetime(ms: i64) -> Value {
    DateTime::from_timestamp_millis(ms)
        .map(|d| Value::DateTime(d.with_timezone(&chrono::Local).naive_local()))
        .unwrap_or(Value::Null)
}

/// Raw (uncoerced) row data loaded from the index.
struct RawRow {
    path: VaultPath,
    title: String,
    modified_ms: i64,
    props: HashMap<String, Value>,
}

impl Engine {
    fn schema_file(folder: &str) -> Result<VaultPath> {
        Ok(if folder.is_empty() {
            VaultPath::new(SCHEMA_FILE)?
        } else {
            VaultPath::new(&format!("{folder}/{SCHEMA_FILE}"))?
        })
    }

    /// Every database folder in the vault (tables and task lists).
    pub fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        let mut out = Vec::new();
        for e in self.storage().list()? {
            if e.path.file_name() != SCHEMA_FILE || e.path.is_hidden() {
                continue;
            }
            let folder = e.path.parent().unwrap_or("").to_string();
            let Ok(schema) = self.database_schema(&folder) else {
                continue;
            };
            let rows = self.index().folder_rows(&folder)?.len();
            out.push(DatabaseInfo {
                folder,
                kind: match schema.kind {
                    DatabaseKind::Table => "table".into(),
                    DatabaseKind::Tasks => "tasks".into(),
                },
                rows,
            });
        }
        out.sort_by(|a, b| a.folder.cmp(&b.folder));
        Ok(out)
    }

    /// Read a database's schema file.
    pub fn database_schema(&self, folder: &str) -> Result<DatabaseSchema> {
        let path = Self::schema_file(folder)?;
        let bytes = self.storage().read(&path)?;
        let text = String::from_utf8(bytes).map_err(|_| EngineError::NotUtf8(path.clone()))?;
        Ok(DatabaseSchema::from_yaml(&text)?)
    }

    /// Create a table database in `folder` with a title property `Name` plus `properties`, and
    /// a default table view. Fails if the folder is already a database.
    pub fn create_database(&mut self, folder: &str, properties: Vec<PropertyDef>) -> Result<()> {
        let folder = folder.trim_matches('/');
        if folder.is_empty() {
            return Err(EngineError::Database(
                "a database needs its own folder".into(),
            ));
        }
        let mut props = vec![PropertyDef::new("Name", PropertyType::Title)];
        props.extend(
            properties
                .into_iter()
                .filter(|p| !matches!(p.kind, PropertyType::Title)),
        );
        let mut schema = DatabaseSchema::table(props);
        schema.table_views = vec![ViewDef::new("Table", Layout::Table)];
        let problems = schema.definition().validate();
        if !problems.is_empty() {
            return Err(EngineError::Database(problems.join("; ")));
        }
        let path = Self::schema_file(folder)?;
        self.storage()
            .write(&path, schema.to_yaml()?.as_bytes(), WriteMode::CreateNew)?;
        Ok(())
    }

    /// Replace a database schema (properties and views). Validated before writing; `expected`
    /// guards against overwriting a concurrent edit (pass `None` to skip the check).
    pub fn save_database_schema(
        &mut self,
        folder: &str,
        schema: &DatabaseSchema,
        expected: Option<selenite_storage::Revision>,
    ) -> Result<()> {
        if schema.kind == DatabaseKind::Table {
            let problems = schema.definition().validate();
            if !problems.is_empty() {
                return Err(EngineError::Database(problems.join("; ")));
            }
        }
        let path = Self::schema_file(folder)?;
        let mode = match expected {
            Some(r) => WriteMode::Replace(r),
            None => WriteMode::Overwrite,
        };
        self.storage()
            .write(&path, schema.to_yaml()?.as_bytes(), mode)?;
        Ok(())
    }

    fn raw_rows(&self, folder: &str) -> Result<Vec<RawRow>> {
        Ok(self
            .index()
            .folder_rows(folder)?
            .into_iter()
            .map(|(summary, kv)| RawRow {
                path: summary.path,
                title: summary.title,
                modified_ms: summary.modified_ms,
                props: kv
                    .into_iter()
                    .filter_map(|(k, j)| {
                        serde_json::from_str::<serde_json::Value>(&j)
                            .ok()
                            .map(|v| (k, json_to_value(&v)))
                    })
                    .collect(),
            })
            .collect())
    }

    /// Stored values of a row coerced to the schema (no computed properties).
    fn stored_values(def: &DatabaseDef, raw: &RawRow) -> BTreeMap<String, Value> {
        let mut values = BTreeMap::new();
        for p in &def.properties {
            let v = match &p.kind {
                PropertyType::Title => Value::Text(raw.title.clone()),
                PropertyType::UpdatedTime => millis_to_datetime(raw.modified_ms),
                PropertyType::CreatedTime => raw
                    .props
                    .get("created")
                    .cloned()
                    .unwrap_or_else(|| millis_to_datetime(raw.modified_ms)),
                PropertyType::Formula { .. } | PropertyType::Rollup { .. } => continue,
                _ => p.coerce(raw.props.get(&p.name).unwrap_or(&Value::Null)),
            };
            values.insert(p.name.clone(), v);
        }
        values
    }

    /// Rows of a table database with all values, computed ones included, unfiltered.
    pub fn database_rows(&self, folder: &str, now: NaiveDateTime) -> Result<Vec<Row>> {
        let schema = self.database_schema(folder)?;
        let def = schema.definition();
        let order = def
            .formula_order()
            .map_err(|e| EngineError::Database(e.to_string()))?;
        let formulas: Vec<(String, Formula)> = order
            .iter()
            .filter_map(|name| match def.property(name).map(|p| &p.kind) {
                Some(PropertyType::Formula { expression }) => {
                    Formula::parse(expression).ok().map(|f| (name.clone(), f))
                }
                _ => None,
            })
            .collect();
        // Related databases' stored values, loaded once per target folder.
        let mut related: HashMap<
            String,
            (DatabaseDef, HashMap<VaultPath, BTreeMap<String, Value>>),
        > = HashMap::new();

        let mut rows = Vec::new();
        for raw in self.raw_rows(folder)? {
            let mut values = Self::stored_values(&def, &raw);
            for p in &def.properties {
                let PropertyType::Rollup {
                    relation,
                    property,
                    function,
                } = &p.kind
                else {
                    continue;
                };
                let Some(PropertyType::Relation { database }) =
                    def.property(relation).map(|r| &r.kind)
                else {
                    values.insert(p.name.clone(), Value::Null);
                    continue;
                };
                if !related.contains_key(database) {
                    let loaded = match self.database_schema(database) {
                        Ok(s) => {
                            let tdef = s.definition();
                            let map = self
                                .raw_rows(database)?
                                .iter()
                                .map(|r| (r.path.clone(), Self::stored_values(&tdef, r)))
                                .collect();
                            (tdef, map)
                        }
                        Err(_) => (DatabaseDef::default(), HashMap::new()),
                    };
                    related.insert(database.clone(), loaded);
                }
                let targets = match values.get(relation) {
                    Some(Value::List(items)) => items.clone(),
                    _ => Vec::new(),
                };
                let mut collected = Vec::new();
                if let Some((_, map)) = related.get(database) {
                    for t in targets {
                        let Some(path) = self.resolve(&t.as_text())? else {
                            continue;
                        };
                        if let Some(vals) = map.get(&path) {
                            collected.push(vals.get(property).cloned().unwrap_or(Value::Null));
                        }
                    }
                }
                values.insert(p.name.clone(), rollup(*function, &collected));
            }
            for (name, f) in &formulas {
                let ctx: HashMap<String, Value> =
                    values.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let v = f.eval(&ctx, now);
                values.insert(name.clone(), v);
            }
            rows.push(Row {
                path: raw.path,
                title: raw.title,
                values,
            });
        }
        Ok(rows)
    }

    /// Apply a view (by name; `None` = first saved view or an unfiltered table) or an ad-hoc
    /// query to a table database.
    pub fn query_database_view(
        &self,
        folder: &str,
        view_name: Option<&str>,
        adhoc: Option<ViewQuery>,
        now: NaiveDateTime,
    ) -> Result<ViewResult> {
        let schema = self.database_schema(folder)?;
        if schema.kind != DatabaseKind::Table {
            return Err(EngineError::Database(format!(
                "{folder} is a task list; use the task filters instead"
            )));
        }
        let def = schema.definition();
        let mut view = match view_name {
            Some(name) => def
                .views
                .iter()
                .find(|v| v.name.eq_ignore_ascii_case(name))
                .cloned()
                .ok_or_else(|| EngineError::Database(format!("no view named {name}")))?,
            None => def
                .views
                .first()
                .cloned()
                .unwrap_or_else(|| ViewDef::new("Table", Layout::Table)),
        };
        if let Some(q) = adhoc {
            view.query = q;
        }
        let today = now.date();
        let mut rows: Vec<Row> = self
            .database_rows(folder, now)?
            .into_iter()
            .filter(|r| {
                view.query
                    .filter
                    .as_ref()
                    .is_none_or(|f| matches(f, r, today))
            })
            .collect();
        sort_rows(&mut rows, &view.query.sorts);
        let groups = view.query.group_by.as_ref().map(|g| {
            group_rows(&rows, g, def.property(g))
                .into_iter()
                .map(|RowGroup { label, rows, .. }| ViewGroup { label, rows })
                .collect()
        });
        Ok(ViewResult {
            folder: folder.to_string(),
            properties: def.properties.clone(),
            views: def.views.clone(),
            view,
            rows,
            groups,
        })
    }

    /// Add a row to a table database. `values` maps property names to JSON values; computed
    /// properties are ignored. Returns the new row's path.
    pub fn create_row(
        &mut self,
        folder: &str,
        title: &str,
        values: &BTreeMap<String, serde_json::Value>,
        now: NaiveDateTime,
    ) -> Result<VaultPath> {
        let schema = self.database_schema(folder)?;
        if schema.kind != DatabaseKind::Table {
            return Err(EngineError::Database(format!(
                "{folder} is not a table database"
            )));
        }
        let title = title.trim();
        if title.is_empty() {
            return Err(EngineError::Database("a row needs a title".into()));
        }
        let folder_opt = (!folder.is_empty()).then_some(folder);
        let mut path = layout::page_path_for_title(title, folder_opt)?;
        let mut n = 2;
        while self.storage().stat(&path)?.is_some() {
            path = layout::page_path_for_title(&format!("{title} {n}"), folder_opt)?;
            n += 1;
            if n > 10_000 {
                return Err(EngineError::Database(
                    "could not find a free file name".into(),
                ));
            }
        }
        let def = schema.definition();
        let mut doc = Document::parse("");
        if path.stem() != title {
            doc.set_property("title", PropertyValue::Text(title.to_string()));
        }
        doc.set_property(
            "created",
            PropertyValue::Text(now.format("%Y-%m-%dT%H:%M").to_string()),
        );
        for p in &def.properties {
            if p.is_computed() || matches!(p.kind, PropertyType::Title) {
                continue;
            }
            if let Some(j) = values.get(&p.name) {
                let v = p.coerce(&json_to_value(j));
                if !v.is_empty() {
                    doc.set_property(&p.name, value_to_property(&v, &p.kind));
                }
            }
        }
        self.create_page(&path, &doc.to_text())?;
        Ok(path)
    }

    /// Set (or clear, with JSON null) one stored property of a row. The body is untouched.
    pub fn set_row_property(
        &mut self,
        path: &VaultPath,
        property: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let folder = path.parent().unwrap_or("");
        let schema = self.database_schema(folder).map_err(|e| match e {
            EngineError::Storage(StorageError::NotFound(_)) => {
                EngineError::Database(format!("{path} is not in a database"))
            }
            other => other,
        })?;
        let def = schema.definition();
        let p = def
            .property(property)
            .ok_or_else(|| EngineError::Database(format!("no property named {property}")))?;
        if p.is_computed() {
            return Err(EngineError::Database(format!("{property} is computed")));
        }
        let (text, rev) = self.read_page(path)?;
        let mut doc = Document::parse(&text);
        if matches!(p.kind, PropertyType::Title) {
            let t = json_to_value(value).as_text();
            if t.trim().is_empty() {
                return Err(EngineError::Database("a row needs a title".into()));
            }
            doc.set_property("title", PropertyValue::Text(t));
        } else {
            let v = p.coerce(&json_to_value(value));
            if v.is_empty() && !matches!(p.kind, PropertyType::Checkbox) {
                doc.remove_property(property);
            } else {
                doc.set_property(property, value_to_property(&v, &p.kind));
            }
        }
        if doc.front_matter_error().is_some() {
            return Err(EngineError::Database(
                "the row has invalid front matter; fix it by hand first".into(),
            ));
        }
        self.save_page(path, &doc.to_text(), Some(rev))?;
        Ok(())
    }
}

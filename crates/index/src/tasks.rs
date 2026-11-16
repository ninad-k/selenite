//! Task operations on the [`Engine`]: task rows live as Markdown files in a Tasks database
//! folder (a folder with `_schema.yaml` of kind `tasks`), with the task fields in front matter.

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};
use selenite_core::filter::Filter;
use selenite_core::{TASK_KEYS, TaskFields, VaultPath, parse_quick_add};
use selenite_storage::{StorageError, WriteMode};
use selenite_vault::{DatabaseSchema, Document, PropertyValue, SCHEMA_FILE, SavedView, layout};
use serde::Serialize;

use crate::db::TaskRecord;
use crate::engine::{Engine, EngineError, Result};

/// A task and the file it lives in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskItem {
    /// Vault path of the task file.
    pub path: VaultPath,
    /// Task fields.
    pub fields: TaskFields,
}

/// One day of the agenda.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgendaDay {
    /// The day.
    pub date: NaiveDate,
    /// Open tasks due that day (recurring tasks appear on every occurrence date).
    pub tasks: Vec<TaskItem>,
}

/// The agenda for a range of days.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Agenda {
    /// Open tasks due before the first day.
    pub overdue: Vec<TaskItem>,
    /// One entry per day, in order (days without tasks included).
    pub days: Vec<AgendaDay>,
}

/// A project (or sub-project) with its open task count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCount {
    /// Project path such as `Home/Garden`.
    pub project: String,
    /// Open tasks directly in this project.
    pub open: usize,
}

fn pv_to_json(v: &PropertyValue) -> serde_json::Value {
    use serde_json::Value;
    match v {
        PropertyValue::Null => Value::Null,
        PropertyValue::Bool(b) => Value::Bool(*b),
        PropertyValue::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                Value::from(*n as i64)
            } else {
                serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number)
            }
        }
        PropertyValue::Text(s) => Value::String(s.clone()),
        PropertyValue::List(items) => Value::Array(items.iter().map(pv_to_json).collect()),
        PropertyValue::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), pv_to_json(v)))
                .collect(),
        ),
    }
}

/// Read task fields from a parsed row file. Lenient: unknown or malformed values fall back to
/// defaults (the file is never rewritten just because it was read). The title defaults to the
/// file stem. Returns `None` only if the fields cannot be interpreted at all.
pub fn task_fields_from_document(doc: &Document, path: &VaultPath) -> Option<TaskFields> {
    let mut map = serde_json::Map::new();
    for key in TASK_KEYS {
        if let Some(v) = doc.property(key) {
            let mut j = pv_to_json(v);
            if *key == "priority" {
                if let Some(s) = j.as_str() {
                    let n = s.trim().trim_start_matches(['p', 'P']).parse::<u8>().ok();
                    j = n.map_or(serde_json::Value::Null, serde_json::Value::from);
                }
            }
            if *key == "labels" {
                if let Some(s) = j.as_str() {
                    j = serde_json::Value::Array(vec![serde_json::Value::String(s.to_string())]);
                }
            }
            if !j.is_null() {
                map.insert((*key).to_string(), j);
            }
        }
    }
    let value = serde_json::Value::Object(map.clone());
    let mut fields: TaskFields = match serde_json::from_value(value) {
        Ok(f) => f,
        Err(_) => {
            // Drop keys one by one until it parses, keeping as much as possible.
            let mut fields = TaskFields::default();
            for (k, v) in map {
                let mut probe = serde_json::to_value(&fields).ok()?;
                if let Some(obj) = probe.as_object_mut() {
                    obj.insert(k, v);
                }
                if let Ok(f) = serde_json::from_value::<TaskFields>(probe) {
                    fields = f;
                }
            }
            fields
        }
    };
    if fields.title.trim().is_empty() {
        fields.title = doc.title().unwrap_or_else(|| path.stem().to_string());
    }
    Some(fields)
}

/// Write task fields into a document's front matter (body untouched). Keys absent from the
/// serialised fields (defaults) are removed.
pub fn apply_task_fields(doc: &mut Document, fields: &TaskFields) -> Result<()> {
    let value = serde_json::to_value(fields).map_err(|e| EngineError::Task(e.to_string()))?;
    let obj = value.as_object().cloned().unwrap_or_default();
    for key in TASK_KEYS {
        match obj.get(*key) {
            Some(v) => {
                let pv: PropertyValue = serde_json::from_value(v.clone())
                    .map_err(|e| EngineError::Task(e.to_string()))?;
                doc.set_property(key, pv);
            }
            None => {
                doc.remove_property(key);
            }
        }
    }
    if doc.front_matter_error().is_some() {
        return Err(EngineError::Task(
            "the task file has invalid front matter; fix it by hand first".into(),
        ));
    }
    Ok(())
}

/// Index columns for a task row.
pub(crate) fn task_record(fields: &TaskFields) -> TaskRecord {
    TaskRecord {
        done: fields.done,
        due: fields.due.as_ref().map(|d| d.to_string()),
        project: fields.project.clone(),
        fields_json: serde_json::to_string(fields).unwrap_or_else(|_| "{}".into()),
    }
}

fn sort_key(t: &TaskItem) -> (bool, bool, Option<NaiveDateTime>, u8, String) {
    let due = t.fields.due_or_start_of_day();
    (
        t.fields.done,
        due.is_none(), // undated tasks last
        due,
        t.fields.priority.as_u8(),
        t.fields.title.to_lowercase(),
    )
}

impl Engine {
    /// Folders currently recognised as Tasks databases.
    pub fn task_databases(&self) -> Vec<String> {
        self.task_dbs.iter().cloned().collect()
    }

    /// The default Tasks database folder, creating it (and its `_schema.yaml`) if needed.
    pub fn ensure_tasks_db(&mut self) -> Result<String> {
        let folder = self.config().tasks_folder.trim_matches('/').to_string();
        let schema_path = if folder.is_empty() {
            VaultPath::new(SCHEMA_FILE)?
        } else {
            VaultPath::new(&format!("{folder}/{SCHEMA_FILE}"))?
        };
        if self.storage().stat(&schema_path)?.is_none() {
            let yaml = DatabaseSchema::tasks().to_yaml()?;
            match self
                .storage()
                .write(&schema_path, yaml.as_bytes(), WriteMode::CreateNew)
            {
                Ok(_) | Err(StorageError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !self.task_dbs.contains(&folder) {
            self.task_dbs.insert(folder.clone());
            self.store_task_dbs()?;
            self.reindex_folder(&folder)?;
        }
        Ok(folder)
    }

    fn free_task_path(&self, title: &str, folder: &str) -> Result<VaultPath> {
        let folder = (!folder.is_empty()).then_some(folder);
        let base = layout::page_path_for_title(title, folder)?;
        if self.storage().stat(&base)?.is_none() {
            return Ok(base);
        }
        for n in 2..10_000 {
            let candidate = layout::page_path_for_title(&format!("{title} {n}"), folder)?;
            if self.storage().stat(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
        Err(EngineError::Task("could not find a free file name".into()))
    }

    /// Create a task row in the default Tasks database. Returns its path.
    pub fn create_task(&mut self, fields: &TaskFields, body: &str) -> Result<VaultPath> {
        if fields.title.trim().is_empty() {
            return Err(EngineError::Task("a task needs a title".into()));
        }
        let folder = self.ensure_tasks_db()?;
        let path = self.free_task_path(&fields.title, &folder)?;
        let mut doc = Document::parse(body);
        apply_task_fields(&mut doc, fields)?;
        self.create_page(&path, &doc.to_text())?;
        Ok(path)
    }

    /// Create a task from one line of natural language. Returns the path and parser warnings.
    pub fn quick_add(
        &mut self,
        input: &str,
        now: NaiveDateTime,
    ) -> Result<(VaultPath, Vec<String>)> {
        let parsed = parse_quick_add(input, now);
        let path = self.create_task(&parsed.fields, "")?;
        Ok((path, parsed.warnings))
    }

    fn require_task(&self, path: &VaultPath) -> Result<()> {
        let folder = path.parent().unwrap_or("");
        if self.task_dbs.contains(folder) {
            Ok(())
        } else {
            Err(EngineError::Task(format!(
                "{path} is not in a Tasks database"
            )))
        }
    }

    /// Read a task's fields from its file.
    pub fn task(&self, path: &VaultPath) -> Result<TaskFields> {
        self.require_task(path)?;
        let (text, _) = self.read_page(path)?;
        task_fields_from_document(&Document::parse(&text), path)
            .ok_or_else(|| EngineError::Task(format!("{path} has unreadable task fields")))
    }

    /// Read-modify-write a task. The write is conditional on the revision that was read, so a
    /// concurrent external edit surfaces as a conflict instead of being overwritten.
    pub fn update_task(
        &mut self,
        path: &VaultPath,
        change: impl FnOnce(&mut TaskFields),
    ) -> Result<TaskFields> {
        self.require_task(path)?;
        let (text, rev) = self.read_page(path)?;
        let mut doc = Document::parse(&text);
        let mut fields = task_fields_from_document(&doc, path)
            .ok_or_else(|| EngineError::Task(format!("{path} has unreadable task fields")))?;
        change(&mut fields);
        apply_task_fields(&mut doc, &fields)?;
        self.save_page(path, &doc.to_text(), Some(rev))?;
        Ok(fields)
    }

    /// Complete a task at `now` (recurring tasks advance to their next date instead).
    pub fn complete_task(&mut self, path: &VaultPath, now: NaiveDateTime) -> Result<TaskFields> {
        self.update_task(path, |f| {
            f.complete(now);
        })
    }

    /// Reopen a completed task.
    pub fn reopen_task(&mut self, path: &VaultPath) -> Result<TaskFields> {
        self.update_task(path, TaskFields::reopen)
    }

    /// Archive a task (moved to the vault trash; nothing is hard-deleted).
    pub fn archive_task(&mut self, path: &VaultPath) -> Result<VaultPath> {
        self.require_task(path)?;
        self.trash_page(path)
    }

    /// Every task in the index (open first, then by due date, priority and title).
    pub fn all_tasks(&self) -> Result<Vec<TaskItem>> {
        let mut out: Vec<TaskItem> = self
            .index()
            .task_rows(false)?
            .into_iter()
            .filter_map(|row| {
                serde_json::from_str::<TaskFields>(&row.fields_json)
                    .ok()
                    .map(|fields| TaskItem {
                        path: row.path,
                        fields,
                    })
            })
            .collect();
        out.sort_by_key(sort_key);
        Ok(out)
    }

    /// Tasks matching a filter, sorted.
    pub fn list_tasks(&self, filter: &Filter, today: NaiveDate) -> Result<Vec<TaskItem>> {
        Ok(self
            .all_tasks()?
            .into_iter()
            .filter(|t| filter.matches(&t.fields, today))
            .collect())
    }

    /// Parse a filter expression and list matching tasks.
    pub fn query_tasks(&self, expr: &str, today: NaiveDate) -> Result<Vec<TaskItem>> {
        let filter =
            Filter::parse(expr, today).map_err(|e| EngineError::Task(format!("filter: {e}")))?;
        self.list_tasks(&filter, today)
    }

    /// Open tasks per day for `days` days starting at `from`, plus overdue tasks.
    pub fn agenda(&self, from: NaiveDate, days: u32) -> Result<Agenda> {
        let days = days.clamp(1, 366);
        let to = from + chrono::Duration::days(i64::from(days) - 1);
        let mut by_day: BTreeMap<NaiveDate, Vec<TaskItem>> = BTreeMap::new();
        let mut overdue = Vec::new();
        for t in self.all_tasks()? {
            if t.fields.done {
                continue;
            }
            let Some(due) = t.fields.due else { continue };
            if due.date < from {
                overdue.push(t.clone());
            }
            let dates: Vec<NaiveDate> = match &t.fields.recurrence {
                Some(rule) => rule
                    .occurrences(due.date, from.max(due.date), days as usize)
                    .into_iter()
                    .take_while(|d| *d <= to)
                    .collect(),
                None => vec![due.date],
            };
            for d in dates.into_iter().filter(|d| *d >= from && *d <= to) {
                by_day.entry(d).or_default().push(t.clone());
            }
        }
        let days = (0..i64::from(days))
            .map(|i| {
                let date = from + chrono::Duration::days(i);
                AgendaDay {
                    date,
                    tasks: by_day.remove(&date).unwrap_or_default(),
                }
            })
            .collect();
        Ok(Agenda { overdue, days })
    }

    /// Projects with open-task counts (sub-projects listed separately), sorted by name.
    pub fn projects(&self) -> Result<Vec<ProjectCount>> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for t in self.all_tasks()? {
            if let Some(p) = t.fields.project {
                let open = usize::from(!t.fields.done);
                // Make every ancestor visible too.
                let segs: Vec<&str> = p.split('/').collect();
                for i in 1..segs.len() {
                    counts.entry(segs[..i].join("/")).or_insert(0);
                }
                *counts.entry(p).or_insert(0) += open;
            }
        }
        Ok(counts
            .into_iter()
            .map(|(project, open)| ProjectCount { project, open })
            .collect())
    }

    /// Labels in use, sorted.
    pub fn labels(&self) -> Result<Vec<String>> {
        let mut labels: Vec<String> = self
            .all_tasks()?
            .into_iter()
            .flat_map(|t| t.fields.labels)
            .collect();
        labels.sort_by_key(|l| l.to_lowercase());
        labels.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        Ok(labels)
    }

    fn schema_path(&self, folder: &str) -> Result<VaultPath> {
        Ok(if folder.is_empty() {
            VaultPath::new(SCHEMA_FILE)?
        } else {
            VaultPath::new(&format!("{folder}/{SCHEMA_FILE}"))?
        })
    }

    /// Saved views of the default Tasks database.
    pub fn saved_views(&mut self) -> Result<Vec<SavedView>> {
        let folder = self.ensure_tasks_db()?;
        let bytes = self.storage().read(&self.schema_path(&folder)?)?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(DatabaseSchema::from_yaml(&text)?.views)
    }

    /// Add or replace (by name) a saved view in the default Tasks database.
    pub fn save_view(&mut self, view: SavedView, today: NaiveDate) -> Result<()> {
        Filter::parse(&view.filter, today)
            .map_err(|e| EngineError::Task(format!("filter: {e}")))?;
        let folder = self.ensure_tasks_db()?;
        let path = self.schema_path(&folder)?;
        let entry = self
            .storage()
            .stat(&path)?
            .ok_or_else(|| StorageError::NotFound(path.clone()))?;
        let bytes = self.storage().read(&path)?;
        let mut schema = DatabaseSchema::from_yaml(&String::from_utf8_lossy(&bytes))?;
        schema
            .views
            .retain(|v| !v.name.eq_ignore_ascii_case(&view.name));
        schema.views.push(view);
        self.storage().write(
            &path,
            schema.to_yaml()?.as_bytes(),
            WriteMode::Replace(entry.revision),
        )?;
        Ok(())
    }
}

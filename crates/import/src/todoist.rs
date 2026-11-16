//! Todoist CSV importer.
//!
//! One Tasks database per project (named after the CSV file). Columns used: TYPE, CONTENT,
//! DESCRIPTION, PRIORITY, INDENT, DATE. In the export PRIORITY runs 1 to 4 with 4 the highest
//! (Selenite p1), matching Todoist's public API where 4 is "very urgent".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use selenite_core::{Priority, TaskFields, VaultPath, parse_quick_add};
use selenite_index::Engine;
use selenite_index::tasks::apply_task_fields;
use selenite_vault::{DatabaseSchema, Document, SCHEMA_FILE};

use crate::sink::Sink;
use crate::{ImportError, ImportOptions, ImportReport, Result, io_err};

/// Import one Todoist project CSV, or every `*.csv` in a folder, into the vault of `engine`.
pub fn import_todoist(
    engine: &mut Engine,
    src: &Path,
    opts: &ImportOptions,
) -> Result<ImportReport> {
    let files: Vec<PathBuf> = if src.is_dir() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(src)
            .map_err(|e| io_err(src.display().to_string(), e))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("csv")))
            .collect();
        v.sort();
        v
    } else if src.is_file() {
        vec![src.to_path_buf()]
    } else {
        return Err(ImportError::Invalid(format!(
            "{} does not exist",
            src.display()
        )));
    };
    let target = opts.target("Todoist");
    let mut sink = Sink::new(engine, target.clone(), opts.dry_run)?;
    let mut made_db = false;
    for file in &files {
        let shown = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let project = file
            .file_stem()
            .map(|n| n.to_string_lossy().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Tasks".into());
        let bytes = std::fs::read(file).map_err(|e| io_err(shown.clone(), e))?;
        match import_project(&mut sink, &target, &project, &bytes, opts, &shown) {
            Ok(true) => made_db = true,
            Ok(false) => {}
            Err(ImportError::Csv(msg)) => sink.skip(shown, msg),
            Err(e) => return Err(e),
        }
    }
    if made_db && !sink.is_dry() {
        // Tasks databases were created after their rows were indexed: reclassify rows.
        sink.engine().rebuild_index()?;
    }
    tracing::info!(tasks = sink.report.tasks, "todoist import finished");
    Ok(sink.report)
}

fn clean_name(s: &str) -> String {
    let c: String = s
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let t = c.trim_matches(|c| c == '.' || c == ' ').to_string();
    if t.is_empty() { "Tasks".into() } else { t }
}

/// Pull `@label` tokens out of a task title.
fn split_labels(content: &str) -> (String, Vec<String>) {
    let mut words = Vec::new();
    let mut labels = Vec::new();
    for w in content.split_whitespace() {
        let rest = w.strip_prefix('@').unwrap_or("");
        if !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            if !labels.iter().any(|l: &String| l == rest) {
                labels.push(rest.to_string());
            }
        } else {
            words.push(w);
        }
    }
    (words.join(" "), labels)
}

fn map_priority(raw: &str) -> Priority {
    match raw.trim() {
        "4" => Priority::P1,
        "3" => Priority::P2,
        "2" => Priority::P3,
        _ => Priority::P4,
    }
}

fn import_project(
    sink: &mut Sink<'_>,
    target: &str,
    project: &str,
    bytes: &[u8],
    opts: &ImportOptions,
    shown: &str,
) -> Result<bool> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .has_headers(true)
        .from_reader(bytes);
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| ImportError::Csv(format!("unreadable header: {e}")))?
        .iter()
        .map(|h| h.trim().to_uppercase())
        .collect();
    let col = |name: &str| headers.iter().position(|h| h == name);
    let (Some(c_type), Some(c_content)) = (col("TYPE"), col("CONTENT")) else {
        return Err(ImportError::Csv(
            "not a Todoist export (TYPE and CONTENT columns missing)".into(),
        ));
    };
    let (c_desc, c_prio, c_indent, c_date) = (
        col("DESCRIPTION"),
        col("PRIORITY"),
        col("INDENT"),
        col("DATE"),
    );

    let folder = sink.unique_dir(&format!("{target}/{}", clean_name(project)))?;
    let schema_path = VaultPath::new(&format!("{folder}/{SCHEMA_FILE}"))?;
    sink.put(&schema_path, DatabaseSchema::tasks().to_yaml()?.as_bytes())?;

    let mut section: Option<String> = None;
    let mut stack: Vec<(u32, String)> = Vec::new();
    let mut last_task: Option<VaultPath> = None;
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let get = |rec: &csv::StringRecord, i: Option<usize>| -> String {
        i.and_then(|i| rec.get(i)).unwrap_or("").trim().to_string()
    };
    for rec in rdr.records() {
        let rec = rec.map_err(|e| ImportError::Csv(format!("unreadable row: {e}")))?;
        let kind = get(&rec, Some(c_type)).to_lowercase();
        let content = get(&rec, Some(c_content));
        match kind.as_str() {
            "section" => {
                section = (!content.is_empty()).then_some(content);
                stack.clear();
            }
            "note" => {
                if let Some(p) = &last_task {
                    notes
                        .entry(p.as_str().to_string())
                        .or_default()
                        .push(content);
                }
            }
            "task" => {
                let (title, labels) = split_labels(&content);
                if title.is_empty() {
                    sink.skip(shown, "task without a title");
                    continue;
                }
                let mut fields = TaskFields::new(title.clone());
                fields.project = Some(project.to_string());
                fields.section = section.clone();
                fields.labels = labels;
                fields.priority = map_priority(&get(&rec, c_prio));
                let indent: u32 = get(&rec, c_indent).parse().unwrap_or(1).max(1);
                while stack.last().is_some_and(|(i, _)| *i >= indent) {
                    stack.pop();
                }
                if indent > 1 {
                    fields.parent = stack.last().map(|(_, stem)| format!("[[{stem}]]"));
                }
                let mut body = get(&rec, c_desc);
                let date = get(&rec, c_date);
                if !date.is_empty() {
                    let parsed = parse_quick_add(&date, opts.now);
                    let ok = parsed.fields.due.is_some() || parsed.fields.recurrence.is_some();
                    if ok {
                        fields.due = parsed.fields.due;
                        fields.recurrence = parsed.fields.recurrence;
                    }
                    if !ok || !parsed.fields.title.trim().is_empty() {
                        if !body.is_empty() {
                            body.push_str("\n\n");
                        }
                        body.push_str(&format!("Original date: {date}"));
                    }
                }
                let path = sink.unique(&format!(
                    "{folder}/{}",
                    selenite_vault::layout::page_path_for_title(&title, None)?.as_str()
                ))?;
                let mut doc = Document::parse(&if body.is_empty() {
                    String::new()
                } else {
                    format!("{body}\n")
                });
                apply_task_fields(&mut doc, &fields)?;
                sink.put(&path, doc.to_text().as_bytes())?;
                sink.report.tasks += 1;
                stack.push((indent, path.stem().to_string()));
                last_task = Some(path);
            }
            "" | "meta" => {}
            other => sink.skip(shown, format!("unsupported row type {other}")),
        }
    }
    append_notes(sink, notes)?;
    Ok(true)
}

/// Todoist notes (comments) are appended to the task file they belong to.
fn append_notes(sink: &mut Sink<'_>, notes: BTreeMap<String, Vec<String>>) -> Result<()> {
    if sink.is_dry() || notes.is_empty() {
        return Ok(());
    }
    for (path, items) in notes {
        let p = VaultPath::new(&path)?;
        let (text, rev) = sink.engine().read_page(&p)?;
        let mut out = text.trim_end().to_string();
        out.push_str("\n\n## Notes\n");
        for n in items {
            out.push_str(&format!("\n- {}", n.replace('\n', " ")));
        }
        out.push('\n');
        sink.engine().save_page(&p, &out, Some(rev))?;
    }
    Ok(())
}

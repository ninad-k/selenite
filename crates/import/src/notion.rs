//! Notion "Markdown & CSV" export importer.
//!
//! The export is a zip (possibly containing further zips). Pages are `Title <32-hex id>.md`,
//! folders `Title <id>/`, databases `Title <id>.csv` (plus `_all.csv` variants) with one page per
//! row in the folder of the same name. See `docs/import.md` for the mapping.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Cursor, Read};
use std::path::Path;

use chrono::{NaiveDate, NaiveDateTime};
use selenite_core::database::SelectOption;
use selenite_core::{PropertyDef, PropertyType, VaultPath};
use selenite_index::Engine;
use selenite_vault::{DatabaseSchema, Document, PropertyValue, SCHEMA_FILE};
use walkdir::WalkDir;

/// Byte ranges of fenced, indented and inline code, sorted by start.
fn code_ranges(body: &str) -> Vec<std::ops::Range<usize>> {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    let mut out = Vec::new();
    for (ev, range) in Parser::new_ext(body, Options::empty()).into_offset_iter() {
        match ev {
            Event::Start(Tag::CodeBlock(_)) | Event::Code(_) => out.push(range),
            _ => {}
        }
    }
    out.sort_by_key(|r| r.start);
    out
}

fn in_ranges(ranges: &[std::ops::Range<usize>], pos: usize) -> bool {
    let idx = ranges.partition_point(|r| r.start <= pos);
    idx > 0 && pos < ranges[idx - 1].end
}

use crate::sink::Sink;
use crate::{ImportError, ImportOptions, ImportReport, Result, io_err};

const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_NESTING: u32 = 3;

/// Import a Notion export (a `.zip` file, or an already extracted folder) into `engine`.
pub fn import_notion(
    engine: &mut Engine,
    src: &Path,
    opts: &ImportOptions,
) -> Result<ImportReport> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    if src.is_dir() {
        for e in WalkDir::new(src).follow_links(false).sort_by_file_name() {
            let e = e.map_err(|e| io_err(src.display().to_string(), e.into()))?;
            if !e.file_type().is_file() {
                continue;
            }
            let Ok(rel) = e.path().strip_prefix(src) else {
                continue;
            };
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let bytes = std::fs::read(e.path()).map_err(|er| io_err(rel.clone(), er))?;
            files.push((rel, bytes));
        }
    } else {
        let bytes = std::fs::read(src).map_err(|e| io_err(src.display().to_string(), e))?;
        let mut total = 0;
        read_zip(bytes, "", 0, &mut files, &mut skipped, &mut total)?;
    }
    if files.is_empty() {
        return Err(ImportError::Invalid("the export contains no files".into()));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let target = opts.target("Notion");
    let mut sink = Sink::new(engine, target.clone(), opts.dry_run)?;
    sink.report.skipped.extend(skipped);
    run(&mut sink, &target, files)?;
    tracing::info!(
        pages = sink.report.pages,
        rows = sink.report.rows,
        attachments = sink.report.attachments,
        "notion import finished"
    );
    Ok(sink.report)
}

fn read_zip(
    bytes: Vec<u8>,
    prefix: &str,
    depth: u32,
    out: &mut Vec<(String, Vec<u8>)>,
    skipped: &mut Vec<(String, String)>,
    total: &mut u64,
) -> Result<()> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| ImportError::Zip(e.to_string()))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ImportError::Zip(e.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let raw_name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let Some(enclosed) = entry.enclosed_name() else {
            skipped.push((raw_name, "unsafe path in archive".into()));
            continue;
        };
        let name = enclosed
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if name.starts_with("__MACOSX/") || name.ends_with(".DS_Store") {
            continue;
        }
        *total += entry.size();
        if *total > MAX_TOTAL_BYTES {
            return Err(ImportError::Zip("archive is too large".into()));
        }
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(MAX_TOTAL_BYTES)
            .read_to_end(&mut data)
            .map_err(|e| io_err(name.clone(), e))?;
        let full = format!("{prefix}{name}");
        if name.to_lowercase().ends_with(".zip") {
            if depth >= MAX_NESTING {
                skipped.push((full, "archives nested too deeply".into()));
                continue;
            }
            let dir = match full.rfind('/') {
                Some(i) => full[..=i].to_string(),
                None => String::new(),
            };
            read_zip(data, &dir, depth + 1, out, skipped, total)?;
        } else {
            out.push((full, data));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Names

/// Split `Title <32 hex>` into the title and the id.
fn split_id(stem: &str) -> (String, Option<String>) {
    if let Some(i) = stem.len().checked_sub(33) {
        if stem.is_char_boundary(i) {
            let (head, tail) = stem.split_at(i);
            if let Some(id) = tail.strip_prefix(' ') {
                if id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                    return (head.trim().to_string(), Some(id.to_lowercase()));
                }
            }
        }
    }
    (stem.to_string(), None)
}

fn fs_clean(s: &str) -> String {
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
    let t = c.trim_matches(|c| c == '.' || c == ' ');
    if t.is_empty() {
        "Untitled".into()
    } else {
        t.to_string()
    }
}

fn clean_dir_component(c: &str) -> String {
    fs_clean(&split_id(c).0)
}

fn clean_file_component(name: &str) -> String {
    let (stem, ext) = crate::sink::split_ext(name);
    format!(
        "{}{}",
        fs_clean(&split_id(stem).0),
        if ext.eq_ignore_ascii_case(".md") {
            ".md"
        } else {
            ext
        }
    )
}

fn parent_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Resolve `rel` against `dir` (both `/` separated); `None` if it escapes the root.
fn resolve_rel(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = h.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode_link_path(p: &str) -> String {
    p.replace('%', "%25")
        .replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
}

fn relative_path(from_dir: &str, to: &str) -> String {
    let a: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').collect();
    let mut common = 0;
    while common < a.len() && common + 1 < b.len() && a[common] == b[common] {
        common += 1;
    }
    let mut out = "../".repeat(a.len() - common);
    out.push_str(&b[common..].join("/"));
    out
}

// ---------------------------------------------------------------------------------------------
// Link rewriting

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dest {
    Page,
    Attachment,
    Database,
}

struct LinkCtx {
    /// original path -> (final path, kind)
    map: HashMap<String, (String, Dest)>,
    /// lower-cased page stems that occur more than once
    ambiguous: HashSet<String>,
}

fn rewrite_links(text: &str, page_orig_dir: &str, page_final: &str, ctx: &LinkCtx) -> String {
    let page_final_dir = parent_of(page_final);
    let code = code_ranges(text);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    let mut search = 0;
    while let Some(rel) = text[search..].find("](") {
        let close = search + rel;
        let after = close + 2;
        search = after;
        let Some(end_rel) = text[after..].find(')') else {
            break;
        };
        let end = after + end_rel;
        // find the opening bracket of the link text
        let bytes = text.as_bytes();
        let mut depth = 0i32;
        let mut start = None;
        let mut k = close;
        while k > pos {
            k -= 1;
            match bytes[k] {
                b']' => depth += 1,
                b'[' if depth == 0 => {
                    start = Some(k);
                    break;
                }
                b'[' => depth -= 1,
                b'\n' if text[k..close].matches('\n').count() > 1 => break,
                _ => {}
            }
        }
        let Some(start) = start else { continue };
        if in_ranges(&code, close) || in_ranges(&code, start) {
            continue;
        }
        let is_image = start > pos && bytes[start - 1] == b'!';
        let link_text = &text[start + 1..close];
        let raw_target = text[after..end].trim();
        let target_nofrag = raw_target.split('#').next().unwrap_or("");
        if target_nofrag.is_empty()
            || raw_target.contains("://")
            || raw_target.starts_with("mailto:")
        {
            continue;
        }
        let decoded = percent_decode(target_nofrag);
        let Some(orig) = resolve_rel(page_orig_dir, &decoded) else {
            continue;
        };
        let Some((final_path, kind)) = ctx.map.get(&orig) else {
            continue;
        };
        let region_start = if is_image { start - 1 } else { start };
        out.push_str(&text[pos..region_start]);
        match kind {
            Dest::Page => {
                let stem = crate::sink::split_ext(final_path.rsplit('/').next().unwrap_or(""))
                    .0
                    .to_string();
                let name = if ctx.ambiguous.contains(&stem.to_lowercase()) {
                    crate::sink::split_ext(final_path).0.to_string()
                } else {
                    stem.clone()
                };
                let embed = if is_image { "!" } else { "" };
                if link_text.is_empty() || link_text == stem || link_text == name {
                    out.push_str(&format!("{embed}[[{name}]]"));
                } else {
                    out.push_str(&format!("{embed}[[{name}|{link_text}]]"));
                }
            }
            Dest::Attachment => {
                let rel = relative_path(page_final_dir, final_path);
                let bang = if is_image { "!" } else { "" };
                out.push_str(&format!("{bang}[{link_text}]({})", encode_link_path(&rel)));
            }
            Dest::Database => out.push_str(link_text),
        }
        pos = end + 1;
        search = pos;
    }
    out.push_str(&text[pos..]);
    out
}

// ---------------------------------------------------------------------------------------------
// Row property block

/// Split the "# Title / Key: value ..." header Notion writes into row pages. Returns the
/// properties and the remaining body (without the heading and the block).
fn split_property_block(text: &str) -> (Vec<(String, String)>, String) {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    if lines.first().is_some_and(|l| l.starts_with("# ")) {
        i = 1;
    }
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    let mut props = Vec::new();
    let mut j = i;
    while j < lines.len() {
        let l = lines[j].trim_end_matches(['\r', '\n']);
        if l.trim().is_empty() {
            break;
        }
        let Some((k, v)) = l.split_once(':') else {
            break;
        };
        let k = k.trim();
        let ok_key = !k.is_empty()
            && k.len() <= 80
            && !k.starts_with(['#', '-', '[', '>', '*', '`', '|', '!'])
            && (v.is_empty() || v.starts_with(' '));
        if !ok_key {
            break;
        }
        props.push((k.to_string(), v.trim().to_string()));
        j += 1;
    }
    if props.is_empty() {
        // Still drop a bare heading: the file name carries the title.
        let rest: String = lines[i..].concat();
        return (props, rest);
    }
    while j < lines.len() && lines[j].trim().is_empty() {
        j += 1;
    }
    (props, lines[j..].concat())
}

// ---------------------------------------------------------------------------------------------
// Schema inference

enum ColType {
    Text,
    Number,
    Date,
    Checkbox,
    Url,
    Select(Vec<String>),
    Multi(Vec<String>),
}

fn parse_date(s: &str) -> Option<String> {
    let s = s.trim().trim_start_matches('@').trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.format("%Y-%m-%d").to_string());
    }
    for f in ["%B %d, %Y %I:%M %p", "%b %d, %Y %I:%M %p", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, f) {
            return Some(dt.format("%Y-%m-%dT%H:%M").to_string());
        }
    }
    for f in ["%B %d, %Y", "%b %d, %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, f) {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }
    None
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "yes" | "true" | "checked" => Some(true),
        "no" | "false" | "unchecked" => Some(false),
        _ => None,
    }
}

fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    let first = t.chars().next()?;
    if !(first.is_ascii_digit() || matches!(first, '-' | '+' | '.')) {
        return None;
    }
    t.parse::<f64>().ok().filter(|n| n.is_finite())
}

fn is_url(s: &str) -> bool {
    let t = s.trim();
    (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace)
}

fn split_multi(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn infer(values: &[&str]) -> ColType {
    let vals: Vec<&str> = values
        .iter()
        .copied()
        .filter(|v| !v.trim().is_empty())
        .collect();
    if vals.is_empty() {
        return ColType::Text;
    }
    if vals.iter().all(|v| parse_bool(v).is_some()) {
        return ColType::Checkbox;
    }
    if vals.iter().all(|v| parse_number(v).is_some()) {
        return ColType::Number;
    }
    if vals.iter().all(|v| parse_date(v).is_some()) {
        return ColType::Date;
    }
    if vals.iter().all(|v| is_url(v)) {
        return ColType::Url;
    }
    let short = |t: &str| t.chars().count() <= 40 && t.split_whitespace().count() <= 5;
    if vals.iter().any(|v| v.contains(',')) {
        let mut tokens: Vec<String> = Vec::new();
        let mut count = 0;
        for v in &vals {
            for t in split_multi(v) {
                count += 1;
                if !tokens.contains(&t) {
                    tokens.push(t);
                }
            }
        }
        if tokens.len() <= 20 && tokens.iter().all(|t| short(t)) && count > tokens.len() {
            return ColType::Multi(tokens);
        }
        return ColType::Text;
    }
    let mut distinct: Vec<String> = Vec::new();
    for v in &vals {
        let t = v.trim().to_string();
        if !distinct.contains(&t) {
            distinct.push(t);
        }
    }
    if distinct.len() <= 20 && distinct.len() < vals.len() && distinct.iter().all(|t| short(t)) {
        return ColType::Select(distinct);
    }
    ColType::Text
}

fn typed_value(t: &ColType, raw: &str) -> Option<PropertyValue> {
    let raw = raw.trim();
    if raw.is_empty() && !matches!(t, ColType::Checkbox) {
        return None;
    }
    Some(match t {
        ColType::Text | ColType::Url => PropertyValue::Text(raw.to_string()),
        ColType::Select(_) => PropertyValue::Text(raw.to_string()),
        ColType::Number => PropertyValue::Number(parse_number(raw)?),
        ColType::Date => PropertyValue::Text(parse_date(raw)?),
        ColType::Checkbox => PropertyValue::Bool(parse_bool(raw).unwrap_or(false)),
        ColType::Multi(_) => PropertyValue::List(
            split_multi(raw)
                .into_iter()
                .map(PropertyValue::Text)
                .collect(),
        ),
    })
}

fn prop_kind(t: &ColType) -> PropertyType {
    match t {
        ColType::Text => PropertyType::Text,
        ColType::Number => PropertyType::Number { format: None },
        ColType::Date => PropertyType::Date,
        ColType::Checkbox => PropertyType::Checkbox,
        ColType::Url => PropertyType::Url,
        ColType::Select(o) => PropertyType::Select {
            options: o.iter().map(SelectOption::new).collect(),
        },
        ColType::Multi(o) => PropertyType::MultiSelect {
            options: o.iter().map(SelectOption::new).collect(),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The import proper

struct Db {
    /// original folder holding the row pages (`.../Title <id>`)
    orig_dir: String,
    final_dir: String,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

fn run(sink: &mut Sink<'_>, target: &str, files: Vec<(String, Vec<u8>)>) -> Result<()> {
    // 1. Databases: pick one CSV per database (prefer the plain export over `_all`).
    let mut csvs: BTreeMap<String, (String, bool)> = BTreeMap::new(); // dir -> (csv path, is_all)
    let mut csv_all_skipped: Vec<String> = Vec::new();
    for (path, _) in &files {
        if !path.to_lowercase().ends_with(".csv") {
            continue;
        }
        let stem = &path[..path.len() - 4];
        let (dir, is_all) = match stem.strip_suffix("_all") {
            Some(s) => (s.to_string(), true),
            None => (stem.to_string(), false),
        };
        match csvs.get(&dir) {
            Some((existing, false)) if is_all => csv_all_skipped.push(path.clone()),
            Some((existing, true)) if !is_all => {
                csv_all_skipped.push(existing.clone());
                csvs.insert(dir, (path.clone(), false));
            }
            Some(_) => csv_all_skipped.push(path.clone()),
            None => {
                csvs.insert(dir, (path.clone(), is_all));
            }
        }
    }
    for p in csv_all_skipped {
        sink.skip(p, "duplicate view of a database export");
    }
    let by_path: HashMap<&str, &Vec<u8>> = files.iter().map(|(p, b)| (p.as_str(), b)).collect();

    // 2. Path mapping. Databases first so their folders prefix-map row pages and attachments.
    let mut dbs: Vec<Db> = Vec::new();
    let mut map: HashMap<String, (String, Dest)> = HashMap::new();
    let mut handled_csv: HashSet<String> = HashSet::new();
    for (dir, (csv_path, _)) in &csvs {
        let bytes = by_path
            .get(csv_path.as_str())
            .copied()
            .cloned()
            .unwrap_or_default();
        let (headers, rows) = match read_csv(&bytes) {
            Ok(v) => v,
            Err(msg) => {
                sink.skip(csv_path.clone(), msg);
                handled_csv.insert(csv_path.clone());
                continue;
            }
        };
        let title_name = clean_dir_component(dir.rsplit('/').next().unwrap_or(dir));
        let parent = map_dir(&dbs, target, parent_of(dir));
        let final_dir = sink.unique_dir(&join(&parent, &title_name))?;
        map.insert(csv_path.clone(), (final_dir.clone(), Dest::Database));
        handled_csv.insert(csv_path.clone());
        dbs.push(Db {
            orig_dir: dir.clone(),
            final_dir,
            headers,
            rows,
        });
    }
    // Sort so that outer databases come before nested ones in prefix lookup.
    dbs.sort_by_key(|d| d.orig_dir.len());

    struct Plan {
        orig: String,
        final_path: VaultPath,
        kind: Dest,
        db: Option<usize>,
    }
    let mut plans: Vec<Plan> = Vec::new();
    for (path, _) in &files {
        if path.to_lowercase().ends_with(".csv") {
            continue;
        }
        let is_page = path.to_lowercase().ends_with(".md");
        let db_idx = dbs
            .iter()
            .rposition(|d| path.starts_with(&format!("{}/", d.orig_dir)));
        let dir = parent_of(path);
        let final_dir = map_dir(&dbs, target, dir);
        let name = clean_file_component(path.rsplit('/').next().unwrap_or(path));
        let desired = join(&final_dir, &name);
        let final_path = match sink_unique(sink, &desired) {
            Ok(p) => p,
            Err(_) => {
                sink.skip(path.clone(), "file name not allowed in a vault");
                continue;
            }
        };
        let direct_row = db_idx.is_some_and(|i| dbs[i].orig_dir == dir);
        map.insert(
            path.clone(),
            (
                final_path.as_str().to_string(),
                if is_page {
                    Dest::Page
                } else {
                    Dest::Attachment
                },
            ),
        );
        plans.push(Plan {
            orig: path.clone(),
            final_path,
            kind: if is_page {
                Dest::Page
            } else {
                Dest::Attachment
            },
            db: if direct_row { db_idx } else { None },
        });
    }
    let mut stem_count: HashMap<String, usize> = HashMap::new();
    for p in plans.iter().filter(|p| p.kind == Dest::Page) {
        *stem_count
            .entry(p.final_path.stem().to_lowercase())
            .or_default() += 1;
    }
    let ctx = LinkCtx {
        map,
        ambiguous: stem_count
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(s, _)| s)
            .collect(),
    };

    // 3. Row pages grouped by database, matched to CSV rows by title.
    let mut row_pages: HashMap<usize, Vec<usize>> = HashMap::new(); // db -> plan indices
    for (i, p) in plans.iter().enumerate() {
        if let (Some(d), Dest::Page) = (p.db, p.kind) {
            row_pages.entry(d).or_default().push(i);
        }
    }
    let mut written: HashSet<usize> = HashSet::new();
    for (di, db) in dbs.iter().enumerate() {
        let cols: Vec<ColType> = (0..db.headers.len())
            .map(|c| {
                if c == 0 {
                    ColType::Text
                } else {
                    infer(&db.rows.iter().map(|r| r[c].as_str()).collect::<Vec<_>>())
                }
            })
            .collect();
        // schema
        let mut names: Vec<String> = Vec::new();
        for (i, h) in db.headers.iter().enumerate() {
            let mut n = h.trim().to_string();
            if n.is_empty() {
                n = if i == 0 {
                    "Name".into()
                } else {
                    format!("Column {}", i + 1)
                };
            }
            let base = n.clone();
            let mut k = 2;
            while names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                n = format!("{base} {k}");
                k += 1;
            }
            names.push(n);
        }
        let mut props = vec![PropertyDef::new(names[0].clone(), PropertyType::Title)];
        for c in 1..names.len() {
            props.push(PropertyDef::new(names[c].clone(), prop_kind(&cols[c])));
        }
        let mut schema = DatabaseSchema::table(props);
        schema.table_views = vec![selenite_core::ViewDef::new(
            "Table",
            selenite_core::database::Layout::Table,
        )];
        if !schema.definition().validate().is_empty() {
            sink.skip(db.orig_dir.clone(), "database schema could not be inferred");
            continue;
        }
        let schema_path = VaultPath::new(&join(&db.final_dir, SCHEMA_FILE))?;
        sink.put(&schema_path, schema.to_yaml()?.as_bytes())?;

        let mut pages_left: Vec<usize> = row_pages.remove(&di).unwrap_or_default();
        for row in &db.rows {
            let title = row[0].trim().to_string();
            let title = if title.is_empty() {
                "Untitled".to_string()
            } else {
                title
            };
            let key = fs_clean(&title).to_lowercase();
            let matched = pages_left.iter().position(|&pi| {
                plans[pi].final_path.stem().to_lowercase() == key
                    || split_id(
                        plans[pi]
                            .orig
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .trim_end_matches(".md"),
                    )
                    .0
                    .to_lowercase()
                        == key
            });
            let (path, body) = match matched {
                Some(pos) => {
                    let pi = pages_left.remove(pos);
                    written.insert(pi);
                    let text =
                        String::from_utf8_lossy(by_path[plans[pi].orig.as_str()]).into_owned();
                    let (_, rest) = split_property_block(&text);
                    let rest = rewrite_links(
                        &rest,
                        parent_of(&plans[pi].orig),
                        plans[pi].final_path.as_str(),
                        &ctx,
                    );
                    (plans[pi].final_path.clone(), rest)
                }
                None => (
                    sink.unique(&join(&db.final_dir, &format!("{}.md", fs_clean(&title))))?,
                    String::new(),
                ),
            };
            let mut doc = Document::parse(&body);
            if path.stem() != title {
                doc.set_property("title", PropertyValue::Text(title.clone()));
            }
            for c in 1..db.headers.len() {
                if let Some(v) = typed_value(&cols[c], &row[c]) {
                    doc.set_property(&names[c], v);
                }
            }
            sink.put(&path, doc.to_text().as_bytes())?;
            sink.report.rows += 1;
        }
        // Pages in the database folder without a CSV row: keep them as rows, properties from
        // their header block.
        for pi in pages_left {
            written.insert(pi);
            let text = String::from_utf8_lossy(by_path[plans[pi].orig.as_str()]).into_owned();
            let (props, rest) = split_property_block(&text);
            let rest = rewrite_links(
                &rest,
                parent_of(&plans[pi].orig),
                plans[pi].final_path.as_str(),
                &ctx,
            );
            let mut doc = Document::parse(&rest);
            for (k, v) in props {
                doc.set_property(&k, PropertyValue::Text(v));
            }
            sink.put(&plans[pi].final_path, doc.to_text().as_bytes())?;
            sink.report.rows += 1;
        }
    }

    // 4. Everything else.
    for (i, plan) in plans.iter().enumerate() {
        if written.contains(&i) {
            continue;
        }
        let bytes = by_path[plan.orig.as_str()];
        match plan.kind {
            Dest::Page => match std::str::from_utf8(bytes) {
                Ok(text) => {
                    let out =
                        rewrite_links(text, parent_of(&plan.orig), plan.final_path.as_str(), &ctx);
                    sink.put(&plan.final_path, out.as_bytes())?;
                    sink.report.pages += 1;
                }
                Err(_) => {
                    sink.put(&plan.final_path, bytes)?;
                    sink.report.attachments += 1;
                    sink.skip(plan.orig.clone(), "page is not UTF-8; copied as a file");
                }
            },
            _ => {
                sink.put(&plan.final_path, bytes)?;
                sink.report.attachments += 1;
            }
        }
    }
    Ok(())
}

fn sink_unique(sink: &mut Sink<'_>, desired: &str) -> Result<VaultPath> {
    sink.unique(desired)
}

/// Final folder for the original folder `dir`: inside a database folder if one contains it,
/// else under the import target. Components lose their ids.
fn map_dir(dbs: &[Db], target: &str, dir: &str) -> String {
    if dir.is_empty() {
        return target.to_string();
    }
    if let Some(d) = dbs
        .iter()
        .filter(|d| dir == d.orig_dir || dir.starts_with(&format!("{}/", d.orig_dir)))
        .max_by_key(|d| d.orig_dir.len())
    {
        let rest = dir[d.orig_dir.len()..].trim_start_matches('/');
        let mut out = d.final_dir.clone();
        for c in rest.split('/').filter(|c| !c.is_empty()) {
            out = join(&out, &clean_dir_component(c));
        }
        return out;
    }
    let mut out = target.to_string();
    for c in dir.split('/') {
        out = join(&out, &clean_dir_component(c));
    }
    out
}

fn read_csv(bytes: &[u8]) -> std::result::Result<(Vec<String>, Vec<Vec<String>>), String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .has_headers(true)
        .from_reader(bytes);
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| format!("unreadable CSV header: {e}"))?
        .iter()
        .map(str::to_string)
        .collect();
    if headers.is_empty() {
        return Err("CSV has no columns".into());
    }
    let mut rows = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| format!("unreadable CSV row: {e}"))?;
        let mut r: Vec<String> = rec.iter().map(str::to_string).collect();
        r.resize(headers.len(), String::new());
        r.truncate(headers.len());
        rows.push(r);
    }
    Ok((headers, rows))
}

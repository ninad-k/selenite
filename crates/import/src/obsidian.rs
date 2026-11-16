//! Obsidian vault folder importer: copies files as they are, keeping the folder structure.
//!
//! Front matter and wikilinks are already compatible, so file contents are never rewritten.
//! `.obsidian/` (app settings, including the daily-notes folder setting) and `.trash/` are skipped.

use std::path::Path;

use selenite_core::VaultPath;
use selenite_index::Engine;
use walkdir::WalkDir;

use crate::sink::Sink;
use crate::{ImportOptions, ImportReport, Result, io_err};

/// Import the Obsidian vault folder `src` into the vault of `engine`.
pub fn import_obsidian(
    engine: &mut Engine,
    src: &Path,
    opts: &ImportOptions,
) -> Result<ImportReport> {
    if !src.is_dir() {
        return Err(crate::ImportError::Invalid(format!(
            "{} is not a folder",
            src.display()
        )));
    }
    let target = opts.target("Obsidian");
    let mut sink = Sink::new(engine, target.clone(), opts.dry_run)?;
    let walker = WalkDir::new(src)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.depth() > 0 && e.file_type().is_dir() && (name == ".obsidian" || name == ".trash"))
        });
    for entry in walker {
        let entry = entry.map_err(|e| io_err(src.display().to_string(), e.into()))?;
        if entry.file_type().is_dir() {
            continue;
        }
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) => r
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            sink.skip(rel, "not a regular file");
            continue;
        }
        let desired = format!("{target}/{rel}");
        if VaultPath::new(&desired).is_err() {
            sink.skip(rel, "file name not allowed in a vault");
            continue;
        }
        let bytes = std::fs::read(entry.path()).map_err(|e| io_err(rel.clone(), e))?;
        let path = sink.unique(&desired)?;
        sink.put(&path, &bytes)?;
        if path.is_markdown() {
            sink.report.pages += 1;
        } else {
            sink.report.attachments += 1;
        }
    }
    tracing::info!(
        pages = sink.report.pages,
        attachments = sink.report.attachments,
        "obsidian import finished"
    );
    Ok(sink.report)
}

//! The write side shared by all importers: collision-free paths and atomic writes.

use std::collections::HashSet;

use selenite_core::VaultPath;
use selenite_index::Engine;
use selenite_storage::WriteMode;
use selenite_vault::layout;

use crate::{ImportError, ImportReport, Result};

/// Collects the report and performs (or, for a dry run, only plans) writes.
pub(crate) struct Sink<'a> {
    engine: &'a mut Engine,
    dry: bool,
    planned: HashSet<String>,
    dirs: HashSet<String>,
    pub report: ImportReport,
}

/// Split `a/b/name.ext` into (`a/b/name`, `.ext`); no extension gives an empty second part.
pub(crate) fn split_ext(path: &str) -> (&str, &str) {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    match path[name_start..].rfind('.') {
        Some(i) if i > 0 => path.split_at(name_start + i),
        _ => (path, ""),
    }
}

fn suffixed(base: &str, ext: &str, n: u32) -> String {
    match n {
        0 => format!("{base}{ext}"),
        1 => format!("{base} (imported){ext}"),
        k => format!("{base} (imported) {k}{ext}"),
    }
}

impl<'a> Sink<'a> {
    pub fn new(engine: &'a mut Engine, target: String, dry_run: bool) -> Result<Self> {
        let dirs = engine
            .storage()
            .list_dirs()?
            .into_iter()
            .map(|d| d.as_str().to_lowercase())
            .collect();
        Ok(Self {
            engine,
            dry: dry_run,
            planned: HashSet::new(),
            dirs,
            report: ImportReport {
                target,
                dry_run,
                ..ImportReport::default()
            },
        })
    }

    fn taken(&self, p: &VaultPath) -> Result<bool> {
        Ok(self.planned.contains(&p.as_str().to_lowercase())
            || self.engine.storage().stat(p)?.is_some())
    }

    /// Reserve a free path derived from `desired` (adds ` (imported)` / ` (imported) N`).
    pub fn unique(&mut self, desired: &str) -> Result<VaultPath> {
        let (base, ext) = split_ext(desired);
        for n in 0..10_000 {
            let p = VaultPath::new(&suffixed(base, ext, n))?;
            if !self.taken(&p)? {
                self.planned.insert(p.as_str().to_lowercase());
                return Ok(p);
            }
        }
        Err(ImportError::Invalid(format!("no free name for {desired}")))
    }

    /// Reserve a free folder name derived from `desired`.
    pub fn unique_dir(&mut self, desired: &str) -> Result<String> {
        for n in 0..10_000 {
            let cand = suffixed(desired, "", n);
            let key = cand.to_lowercase();
            if !self.dirs.contains(&key) {
                self.dirs.insert(key);
                return Ok(cand);
            }
        }
        Err(ImportError::Invalid(format!(
            "no free folder name for {desired}"
        )))
    }

    /// Write a new file (never overwrites). Pages go through the engine so the index follows.
    pub fn put(&mut self, path: &VaultPath, bytes: &[u8]) -> Result<()> {
        self.planned.insert(path.as_str().to_lowercase());
        if self.dry {
            return Ok(());
        }
        if layout::is_page(path) {
            if let Ok(text) = std::str::from_utf8(bytes) {
                self.engine.create_page(path, text)?;
                return Ok(());
            }
        }
        self.engine
            .storage()
            .write(path, bytes, WriteMode::CreateNew)?;
        Ok(())
    }

    pub fn skip(&mut self, path: impl Into<String>, reason: impl Into<String>) {
        self.report.skipped.push((path.into(), reason.into()));
    }

    pub fn engine(&mut self) -> &mut Engine {
        self.engine
    }

    pub fn is_dry(&self) -> bool {
        self.dry
    }
}

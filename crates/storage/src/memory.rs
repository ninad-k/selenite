//! In-memory [`StorageProvider`], useful for tests of other crates.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use crate::{
    Entry, Result, Revision, StorageError, StorageProvider, WriteMode, unix_secs, vault_path,
};
use selenite_core::VaultPath;

struct File {
    data: Vec<u8>,
    modified: SystemTime,
    rev: u64,
}

#[derive(Default)]
struct State {
    files: BTreeMap<String, File>,
    dirs: BTreeSet<String>,
    counter: u64,
}

/// A [`StorageProvider`] that keeps everything in memory. Revisions are an incrementing counter.
#[derive(Default)]
pub struct MemoryStorage {
    state: Mutex<State>,
}

impl MemoryStorage {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| StorageError::Internal("memory storage lock poisoned".into()))
    }
}

fn entry(path: &str, f: &File) -> Result<Entry> {
    Ok(Entry {
        path: vault_path(path)?,
        size: f.data.len() as u64,
        modified: f.modified,
        revision: Revision(f.rev.to_string()),
    })
}

fn add_parents(dirs: &mut BTreeSet<String>, path: &str) {
    let mut idx = 0;
    while let Some(i) = path[idx..].find('/') {
        idx += i;
        dirs.insert(path[..idx].to_string());
        idx += 1;
    }
}

impl State {
    fn put(&mut self, path: &str, data: Vec<u8>) -> Result<Entry> {
        self.counter += 1;
        add_parents(&mut self.dirs, path);
        let f = File {
            data,
            modified: SystemTime::now(),
            rev: self.counter,
        };
        let e = entry(path, &f)?;
        self.files.insert(path.to_string(), f);
        Ok(e)
    }
}

impl StorageProvider for MemoryStorage {
    fn kind(&self) -> &'static str {
        "memory"
    }

    fn list(&self) -> Result<Vec<Entry>> {
        let s = self.lock()?;
        s.files.iter().map(|(p, f)| entry(p, f)).collect()
    }

    fn list_dirs(&self) -> Result<Vec<VaultPath>> {
        let s = self.lock()?;
        s.dirs.iter().map(|d| vault_path(d)).collect()
    }

    fn stat(&self, path: &VaultPath) -> Result<Option<Entry>> {
        let s = self.lock()?;
        s.files
            .get(path.as_str())
            .map(|f| entry(path.as_str(), f))
            .transpose()
    }

    fn read(&self, path: &VaultPath) -> Result<Vec<u8>> {
        let s = self.lock()?;
        s.files
            .get(path.as_str())
            .map(|f| f.data.clone())
            .ok_or_else(|| StorageError::NotFound(path.clone()))
    }

    fn write(&self, path: &VaultPath, data: &[u8], mode: WriteMode) -> Result<Entry> {
        let mut s = self.lock()?;
        let current = s.files.get(path.as_str());
        match mode {
            WriteMode::CreateNew if current.is_some() => {
                return Err(StorageError::AlreadyExists(path.clone()));
            }
            WriteMode::Replace(expected) => {
                let actual = current.map(|f| Revision(f.rev.to_string()));
                if actual.as_ref() != Some(&expected) {
                    return Err(StorageError::Conflict {
                        path: path.clone(),
                        expected,
                        actual,
                    });
                }
            }
            _ => {}
        }
        s.put(path.as_str(), data.to_vec())
    }

    fn rename(&self, from: &VaultPath, to: &VaultPath) -> Result<Entry> {
        let mut s = self.lock()?;
        if !s.files.contains_key(from.as_str()) {
            return Err(StorageError::NotFound(from.clone()));
        }
        if s.files.contains_key(to.as_str()) {
            return Err(StorageError::AlreadyExists(to.clone()));
        }
        let Some(f) = s.files.remove(from.as_str()) else {
            return Err(StorageError::NotFound(from.clone()));
        };
        add_parents(&mut s.dirs, to.as_str());
        let e = entry(to.as_str(), &f)?;
        s.files.insert(to.as_str().to_string(), f);
        Ok(e)
    }

    fn trash(&self, path: &VaultPath) -> Result<VaultPath> {
        let mut s = self.lock()?;
        let Some(f) = s.files.remove(path.as_str()) else {
            return Err(StorageError::NotFound(path.clone()));
        };
        let base = format!(".selenite/trash/{}-{}", unix_secs(), path.file_name());
        let mut candidate = base.clone();
        let mut n = 1;
        while s.files.contains_key(&candidate) {
            candidate = format!("{base}.{n}");
            n += 1;
        }
        add_parents(&mut s.dirs, &candidate);
        let p = vault_path(&candidate)?;
        s.files.insert(candidate, f);
        Ok(p)
    }

    fn create_dir(&self, path: &VaultPath) -> Result<()> {
        let mut s = self.lock()?;
        add_parents(&mut s.dirs, path.as_str());
        s.dirs.insert(path.as_str().to_string());
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn p(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }

    #[test]
    fn write_modes_and_revisions() {
        let m = MemoryStorage::new();
        let e1 = m.write(&p("a/b.md"), b"one", WriteMode::CreateNew).unwrap();
        assert!(matches!(
            m.write(&p("a/b.md"), b"x", WriteMode::CreateNew),
            Err(StorageError::AlreadyExists(_))
        ));
        let e2 = m
            .write(
                &p("a/b.md"),
                b"two",
                WriteMode::Replace(e1.revision.clone()),
            )
            .unwrap();
        assert_ne!(e1.revision, e2.revision);
        assert!(matches!(
            m.write(&p("a/b.md"), b"three", WriteMode::Replace(e1.revision)),
            Err(StorageError::Conflict { .. })
        ));
        assert_eq!(m.read(&p("a/b.md")).unwrap(), b"two");
        assert_eq!(m.list_dirs().unwrap(), vec![p("a")]);
    }

    #[test]
    fn rename_trash_list() {
        let m = MemoryStorage::new();
        m.write(&p("a.md"), b"x", WriteMode::Overwrite).unwrap();
        m.rename(&p("a.md"), &p("d/b.md")).unwrap();
        assert!(m.stat(&p("a.md")).unwrap().is_none());
        let t = m.trash(&p("d/b.md")).unwrap();
        assert!(t.as_str().starts_with(".selenite/trash/"));
        assert_eq!(m.read(&t).unwrap(), b"x");
        assert!(matches!(
            m.read(&p("d/b.md")),
            Err(StorageError::NotFound(_))
        ));
    }
}

//! [`StorageProvider`] backed by a directory on the local filesystem.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

use crate::{
    Entry, Result, Revision, StorageError, StorageProvider, WriteMode, unix_secs, vault_path,
};
use selenite_core::VaultPath;

const TMP_PREFIX: &str = ".selenite-tmp-";

/// A vault stored as a plain folder on disk.
///
/// Writes are atomic (temp file in the target directory, then rename). The revision check in
/// [`WriteMode::Replace`] and the rename are not a single atomic step: another process can
/// modify the file inside that window (the TOCTOU window is the rename itself). Sync handles
/// such races by detecting the changed revision on the next pass.
#[derive(Debug, Clone)]
pub struct LocalFolder {
    root: PathBuf,
}

fn io_err(path: impl Into<String>, source: io::Error) -> StorageError {
    StorageError::Io {
        path: path.into(),
        source,
    }
}

fn revision_of(meta: &fs::Metadata) -> (SystemTime, Revision) {
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    let nanos = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (modified, Revision(format!("{nanos}-{}", meta.len())))
}

/// Make sure `file`'s mtime is strictly after `previous`.
///
/// Revisions are mtime + size. Two quick saves of equal length can land in the same clock tick
/// (or the clock can step back), which would give the new content the old revision and let a
/// stale conditional write through. Bumping the mtime just past the previous one rules that out
/// for writes made through this provider. The 2 s step covers file systems with coarse
/// timestamps (FAT).
fn ensure_newer(file: &fs::File, previous: SystemTime) -> io::Result<()> {
    for step in [Duration::from_millis(1), Duration::from_secs(2)] {
        if file.metadata()?.modified()? > previous {
            return Ok(());
        }
        file.set_modified(previous + step)?;
    }
    Ok(())
}

fn entry_from(path: &VaultPath, meta: &fs::Metadata) -> Entry {
    let (modified, revision) = revision_of(meta);
    Entry {
        path: path.clone(),
        size: meta.len(),
        modified,
        revision,
    }
}

impl LocalFolder {
    /// Open an existing directory. The root is canonicalized.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let shown = root.display().to_string();
        let root = root.canonicalize().map_err(|e| io_err(shown.clone(), e))?;
        if !root.is_dir() {
            return Err(StorageError::InvalidPath(format!(
                "{shown} is not a directory"
            )));
        }
        Ok(Self { root })
    }

    /// Create the directory (and parents) if needed, then open it.
    pub fn create(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| io_err(root.display().to_string(), e))?;
        Self::open(root)
    }

    /// The canonical root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Map a vault path onto an OS path under the root. Does not touch the disk.
    pub fn resolve(&self, p: &VaultPath) -> PathBuf {
        let mut out = self.root.clone();
        out.extend(p.as_str().split('/'));
        out
    }

    /// Resolve and verify that no existing ancestor (or the file itself) is a link leading
    /// outside the root.
    fn safe_resolve(&self, p: &VaultPath) -> Result<PathBuf> {
        let full = self.resolve(p);
        let mut probe: &Path = &full;
        loop {
            if fs::symlink_metadata(probe).is_ok() {
                break;
            }
            match probe.parent() {
                Some(parent) => probe = parent,
                None => {
                    return Err(StorageError::InvalidPath(p.to_string()));
                }
            }
        }
        let real = probe.canonicalize().map_err(|e| io_err(p.to_string(), e))?;
        if !real.starts_with(&self.root) {
            return Err(StorageError::InvalidPath(format!(
                "{p} resolves outside the storage root"
            )));
        }
        Ok(full)
    }

    fn rel_path(&self, abs: &Path) -> Option<VaultPath> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
        VaultPath::new(&parts?.join("/")).ok()
    }

    fn walk(&self) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
        WalkDir::new(&self.root)
            .follow_links(false)
            .min_depth(1)
            .into_iter()
    }
}

impl StorageProvider for LocalFolder {
    fn kind(&self) -> &'static str {
        "local"
    }

    fn list(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for item in self.walk() {
            let de = item.map_err(|e| {
                let path = e
                    .path()
                    .and_then(|p| self.rel_path(p))
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                io_err(
                    path,
                    e.into_io_error()
                        .unwrap_or_else(|| io::Error::other("directory walk failed")),
                )
            })?;
            if !de.file_type().is_file() {
                continue;
            }
            if de.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
                continue;
            }
            let Some(rel) = self.rel_path(de.path()) else {
                tracing::warn!(path = %de.path().display(), "skipping file with invalid vault path");
                continue;
            };
            let meta = de
                .metadata()
                .map_err(|e| io_err(rel.to_string(), io::Error::other(e)))?;
            out.push(entry_from(&rel, &meta));
        }
        Ok(out)
    }

    fn list_dirs(&self) -> Result<Vec<VaultPath>> {
        let mut out = Vec::new();
        for item in self.walk() {
            let de = item.map_err(|e| {
                io_err(
                    String::new(),
                    e.into_io_error()
                        .unwrap_or_else(|| io::Error::other("directory walk failed")),
                )
            })?;
            if !de.file_type().is_dir() {
                continue;
            }
            match self.rel_path(de.path()) {
                Some(rel) => out.push(rel),
                None => {
                    tracing::warn!(path = %de.path().display(), "skipping directory with invalid vault path");
                }
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &VaultPath) -> Result<Option<Entry>> {
        let full = self.safe_resolve(path)?;
        match fs::metadata(&full) {
            Ok(m) if m.is_file() => Ok(Some(entry_from(path, &m))),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(path.to_string(), e)),
        }
    }

    fn read(&self, path: &VaultPath) -> Result<Vec<u8>> {
        let full = self.safe_resolve(path)?;
        fs::read(&full).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => StorageError::NotFound(path.clone()),
            _ => io_err(path.to_string(), e),
        })
    }

    fn write(&self, path: &VaultPath, data: &[u8], mode: WriteMode) -> Result<Entry> {
        let full = self.safe_resolve(path)?;
        let parent = full
            .parent()
            .ok_or_else(|| StorageError::InvalidPath(path.to_string()))?;
        let ioe = |e: io::Error| io_err(path.to_string(), e);
        fs::create_dir_all(parent).map_err(ioe)?;
        // Re-check after creating directories (a link could have been created in between).
        self.safe_resolve(path)?;

        let previous = match &mode {
            WriteMode::CreateNew => {
                if fs::symlink_metadata(&full).is_ok() {
                    return Err(StorageError::AlreadyExists(path.clone()));
                }
                None
            }
            WriteMode::Replace(expected) => {
                let current = self.stat(path)?;
                let actual = current.as_ref().map(|e| e.revision.clone());
                if actual.as_ref() != Some(expected) {
                    return Err(StorageError::Conflict {
                        path: path.clone(),
                        expected: expected.clone(),
                        actual,
                    });
                }
                current
            }
            WriteMode::Overwrite => self.stat(path)?,
        };

        let mut tmp = tempfile::Builder::new()
            .prefix(TMP_PREFIX)
            .tempfile_in(parent)
            .map_err(ioe)?;
        tmp.write_all(data).map_err(ioe)?;
        if let Some(prev) = &previous {
            ensure_newer(tmp.as_file(), prev.modified).map_err(ioe)?;
        }
        tmp.as_file().sync_all().map_err(ioe)?;

        let file = if matches!(mode, WriteMode::CreateNew) {
            tmp.persist_noclobber(&full).map_err(|e| {
                if e.error.kind() == io::ErrorKind::AlreadyExists {
                    StorageError::AlreadyExists(path.clone())
                } else {
                    ioe(e.error)
                }
            })?
        } else {
            tmp.persist(&full).map_err(|e| ioe(e.error))?
        };
        let meta = file.metadata().map_err(ioe)?;
        Ok(entry_from(path, &meta))
    }

    fn rename(&self, from: &VaultPath, to: &VaultPath) -> Result<Entry> {
        let src = self.safe_resolve(from)?;
        let dst = self.safe_resolve(to)?;
        match fs::metadata(&src) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(StorageError::NotFound(from.clone())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(from.clone()));
            }
            Err(e) => return Err(io_err(from.to_string(), e)),
        }
        if fs::symlink_metadata(&dst).is_ok() {
            return Err(StorageError::AlreadyExists(to.clone()));
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err(to.to_string(), e))?;
        }
        self.safe_resolve(to)?;
        fs::rename(&src, &dst).map_err(|e| io_err(to.to_string(), e))?;
        let meta = fs::metadata(&dst).map_err(|e| io_err(to.to_string(), e))?;
        Ok(entry_from(to, &meta))
    }

    fn trash(&self, path: &VaultPath) -> Result<VaultPath> {
        let src = self.safe_resolve(path)?;
        match fs::metadata(&src) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(StorageError::NotFound(path.clone())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(path.clone()));
            }
            Err(e) => return Err(io_err(path.to_string(), e)),
        }
        let base = format!(".selenite/trash/{}-{}", unix_secs(), path.file_name());
        let mut candidate = vault_path(&base)?;
        let mut n = 1;
        while fs::symlink_metadata(self.resolve(&candidate)).is_ok() {
            candidate = vault_path(&format!("{base}.{n}"))?;
            n += 1;
        }
        let dst = self.safe_resolve(&candidate)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err(candidate.to_string(), e))?;
        }
        self.safe_resolve(&candidate)?;
        fs::rename(&src, &dst).map_err(|e| io_err(path.to_string(), e))?;
        Ok(candidate)
    }

    fn create_dir(&self, path: &VaultPath) -> Result<()> {
        let full = self.safe_resolve(path)?;
        fs::create_dir_all(&full).map_err(|e| io_err(path.to_string(), e))?;
        self.safe_resolve(path)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn p(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }

    fn setup() -> (tempfile::TempDir, LocalFolder) {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFolder::open(dir.path()).unwrap();
        (dir, fs)
    }

    #[test]
    fn open_requires_existing_dir_and_create_makes_it() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(LocalFolder::open(&missing).is_err());
        let made = LocalFolder::create(&missing).unwrap();
        assert!(made.root().is_dir());
        assert_eq!(made.kind(), "local");
        let file = dir.path().join("f.txt");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(
            LocalFolder::open(&file),
            Err(StorageError::InvalidPath(_))
        ));
    }

    #[test]
    fn rapid_same_size_writes_always_get_a_new_revision() {
        // Revisions are mtime + size; quick equal-length saves can share a clock tick, which
        // would let a stale `Replace` through. Each write must still produce a new revision.
        let (_d, s) = setup();
        let path = p("Note.md");
        let mut rev = s
            .write(&path, b"v0\n", WriteMode::CreateNew)
            .unwrap()
            .revision;
        for i in 1..=50 {
            let data = format!("v{}\n", i % 10);
            let next = s
                .write(&path, data.as_bytes(), WriteMode::Replace(rev.clone()))
                .unwrap()
                .revision;
            assert_ne!(next, rev, "write {i} reused the previous revision");
            assert!(matches!(
                s.write(&path, b"xx\n", WriteMode::Replace(rev)),
                Err(StorageError::Conflict { .. })
            ));
            rev = next;
        }
        let next = s
            .write(&path, b"zz\n", WriteMode::Overwrite)
            .unwrap()
            .revision;
        assert_ne!(next, rev, "overwrite reused the previous revision");
    }

    #[test]
    fn a_write_never_leaves_the_mtime_at_or_before_the_previous_one() {
        // Simulates a coarse clock (two saves in one tick) or a clock that stepped back: the
        // existing file's mtime is ahead of "now" when the next write happens.
        let (d, s) = setup();
        let path = p("Note.md");
        s.write(&path, b"v1\n", WriteMode::CreateNew).unwrap();
        let ahead = SystemTime::now() + std::time::Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(d.path().join("Note.md"))
            .unwrap()
            .set_modified(ahead)
            .unwrap();
        let before = s.stat(&path).unwrap().unwrap();
        assert_eq!(before.modified, ahead);
        for mode in [
            WriteMode::Replace(before.revision.clone()),
            WriteMode::Overwrite,
        ] {
            let prev = s.stat(&path).unwrap().unwrap();
            let after = s.write(&path, b"v2\n", mode).unwrap();
            assert!(after.modified > prev.modified, "mtime did not move forward");
            assert_ne!(after.revision, prev.revision);
            assert_eq!(s.stat(&path).unwrap().unwrap().revision, after.revision);
        }
    }

    #[test]
    fn write_read_stat_roundtrip_creates_parents() {
        let (_d, s) = setup();
        let e = s
            .write(&p("a/b/c.md"), b"hello", WriteMode::CreateNew)
            .unwrap();
        assert_eq!(e.size, 5);
        assert_eq!(s.read(&p("a/b/c.md")).unwrap(), b"hello");
        assert_eq!(
            s.stat(&p("a/b/c.md")).unwrap().unwrap().revision,
            e.revision
        );
        assert!(s.stat(&p("zzz.md")).unwrap().is_none());
        assert!(matches!(
            s.read(&p("zzz.md")),
            Err(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn create_new_on_existing_is_already_exists() {
        let (_d, s) = setup();
        s.write(&p("a.md"), b"one", WriteMode::CreateNew).unwrap();
        let err = s
            .write(&p("a.md"), b"two", WriteMode::CreateNew)
            .unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(_)));
        assert_eq!(s.read(&p("a.md")).unwrap(), b"one");
    }

    #[test]
    fn replace_with_stale_revision_conflicts_and_leaves_file() {
        let (_d, s) = setup();
        let e1 = s.write(&p("a.md"), b"one", WriteMode::CreateNew).unwrap();
        let e2 = s
            .write(
                &p("a.md"),
                b"three!",
                WriteMode::Replace(e1.revision.clone()),
            )
            .unwrap();
        assert_ne!(e1.revision, e2.revision);
        let err = s
            .write(&p("a.md"), b"stale", WriteMode::Replace(e1.revision))
            .unwrap_err();
        match err {
            StorageError::Conflict { actual, .. } => assert_eq!(actual, Some(e2.revision)),
            other => panic!("expected conflict, got {other:?}"),
        }
        assert_eq!(s.read(&p("a.md")).unwrap(), b"three!");
        // Replace on a missing file is also a conflict.
        let err = s
            .write(
                &p("nope.md"),
                b"x",
                WriteMode::Replace(Revision("1-1".into())),
            )
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict { actual: None, .. }));
    }

    #[test]
    fn overwrite_replaces_existing_file() {
        let (_d, s) = setup();
        s.write(&p("a.md"), b"one", WriteMode::Overwrite).unwrap();
        s.write(&p("a.md"), b"two", WriteMode::Overwrite).unwrap();
        assert_eq!(s.read(&p("a.md")).unwrap(), b"two");
    }

    #[test]
    fn rename_moves_and_refuses_existing_target() {
        let (_d, s) = setup();
        s.write(&p("a.md"), b"A", WriteMode::CreateNew).unwrap();
        s.write(&p("b.md"), b"B", WriteMode::CreateNew).unwrap();
        assert!(matches!(
            s.rename(&p("a.md"), &p("b.md")),
            Err(StorageError::AlreadyExists(_))
        ));
        assert!(matches!(
            s.rename(&p("missing.md"), &p("c.md")),
            Err(StorageError::NotFound(_))
        ));
        let e = s.rename(&p("a.md"), &p("sub/dir/a2.md")).unwrap();
        assert_eq!(e.path, p("sub/dir/a2.md"));
        assert!(s.stat(&p("a.md")).unwrap().is_none());
        assert_eq!(s.read(&p("sub/dir/a2.md")).unwrap(), b"A");
    }

    #[test]
    fn trash_moves_file_out_of_list_without_deleting() {
        let (_d, s) = setup();
        s.write(&p("n/x.md"), b"keep me", WriteMode::CreateNew)
            .unwrap();
        let t = s.trash(&p("n/x.md")).unwrap();
        assert!(t.as_str().starts_with(".selenite/trash/"));
        assert!(t.as_str().ends_with("-x.md"));
        assert!(s.stat(&p("n/x.md")).unwrap().is_none());
        assert_eq!(s.read(&t).unwrap(), b"keep me");
        let listed: Vec<_> = s.list().unwrap().into_iter().map(|e| e.path).collect();
        assert!(!listed.contains(&p("n/x.md")));
        assert!(matches!(
            s.trash(&p("n/x.md")),
            Err(StorageError::NotFound(_))
        ));
        // Trashing a same-named file in the same second does not collide.
        s.write(&p("n/x.md"), b"again", WriteMode::CreateNew)
            .unwrap();
        let t2 = s.trash(&p("n/x.md")).unwrap();
        assert_ne!(t, t2);
        assert_eq!(s.read(&t).unwrap(), b"keep me");
    }

    #[test]
    fn list_includes_nested_and_hidden_excludes_tmp_and_dirs() {
        let (d, s) = setup();
        s.write(&p("top.md"), b"1", WriteMode::CreateNew).unwrap();
        s.write(&p("a/b/deep.md"), b"2", WriteMode::CreateNew)
            .unwrap();
        s.write(&p(".hidden/cfg.json"), b"3", WriteMode::CreateNew)
            .unwrap();
        s.write(&p(".dot"), b"4", WriteMode::CreateNew).unwrap();
        fs::create_dir_all(d.path().join("empty")).unwrap();
        fs::write(d.path().join("a").join(".selenite-tmp-abc"), b"junk").unwrap();
        let got: BTreeSet<String> = s
            .list()
            .unwrap()
            .into_iter()
            .map(|e| e.path.to_string())
            .collect();
        let want: BTreeSet<String> = ["top.md", "a/b/deep.md", ".hidden/cfg.json", ".dot"]
            .iter()
            .map(|x| x.to_string())
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn list_dirs_includes_empty_dirs() {
        let (_d, s) = setup();
        s.create_dir(&p("empty/inner")).unwrap();
        s.write(&p("full/f.md"), b"x", WriteMode::CreateNew)
            .unwrap();
        let got: BTreeSet<String> = s
            .list_dirs()
            .unwrap()
            .into_iter()
            .map(|d| d.to_string())
            .collect();
        let want: BTreeSet<String> = ["empty", "empty/inner", "full"]
            .iter()
            .map(|x| x.to_string())
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn no_temp_files_left_after_writes() {
        let (d, s) = setup();
        s.write(&p("a.md"), b"1", WriteMode::Overwrite).unwrap();
        s.write(&p("a.md"), b"2", WriteMode::Overwrite).unwrap();
        let _ = s.write(&p("a.md"), b"3", WriteMode::CreateNew);
        let names: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.md".to_string()]);
    }

    #[cfg(unix)]
    fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[test]
    fn symlinked_dir_pointing_outside_root_is_rejected() {
        let (d, s) = setup();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        if symlink_dir(outside.path(), &d.path().join("link")).is_err() {
            // Creating symlinks may need privileges (e.g. Windows without developer mode).
            return;
        }
        assert!(matches!(
            s.read(&p("link/secret.txt")),
            Err(StorageError::InvalidPath(_))
        ));
        assert!(matches!(
            s.write(&p("link/new.md"), b"x", WriteMode::Overwrite),
            Err(StorageError::InvalidPath(_))
        ));
        assert!(matches!(
            s.write(&p("link/sub/new.md"), b"x", WriteMode::Overwrite),
            Err(StorageError::InvalidPath(_))
        ));
        assert!(!outside.path().join("new.md").exists());
        assert!(!outside.path().join("sub").exists());
        // Listing does not follow the link either.
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn resolve_maps_segments_under_root() {
        let (_d, s) = setup();
        let r = s.resolve(&p("a/b.md"));
        assert!(r.starts_with(s.root()));
        assert!(r.ends_with(Path::new("a").join("b.md")));
    }
}

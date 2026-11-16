//! Validated, vault-relative file paths.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a string was rejected as a [`VaultPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The path was empty.
    #[error("path is empty")]
    Empty,
    /// The path was absolute or had a drive/root prefix.
    #[error("path must be relative to the vault root")]
    Absolute,
    /// The path contained `.` or `..` components or an empty segment.
    #[error("path contains a forbidden component")]
    BadComponent,
    /// The path contained a character that is not portable across platforms.
    #[error("path contains a forbidden character")]
    BadChar,
}

/// A normalised path relative to the vault root, always using `/` separators.
///
/// Construction rejects absolute paths and `..`, so a `VaultPath` can never escape the vault.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VaultPath(String);

impl VaultPath {
    /// Parse and validate a vault-relative path. Backslashes are accepted and normalised to `/`.
    pub fn new(raw: &str) -> Result<Self, PathError> {
        let s = raw.replace('\\', "/");
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s.starts_with('/') || s.chars().nth(1) == Some(':') {
            return Err(PathError::Absolute);
        }
        for seg in s.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err(PathError::BadComponent);
            }
            if seg.chars().any(|c| {
                c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            }) {
                return Err(PathError::BadChar);
            }
        }
        Ok(Self(s))
    }

    /// The path as a `/`-separated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Final path segment, e.g. `Note.md`.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// File name without its last extension, e.g. `Note` for `dir/Note.md`.
    pub fn stem(&self) -> &str {
        let name = self.file_name();
        match name.rfind('.') {
            Some(i) if i > 0 => &name[..i],
            _ => name,
        }
    }

    /// Lower-cased extension without the dot, if any.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        name.rfind('.')
            .filter(|&i| i > 0)
            .map(|i| name[i + 1..].to_ascii_lowercase())
    }

    /// Parent folder, or `None` for a file at the vault root.
    pub fn parent(&self) -> Option<&str> {
        self.0.rfind('/').map(|i| &self.0[..i])
    }

    /// Join a relative child path onto this path, treating `self` as a folder.
    pub fn join(&self, child: &str) -> Result<Self, PathError> {
        Self::new(&format!("{}/{}", self.0, child))
    }

    /// True if this is a Markdown page (`.md`).
    pub fn is_markdown(&self) -> bool {
        self.extension().as_deref() == Some("md")
    }

    /// True if any segment starts with `.` (hidden files such as `.selenite/` or `.obsidian/`).
    pub fn is_hidden(&self) -> bool {
        self.0.split('/').any(|s| s.starts_with('.'))
    }
}

impl fmt::Display for VaultPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for VaultPath {
    type Error = PathError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(&s)
    }
}

impl From<VaultPath> for String {
    fn from(p: VaultPath) -> Self {
        p.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_normalises() {
        let p = VaultPath::new("Daily\\2026-11-16.md").unwrap();
        assert_eq!(p.as_str(), "Daily/2026-11-16.md");
        assert_eq!(p.stem(), "2026-11-16");
        assert_eq!(p.parent(), Some("Daily"));
        assert!(p.is_markdown());
    }

    #[test]
    fn rejects_escapes() {
        assert_eq!(VaultPath::new("../x.md"), Err(PathError::BadComponent));
        assert_eq!(VaultPath::new("a//b.md"), Err(PathError::BadComponent));
        assert_eq!(VaultPath::new("/etc/passwd"), Err(PathError::Absolute));
        assert_eq!(VaultPath::new("C:/x.md"), Err(PathError::Absolute));
        assert_eq!(VaultPath::new("a?.md"), Err(PathError::BadChar));
        assert_eq!(VaultPath::new(""), Err(PathError::Empty));
    }

    #[test]
    fn hidden_detection() {
        assert!(VaultPath::new(".selenite/index.sqlite").unwrap().is_hidden());
        assert!(!VaultPath::new("notes/a.md").unwrap().is_hidden());
    }

    #[test]
    fn serde_round_trip_validates() {
        let p: VaultPath = serde_json_like("a/b.md");
        assert_eq!(p.as_str(), "a/b.md");
    }

    fn serde_json_like(s: &str) -> VaultPath {
        VaultPath::try_from(s.to_string()).unwrap()
    }
}

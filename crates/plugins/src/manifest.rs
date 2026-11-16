//! Plugin manifests: parsing and validation. A manifest is untrusted input.

use serde::{Deserialize, Serialize};

use crate::PluginError;

/// File name of the manifest inside a plugin folder.
pub const MANIFEST_FILE: &str = "manifest.json";
/// Longest accepted plugin id.
pub const MAX_ID_LEN: usize = 48;

/// A capability a plugin can ask for. Anything not listed in the manifest is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// `vault.search`, `vault.read`, `vault.backlinks`.
    Read,
    /// `vault.createPage`, `vault.appendBlocks`. Never delete, rename or settings.
    Write,
    /// `ui.registerCommand`.
    Commands,
}

impl Permission {
    /// The name used in manifests and audit entries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Commands => "commands",
        }
    }
}

fn default_main() -> String {
    "main.js".to_string()
}

/// A validated `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Unique id: lower-case letters, digits and single hyphens; also the folder name.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Version text (digits, letters, `.`, `-`, `+`).
    pub version: String,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// Requested permissions (no duplicates).
    pub permissions: Vec<Permission>,
    /// Script file inside the plugin folder (default `main.js`).
    #[serde(default = "default_main")]
    pub main: String,
}

/// True for an id such as `word-count`: `[a-z0-9]+(-[a-z0-9]+)*`, at most [`MAX_ID_LEN`] chars.
#[must_use]
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

fn bad(msg: impl Into<String>) -> PluginError {
    PluginError::InvalidManifest(msg.into())
}

impl Manifest {
    /// Parse and validate manifest JSON.
    ///
    /// # Errors
    ///
    /// [`PluginError::InvalidManifest`] for malformed JSON, an unknown permission, a bad id or
    /// `main`, empty or oversized fields, or duplicate permissions.
    pub fn parse(json: &str) -> Result<Self, PluginError> {
        let m: Manifest = serde_json::from_str(json).map_err(|e| bad(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    /// Check every field.
    ///
    /// # Errors
    ///
    /// [`PluginError::InvalidManifest`] describing the first problem.
    pub fn validate(&self) -> Result<(), PluginError> {
        if !is_valid_id(&self.id) {
            return Err(bad(format!(
                "invalid id `{}`: use lower-case letters, digits and single hyphens (at most \
                 {MAX_ID_LEN} characters)",
                self.id
            )));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 80 {
            return Err(bad("name must be 1 to 80 characters"));
        }
        let version_ok = !self.version.is_empty()
            && self.version.len() <= 32
            && self
                .version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        if !version_ok {
            return Err(bad(
                "version must be 1 to 32 characters of letters, digits, . - +",
            ));
        }
        if self.description.chars().count() > 300 {
            return Err(bad("description is longer than 300 characters"));
        }
        for (i, p) in self.permissions.iter().enumerate() {
            if self.permissions[..i].contains(p) {
                return Err(bad(format!("duplicate permission `{}`", p.as_str())));
            }
        }
        let main_ok = self.main.len() <= 64
            && self.main.ends_with(".js")
            && self.main.len() > 3
            && !self.main.starts_with('.')
            && self
                .main
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            && !self.main.contains("..");
        if !main_ok {
            return Err(bad(
                "main must be a .js file name in the plugin folder (letters, digits, . - _)",
            ));
        }
        Ok(())
    }

    /// Whether the manifest grants `p`.
    #[must_use]
    pub fn allows(&self, p: Permission) -> bool {
        self.permissions.contains(&p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Manifest, PluginError> {
        Manifest::parse(json)
    }

    const GOOD: &str = r#"{"id":"word-count","name":"Word count","version":"1.0.0",
        "description":"Counts words","permissions":["read","commands"],"main":"main.js"}"#;

    #[test]
    fn parses_a_good_manifest() {
        let m = parse(GOOD).expect("valid manifest");
        assert_eq!(m.id, "word-count");
        assert!(m.allows(Permission::Read) && m.allows(Permission::Commands));
        assert!(!m.allows(Permission::Write));
    }

    #[test]
    fn main_and_description_default() {
        let m = parse(r#"{"id":"a","name":"A","version":"0.1","permissions":[]}"#)
            .expect("valid manifest");
        assert_eq!(m.main, "main.js");
        assert_eq!(m.description, "");
    }

    #[test]
    fn rejects_bad_ids() {
        for id in [
            "",
            "Upper",
            "has space",
            "../evil",
            "a/b",
            "a\\b",
            "-lead",
            "trail-",
            "dou--ble",
            "under_score",
            "dot.dot",
            "ünï",
        ] {
            let json = format!(
                r#"{{"id":"{}","name":"x","version":"1","permissions":[]}}"#,
                id.replace('\\', "\\\\")
            );
            assert!(parse(&json).is_err(), "accepted id {id:?}");
        }
        let long = "a".repeat(MAX_ID_LEN + 1);
        let json = format!(r#"{{"id":"{long}","name":"x","version":"1","permissions":[]}}"#);
        assert!(parse(&json).is_err());
    }

    #[test]
    fn rejects_unknown_permissions() {
        for perm in ["network", "delete", "settings", "READ", "rename", ""] {
            let json = format!(r#"{{"id":"a","name":"x","version":"1","permissions":["{perm}"]}}"#);
            let err = parse(&json).expect_err("unknown permission accepted");
            assert!(matches!(err, PluginError::InvalidManifest(_)));
        }
    }

    #[test]
    fn rejects_duplicates_bad_main_and_garbage() {
        let dup = r#"{"id":"a","name":"x","version":"1","permissions":["read","read"]}"#;
        assert!(parse(dup).is_err());
        for main in [
            "../main.js",
            "sub/main.js",
            "main.html",
            ".js",
            "",
            "a b.js",
            "C:\\x.js",
        ] {
            let json = format!(
                r#"{{"id":"a","name":"x","version":"1","permissions":[],"main":"{}"}}"#,
                main.replace('\\', "\\\\")
            );
            assert!(parse(&json).is_err(), "accepted main {main:?}");
        }
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"id":"a"}"#).is_err());
        assert!(parse(r#"{"id":"a","name":" ","version":"1","permissions":[]}"#).is_err());
        assert!(parse(r#"{"id":"a","name":"x","version":"","permissions":[]}"#).is_err());
    }
}

//! Database folders: a folder containing [`SCHEMA_FILE`] plus one Markdown file per row.

use serde::{Deserialize, Serialize};

use crate::VaultError;

/// File name that turns a folder into a database.
pub const SCHEMA_FILE: &str = "_schema.yaml";

/// What kind of rows a database holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseKind {
    /// Generic rows with user-defined properties (Workspace mode databases).
    #[default]
    Table,
    /// Task rows (front matter keys from `selenite_core::TASK_KEYS`).
    Tasks,
}

/// A named, saved filter over a database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    /// Display name.
    pub name: String,
    /// Filter expression (task filter language for task databases).
    pub filter: String,
    /// Layout hint: `list`, `board`, `table`, `calendar`, `gallery`, `timeline`.
    #[serde(default = "default_layout")]
    pub layout: String,
}

fn default_layout() -> String {
    "list".into()
}

/// Contents of `_schema.yaml`. Unknown keys are ignored so newer files stay readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseSchema {
    /// Row kind.
    pub kind: DatabaseKind,
    /// Schema format version.
    pub version: u32,
    /// Saved views.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<SavedView>,
}

impl Default for DatabaseSchema {
    fn default() -> Self {
        Self {
            kind: DatabaseKind::Table,
            version: 1,
            views: Vec::new(),
        }
    }
}

impl DatabaseSchema {
    /// A new task database schema.
    pub fn tasks() -> Self {
        Self {
            kind: DatabaseKind::Tasks,
            ..Self::default()
        }
    }

    /// Parse a schema file. An empty document yields the defaults.
    pub fn from_yaml(text: &str) -> Result<Self, VaultError> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_norway::from_str(text).map_err(|e| VaultError::Config(e.to_string()))
    }

    /// Serialise to YAML.
    pub fn to_yaml(&self) -> Result<String, VaultError> {
        serde_norway::to_string(self).map_err(|e| VaultError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let mut s = DatabaseSchema::tasks();
        s.views.push(SavedView {
            name: "Urgent".into(),
            filter: "p1 & open".into(),
            layout: "board".into(),
        });
        let y = s.to_yaml().unwrap();
        assert_eq!(DatabaseSchema::from_yaml(&y).unwrap(), s);
        assert_eq!(
            DatabaseSchema::from_yaml("").unwrap().kind,
            DatabaseKind::Table
        );
        let partial = DatabaseSchema::from_yaml("kind: tasks\nfuture_key: 1\n").unwrap();
        assert_eq!(partial.kind, DatabaseKind::Tasks);
        let v = DatabaseSchema::from_yaml("views:\n  - name: A\n    filter: today\n").unwrap();
        assert_eq!(v.views[0].layout, "list");
    }
}

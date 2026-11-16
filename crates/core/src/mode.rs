//! Space modes: UI shells over the same file-based data.

use serde::{Deserialize, Serialize};

/// The interface style a space is presented in. Switching mode never migrates data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Plain Markdown files with a file tree, wikilinks, backlinks and daily notes.
    #[default]
    Vault,
    /// Block pages and databases with multiple views.
    Workspace,
    /// Task lists: inbox, today, upcoming, projects.
    Tasks,
}

impl Mode {
    /// All modes in display order.
    pub const ALL: [Mode; 3] = [Mode::Vault, Mode::Workspace, Mode::Tasks];

    /// Stable machine identifier stored in config files.
    pub fn id(self) -> &'static str {
        match self {
            Mode::Vault => "vault",
            Mode::Workspace => "workspace",
            Mode::Tasks => "tasks",
        }
    }
}

/// User-configurable display names for each mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeNames {
    /// Label for [`Mode::Vault`].
    pub vault: String,
    /// Label for [`Mode::Workspace`].
    pub workspace: String,
    /// Label for [`Mode::Tasks`].
    pub tasks: String,
}

impl Default for ModeNames {
    fn default() -> Self {
        Self {
            vault: "Vault".into(),
            workspace: "Workspace".into(),
            tasks: "Tasks".into(),
        }
    }
}

impl ModeNames {
    /// Display label for a mode.
    pub fn label(&self, mode: Mode) -> &str {
        match mode {
            Mode::Vault => &self.vault,
            Mode::Workspace => &self.workspace,
            Mode::Tasks => &self.tasks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_labels_match_ids_case_insensitively() {
        let names = ModeNames::default();
        for m in Mode::ALL {
            assert_eq!(names.label(m).to_lowercase(), m.id());
        }
    }
}

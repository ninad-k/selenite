//! Per-vault configuration stored at `.selenite/config.yaml`.

use selenite_core::{Mode, ModeNames};
use serde::{Deserialize, Serialize};

use crate::VaultError;

/// Vault-relative location of the configuration file.
pub const CONFIG_PATH: &str = ".selenite/config.yaml";

/// Per-vault settings. Missing fields fall back to their defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultConfig {
    /// Interface mode the space opens in.
    pub mode: Mode,
    /// Display names for each mode.
    pub mode_names: ModeNames,
    /// Folder holding daily notes (vault-relative, may be empty for the root).
    pub daily_folder: String,
    /// `chrono` format string for daily note file names (without extension).
    pub daily_format: String,
    /// Config schema version.
    pub version: u32,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            mode_names: ModeNames::default(),
            daily_folder: "Daily".into(),
            daily_format: "%Y-%m-%d".into(),
            version: 1,
        }
    }
}

impl VaultConfig {
    /// Parse a config file. An empty document yields the defaults.
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
    fn defaults() {
        let c = VaultConfig::default();
        assert_eq!(c.mode, Mode::Vault);
        assert_eq!(c.daily_folder, "Daily");
        assert_eq!(c.daily_format, "%Y-%m-%d");
        assert_eq!(c.version, 1);
        assert_eq!(CONFIG_PATH, ".selenite/config.yaml");
    }

    #[test]
    fn missing_fields_default() {
        assert_eq!(VaultConfig::from_yaml("").unwrap(), VaultConfig::default());
        assert_eq!(
            VaultConfig::from_yaml("{}").unwrap(),
            VaultConfig::default()
        );
        let c = VaultConfig::from_yaml("mode: tasks\n").unwrap();
        assert_eq!(c.mode, Mode::Tasks);
        assert_eq!(c.daily_folder, "Daily");
    }

    #[test]
    fn yaml_round_trip() {
        let c = VaultConfig {
            mode: Mode::Workspace,
            daily_folder: "Journal".into(),
            daily_format: "%d-%m-%Y".into(),
            ..VaultConfig::default()
        };
        let y = c.to_yaml().unwrap();
        assert_eq!(VaultConfig::from_yaml(&y).unwrap(), c);
    }

    #[test]
    fn bad_yaml_is_error() {
        assert!(matches!(
            VaultConfig::from_yaml("mode: [unterminated"),
            Err(VaultError::Config(_))
        ));
        assert!(VaultConfig::from_yaml("mode: nonsense\n").is_err());
    }
}

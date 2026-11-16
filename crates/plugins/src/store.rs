//! Finding plugins in the vault and recording which ones the user enabled.
//!
//! Plugins live in `.selenite/plugins/<id>/` (`manifest.json` plus the script named by `main`).
//! Everything goes through the engine's storage provider. Enabling is an explicit user action
//! stored in `.selenite/plugins/enabled.json`; it records a content hash of the manifest and
//! script, so a plugin whose files change afterwards (edited, replaced by sync) stops running
//! until the user enables it again. `enabled.json` is never synced.

use std::collections::BTreeMap;

use selenite_core::VaultPath;
use selenite_index::Engine;
use selenite_storage::{StorageError, WriteMode, content_hash};
use serde::{Deserialize, Serialize};

use crate::PluginError;
use crate::manifest::{MANIFEST_FILE, Manifest, Permission, is_valid_id};

/// Folder holding all plugins.
pub const PLUGINS_DIR: &str = ".selenite/plugins";
/// The enablement record.
pub const ENABLED_FILE: &str = ".selenite/plugins/enabled.json";
/// Largest accepted plugin script.
pub const MAX_SOURCE_BYTES: usize = 512 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct EnabledFile {
    #[serde(default)]
    version: u32,
    /// Plugin id to the content hash that was approved.
    #[serde(default)]
    enabled: BTreeMap<String, String>,
}

/// A plugin as listed to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginInfo {
    /// Folder name (equals the manifest id when the manifest is valid).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Version text.
    pub version: String,
    /// Description.
    pub description: String,
    /// Requested permissions.
    pub permissions: Vec<Permission>,
    /// True when the user enabled exactly the files that are on disk now.
    pub enabled: bool,
    /// True when the plugin was enabled but its files changed since; it will not run.
    pub changed: bool,
    /// Why the plugin cannot be used (invalid manifest, missing script, ...).
    pub error: Option<String>,
}

/// A plugin ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPlugin {
    /// The validated manifest.
    pub manifest: Manifest,
    /// The plugin script.
    pub source: String,
    /// Hash of manifest and script bytes (what enabling approves).
    pub hash: String,
}

fn vp(s: &str) -> Result<VaultPath, PluginError> {
    VaultPath::new(s).map_err(|e| PluginError::Io(e.to_string()))
}

fn io(e: &StorageError) -> PluginError {
    PluginError::Io(e.to_string())
}

fn read_enabled(engine: &Engine) -> Result<EnabledFile, PluginError> {
    let path = vp(ENABLED_FILE)?;
    match engine.storage().stat(&path).map_err(|e| io(&e))? {
        None => Ok(EnabledFile::default()),
        Some(_) => {
            let bytes = engine.storage().read(&path).map_err(|e| io(&e))?;
            // A damaged file means nothing is enabled, never that everything is.
            Ok(serde_json::from_slice(&bytes).unwrap_or_default())
        }
    }
}

fn write_enabled(engine: &Engine, file: &EnabledFile) -> Result<(), PluginError> {
    let json = serde_json::to_string_pretty(file).map_err(|e| PluginError::Io(e.to_string()))?;
    engine
        .storage()
        .write(&vp(ENABLED_FILE)?, json.as_bytes(), WriteMode::Overwrite)
        .map_err(|e| io(&e))?;
    Ok(())
}

/// Read and check the files of plugin `id` (enabled or not).
fn read_files(engine: &Engine, id: &str) -> Result<LoadedPlugin, PluginError> {
    if !is_valid_id(id) {
        return Err(PluginError::NotFound(id.to_string()));
    }
    let dir = format!("{PLUGINS_DIR}/{id}");
    let manifest_path = vp(&format!("{dir}/{MANIFEST_FILE}"))?;
    if engine
        .storage()
        .stat(&manifest_path)
        .map_err(|e| io(&e))?
        .is_none()
    {
        return Err(PluginError::NotFound(id.to_string()));
    }
    let manifest_bytes = engine.storage().read(&manifest_path).map_err(|e| io(&e))?;
    let manifest_text = String::from_utf8(manifest_bytes.clone())
        .map_err(|_| PluginError::InvalidManifest("manifest is not UTF-8".into()))?;
    let manifest = Manifest::parse(&manifest_text)?;
    if manifest.id != id {
        return Err(PluginError::InvalidManifest(format!(
            "folder `{id}` does not match manifest id `{}`",
            manifest.id
        )));
    }
    let main_path = vp(&format!("{dir}/{}", manifest.main))?;
    let entry = engine
        .storage()
        .stat(&main_path)
        .map_err(|e| io(&e))?
        .ok_or_else(|| PluginError::InvalidManifest(format!("missing script {}", manifest.main)))?;
    if usize::try_from(entry.size).unwrap_or(usize::MAX) > MAX_SOURCE_BYTES {
        return Err(PluginError::InvalidManifest(format!(
            "script is larger than {} KiB",
            MAX_SOURCE_BYTES / 1024
        )));
    }
    let source_bytes = engine.storage().read(&main_path).map_err(|e| io(&e))?;
    let source = String::from_utf8(source_bytes.clone())
        .map_err(|_| PluginError::InvalidManifest("script is not UTF-8".into()))?;
    let mut all = manifest_bytes;
    all.push(0);
    all.extend_from_slice(&source_bytes);
    Ok(LoadedPlugin {
        manifest,
        source,
        hash: content_hash(&all),
    })
}

/// All plugin folders with their state, ordered by id.
///
/// # Errors
///
/// [`PluginError::Io`] if storage cannot be listed. A broken plugin is reported in its
/// [`PluginInfo::error`], not as an error here.
pub fn discover(engine: &Engine) -> Result<Vec<PluginInfo>, PluginError> {
    let enabled = read_enabled(engine)?;
    let mut ids: Vec<String> = engine
        .storage()
        .list()
        .map_err(|e| io(&e))?
        .into_iter()
        .filter_map(|e| {
            let rest = e
                .path
                .as_str()
                .strip_prefix(PLUGINS_DIR)?
                .strip_prefix('/')?;
            let (dir, file) = rest.split_once('/')?;
            (file == MANIFEST_FILE).then(|| dir.to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids
        .into_iter()
        .map(|id| match read_files(engine, &id) {
            Ok(p) => {
                let approved = enabled.enabled.get(&id);
                PluginInfo {
                    enabled: approved == Some(&p.hash),
                    changed: approved.is_some_and(|h| *h != p.hash),
                    id,
                    name: p.manifest.name,
                    version: p.manifest.version,
                    description: p.manifest.description,
                    permissions: p.manifest.permissions,
                    error: None,
                }
            }
            Err(e) => PluginInfo {
                name: id.clone(),
                id,
                version: String::new(),
                description: String::new(),
                permissions: Vec::new(),
                enabled: false,
                changed: false,
                error: Some(e.to_string()),
            },
        })
        .collect())
}

/// Enable or disable plugin `id`. Enabling approves the files exactly as they are now.
///
/// This is only ever called from a user action in the desktop app; no plugin RPC reaches it.
///
/// # Errors
///
/// [`PluginError::NotFound`] or [`PluginError::InvalidManifest`] when enabling a missing or
/// broken plugin; [`PluginError::Io`] if the record cannot be written.
pub fn set_enabled(engine: &Engine, id: &str, enabled: bool) -> Result<(), PluginError> {
    let mut file = read_enabled(engine)?;
    file.version = 1;
    if enabled {
        let plugin = read_files(engine, id)?;
        file.enabled.insert(id.to_string(), plugin.hash);
    } else {
        file.enabled.remove(id);
    }
    write_enabled(engine, &file)
}

/// Load plugin `id` for running: it must exist, be valid and be enabled with unchanged files.
///
/// # Errors
///
/// [`PluginError::Disabled`] when not enabled, [`PluginError::Changed`] when its files changed
/// since it was enabled, otherwise the reason it cannot be read.
pub fn load_enabled(engine: &Engine, id: &str) -> Result<LoadedPlugin, PluginError> {
    let plugin = read_files(engine, id)?;
    let enabled = read_enabled(engine)?;
    match enabled.enabled.get(id) {
        None => Err(PluginError::Disabled(id.to_string())),
        Some(h) if *h != plugin.hash => Err(PluginError::Changed(id.to_string())),
        Some(_) => Ok(plugin),
    }
}

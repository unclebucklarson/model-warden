//! Persisted app configuration. Every value has a working default so a
//! missing or partial config file never blocks startup — the file only
//! records what the user changed.
//!
//! Pattern harvested from llamacppCodeConf (src/core/settings.rs).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Shelf directories to scan for GGUFs (the Ollama stores and HF hub
    /// cache are found automatically and aren't listed here).
    pub scan_dirs: Vec<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut scan_dirs = Vec::new();
        if let Some(home) = std::env::home_dir() {
            scan_dirs.push(home.join("models"));
        }
        Self { scan_dirs }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// `$XDG_CONFIG_HOME/modelwarden` (fallback `~/.config/modelwarden`).
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("modelwarden")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// `$XDG_STATE_HOME/modelwarden` (fallback `~/.local/state/modelwarden`) —
/// manifests and the merged inventory live here.
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/state")
        })
        .join("modelwarden")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_tolerates_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        // Partial file: unknown keys ignored, missing keys default.
        std::fs::write(&path, r#"{"unknown_future_key": true}"#).unwrap();
        let cfg = AppConfig::load(&path);
        assert_eq!(cfg, AppConfig::default());

        // Full roundtrip.
        let mut cfg2 = cfg.clone();
        cfg2.scan_dirs.push(PathBuf::from("/mnt/nas/models"));
        cfg2.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path), cfg2);

        // Garbage file → defaults, not a crash.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(AppConfig::load(&path), AppConfig::default());
    }
}

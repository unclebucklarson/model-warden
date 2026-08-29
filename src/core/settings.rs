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
    /// User-registered roots: removable drives, NAS mounts, backup targets.
    /// Identified by fs UUID / marker file so they survive remounts.
    pub roots: Vec<RegisteredRoot>,
    /// Auto-discover the Ollama stores and HF hub cache (default). Off means
    /// warden only looks at `scan_dirs` and registered roots.
    pub discover_stores: bool,
    /// HuggingFace token for gated/private repos. Stored plainly, like the
    /// hf CLI's own `~/.cache/huggingface/token`; leave unset to fall back
    /// to $HF_TOKEN or that file.
    pub hf_token: Option<String>,
}

/// A root the user registered with `warden roots add` (or the GUI dialog).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisteredRoot {
    /// Stable id — from the drive's fs UUID or its marker file, so the same
    /// drive keeps its identity across remount points.
    pub id: String,
    /// Where it was last mounted. A missing path means offline, not gone.
    pub path: PathBuf,
    pub label: Option<String>,
    pub fs_uuid: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut scan_dirs = Vec::new();
        if let Some(home) = std::env::home_dir() {
            scan_dirs.push(home.join("models"));
        }
        Self {
            scan_dirs,
            roots: Vec::new(),
            discover_stores: true,
            hf_token: None,
        }
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

/// Validate a scan-dirs edit before it touches the config: every path
/// must exist (named in the error when not), the list can't be empty
/// (warden needs a shelf — the first entry), duplicates collapse, and
/// order is preserved (downloads land in the FIRST entry).
pub fn normalize_scan_dirs(dirs: &[std::path::PathBuf]) -> anyhow::Result<Vec<std::path::PathBuf>> {
    use anyhow::{Context, bail};
    if dirs.is_empty() {
        bail!("at least one shelf directory is required");
    }
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for d in dirs {
        let canon = d
            .canonicalize()
            .with_context(|| format!("{} does not exist", d.display()))?;
        if !canon.is_dir() {
            bail!("{} is not a directory", canon.display());
        }
        if !out.contains(&canon) {
            out.push(canon);
        }
    }
    Ok(out)
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
    #[test]
    fn scan_dirs_normalize_validates_and_dedupes() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        // Warden needs a shelf: an empty list is refused.
        assert!(normalize_scan_dirs(&[]).is_err());
        // A missing directory is refused BY NAME, before anything saves.
        let err = normalize_scan_dirs(&[d.path().join("no-such")]).unwrap_err();
        assert!(format!("{err}").contains("no-such"), "{err}");
        // Valid dirs canonicalize; duplicates collapse; order survives.
        let out = normalize_scan_dirs(&[a.clone(), a.clone()]).unwrap();
        assert_eq!(out, vec![a.canonicalize().unwrap()]);
    }
}
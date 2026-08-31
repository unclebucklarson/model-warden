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
    /// Infallible load for read paths: a missing config means defaults.
    /// A *corrupt* config also yields defaults here, but `save` will
    /// refuse to overwrite it — see `load_checked`.
    pub fn load(path: &Path) -> Self {
        if path.exists() {
            tighten(path);
        }
        Self::load_checked(path).unwrap_or_default()
    }

    /// Distinguish "no config" (defaults, fine) from "unreadable config"
    /// (an error). Silently treating corruption as absence used to lose
    /// every registered root the moment anything called `save`.
    pub fn load_checked(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))
    }

    /// Write the config owner-only, atomically, and never over a file we
    /// could not parse (that file may hold roots this process never saw).
    pub fn save(&self, path: &Path) -> Result<()> {
        if path.exists() {
            Self::load_checked(path).with_context(|| {
                format!(
                    "refusing to overwrite {} — fix or remove it first",
                    path.display()
                )
            })?;
        }
        if let Some(dir) = path.parent() {
            create_private_dir(dir)?;
        }
        write_private(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// Create a directory tree that only the owner can enter — warden's
/// state and config hold a token, full paths of every model, and the
/// manifests that other code trusts.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write a file owner-only and atomically (temp + rename, so a crash
/// mid-write cannot leave the truncated file that `load` would then
/// treat as corrupt).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);
    tighten(&tmp);
    std::fs::rename(&tmp, path).with_context(|| format!("finalizing {}", path.display()))
}

/// Tighten anything an older version left group- or world-readable:
/// the config (it holds the HF token), the state directory, and every
/// file in it (manifests and the journal name every model path on the
/// machine). Cheap, idempotent, and self-healing across upgrades — both
/// binaries call this once at startup.
pub fn harden_existing() {
    let cfg = config_file();
    if cfg.exists() {
        tighten(&cfg);
    }
    for dir in [config_dir(), state_dir()] {
        if !dir.exists() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = std::fs::metadata(&dir)
                && md.permissions().mode() & 0o077 != 0
            {
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        harden_tree(&dir, 0);
    }
}

fn harden_tree(dir: &Path, depth: usize) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => harden_tree(&p, depth + 1),
            Ok(t) if t.is_file() => tighten(&p),
            _ => {}
        }
    }
}

/// Repair permissions on a file an older version left world-readable.
pub fn tighten(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(path)
            && md.permissions().mode() & 0o077 != 0
        {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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

    #[cfg(unix)]
    #[test]
    fn config_and_state_are_private_and_get_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nested/config.json");
        let cfg = AppConfig {
            hf_token: Some("hf_secret".into()),
            ..Default::default()
        };
        cfg.save(&cfg_path).unwrap();

        // The token lives here: owner-only, like hf's own token file.
        let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must not be readable by others");
        let dmode = std::fs::metadata(cfg_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700, "its directory must not be traversable");

        // A file left world-readable by an older version is tightened on
        // load rather than silently leaking forever.
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o664)).unwrap();
        let _ = AppConfig::load(&cfg_path);
        let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "load must repair loose permissions");
    }

    #[test]
    fn a_corrupt_config_is_refused_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        // Absence means defaults; corruption must NOT, or the next save
        // overwrites the user's registered roots with an empty list.
        assert!(AppConfig::load_checked(&path).is_err());
        assert!(AppConfig::load_checked(&dir.path().join("missing.json")).is_ok());
        // And save must refuse to clobber a file it could not parse.
        let cfg = AppConfig::default();
        assert!(cfg.save(&path).is_err());
    }
}
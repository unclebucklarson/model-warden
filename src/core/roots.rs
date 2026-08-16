//! Storage roots: the places model files live. Each root gets a stable id,
//! a kind, and — for user-registered drives — removable-media identity.
//!
//! Identity scheme (spike 4): the filesystem UUID from `/dev/disk/by-uuid`
//! is primary (a superblock property, stable across remounts); a
//! `.modelwarden/root-id` marker file on the drive is the fallback for
//! filesystems with weak or shared UUIDs — and it also means a drive
//! re-registered after a remount keeps its id. `by-uuid` only lists
//! *attached* devices, so an unplugged drive is identified by its stored
//! manifest, never by probing.

use crate::core::scan;
use crate::core::settings::{AppConfig, RegisteredRoot};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// Warden-owned: the user's shelf. Write operations allowed.
    Shelf,
    /// Foreign store — report-only, never written into.
    Ollama,
    /// Foreign store — report-only, never written into.
    HfHub,
    /// A user-registered drive or NAS mount — warden-owned, offline-able.
    Removable,
}

impl RootKind {
    /// Whether warden may create or link files inside this root.
    pub fn owned(self) -> bool {
        matches!(self, RootKind::Shelf | RootKind::Removable)
    }

    pub fn label(self) -> &'static str {
        match self {
            RootKind::Shelf => "shelf",
            RootKind::Ollama => "ollama",
            RootKind::HfHub => "hf-hub",
            RootKind::Removable => "drive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootSpec {
    /// Stable, filename-safe id. Discovered roots hash their path
    /// (`<kind>-<8 hex>`); registered drives use fs-UUID/marker identity.
    pub id: String,
    pub kind: RootKind,
    pub path: PathBuf,
    #[serde(default)]
    pub label: Option<String>,
}

/// Every root this machine knows: configured shelf dirs, the Ollama stores
/// and HF hub cache that exist, and every registered drive — including
/// offline ones (a missing drive is offline, not gone).
pub fn discover_roots(cfg: &AppConfig) -> Vec<RootSpec> {
    let mut roots = Vec::new();
    for dir in &cfg.scan_dirs {
        roots.push(RootSpec {
            id: root_id(RootKind::Shelf, &dir.to_string_lossy()),
            kind: RootKind::Shelf,
            path: dir.clone(),
            label: None,
        });
    }
    if cfg.discover_stores {
        for store in scan::default_ollama_stores() {
            roots.push(RootSpec {
                id: root_id(RootKind::Ollama, &store.to_string_lossy()),
                kind: RootKind::Ollama,
                path: store,
                label: None,
            });
        }
        if let Some(hub) = scan::default_hf_hub() {
            roots.push(RootSpec {
                id: root_id(RootKind::HfHub, &hub.to_string_lossy()),
                kind: RootKind::HfHub,
                path: hub,
                label: None,
            });
        }
    }
    for r in &cfg.roots {
        roots.push(RootSpec {
            id: r.id.clone(),
            kind: RootKind::Removable,
            path: r.path.clone(),
            label: r.label.clone(),
        });
    }
    roots
}

/// Register a directory (drive mount, NAS path) as a warden root and give
/// it durable identity. Idempotent on the marker: re-adding a remounted
/// drive keeps its id. Errors if the path is already registered.
pub fn register_root(
    cfg: &mut AppConfig,
    path: &Path,
    label: Option<String>,
) -> Result<RegisteredRoot> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let fs_uuid = fs_uuid_of(&path);
    let id = match read_marker(&path) {
        Some(id) => id,
        None => {
            let id = match &fs_uuid {
                Some(u) => format!("ext-{}", &u.replace('-', "").to_lowercase()[..8.min(u.len())]),
                None => generated_id(&path),
            };
            write_marker(&path, &id)?;
            id
        }
    };
    if cfg.roots.iter().any(|r| r.id == id || r.path == path) {
        bail!("{} is already registered (id {id})", path.display());
    }
    let root = RegisteredRoot {
        id,
        path,
        label,
        fs_uuid,
    };
    cfg.roots.push(root.clone());
    Ok(root)
}

/// The filesystem UUID of whatever holds `path`, when the kernel exposes
/// one: match the path's st_dev against the block devices under
/// `/dev/disk/by-uuid`. Filesystems with anonymous devices (tmpfs, btrfs
/// subvolume mounts) return None — the marker file covers those.
pub fn fs_uuid_of(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path).ok()?.dev();
    for e in std::fs::read_dir("/dev/disk/by-uuid").ok()?.flatten() {
        if let Ok(target) = std::fs::canonicalize(e.path())
            && let Ok(md) = std::fs::metadata(&target)
            && md.rdev() == dev
        {
            return Some(e.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

fn marker_path(root: &Path) -> PathBuf {
    root.join(".modelwarden/root-id")
}

fn read_marker(root: &Path) -> Option<String> {
    let id = std::fs::read_to_string(marker_path(root)).ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn write_marker(root: &Path, id: &str) -> Result<()> {
    let p = marker_path(root);
    std::fs::create_dir_all(p.parent().unwrap())
        .with_context(|| format!("creating {}", p.parent().unwrap().display()))?;
    std::fs::write(&p, format!("{id}\n")).with_context(|| format!("writing {}", p.display()))
}

/// Process-random id for filesystems with no usable UUID. Uniqueness comes
/// from `RandomState`'s per-process seed; stability comes from the marker
/// file this gets written into, not from re-derivation.
fn generated_id(path: &Path) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write(path.to_string_lossy().as_bytes());
    format!("ext-{:08x}", h.finish() as u32)
}

fn root_id(kind: RootKind, path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.as_bytes());
    format!(
        "{}-{:02x}{:02x}{:02x}{:02x}",
        kind.label(),
        digest[0],
        digest[1],
        digest[2],
        digest[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinguish_paths() {
        let a = root_id(RootKind::Shelf, "/home/x/models");
        assert_eq!(a, root_id(RootKind::Shelf, "/home/x/models"));
        assert_ne!(a, root_id(RootKind::Shelf, "/home/y/models"));
        assert!(a.starts_with("shelf-"));
    }

    #[test]
    fn owned_roots_are_shelf_and_removable() {
        assert!(RootKind::Shelf.owned());
        assert!(RootKind::Removable.owned());
        assert!(!RootKind::Ollama.owned());
        assert!(!RootKind::HfHub.owned());
    }

    #[test]
    fn registering_writes_a_marker_and_keeps_identity_across_reregistration() {
        let drive = tempfile::tempdir().unwrap();
        let mut cfg = AppConfig::default();
        // tmpfs has an anonymous device — exercises the marker fallback.
        let root = register_root(&mut cfg, drive.path(), Some("archive1".into())).unwrap();
        assert!(marker_path(&root.path).is_file());

        // Re-adding the same drive is refused but would keep the same id.
        let err = register_root(&mut cfg, drive.path(), None).unwrap_err();
        assert!(format!("{err}").contains("already registered"));

        // A fresh config (simulating another machine/session) re-reads the
        // marker: same drive, same id.
        let mut cfg2 = AppConfig::default();
        let again = register_root(&mut cfg2, drive.path(), None).unwrap();
        assert_eq!(again.id, root.id);
    }

    #[test]
    fn registered_roots_are_discovered_even_when_offline() {
        let mut cfg = AppConfig {
            scan_dirs: vec![],
            roots: vec![],
            discover_stores: false,
        };
        cfg.roots.push(crate::core::settings::RegisteredRoot {
            id: "ext-cafe0123".into(),
            path: "/media/nowhere/archive1".into(),
            label: Some("archive1".into()),
            fs_uuid: None,
        });
        let roots = discover_roots(&cfg);
        let ext = roots.iter().find(|r| r.id == "ext-cafe0123").unwrap();
        assert_eq!(ext.kind, RootKind::Removable);
        assert_eq!(ext.label.as_deref(), Some("archive1"));
    }
}

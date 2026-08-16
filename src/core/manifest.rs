//! Per-root manifests and the merged inventory.
//!
//! One JSON manifest per storage root (schema proven in spike 2), all under
//! warden's state dir — never inside a foreign store. The merged inventory
//! groups every location by content identity and is what consumers will
//! eventually read (schema_version stays 0 until it's published at M6).
//!
//! Writes are atomic (temp + rename) and keep a `.bak` of the previous
//! version. A root whose path is missing keeps its manifest — offline is
//! not gone.

use crate::core::gguf::GgufMeta;
use crate::core::identity::Fingerprint;
use crate::core::roots::{RootKind, RootSpec};
use crate::core::scan::{self, ModelFile, Source};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootManifest {
    pub schema_version: u32,
    pub root: RootSpec,
    pub generated_unix: u64,
    pub files: Vec<FileRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Relative to the root's path.
    pub rel_path: PathBuf,
    pub size: u64,
    /// `None` when the file was inaccessible at scan time.
    pub fingerprint: Option<Fingerprint>,
    /// The canonical identity, filled lazily by the hash worker.
    pub sha256: Option<String>,
    /// What the owning tool calls it (Ollama `model:tag`, HF `org/repo`).
    pub name: Option<String>,
    pub meta: Option<GgufMeta>,
    pub accessible: bool,
}

/// Scan one root and reconcile with its previous manifest: a file whose
/// fingerprint is unchanged keeps its stored sha256; anything changed or new
/// starts unhashed.
pub fn build_root_manifest(spec: &RootSpec, previous: Option<&RootManifest>) -> RootManifest {
    let models: Vec<ModelFile> = match spec.kind {
        RootKind::Shelf => scan::shelf_models(&spec.path),
        RootKind::Ollama => scan::ollama_models(&spec.path),
        RootKind::HfHub => scan::hf_hub_models(&spec.path),
    };
    let prior: BTreeMap<&Path, &FileRecord> = previous
        .map(|p| {
            p.files
                .iter()
                .map(|f| (f.rel_path.as_path(), f))
                .collect()
        })
        .unwrap_or_default();

    let files = models
        .iter()
        .map(|m| {
            let rel_path = m
                .path
                .strip_prefix(&spec.path)
                .unwrap_or(&m.path)
                .to_path_buf();
            let fingerprint = m.accessible.then(|| Fingerprint::of(&m.path).ok()).flatten();
            let sha256 = prior.get(rel_path.as_path()).and_then(|old| {
                (old.fingerprint.is_some() && old.fingerprint == fingerprint)
                    .then(|| old.sha256.clone())
                    .flatten()
            });
            let name = match &m.source {
                Source::Ollama { name } => Some(name.clone()),
                Source::HfHub { repo } => Some(repo.clone()),
                Source::Shelf => None,
            };
            FileRecord {
                rel_path,
                size: m.file_size,
                fingerprint,
                sha256,
                name,
                meta: m.meta.clone(),
                accessible: m.accessible,
            }
        })
        .collect();

    RootManifest {
        schema_version: SCHEMA_VERSION,
        root: spec.clone(),
        generated_unix: now_unix(),
        files,
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn manifest_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("roots")
}

pub fn manifest_path(state_dir: &Path, root_id: &str) -> PathBuf {
    manifest_dir(state_dir).join(format!("{root_id}.json"))
}

pub fn inventory_path(state_dir: &Path) -> PathBuf {
    state_dir.join("inventory.json")
}

pub fn load_manifest(path: &Path) -> Option<RootManifest> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Every stored manifest, whether or not its root is currently reachable —
/// this is exactly how offline media stay queryable.
pub fn load_all_manifests(state_dir: &Path) -> Vec<RootManifest> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(manifest_dir(state_dir)) else {
        return out;
    };
    for e in entries.flatten() {
        if e.path().extension().is_some_and(|x| x == "json")
            && let Some(m) = load_manifest(&e.path())
        {
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.root.id.cmp(&b.root.id));
    out
}

/// Atomic write with a `.bak` of what was there before.
pub fn save_json<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    if path.exists() {
        let bak = path.with_extension("json.bak");
        std::fs::rename(path, &bak).with_context(|| format!("keeping {}", bak.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("finalizing {}", path.display()))
}

// ---- merged inventory ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub schema_version: u32,
    pub generated_unix: u64,
    pub roots: Vec<RootSpec>,
    /// Keyed by identity: `sha256:<hex>` when hashed, `pending:<dev>:<ino>:<size>`
    /// while the hash worker hasn't reached it, `unknown:<root>:<rel>` when
    /// the bytes are unreachable.
    pub models: BTreeMap<String, ModelEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub size: u64,
    pub display_name: String,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub root_id: String,
    pub kind: RootKind,
    pub rel_path: PathBuf,
    pub accessible: bool,
    /// `(dev, ino)` — lets duplicate reporting tell hardlinks (one set of
    /// bytes) from real copies (reclaimable). (0,0) when unknown.
    pub dev: u64,
    pub ino: u64,
}

pub fn merge(manifests: &[RootManifest]) -> Inventory {
    let mut models: BTreeMap<String, ModelEntry> = BTreeMap::new();
    for m in manifests {
        let root_online = m.root.path.exists();
        for f in &m.files {
            let key = match (&f.sha256, &f.fingerprint) {
                (Some(h), _) => format!("sha256:{h}"),
                (None, Some(fp)) => format!("pending:{}:{}:{}", fp.dev, fp.ino, fp.size),
                (None, None) => format!("unknown:{}:{}", m.root.id, f.rel_path.display()),
            };
            let display_name = f.name.clone().unwrap_or_else(|| {
                f.rel_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| f.rel_path.display().to_string())
            });
            let entry = models.entry(key).or_insert_with(|| ModelEntry {
                size: f.size,
                display_name,
                locations: Vec::new(),
            });
            entry.locations.push(Location {
                root_id: m.root.id.clone(),
                kind: m.root.kind,
                rel_path: f.rel_path.clone(),
                accessible: f.accessible && root_online,
                dev: f.fingerprint.map(|fp| fp.dev).unwrap_or(0),
                ino: f.fingerprint.map(|fp| fp.ino).unwrap_or(0),
            });
        }
    }
    Inventory {
        schema_version: SCHEMA_VERSION,
        generated_unix: now_unix(),
        roots: manifests.iter().map(|m| m.root.clone()).collect(),
        models,
    }
}

/// Progress reporting for `refresh` — both frontends render these; a 22 GiB
/// file takes ~35s, so byte-level progress matters.
#[derive(Debug, Clone)]
pub enum RefreshEvent {
    HashStart { label: String, size: u64 },
    /// Sent every ~64 MiB, not every read.
    HashProgress { label: String, done: u64, total: u64 },
    HashDone { label: String, secs: f32 },
    HashFailed { label: String, error: String },
}

/// The whole M2 write path in one place: rescan the given roots, carry
/// forward hashes whose fingerprints still match, hash what's missing,
/// persist per-root manifests, and merge ALL stored manifests (offline roots
/// included) into the inventory. Returns the merged inventory.
/// Callers pass `roots::discover_roots(&cfg)`.
pub fn refresh(
    specs: &[RootSpec],
    state: &Path,
    mut on: impl FnMut(RefreshEvent),
) -> Result<Inventory> {
    use crate::core::identity;

    let mut manifests = Vec::new();
    for spec in specs {
        let previous = load_manifest(&manifest_path(state, &spec.id));
        manifests.push(build_root_manifest(spec, previous.as_ref()));
    }

    for m in &mut manifests {
        let root_path = m.root.path.clone();
        for f in &mut m.files {
            if f.sha256.is_some() || !f.accessible {
                continue;
            }
            let label = f
                .name
                .clone()
                .unwrap_or_else(|| f.rel_path.display().to_string());
            on(RefreshEvent::HashStart {
                label: label.clone(),
                size: f.size,
            });
            let started = std::time::Instant::now();
            let mut last_reported = 0u64;
            let result = identity::sha256_file(&root_path.join(&f.rel_path), |done, total| {
                if done - last_reported >= 64 * 1024 * 1024 || done == total {
                    last_reported = done;
                    on(RefreshEvent::HashProgress {
                        label: label.clone(),
                        done,
                        total,
                    });
                }
            });
            match result {
                Ok(hex) => {
                    f.sha256 = Some(hex);
                    on(RefreshEvent::HashDone {
                        label,
                        secs: started.elapsed().as_secs_f32(),
                    });
                }
                Err(e) => on(RefreshEvent::HashFailed {
                    label,
                    error: e.to_string(),
                }),
            }
        }
    }

    for m in &manifests {
        save_json(m, &manifest_path(state, &m.root.id))
            .with_context(|| format!("saving manifest for {}", m.root.path.display()))?;
    }
    let inv = merge(&load_all_manifests(state));
    save_json(&inv, &inventory_path(state)).context("saving inventory")?;
    Ok(inv)
}

pub fn load_inventory(state: &Path) -> Option<Inventory> {
    serde_json::from_str(&std::fs::read_to_string(inventory_path(state)).ok()?).ok()
}

#[derive(Debug, Clone, Serialize)]
pub struct DupGroup {
    pub sha256: String,
    pub size: u64,
    pub display_name: String,
    pub locations: Vec<Location>,
    /// Bytes freeable by collapsing distinct inodes to one: `(inodes-1) * size`.
    pub reclaimable: u64,
}

/// Hash-identical content present as more than one set of bytes. Hardlinked
/// paths count as ONE set — nothing to reclaim there.
pub fn dup_groups(inv: &Inventory) -> Vec<DupGroup> {
    let mut out = Vec::new();
    for (key, entry) in &inv.models {
        let Some(hash) = key.strip_prefix("sha256:") else {
            continue;
        };
        let mut inodes: Vec<(u64, u64)> = entry
            .locations
            .iter()
            .filter(|l| l.accessible)
            .map(|l| (l.dev, l.ino))
            .collect();
        inodes.sort();
        inodes.dedup();
        if inodes.len() > 1 {
            out.push(DupGroup {
                sha256: hash.to_string(),
                size: entry.size,
                display_name: entry.display_name.clone(),
                locations: entry.locations.clone(),
                reclaimable: (inodes.len() as u64 - 1) * entry.size,
            });
        }
    }
    out.sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gguf::tests::synthetic_gguf;

    fn shelf_spec(path: &Path) -> RootSpec {
        RootSpec {
            id: "shelf-test".into(),
            kind: RootKind::Shelf,
            path: path.to_path_buf(),
        }
    }

    #[test]
    fn manifest_roundtrips_through_disk() {
        let shelf = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shelf.path().join("M")).unwrap();
        std::fs::write(
            shelf.path().join("M/m.gguf"),
            synthetic_gguf("llama", 4096, 15),
        )
        .unwrap();

        let m = build_root_manifest(&shelf_spec(shelf.path()), None);
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].rel_path, PathBuf::from("M/m.gguf"));
        assert!(m.files[0].fingerprint.is_some());
        assert!(m.files[0].sha256.is_none(), "hashing is lazy");

        let path = manifest_path(state.path(), &m.root.id);
        save_json(&m, &path).unwrap();
        let loaded = load_all_manifests(state.path());
        assert_eq!(loaded, vec![m]);
    }

    #[test]
    fn unchanged_fingerprint_carries_the_hash_forward() {
        let shelf = tempfile::tempdir().unwrap();
        std::fs::write(shelf.path().join("m.gguf"), synthetic_gguf("llama", 1, 1)).unwrap();
        let spec = shelf_spec(shelf.path());

        let mut first = build_root_manifest(&spec, None);
        first.files[0].sha256 = Some("cafe".into());
        let second = build_root_manifest(&spec, Some(&first));
        assert_eq!(second.files[0].sha256.as_deref(), Some("cafe"));

        // Rewriting the file invalidates the stored hash.
        std::fs::write(
            shelf.path().join("m.gguf"),
            synthetic_gguf("qwen3", 2048, 15),
        )
        .unwrap();
        let third = build_root_manifest(&spec, Some(&first));
        assert!(third.files[0].sha256.is_none(), "changed bytes → rehash");
    }

    #[test]
    fn save_is_atomic_and_keeps_a_bak() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("roots/x.json");
        save_json(&serde_json::json!({"v": 1}), &path).unwrap();
        save_json(&serde_json::json!({"v": 2}), &path).unwrap();
        let cur: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let bak: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path.with_extension("json.bak")).unwrap())
                .unwrap();
        assert_eq!(cur["v"], 2);
        assert_eq!(bak["v"], 1);
    }

    #[test]
    fn merge_keeps_offline_roots_and_groups_by_hash() {
        let online = tempfile::tempdir().unwrap();
        let mut a = RootManifest {
            schema_version: SCHEMA_VERSION,
            root: shelf_spec(online.path()),
            generated_unix: 1,
            files: vec![FileRecord {
                rel_path: "m.gguf".into(),
                size: 100,
                fingerprint: Some(Fingerprint {
                    size: 100,
                    mtime_s: 0,
                    mtime_nsec: 0,
                    dev: 1,
                    ino: 10,
                }),
                sha256: Some("aa".into()),
                name: None,
                meta: None,
                accessible: true,
            }],
        };
        // Same content recorded on an unplugged drive.
        let mut b = a.clone();
        b.root = RootSpec {
            id: "shelf-offline".into(),
            kind: RootKind::Shelf,
            path: "/media/nowhere/archive".into(),
        };
        b.files[0].fingerprint = None;
        b.files[0].rel_path = "backup/m.gguf".into();
        a.root.id = "shelf-online".into();

        let inv = merge(&[a, b]);
        assert_eq!(inv.models.len(), 1, "one content, two locations");
        let entry = &inv.models["sha256:aa"];
        assert_eq!(entry.locations.len(), 2);
        let offline = entry
            .locations
            .iter()
            .find(|l| l.root_id == "shelf-offline")
            .unwrap();
        assert!(!offline.accessible, "offline, not gone — and not dropped");
    }

    #[test]
    fn refresh_hashes_once_then_reuses_stored_hashes() {
        let shelf = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(
            shelf.path().join("m.gguf"),
            synthetic_gguf("llama", 4096, 15),
        )
        .unwrap();
        let specs = vec![shelf_spec(shelf.path())];

        let mut events = Vec::new();
        let inv = refresh(&specs, state.path(), |ev| {
            if matches!(ev, RefreshEvent::HashStart { .. }) {
                events.push(());
            }
        })
        .unwrap();
        assert_eq!(events.len(), 1, "one file hashed");
        assert_eq!(inv.models.len(), 1);
        assert!(inv.models.keys().next().unwrap().starts_with("sha256:"));
        assert!(inventory_path(state.path()).exists());

        // Unchanged file: second refresh hashes nothing, identity survives.
        let mut second_events = Vec::new();
        let inv2 = refresh(&specs, state.path(), |ev| {
            if matches!(ev, RefreshEvent::HashStart { .. }) {
                second_events.push(());
            }
        })
        .unwrap();
        assert!(second_events.is_empty(), "fingerprint match → no rehash");
        assert_eq!(
            inv.models.keys().collect::<Vec<_>>(),
            inv2.models.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn dup_groups_ignore_hardlinks_and_rank_by_reclaimable() {
        let mut models = BTreeMap::new();
        let loc = |root: &str, ino: u64| Location {
            root_id: root.into(),
            kind: RootKind::Shelf,
            rel_path: "x".into(),
            accessible: true,
            dev: 1,
            ino,
        };
        // Two locations, same inode: a hardlink, not a duplicate.
        models.insert(
            "sha256:linked".to_string(),
            ModelEntry {
                size: 500,
                display_name: "linked".into(),
                locations: vec![loc("a", 1), loc("b", 1)],
            },
        );
        // Three locations, two inodes: one reclaimable copy.
        models.insert(
            "sha256:copied".to_string(),
            ModelEntry {
                size: 700,
                display_name: "copied".into(),
                locations: vec![loc("a", 2), loc("a", 3), loc("b", 2)],
            },
        );
        let inv = Inventory {
            schema_version: SCHEMA_VERSION,
            generated_unix: 0,
            roots: vec![],
            models,
        };
        let groups = dup_groups(&inv);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sha256, "copied");
        assert_eq!(groups[0].reclaimable, 700);
    }
}

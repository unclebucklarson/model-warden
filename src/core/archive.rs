//! Archival tiers: promote cache-owned models onto the shelf (no other tool
//! prunes it there), demote shelf models to cold storage.
//!
//! Promotion is harvested from llamacppCodeConf's `archive_to_shelf`,
//! rebuilt over the catalog: hardlink when source and shelf share a
//! filesystem (instant, zero extra disk), verified copy otherwise.
//!
//! Demotion is a verified copy to a registered cold root; the shelf copy is
//! removed only when the caller explicitly asks (`remove_source`) and only
//! after the cold copy's read-back hash matched — a verified move never
//! loses bytes.

use crate::core::backup::{self, BackupEvent};
use crate::core::identity;
use crate::core::manifest::{self, FileRecord, Inventory, Location, ModelEntry, RootManifest};
use crate::core::roots::{RootKind, RootSpec};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Pull a cache-owned model onto the shelf. Returns the new shelf path.
pub fn promote(
    inv: &Inventory,
    key: &str,
    entry: &ModelEntry,
    shelf_root: &Path,
    on: &mut impl FnMut(BackupEvent),
) -> Result<PathBuf> {
    if entry
        .locations
        .iter()
        .any(|l| l.kind.owned() && inv.live_accessible(l))
    {
        bail!("{} is already on owned storage", entry.display_name);
    }
    let Some(src_loc) = entry.locations.iter().find(|l| inv.live_accessible(l)) else {
        bail!("{} has no reachable copy", entry.display_name);
    };
    let src_root = inv
        .root(&src_loc.root_id)
        .context("source root missing from inventory")?;
    // Resolve symlinks (HF snapshots symlink into blobs/) so we link/copy
    // the real bytes.
    let src = src_root
        .path
        .join(&src_loc.rel_path)
        .canonicalize()
        .with_context(|| format!("resolving {}", src_loc.rel_path.display()))?;

    let (subdir, file_name) = match src_loc.kind {
        RootKind::Ollama => {
            let safe = entry.display_name.replace([':', '/'], "-");
            // Ollama blobs are extensionless; give the shelf copy a name.
            (safe.clone(), format!("{safe}.gguf"))
        }
        RootKind::HfHub => {
            // Path relative to the snapshot revision, so companions in
            // subfolders (1_Pooling/config.json) don't collide on promote.
            let sub: std::path::PathBuf =
                src_loc.rel_path.components().skip(3).collect();
            (
                entry
                    .display_name
                    .rsplit('/')
                    .next()
                    .unwrap_or("model")
                    .to_string(),
                if sub.as_os_str().is_empty() {
                    "model.gguf".to_string()
                } else {
                    sub.display().to_string()
                },
            )
        }
        RootKind::Shelf | RootKind::Removable => unreachable!("guarded above"),
    };
    let dir = shelf_root.join(subdir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(file_name);
    if dest.exists() {
        bail!("{} already exists — refusing to overwrite", dest.display());
    }
    if std::fs::hard_link(&src, &dest).is_err() {
        // Different filesystem: verified copy (three-way, .partial temp).
        let Some(hash) = key.strip_prefix("sha256:") else {
            bail!(
                "{} isn't hashed yet — run `warden hash` before a cross-device archive",
                entry.display_name
            );
        };
        backup::copy_verified(&src, &dest, hash, entry.size, &entry.display_name, backup::Publish::New, on)?;
    }
    Ok(dest)
}

pub struct DemoteOutcome {
    pub dest: PathBuf,
    pub removed_source: Option<PathBuf>,
}

/// Verified copy of a shelf-resident content to a cold root; with
/// `remove_source`, the shelf copy is deleted afterwards — strictly after
/// the cold copy's read-back hash matched. Updates the cold root's carried
/// manifest so the drive stays self-describing.
pub fn demote(
    inv: &Inventory,
    key: &str,
    entry: &ModelEntry,
    target: &RootSpec,
    remove_source: bool,
    on: &mut impl FnMut(BackupEvent),
) -> Result<DemoteOutcome> {
    if !target.kind.owned() {
        bail!("cold storage must be an owned root, not {}", target.kind.label());
    }
    if !target.path.is_dir() {
        bail!("{} is offline — cannot demote to it", target.path.display());
    }
    let Some(hash) = key.strip_prefix("sha256:") else {
        bail!("{} isn't hashed yet — run `warden hash` first", entry.display_name);
    };
    let Some(src_loc) = entry
        .locations
        .iter()
        .find(|l| l.kind == RootKind::Shelf && inv.live_accessible(l))
    else {
        bail!("{} has no live shelf copy to demote", entry.display_name);
    };
    let src_root = inv
        .root(&src_loc.root_id)
        .context("source root missing from inventory")?;
    let src = src_root.path.join(&src_loc.rel_path);

    let dest = target.path.join(&src_loc.rel_path);
    let fingerprint = if dest.exists() {
        // Already demoted earlier? Only acceptable if it's this content.
        let existing = identity::sha256_file(&dest, |_, _| {})?;
        if existing != hash {
            bail!("{} already exists with different content", dest.display());
        }
        identity::Fingerprint::of(&dest)?
    } else {
        backup::copy_verified(&src, &dest, hash, entry.size, &entry.display_name, backup::Publish::New, on)?
    };

    // Record on the drive's own manifest before anything is removed.
    let mpath = backup::target_manifest_path(&target.path);
    let mut man = manifest::load_manifest(&mpath)
        .map(|m| manifest::sanitize_carried(m, &target.path).0)
        .unwrap_or(RootManifest {
        schema_version: manifest::SCHEMA_VERSION,
        root: target.clone(),
        generated_unix: manifest::now_unix(),
        files: Vec::new(),
    });
    man.root = target.clone();
    if !man
        .files
        .iter()
        .any(|f| f.rel_path == src_loc.rel_path && f.sha256.as_deref() == Some(hash))
    {
        man.files.push(FileRecord {
            rel_path: src_loc.rel_path.clone(),
            size: entry.size,
            fingerprint: Some(fingerprint),
            sha256: Some(hash.to_string()),
            name: Some(entry.display_name.clone()),
            meta: entry.meta.clone(),
            accessible: true,
            verified_unix: Some(manifest::now_unix()),
        });
    }
    man.generated_unix = manifest::now_unix();
    manifest::save_json(&man, &mpath)?;

    let removed_source = if remove_source {
        std::fs::remove_file(&src).with_context(|| format!("removing {}", src.display()))?;
        Some(src)
    } else {
        None
    };
    Ok(DemoteOutcome {
        dest,
        removed_source,
    })
}

/// Bring a content back from a drive to the shelf — the return leg of
/// backup/demote. A verified copy (three-way, `.partial` temp), keeping the
/// layout the drive already has. The drive is never modified.
pub fn restore(
    inv: &Inventory,
    key: &str,
    entry: &ModelEntry,
    shelf_root: &Path,
    on: &mut impl FnMut(BackupEvent),
) -> Result<PathBuf> {
    let Some(hash) = key.strip_prefix("sha256:") else {
        bail!("{} isn't hashed in the catalog", entry.display_name);
    };
    if entry
        .locations
        .iter()
        .any(|l| l.kind == RootKind::Shelf && inv.live_accessible(l))
    {
        bail!("{} is already on the shelf", entry.display_name);
    }
    let Some(src_loc) = entry
        .locations
        .iter()
        .find(|l| l.kind == RootKind::Removable && inv.live_accessible(l))
    else {
        // Distinguish "drive not plugged in" from "no drive has it".
        if entry
            .locations
            .iter()
            .any(|l| l.kind == RootKind::Removable)
        {
            bail!(
                "{} lives on an offline drive — plug it in first (`warden where` names it)",
                entry.display_name
            );
        }
        bail!(
            "{} has no copy on any drive; if it's in a cache store, `warden archive` it instead",
            entry.display_name
        );
    };
    let src_root = inv
        .root(&src_loc.root_id)
        .context("source root missing from inventory")?;
    let src = src_root.path.join(&src_loc.rel_path);
    let dest = shelf_root.join(&src_loc.rel_path);
    if dest.exists() {
        bail!("{} already exists — refusing to overwrite", dest.display());
    }
    backup::copy_verified(&src, &dest, hash, entry.size, &entry.display_name, backup::Publish::New, on)?;
    Ok(dest)
}

/// Find catalog entries matching a query: name substring, path substring,
/// or a sha256 prefix — the tiebreaker when two contents share a name.
pub fn find<'a>(
    inv: &'a Inventory,
    query: &str,
) -> Vec<(&'a String, &'a ModelEntry)> {
    let q = query.to_lowercase();
    inv.models
        .iter()
        .filter(|(key, e)| {
            key.strip_prefix("sha256:")
                .is_some_and(|h| h.starts_with(&q))
                || e.display_name.to_lowercase().contains(&q)
                || e.locations
                    .iter()
                    .any(|l| l.rel_path.to_string_lossy().to_lowercase().contains(&q))
        })
        .collect()
}

/// The location a promote would read from — foreign stores only.
pub fn promotable_location<'a>(inv: &Inventory, entry: &'a ModelEntry) -> Option<&'a Location> {
    if entry
        .locations
        .iter()
        .any(|l| l.kind.owned() && inv.live_accessible(l))
    {
        return None;
    }
    entry.locations.iter().find(|l| inv.live_accessible(l))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gguf::tests::synthetic_gguf;
    use crate::core::manifest::{build_root_manifest, merge};

    /// A synthetic HF-hub root + shelf, both cataloged and hashed.
    fn world() -> (tempfile::TempDir, tempfile::TempDir, Inventory) {
        let hub = tempfile::tempdir().unwrap();
        let shelf = tempfile::tempdir().unwrap();
        let repo = hub.path().join("models--unsloth--Cold-GGUF");
        let blob_dir = repo.join("blobs");
        let snap = repo.join("snapshots/rev1");
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::create_dir_all(&snap).unwrap();
        let bytes = synthetic_gguf("qwen3", 4096, 15);
        std::fs::write(blob_dir.join("aabbcc"), &bytes).unwrap();
        std::os::unix::fs::symlink(blob_dir.join("aabbcc"), snap.join("Cold-Q4_K_M.gguf"))
            .unwrap();

        let hub_spec = RootSpec {
            id: "hf-hub-test".into(),
            kind: RootKind::HfHub,
            path: hub.path().to_path_buf(),
            label: None,
        };
        let shelf_spec = RootSpec {
            id: "shelf-test".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let mut manifests = vec![
            build_root_manifest(&hub_spec, None),
            build_root_manifest(&shelf_spec, None),
        ];
        for m in &mut manifests {
            let root = m.root.path.clone();
            for f in &mut m.files {
                f.sha256 =
                    Some(identity::sha256_file(&root.join(&f.rel_path), |_, _| {}).unwrap());
            }
        }
        let inv = merge(&manifests);
        (hub, shelf, inv)
    }

    #[test]
    fn promote_hardlinks_cache_files_onto_the_shelf() {
        use std::os::unix::fs::MetadataExt;
        let (hub, shelf, inv) = world();
        let (key, entry) = inv.models.iter().next().unwrap();
        let dest = promote(&inv, key, entry, shelf.path(), &mut |_| {}).unwrap();
        assert!(dest.ends_with("Cold-GGUF/Cold-Q4_K_M.gguf"));
        // Same tempfs → hardlink of the BLOB (symlink resolved).
        let blob = hub.path().join("models--unsloth--Cold-GGUF/blobs/aabbcc");
        assert_eq!(
            std::fs::metadata(&dest).unwrap().ino(),
            std::fs::metadata(&blob).unwrap().ino()
        );
        // Refuses to overwrite on a second run.
        assert!(promote(&inv, key, entry, shelf.path(), &mut |_| {}).is_err());
    }

    #[test]
    fn restore_brings_drive_copies_back_to_the_shelf() {
        let drive = tempfile::tempdir().unwrap();
        let shelf = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(drive.path().join("Fam")).unwrap();
        std::fs::write(
            drive.path().join("Fam/cold.gguf"),
            synthetic_gguf("llama", 2048, 15),
        )
        .unwrap();
        let drive_spec = RootSpec {
            id: "ext-drive001".into(),
            kind: RootKind::Removable,
            path: drive.path().to_path_buf(),
            label: Some("archive1".into()),
        };
        let mut man = build_root_manifest(&drive_spec, None);
        for f in &mut man.files {
            f.sha256 = Some(
                identity::sha256_file(&drive_spec.path.join(&f.rel_path), |_, _| {}).unwrap(),
            );
        }
        let inv = merge(&[man.clone()]);
        let (key, entry) = inv.models.iter().next().unwrap();

        let dest = restore(&inv, key, entry, shelf.path(), &mut |_| {}).unwrap();
        assert_eq!(dest, shelf.path().join("Fam/cold.gguf"));
        assert!(dest.is_file());
        assert!(
            drive.path().join("Fam/cold.gguf").is_file(),
            "the drive is never modified"
        );
        // Second restore refuses (already exists at dest).
        assert!(restore(&inv, key, entry, shelf.path(), &mut |_| {}).is_err());

        // Offline drive: helpful refusal, not a copy attempt.
        let mut offline = man;
        offline.root.path = "/media/nowhere/unplugged".into();
        let inv2 = merge(&[offline]);
        let (key2, entry2) = inv2.models.iter().next().unwrap();
        let err = restore(&inv2, key2, entry2, shelf.path(), &mut |_| {}).unwrap_err();
        assert!(format!("{err}").contains("offline drive"));
    }

    #[test]
    fn demote_moves_bytes_only_after_verification() {
        let (_hub, shelf, _) = world();
        // Give the shelf its own file and catalog it.
        std::fs::create_dir_all(shelf.path().join("Big")).unwrap();
        std::fs::write(
            shelf.path().join("Big/warm.gguf"),
            synthetic_gguf("llama", 2048, 15),
        )
        .unwrap();
        let shelf_spec = RootSpec {
            id: "shelf-test".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let mut man = build_root_manifest(&shelf_spec, None);
        for f in &mut man.files {
            f.sha256 = Some(
                identity::sha256_file(&shelf_spec.path.join(&f.rel_path), |_, _| {}).unwrap(),
            );
        }
        let inv = merge(&[man]);
        let (key, entry) = inv
            .models
            .iter()
            .find(|(_, e)| e.display_name == "warm")
            .unwrap();

        let cold = tempfile::tempdir().unwrap();
        let cold_spec = RootSpec {
            id: "ext-cold0001".into(),
            kind: RootKind::Removable,
            path: cold.path().to_path_buf(),
            label: Some("cold".into()),
        };
        // Without remove_source: copy exists, original stays.
        let out = demote(&inv, key, entry, &cold_spec, false, &mut |_| {}).unwrap();
        assert!(out.dest.is_file());
        assert!(out.removed_source.is_none());
        assert!(shelf.path().join("Big/warm.gguf").is_file());
        // The drive's carried manifest knows the file.
        let carried =
            manifest::load_manifest(&backup::target_manifest_path(cold.path())).unwrap();
        assert_eq!(carried.files.len(), 1);

        // With remove_source: idempotent on the copy, then the shelf copy goes.
        let out2 = demote(&inv, key, entry, &cold_spec, true, &mut |_| {}).unwrap();
        assert_eq!(out2.removed_source.as_deref(), Some(shelf.path().join("Big/warm.gguf").as_path()));
        assert!(!shelf.path().join("Big/warm.gguf").exists());
        assert!(out2.dest.is_file(), "cold copy intact");
    }
}

//! Reclaim: collapse hash-identical copies to one set of bytes by
//! hardlinking — the only way warden ever frees space, because it provably
//! preserves the bytes.
//!
//! Rules, in order of "never lose data": only within one filesystem
//! (hardlinks can't cross devices), only paths in warden-owned roots
//! (foreign stores are reported, never touched), and both sides are
//! re-hashed immediately before linking — the catalog's word alone is not
//! enough to destroy an inode. The link lands via a temp name + rename, so
//! no window exists where the path is missing.

use crate::core::identity;
use crate::core::manifest::{self, DupGroup, Inventory};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReclaimReport {
    pub relinked: Vec<PathBuf>,
    pub freed: u64,
    /// Duplicate inodes that live (partly) in foreign stores — reported,
    /// untouched.
    pub skipped_foreign: usize,
    pub failed: usize,
}

#[derive(Debug, Clone)]
pub enum ReclaimEvent {
    Group { name: String, size: u64 },
    Verifying { path: PathBuf },
    Relinked { path: PathBuf },
    SkippedForeign { path: PathBuf },
    Failed { path: PathBuf, error: String },
}

/// Hardlink-collapse every reclaimable duplicate group. `dry_run` walks the
/// same decisions without touching anything (the CLI's default mode).
pub fn reclaim(
    inv: &Inventory,
    dry_run: bool,
    mut on: impl FnMut(ReclaimEvent),
) -> Result<ReclaimReport> {
    let mut report = ReclaimReport::default();
    for group in manifest::dup_groups(inv) {
        on(ReclaimEvent::Group {
            name: group.display_name.clone(),
            size: group.size,
        });
        reclaim_group(inv, &group, dry_run, &mut report, &mut on);
    }
    Ok(report)
}

fn reclaim_group(
    inv: &Inventory,
    group: &DupGroup,
    dry_run: bool,
    report: &mut ReclaimReport,
    on: &mut impl FnMut(ReclaimEvent),
) {
    // Locations by (dev, ino): each inode is one set of bytes with 1+ paths.
    let mut inodes: BTreeMap<(u64, u64), Vec<&manifest::Location>> = BTreeMap::new();
    for l in group
        .locations
        .iter()
        .filter(|l| l.dev != 0 && inv.live_accessible(l))
    {
        inodes.entry((l.dev, l.ino)).or_default().push(l);
    }
    // Per device: the inode with the most paths survives (it is already the
    // most-shared copy); owned-root inodes win ties.
    let mut by_dev: BTreeMap<u64, Vec<((u64, u64), Vec<&manifest::Location>)>> = BTreeMap::new();
    for (key, locs) in inodes {
        by_dev.entry(key.0).or_default().push((key, locs));
    }
    for (_dev, mut sets) in by_dev {
        if sets.len() < 2 {
            continue;
        }
        sets.sort_by_key(|(_, locs)| {
            (
                std::cmp::Reverse(locs.len()),
                std::cmp::Reverse(locs.iter().any(|l| l.kind.owned())),
            )
        });
        let (survivor_key, survivor_locs) = sets[0].clone();
        let Some(survivor_abs) = abs_path(inv, survivor_locs[0]) else {
            continue;
        };
        let mut survivor_checked = false;
        for (victim_key, victim_locs) in sets.into_iter().skip(1) {
            debug_assert_ne!(victim_key, survivor_key);
            // Every path of the victim inode must be in an owned root —
            // replacing only some paths frees nothing, and foreign stores
            // are never touched.
            if !victim_locs.iter().all(|l| l.kind.owned()) {
                for l in &victim_locs {
                    on(ReclaimEvent::SkippedForeign {
                        path: abs_path(inv, l).unwrap_or_else(|| l.rel_path.clone()),
                    });
                }
                report.skipped_foreign += 1;
                continue;
            }
            let victim_paths: Vec<PathBuf> =
                victim_locs.iter().filter_map(|l| abs_path(inv, l)).collect();
            if victim_paths.len() != victim_locs.len() {
                report.failed += 1;
                continue;
            }
            if dry_run {
                report.relinked.extend(victim_paths);
                report.freed += group.size;
                continue;
            }
            // The catalog says these match; prove it against the bytes as
            // they are RIGHT NOW before destroying an inode.
            if !survivor_checked {
                on(ReclaimEvent::Verifying {
                    path: survivor_abs.clone(),
                });
                match identity::sha256_file(&survivor_abs, |_, _| {}) {
                    Ok(h) if h == group.sha256 => survivor_checked = true,
                    Ok(_) | Err(_) => {
                        on(ReclaimEvent::Failed {
                            path: survivor_abs.clone(),
                            error: "survivor no longer matches the catalog — rerun `warden hash`"
                                .into(),
                        });
                        report.failed += 1;
                        return;
                    }
                }
            }
            on(ReclaimEvent::Verifying {
                path: victim_paths[0].clone(),
            });
            match identity::sha256_file(&victim_paths[0], |_, _| {}) {
                Ok(h) if h == group.sha256 => {}
                Ok(_) | Err(_) => {
                    on(ReclaimEvent::Failed {
                        path: victim_paths[0].clone(),
                        error: "no longer matches the catalog — rerun `warden hash`".into(),
                    });
                    report.failed += 1;
                    continue;
                }
            }
            let mut ok = true;
            for victim in &victim_paths {
                if let Err(e) = relink(&survivor_abs, victim) {
                    on(ReclaimEvent::Failed {
                        path: victim.clone(),
                        error: format!("{e:#}"),
                    });
                    report.failed += 1;
                    ok = false;
                    break;
                }
                on(ReclaimEvent::Relinked {
                    path: victim.clone(),
                });
                report.relinked.push(victim.clone());
            }
            if ok {
                report.freed += group.size;
            }
        }
    }
}

/// Replace `victim` with a hardlink to `survivor` atomically: link to a
/// temp name, rename over. The path never goes missing; the old inode is
/// freed when its last reference drops.
///
/// The survivor is canonicalized first: `hard_link` does NOT follow
/// symlinks, and an HF snapshot path IS a symlink into blobs/ — linking it
/// raw would plant a relative symlink that dangles from the victim's
/// directory (found the hard way on real data). Link the bytes, never the
/// pointer.
fn relink(survivor: &std::path::Path, victim: &std::path::Path) -> Result<()> {
    use anyhow::Context;
    let real = survivor
        .canonicalize()
        .with_context(|| format!("resolving {}", survivor.display()))?;
    let tmp = victim.with_extension("gguf.wardenlink");
    std::fs::hard_link(&real, &tmp)
        .with_context(|| format!("linking {} → {}", real.display(), tmp.display()))?;
    std::fs::rename(&tmp, victim).with_context(|| format!("replacing {}", victim.display()))
}

fn abs_path(inv: &Inventory, loc: &manifest::Location) -> Option<PathBuf> {
    Some(inv.root(&loc.root_id)?.path.join(&loc.rel_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gguf::tests::synthetic_gguf;
    use crate::core::manifest::{build_root_manifest, merge};
    use crate::core::roots::{RootKind, RootSpec};

    /// Shelf with the same content twice (independent copies) and one
    /// unrelated file.
    fn dup_world() -> (tempfile::TempDir, Inventory) {
        let shelf = tempfile::tempdir().unwrap();
        let bytes = synthetic_gguf("qwen3", 4096, 15);
        std::fs::create_dir_all(shelf.path().join("A")).unwrap();
        std::fs::create_dir_all(shelf.path().join("B")).unwrap();
        std::fs::write(shelf.path().join("A/model.gguf"), &bytes).unwrap();
        std::fs::write(shelf.path().join("B/copy.gguf"), &bytes).unwrap();
        std::fs::write(
            shelf.path().join("A/other.gguf"),
            synthetic_gguf("llama", 1024, 1),
        )
        .unwrap();
        let spec = RootSpec {
            id: "shelf-test".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let mut man = build_root_manifest(&spec, None);
        for f in &mut man.files {
            f.sha256 = Some(
                identity::sha256_file(&spec.path.join(&f.rel_path), |_, _| {}).unwrap(),
            );
        }
        (shelf, merge(&[man]))
    }

    #[test]
    fn dry_run_reports_but_touches_nothing() {
        use std::os::unix::fs::MetadataExt;
        let (shelf, inv) = dup_world();
        let before = std::fs::metadata(shelf.path().join("B/copy.gguf"))
            .unwrap()
            .ino();
        let report = reclaim(&inv, true, |_| {}).unwrap();
        assert_eq!(report.relinked.len(), 1);
        assert!(report.freed > 0);
        assert_eq!(
            std::fs::metadata(shelf.path().join("B/copy.gguf"))
                .unwrap()
                .ino(),
            before,
            "dry run must not relink"
        );
    }

    #[test]
    fn reclaim_collapses_owned_copies_to_one_inode() {
        use std::os::unix::fs::MetadataExt;
        let (shelf, inv) = dup_world();
        let report = reclaim(&inv, false, |_| {}).unwrap();
        assert_eq!(report.relinked.len(), 1);
        assert_eq!(report.failed, 0);
        let a = std::fs::metadata(shelf.path().join("A/model.gguf")).unwrap();
        let b = std::fs::metadata(shelf.path().join("B/copy.gguf")).unwrap();
        assert_eq!(a.ino(), b.ino(), "one set of bytes now");
        // Unrelated file untouched, no temp litter.
        assert!(shelf.path().join("A/other.gguf").is_file());
        assert!(!shelf.path().join("B/copy.gguf.wardenlink").exists());
    }

    #[test]
    fn relink_through_a_symlink_survivor_links_the_bytes_not_the_pointer() {
        use std::os::unix::fs::MetadataExt;
        // Regression: real-data incident. The survivor's catalog path was an
        // HF snapshot SYMLINK; hard_link doesn't follow symlinks, so the
        // victim became a dangling relative symlink. relink must resolve to
        // the real bytes first.
        let world = tempfile::tempdir().unwrap();
        let bytes = synthetic_gguf("qwen3", 4096, 15);
        // hub-style: blob + snapshot symlink with a RELATIVE target.
        let repo = world.path().join("hub/models--org--R");
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        std::fs::create_dir_all(repo.join("snapshots/rev1")).unwrap();
        std::fs::write(repo.join("blobs/bee"), &bytes).unwrap();
        std::os::unix::fs::symlink("../../blobs/bee", repo.join("snapshots/rev1/m.gguf"))
            .unwrap();
        // shelf: an independent duplicate copy (the victim-to-be), plus an
        // archived hardlink of the blob — that makes the blob's inode the
        // most-shared (survivor), with the HF SYMLINK path listed first
        // among its locations, exactly the real-data shape.
        let shelf = world.path().join("shelf");
        std::fs::create_dir_all(&shelf).unwrap();
        std::fs::write(shelf.join("m-copy.gguf"), &bytes).unwrap();
        std::fs::hard_link(repo.join("blobs/bee"), shelf.join("m-archived.gguf")).unwrap();

        let specs = [
            RootSpec {
                id: "hf-test".into(),
                kind: RootKind::HfHub,
                path: world.path().join("hub"),
                label: None,
            },
            RootSpec {
                id: "shelf-test".into(),
                kind: RootKind::Shelf,
                path: shelf.clone(),
                label: None,
            },
        ];
        let mut manifests: Vec<_> = specs.iter().map(|s| build_root_manifest(s, None)).collect();
        for m in &mut manifests {
            let root = m.root.path.clone();
            for f in &mut m.files {
                f.sha256 = Some(
                    identity::sha256_file(&root.join(&f.rel_path), |_, _| {}).unwrap(),
                );
            }
        }
        let inv = merge(&manifests);
        let report = reclaim(&inv, false, |_| {}).unwrap();
        assert_eq!(report.failed, 0);
        assert_eq!(report.relinked.len(), 1);
        let victim = &report.relinked[0];
        let md = std::fs::symlink_metadata(victim).unwrap();
        assert!(
            md.file_type().is_file(),
            "victim must be a regular hardlink, not a symlink: {victim:?}"
        );
        assert_eq!(
            md.ino(),
            std::fs::metadata(repo.join("blobs/bee")).unwrap().ino(),
            "and it must share the blob's inode"
        );
    }

    #[test]
    fn changed_bytes_abort_the_relink() {
        let (shelf, inv) = dup_world();
        // The catalog is now stale for this file.
        std::fs::write(shelf.path().join("B/copy.gguf"), b"changed after hash").unwrap();
        let report = reclaim(&inv, false, |_| {}).unwrap();
        assert_eq!(report.relinked.len(), 0);
        assert_eq!(report.failed, 1);
        assert_eq!(
            std::fs::read(shelf.path().join("B/copy.gguf")).unwrap(),
            b"changed after hash",
            "nothing was destroyed"
        );
    }

    #[test]
    fn foreign_store_copies_are_never_touched() {
        let (_shelf, mut inv) = dup_world();
        // Rewrite the duplicate's second location as if it lived in the HF
        // cache (foreign kind) — same dev/ino data.
        let key = inv
            .models
            .iter()
            .find(|(_, e)| e.locations.len() > 1)
            .map(|(k, _)| k.clone())
            .unwrap();
        let entry = inv.models.get_mut(&key).unwrap();
        entry.locations[1].kind = RootKind::HfHub;
        let report = reclaim(&inv, false, |_| {}).unwrap();
        assert_eq!(report.relinked.len(), 0);
        assert_eq!(report.skipped_foreign, 1);
    }
}

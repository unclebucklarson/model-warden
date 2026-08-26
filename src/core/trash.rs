//! Two-stage deletion: the only path by which warden ever destroys model
//! bytes, and it takes two separate decisions to get there.
//!
//! Stage 1 — `delete`: the bundle is *moved* (same-filesystem rename —
//! instant, free) into `<root>/.modelwarden/trash/`, keeping its relative
//! layout. Trashed files leave every scan and view but stay fully intact;
//! `restore` is a rename back. Nothing is destroyed.
//!
//! Stage 2 — `empty`: the explicit, irreversible act. Only here do bytes
//! stop existing, and only files strictly inside a trash directory.
//!
//! Foreign stores (Ollama, HF cache) are never touched: deleting a model
//! that also lives there yields the owning tool's own command for the
//! user to run themselves — offered, never executed.
//!
//! Companions shared with a surviving model (an mmproj two models need)
//! are automatically kept — the bundle asymmetry identifies them.

use crate::core::manifest::{self, Inventory};
use crate::core::roots::{RootKind, RootSpec};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn trash_dir(root: &Path) -> PathBuf {
    root.join(".modelwarden/trash")
}

/// What actually gets deleted for a selection, and what is spared: the
/// union of the selected models' bundles, minus any content a model
/// OUTSIDE the selection still needs.
pub fn deletable_set(inv: &Inventory, keys: &[String]) -> (BTreeSet<String>, Vec<(String, String)>) {
    let selected: BTreeSet<&String> = keys.iter().collect();
    let union: BTreeSet<String> = keys
        .iter()
        .flat_map(|k| manifest::bundle_for(inv, k))
        .collect();
    let mut del = BTreeSet::new();
    let mut kept = Vec::new();
    'cand: for c in union {
        for (other, e) in &inv.models {
            if selected.contains(other) || *other == c {
                continue;
            }
            if manifest::bundle_for(inv, other).iter().any(|m| *m == c) {
                let name = inv
                    .models
                    .get(&c)
                    .map(|ce| ce.display_name.clone())
                    .unwrap_or_else(|| c.clone());
                kept.push((name, format!("still required by {}", e.display_name)));
                continue 'cand;
            }
        }
        del.insert(c);
    }
    (del, kept)
}

#[derive(Debug, Default)]
pub struct TrashReport {
    /// (display name, path now inside a trash dir)
    pub trashed: Vec<(String, PathBuf)>,
    /// (display name, the owning tool's command — offered, never run)
    pub foreign: Vec<(String, String)>,
    /// (display name, offline root label/id the copy is stranded on)
    pub offline: Vec<(String, String)>,
}

/// Stage 1: move every live owned-root copy of the given contents into
/// its root's trash. Renames only — no bytes are copied or destroyed.
pub fn move_to_trash(inv: &Inventory, del: &BTreeSet<String>) -> Result<TrashReport> {
    let mut report = TrashReport::default();
    for key in del {
        let Some(entry) = inv.models.get(key) else { continue };
        for loc in &entry.locations {
            let Some(root) = inv.roots.iter().find(|r| r.id == loc.root_id) else {
                continue;
            };
            if !root.kind.owned() {
                if let Some(cmd) = owner_removal_command(&entry.display_name, loc.kind, &loc.rel_path)
                {
                    if !report.foreign.iter().any(|(_, c)| *c == cmd) {
                        report.foreign.push((entry.display_name.clone(), cmd));
                    }
                }
                continue;
            }
            if !inv.live_accessible(loc) {
                report.offline.push((entry.display_name.clone(), root.id.clone()));
                continue;
            }
            let src = root.path.join(&loc.rel_path);
            let mut dst = trash_dir(&root.path).join(&loc.rel_path);
            if let Some(dir) = dst.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            // A same-named file already in the trash never blocks a delete
            // and is never overwritten — the new arrival gets a suffix.
            let mut n = 1;
            while dst.exists() {
                let name = format!(
                    "{}.{}",
                    loc.rel_path
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".into()),
                    n
                );
                dst = dst.with_file_name(name);
                n += 1;
            }
            std::fs::rename(&src, &dst)
                .with_context(|| format!("moving {} to trash", src.display()))?;
            report.trashed.push((entry.display_name.clone(), dst.clone()));
        }
    }
    Ok(report)
}

/// The owner commands a deletion would surface, for previewing in a
/// confirm dialog before anything moves.
pub fn foreign_commands(inv: &Inventory, del: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in del {
        let Some(entry) = inv.models.get(key) else { continue };
        for loc in &entry.locations {
            if let Some(root) = inv.roots.iter().find(|r| r.id == loc.root_id)
                && !root.kind.owned()
                && let Some(cmd) = owner_removal_command(&entry.display_name, loc.kind, &loc.rel_path)
                && !out.contains(&cmd)
            {
                out.push(cmd);
            }
        }
    }
    out
}

/// The owning tool's removal command for a foreign copy — for the user
/// to run themselves; warden takes no action in foreign stores here.
fn owner_removal_command(display_name: &str, kind: RootKind, rel_path: &Path) -> Option<String> {
    match kind {
        RootKind::Ollama => Some(format!(
            "ollama rm {}",
            manifest::ollama_base(display_name)
        )),
        RootKind::HfHub => {
            let first = rel_path.components().next()?;
            let dir = first.as_os_str().to_string_lossy();
            let repo = dir.strip_prefix("models--")?.replace("--", "/");
            Some(format!("hf cache rm {repo} -y"))
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct TrashedFile {
    pub root_id: String,
    pub root_label: String,
    pub root_path: PathBuf,
    /// Path relative to the trash dir — also where `restore` puts it back.
    pub rel_path: PathBuf,
    pub size: u64,
    pub trashed_unix: u64,
}

/// Everything sitting in the trash of every reachable owned root. The
/// filesystem is the record — no index to corrupt, and a human can
/// rescue files with a file manager alone.
pub fn list(roots: &[RootSpec]) -> Vec<TrashedFile> {
    let mut out = Vec::new();
    for root in roots {
        if !root.kind.owned() || !root.path.is_dir() {
            continue;
        }
        let td = trash_dir(&root.path);
        walk(&td, &td, root, &mut out);
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

fn walk(dir: &Path, base: &Path, root: &RootSpec, out: &mut Vec<TrashedFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, base, root, out);
        } else if let Ok(md) = e.metadata() {
            out.push(TrashedFile {
                root_id: root.id.clone(),
                root_label: root.label.clone().unwrap_or_else(|| root.id.clone()),
                root_path: root.path.clone(),
                rel_path: p.strip_prefix(base).unwrap_or(&p).to_path_buf(),
                size: md.len(),
                trashed_unix: md
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            });
        }
    }
}

/// Rename a trashed file back to its place in the root. Refuses to
/// overwrite anything that reappeared there in the meantime.
pub fn restore(root: &RootSpec, rel: &Path) -> Result<PathBuf> {
    let src = trash_dir(&root.path).join(rel);
    if !src.is_file() {
        bail!("{} is not in the trash of {}", rel.display(), root.path.display());
    }
    let dst = root.path.join(rel);
    if dst.exists() {
        bail!("{} already exists — refusing to overwrite", dst.display());
    }
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::rename(&src, &dst).with_context(|| format!("restoring {}", dst.display()))?;
    prune_empty_dirs(&trash_dir(&root.path));
    Ok(dst)
}

/// Stage 2 — the point of no return. Destroys every file in this root's
/// trash directory, and nothing else: the target is recomputed from the
/// root here, never taken from a caller, so only `.modelwarden/trash`
/// contents can ever be affected.
pub fn empty(root: &RootSpec) -> Result<(usize, u64)> {
    let td = trash_dir(&root.path);
    if !td.is_dir() {
        return Ok((0, 0));
    }
    let mut files = Vec::new();
    walk(&td, &td, root, &mut files);
    let count = files.len();
    let bytes: u64 = files.iter().map(|f| f.size).sum();
    std::fs::remove_dir_all(&td).with_context(|| format!("emptying {}", td.display()))?;
    Ok((count, bytes))
}

fn prune_empty_dirs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            prune_empty_dirs(&p);
            let _ = std::fs::remove_dir(&p); // fails unless empty — fine
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gguf::tests::synthetic_gguf;
    use crate::core::manifest::{build_root_manifest, merge};

    fn env() -> (tempfile::TempDir, RootSpec, Inventory) {
        let shelf = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shelf.path().join("Vision")).unwrap();
        let a = synthetic_gguf("llama", 8192, 15);
        let mut b = synthetic_gguf("clip", 0, 1);
        b.extend_from_slice(b"different");
        std::fs::write(shelf.path().join("Vision/model-Q4.gguf"), &a).unwrap();
        std::fs::write(shelf.path().join("Vision/mmproj-F16.gguf"), &b).unwrap();
        let spec = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let man = build_root_manifest(&spec, None);
        let inv = merge(&[man]);
        (shelf, spec, inv)
    }

    #[test]
    fn shared_companions_survive_a_delete() {
        let shelf = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shelf.path().join("V")).unwrap();
        let base = synthetic_gguf("llama", 8192, 15);
        let mut second = base.clone();
        second.extend_from_slice(b"2");
        let mut proj = base.clone();
        proj.extend_from_slice(b"p");
        std::fs::write(shelf.path().join("V/big-Q4.gguf"), &base).unwrap();
        std::fs::write(shelf.path().join("V/big-Q8.gguf"), &second).unwrap();
        std::fs::write(shelf.path().join("V/mmproj-F16.gguf"), &proj).unwrap();
        let spec = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let inv = merge(&[build_root_manifest(&spec, None)]);
        let q4 = inv
            .models
            .iter()
            .find(|(_, e)| e.display_name.contains("Q4"))
            .map(|(k, _)| k.clone())
            .unwrap();
        // Deleting Q4 alone: the projector is still required by Q8 — kept.
        let (del, kept) = deletable_set(&inv, &[q4.clone()]);
        assert_eq!(del.len(), 1, "only Q4 goes: {del:?}");
        assert!(kept.iter().any(|(n, why)| n.contains("mmproj") && why.contains("Q8")));
        // Deleting both quants: nothing needs the projector — it goes too.
        let q8 = inv
            .models
            .iter()
            .find(|(_, e)| e.display_name.contains("Q8"))
            .map(|(k, _)| k.clone())
            .unwrap();
        let (del, kept) = deletable_set(&inv, &[q4, q8]);
        assert_eq!(del.len(), 3, "{del:?}");
        assert!(kept.is_empty());
    }

    #[test]
    fn trash_roundtrip_moves_restores_and_only_empty_destroys() {
        let (_shelf, spec, inv) = env();
        let key = inv
            .models
            .iter()
            .find(|(_, e)| e.display_name.contains("model"))
            .map(|(k, _)| k.clone())
            .unwrap();
        let (del, _) = deletable_set(&inv, &[key]);
        let report = move_to_trash(&inv, &del).unwrap();
        assert_eq!(report.trashed.len(), 2, "bundle moved whole: {report:?}");
        assert!(report.foreign.is_empty());
        // Stage 1 destroyed nothing; layout preserved inside the trash.
        assert!(!spec.path.join("Vision/model-Q4.gguf").exists());
        assert!(trash_dir(&spec.path).join("Vision/model-Q4.gguf").is_file());

        // Listed, restorable.
        let listed = list(std::slice::from_ref(&spec));
        assert_eq!(listed.len(), 2);
        let back = restore(&spec, Path::new("Vision/model-Q4.gguf")).unwrap();
        assert!(back.is_file());
        assert_eq!(list(std::slice::from_ref(&spec)).len(), 1);
        // Restore refuses to overwrite a reappeared file.
        std::fs::write(trash_dir(&spec.path).join("Vision/model-Q4.gguf"), b"x").unwrap();
        assert!(restore(&spec, Path::new("Vision/model-Q4.gguf")).is_err());
        std::fs::remove_file(trash_dir(&spec.path).join("Vision/model-Q4.gguf")).unwrap();

        // Stage 2: empty destroys exactly the trash, nothing else.
        let (count, bytes) = empty(&spec).unwrap();
        assert_eq!(count, 1);
        assert!(bytes > 0);
        assert!(list(std::slice::from_ref(&spec)).is_empty());
        assert!(back.is_file(), "restored file untouched by empty");
    }

    #[test]
    fn foreign_copies_yield_commands_never_actions() {
        assert_eq!(
            owner_removal_command("ghost:latest", RootKind::Ollama, Path::new("blobs/sha256-x")),
            Some("ollama rm ghost:latest".into())
        );
        assert_eq!(
            owner_removal_command(
                "org/Repo",
                RootKind::HfHub,
                Path::new("models--org--Repo/snapshots/rev/m.gguf")
            ),
            Some("hf cache rm org/Repo -y".into())
        );
        assert_eq!(
            owner_removal_command("x", RootKind::Shelf, Path::new("m.gguf")),
            None
        );
    }
}

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
    let union = manifest::bundle_union(inv, keys);
    // This asks bundle_for once per model in the catalog; one index
    // serves them all.
    let idx = manifest::BundleIndex::of(inv);
    let mut del = BTreeSet::new();
    let mut kept = Vec::new();
    // A candidate is spared only when a model OUTSIDE the deletion union
    // still needs it. Members of the union never anchor a keep — for
    // symmetric bundles (split parts) each part "requires" its sibling,
    // and checking against the selection instead of the union inverted
    // the delete: the chosen part was kept and its sibling trashed.
    'cand: for c in union.clone() {
        for (other, e) in &inv.models {
            if union.contains(other) || *other == c {
                continue;
            }
            if manifest::bundle_for_indexed(inv, &idx, other).iter().any(|m| *m == c) {
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
                let label = root.label.clone().unwrap_or_else(|| root.id.clone());
                report.offline.push((entry.display_name.clone(), label));
                continue;
            }
            let Ok(dst) = trash_one(&root.path, &loc.rel_path) else {
                continue;
            };
            report.trashed.push((entry.display_name.clone(), dst));
        }
    }
    Ok(report)
}

/// Move one file inside an owned root into that root's trash, and
/// return where it landed. Nothing is destroyed: this is a rename.
///
/// Shared by `delete` and by `demote --remove-source`, so the one
/// operation advertised as the *safe* move is as recoverable as the one
/// advertised as a deletion.
pub fn trash_one(root_path: &Path, rel: &Path) -> Result<PathBuf> {
    // A catalog path must be genuinely inside its root before anything
    // is moved: deletion is not a place to trust input.
    let safe_rel = manifest::sanitize_rel(rel)?;
    let src = root_path.join(&safe_rel);
    let mut dst = trash_dir(root_path).join(&safe_rel);
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // A same-named file already in the trash never blocks a delete and is
    // never overwritten — the new arrival gets a counter BEFORE the
    // extension ("model.1.gguf", never "model.gguf.1"), so a later
    // restore yields a name the scanner still catalogs.
    let base = safe_rel
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mut n = 1;
    while dst.exists() {
        let name = match base.rsplit_once('.') {
            Some((stem, ext)) => format!("{stem}.{n}.{ext}"),
            None => format!("{base}.{n}"),
        };
        dst = dst.with_file_name(name);
        n += 1;
    }
    crate::core::fsx::rename_noreplace(&src, &dst)
        .with_context(|| format!("moving {} to trash", src.display()))?;
    // Rename preserves mtime, but the trash listing derives its "trashed
    // N ago" age from it — stamp now, or a year-old file deleted today
    // reads as ancient and safe to destroy.
    if let Ok(f) = std::fs::OpenOptions::new().append(true).open(&dst) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    Ok(dst)
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
            let repo = crate::core::scan::hf_repo_from_dirname(&dir)?;
            // hf's cache rm wants the typed id ("model/org/name") — the bare
            // repo id silently matches nothing ("Could not find in cache",
            // exit 0). Found in the field.
            Some(format!("hf cache rm model/{repo} -y"))
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

/// The trash holds moved bundles: a safetensors container is the
/// deepest thing in it, and those are a handful of levels. A cap this
/// far above the real shape only ever stops something pathological.
const TRASH_MAX_DEPTH: usize = 16;

fn walk(dir: &Path, base: &Path, root: &RootSpec, out: &mut Vec<TrashedFile>) {
    walk_at(dir, base, root, 0, out)
}

fn walk_at(dir: &Path, base: &Path, root: &RootSpec, depth: usize, out: &mut Vec<TrashedFile>) {
    if depth > TRASH_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if crate::core::fsx::is_real_dir(&p) {
            walk_at(&p, base, root, depth + 1, out);
        } else if p.is_dir() {
            // A symlink to a directory: not descended into (that is the
            // loop), and not a trashed file either. Skipped entirely.
            continue;
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

/// Everything that should come back WITH `rel` — the bundle rules applied
/// to trash contents by filename, since the trash carries no catalog:
/// split siblings and same-directory projectors ride along; a weights
/// file (or a directory holding one) brings its whole subtree; restoring
/// a projector alone stays alone — the same asymmetry as bundle_for.
/// Restore must mirror delete: delete trashes the bundle, so a restore
/// that brought back one file would strand the rest for the next empty.
pub fn restore_set(root: &RootSpec, rel: &Path) -> Vec<PathBuf> {
    let td = trash_dir(&root.path);
    let mut all = Vec::new();
    walk(&td, &td, root, &mut all);
    restore_set_in(&all, rel)
}

/// The same answer from a trash listing the caller already has.
///
/// Restoring a three-part bundle walked the whole trash three times —
/// once per part — and the projector shortcut below walked it even when
/// it was about to ignore the result entirely.
pub fn restore_set_in(all: &[TrashedFile], rel: &Path) -> Vec<PathBuf> {
    let fname = rel
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    if manifest::is_projector_name(&fname) {
        return vec![rel.to_path_buf()];
    }
    let dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
    let name_of = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let weights_container = !dir.as_os_str().is_empty()
        && (crate::core::scan::is_weights_filename(&fname)
            || all.iter().any(|f| {
                f.rel_path.starts_with(&dir)
                    && crate::core::scan::is_weights_filename(&name_of(&f.rel_path))
            }));
    if weights_container {
        return all
            .iter()
            .filter(|f| f.rel_path.starts_with(&dir))
            .map(|f| f.rel_path.clone())
            .collect();
    }
    let my_split = crate::core::acquire::split_parts(&fname).map(|(p, _, c)| (p.to_string(), c));
    let mut out = vec![rel.to_path_buf()];
    for f in all {
        if f.rel_path == rel || f.rel_path.parent().map(Path::to_path_buf).unwrap_or_default() != dir {
            continue;
        }
        let f2 = name_of(&f.rel_path);
        let same_split = my_split.as_ref().is_some_and(|(p, c)| {
            crate::core::acquire::split_parts(&f2).is_some_and(|(p2, _, c2)| p2 == p && c2 == *c)
        });
        if same_split || manifest::is_projector_name(&f2) {
            out.push(f.rel_path.clone());
        }
    }
    out
}

/// Rename a trashed file back to its place in the root. Refuses to
/// overwrite anything that reappeared there in the meantime.
pub fn restore(root: &RootSpec, rel: &Path) -> Result<PathBuf> {
    let rel = &manifest::sanitize_rel(rel)?;
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
    crate::core::fsx::rename_noreplace(&src, &dst)
        .with_context(|| format!("restoring {}", dst.display()))?;
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
    fn deleting_any_split_part_takes_the_whole_set_not_the_inverse() {
        // Regression: split parts are SYMMETRIC bundle members; the old
        // keep-check treated the unselected sibling as a keeper, so
        // deleting part 1 kept part 1 and trashed part 2.
        let shelf = tempfile::tempdir().unwrap();
        let base = synthetic_gguf("llama", 8192, 15);
        let mut p2 = base.clone();
        p2.extend_from_slice(b"2");
        std::fs::write(shelf.path().join("big-00001-of-00002.gguf"), &base).unwrap();
        std::fs::write(shelf.path().join("big-00002-of-00002.gguf"), &p2).unwrap();
        let spec = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let inv = merge(&[build_root_manifest(&spec, None)]);
        let p1_key = inv
            .models
            .iter()
            .find(|(_, e)| e.display_name.contains("00001"))
            .map(|(k, _)| k.clone())
            .unwrap();
        let (del, kept) = deletable_set(&inv, &[p1_key]);
        assert_eq!(del.len(), 2, "both parts go together: {del:?}");
        assert!(kept.is_empty(), "no half-model survivors: {kept:?}");
    }

    #[test]
    fn collision_suffix_keeps_the_extension_scannable() {
        let (_shelf, spec, inv) = env();
        let key = inv
            .models
            .iter()
            .find(|(_, e)| e.display_name.contains("model"))
            .map(|(k, _)| k.clone())
            .unwrap();
        let (del, _) = deletable_set(&inv, &[key]);
        // Seed a same-named file already in the trash.
        let taken = trash_dir(&spec.path).join("Vision/model-Q4.gguf");
        std::fs::create_dir_all(taken.parent().unwrap()).unwrap();
        std::fs::write(&taken, b"earlier occupant").unwrap();
        let report = move_to_trash(&inv, &del).unwrap();
        let renamed = report
            .trashed
            .iter()
            .find(|(_, p)| p.to_string_lossy().contains("model-Q4"))
            .unwrap();
        assert!(
            renamed.1.to_string_lossy().ends_with("model-Q4.1.gguf"),
            "suffix goes before the extension: {renamed:?}"
        );
    }

    #[test]
    fn restore_brings_the_bundle_back_like_delete_took_it() {
        let shelf = tempfile::tempdir().unwrap();
        let base = synthetic_gguf("llama", 8192, 15);
        let mut b = base.clone();
        b.extend_from_slice(b"b");
        let mut c = base.clone();
        c.extend_from_slice(b"c");
        let mut d = base.clone();
        d.extend_from_slice(b"d");
        std::fs::create_dir_all(shelf.path().join("V")).unwrap();
        std::fs::write(shelf.path().join("V/big-00001-of-00002.gguf"), &base).unwrap();
        std::fs::write(shelf.path().join("V/big-00002-of-00002.gguf"), &b).unwrap();
        std::fs::write(shelf.path().join("V/mmproj-F16.gguf"), &c).unwrap();
        std::fs::write(shelf.path().join("V/other-Q4.gguf"), &d).unwrap();
        let spec = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let inv = merge(&[build_root_manifest(&spec, None)]);
        // Trash everything so the trash holds the split pair, the
        // projector, and an unrelated model side by side.
        let all_keys: Vec<String> = inv.models.keys().cloned().collect();
        let (del, _) = deletable_set(&inv, &all_keys);
        move_to_trash(&inv, &del).unwrap();
        // Restoring one split part expands to both parts + projector,
        // never the unrelated neighbor.
        let set = restore_set(&spec, Path::new("V/big-00001-of-00002.gguf"));
        let names: BTreeSet<String> = set
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains("big-00001-of-00002.gguf"));
        assert!(names.contains("big-00002-of-00002.gguf"));
        assert!(names.contains("mmproj-F16.gguf"));
        assert!(!names.contains("other-Q4.gguf"), "{names:?}");
        // Restoring the projector alone stays alone (asymmetric).
        let solo = restore_set(&spec, Path::new("V/mmproj-F16.gguf"));
        assert_eq!(solo.len(), 1);
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
            Some("hf cache rm model/org/Repo -y".into())
        );
        assert_eq!(
            owner_removal_command("x", RootKind::Shelf, Path::new("m.gguf")),
            None
        );
    }

    #[test]
    fn a_symlink_loop_in_the_trash_does_not_take_the_process_down() {
        // `walk` recursed with no depth cap and descended through
        // `is_dir()`, which follows links. One link pointing at an
        // ancestor — and a drive carries its own trash directory, so it
        // can arrive with one — got walked over and over until the
        // kernel's own symlink limit stopped it: measured at 42 listings
        // of ONE trashed file, which is 42x its bytes in the trash total
        // the Empty-Trash confirmation shows the user.
        let root_dir = tempfile::tempdir().unwrap();
        let td = trash_dir(root_dir.path());
        std::fs::create_dir_all(td.join("deep")).unwrap();
        std::fs::write(td.join("deep/kept.gguf"), b"bytes").unwrap();
        std::os::unix::fs::symlink(&td, td.join("deep/loop")).unwrap();

        let root = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: root_dir.path().to_path_buf(),
            label: None,
        };
        let mut out = Vec::new();
        walk(&td, &td, &root, &mut out);
        let names: Vec<_> = out.iter().map(|f| f.rel_path.display().to_string()).collect();
        assert_eq!(names, vec!["deep/kept.gguf"], "the real file, once, and no loop");
    }
}

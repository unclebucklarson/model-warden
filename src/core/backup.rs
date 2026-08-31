//! Verified backups: every distinct content copied once to a target root,
//! human-readable layout, and no copy counts until it has been read back
//! from the target and its hash matched.
//!
//! Verification is three-way: the expected hash from the catalog, the hash
//! of the source as it was read, and the hash of the destination read back
//! after writing. Any disagreement fails that file and leaves nothing
//! half-written (copies go through a `.partial` temp name).

use crate::core::identity;
use crate::core::manifest::{
    self, FileRecord, Inventory, Location, ModelEntry, RootManifest, SCHEMA_VERSION,
};
use crate::core::roots::{RootKind, RootSpec};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum BackupEvent {
    FileStart { label: String, size: u64 },
    /// Copy then read-back verify; `phase` says which. Sent every ~64 MiB.
    FileProgress { label: String, phase: &'static str, done: u64, total: u64 },
    FileDone { label: String, secs: f32 },
    Skipped { label: String, reason: String },
    Failed { label: String, error: String },
}

impl BackupEvent {
    /// The durable activity-log line, worded identically in both frontends;
    /// `None` for transient progress ticks, which are never logged.
    pub fn log_line(&self) -> Option<String> {
        match self {
            Self::FileStart { .. } | Self::FileProgress { .. } => None,
            Self::FileDone { label, secs } => Some(format!("verified {label} in {secs:.0}s")),
            Self::Skipped { label, reason } => Some(format!("skipped {label}: {reason}")),
            Self::Failed { label, error } => Some(format!("FAILED {label}: {error}")),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BackupReport {
    pub copied: usize,
    pub copied_bytes: u64,
    pub skipped_already: usize,
    pub failed: usize,
}

/// The manifest a target drive carries at `<target>/.modelwarden/manifest.json`
/// — the drive stays self-describing while unplugged.
pub fn target_manifest_path(target: &Path) -> PathBuf {
    target.join(".modelwarden/manifest.json")
}

/// Back up hashed, reachable contents that aren't on the target yet.
/// `selection`: catalog keys to back up — each is expanded to its full
/// bundle (split parts, mmproj/projector companions), because a fragment of
/// a model is not a backup. `None` backs up everything. Returns the updated
/// target manifest (caller persists it to the state dir AND the target
/// itself) plus the report.
pub fn backup(
    inv: &Inventory,
    target: &RootSpec,
    selection: Option<&[String]>,
    mut on: impl FnMut(BackupEvent),
) -> Result<(RootManifest, BackupReport)> {
    let expanded: Option<std::collections::BTreeSet<String>> = selection.map(|keys| {
        keys.iter()
            .flat_map(|k| manifest::bundle_for(inv, k))
            .collect()
    });
    if !target.kind.owned() {
        bail!("backup target must be an owned root, not {}", target.kind.label());
    }
    if !target.path.is_dir() {
        bail!("backup target {} is not a directory (offline?)", target.path.display());
    }
    // The target's own manifest is untrusted (it lives on removable
    // media): scrub it before its records are believed. A forged
    // "already have this hash" record would otherwise make backup copy
    // nothing and report success.
    let mut man = manifest::load_manifest(&target_manifest_path(&target.path))
        .map(|m| manifest::sanitize_carried(m, &target.path).0)
        .unwrap_or(
        RootManifest {
            schema_version: SCHEMA_VERSION,
            root: target.clone(),
            generated_unix: manifest::now_unix(),
            files: Vec::new(),
        },
    );
    // The mount point can move between sessions; the manifest follows the
    // registered spec.
    man.root = target.clone();

    let mut report = BackupReport::default();
    for (key, entry) in &inv.models {
        if let Some(sel) = &expanded
            && !sel.contains(key)
        {
            continue;
        }
        let Some(hash) = key.strip_prefix("sha256:") else {
            on(BackupEvent::Skipped {
                label: entry.display_name.clone(),
                reason: "not hashed yet — run `warden hash`".into(),
            });
            continue;
        };
        if man.files.iter().any(|f| f.sha256.as_deref() == Some(hash)) {
            report.skipped_already += 1;
            continue;
        }
        if entry.locations.iter().any(|l| l.root_id == target.id) {
            report.skipped_already += 1;
            continue;
        }
        let Some(src_loc) = entry.locations.iter().find(|l| inv.live_accessible(l)) else {
            on(BackupEvent::Skipped {
                label: entry.display_name.clone(),
                reason: "no reachable copy".into(),
            });
            continue;
        };
        let Some(src_root) = inv.root(&src_loc.root_id) else {
            continue;
        };
        let src = src_root.path.join(&src_loc.rel_path);
        let rel_dest = dest_layout(entry, src_loc);
        let dest = target.path.join(&rel_dest);
        let label = entry.display_name.clone();

        if dest.exists() {
            on(BackupEvent::Failed {
                label,
                error: format!(
                    "{} already exists with different content — refusing to overwrite",
                    dest.display()
                ),
            });
            report.failed += 1;
            continue;
        }

        on(BackupEvent::FileStart {
            label: label.clone(),
            size: entry.size,
        });
        let started = std::time::Instant::now();
        match copy_verified(&src, &dest, hash, entry.size, &label, Publish::New, &mut on) {
            Ok(fingerprint) => {
                man.files.push(FileRecord {
                    rel_path: rel_dest,
                    size: entry.size,
                    fingerprint: Some(fingerprint),
                    sha256: Some(hash.to_string()),
                    name: Some(entry.display_name.clone()),
                    meta: entry.meta.clone(),
                    accessible: true,
                    verified_unix: Some(manifest::now_unix()),
                });
                report.copied += 1;
                report.copied_bytes += entry.size;
                on(BackupEvent::FileDone {
                    label,
                    secs: started.elapsed().as_secs_f32(),
                });
            }
            Err(e) => {
                report.failed += 1;
                on(BackupEvent::Failed {
                    label,
                    error: format!("{e:#}"),
                });
            }
        }
    }
    man.generated_unix = manifest::now_unix();
    Ok((man, report))
}

/// Where a content lands on the target: shelf/drive files keep their
/// layout; Ollama models get a name-derived folder; HF files get
/// `<repo-last-segment>/<filename>`.
fn dest_layout(entry: &ModelEntry, loc: &Location) -> PathBuf {
    match loc.kind {
        RootKind::Shelf | RootKind::Removable => loc.rel_path.clone(),
        RootKind::Ollama => {
            let safe = entry.display_name.replace([':', '/'], "-");
            PathBuf::from(&safe).join(format!("{safe}.gguf"))
        }
        RootKind::HfHub => {
            let family = entry
                .display_name
                .rsplit('/')
                .next()
                .unwrap_or("model")
                .to_string();
            // Keep the path relative to the snapshot revision (skip
            // models--x/snapshots/<rev>) — flattening to the filename would
            // collide when a bundle holds e.g. config.json in two subdirs.
            let sub: PathBuf = loc.rel_path.components().skip(3).collect();
            let sub = if sub.as_os_str().is_empty() {
                PathBuf::from("model.gguf")
            } else {
                sub
            };
            PathBuf::from(family).join(sub)
        }
    }
}

/// What a finished copy may do to an existing destination.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Publish {
    /// Refuse if anything is there — enforced by the rename itself, so
    /// there is no window between checking and publishing.
    New,
    /// Deliberately replace it. Only `repair` does this: the whole point
    /// is to overwrite bytes already known to be corrupt, and the old
    /// file stays put until the verified replacement is ready.
    Replace,
}

/// Copy `src` → `dest` through a `.partial` temp: hash the source as it is
/// read, require it to match `expected`, then read the finished temp back
/// and require that to match too. Only then rename into place. Returns the
/// destination's fingerprint.
pub(crate) fn copy_verified(
    src: &Path,
    dest: &Path,
    expected: &str,
    total: u64,
    label: &str,
    publish: Publish,
    on: &mut impl FnMut(BackupEvent),
) -> Result<identity::Fingerprint> {
    use sha2::{Digest, Sha256};
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = crate::core::fsx::temp_sibling(dest, "partial");

    let mut reader = std::io::BufReader::with_capacity(
        4 * 1024 * 1024,
        std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?,
    );
    let mut writer = std::io::BufWriter::with_capacity(
        4 * 1024 * 1024,
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut done = 0u64;
    let mut last = 0u64;
    let result: Result<()> = (|| {
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n])?;
            done += n as u64;
            if done - last >= 64 * 1024 * 1024 {
                last = done;
                on(BackupEvent::FileProgress {
                    label: label.to_string(),
                    phase: "copy",
                    done,
                    total,
                });
            }
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("copying {}", src.display()));
    }
    drop(writer);

    let source_hash = hex(&hasher.finalize());
    if source_hash != expected {
        let _ = std::fs::remove_file(&tmp);
        bail!("source changed since it was cataloged (run `warden hash` again)");
    }

    // Read the bytes back off the target — a copy only counts once the
    // destination itself has produced the right hash.
    let mut last = 0u64;
    let readback = identity::sha256_file(&tmp, |done, total| {
        if done - last >= 64 * 1024 * 1024 {
            last = done;
            on(BackupEvent::FileProgress {
                label: label.to_string(),
                phase: "verify",
                done,
                total,
            });
        }
    })?;
    if readback != expected {
        let _ = std::fs::remove_file(&tmp);
        bail!("read-back hash mismatch — target wrote bad bytes, nothing kept");
    }

    // Refuse-overwrite is enforced by the rename itself, not by an
    // exists() check that another process can win.
    let published = match publish {
        Publish::New => crate::core::fsx::rename_noreplace(&tmp, dest),
        Publish::Replace => std::fs::rename(&tmp, dest).map_err(anyhow::Error::from),
    };
    if let Err(e) = published {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("finalizing {}", dest.display()));
    }
    identity::Fingerprint::of(dest)
}

fn hex(digest: &[u8]) -> String {
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---- verify ----

#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyReport {
    pub ok: usize,
    pub mismatched: Vec<PathBuf>,
    pub missing: Vec<PathBuf>,
    pub unhashed: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RepairReport {
    pub repaired: usize,
    pub unrepairable: Vec<PathBuf>,
}

/// Re-copy a root's mismatched/missing files from a live source elsewhere
/// in the catalog. A corrupt copy is never deleted first: the verified
/// replacement lands via `.partial` and an atomic rename over it — if the
/// repair fails at any point, the old bytes (however bad) are still there.
/// Files whose content no live root holds are reported unrepairable.
pub fn repair(
    inv: &Inventory,
    man: &mut RootManifest,
    report: &VerifyReport,
    mut on: impl FnMut(BackupEvent),
) -> Result<RepairReport> {
    if !man.root.kind.owned() {
        bail!("refusing to repair inside a foreign store");
    }
    let mut out = RepairReport::default();
    let broken: Vec<PathBuf> = report
        .mismatched
        .iter()
        .chain(report.missing.iter())
        .cloned()
        .collect();
    for rel in broken {
        // Defence in depth: a manifest record must never steer a write
        // outside its own root, even if it reached here unscrubbed.
        if manifest::sanitize_rel(&rel).is_err() {
            on(BackupEvent::Failed {
                label: rel.display().to_string(),
                error: "unsafe path in manifest — refusing to write".into(),
            });
            out.unrepairable.push(rel);
            continue;
        }
        let Some(rec) = man.files.iter_mut().find(|f| f.rel_path == rel) else {
            continue;
        };
        let Some(hash) = rec.sha256.clone() else {
            out.unrepairable.push(rel);
            continue;
        };
        let label = rec
            .name
            .clone()
            .unwrap_or_else(|| rel.display().to_string());
        // A live copy of the same content, anywhere but this root.
        let source = inv
            .models
            .get(&format!("sha256:{hash}"))
            .and_then(|entry| {
                entry
                    .locations
                    .iter()
                    .find(|l| l.root_id != man.root.id && inv.live_accessible(l))
                    .and_then(|l| inv.root(&l.root_id).map(|r| r.path.join(&l.rel_path)))
            });
        let Some(src) = source else {
            on(BackupEvent::Skipped {
                label,
                reason: "no live copy of this content anywhere else".into(),
            });
            out.unrepairable.push(rel);
            continue;
        };
        let dest = man.root.path.join(&rel);
        on(BackupEvent::FileStart {
            label: label.clone(),
            size: rec.size,
        });
        let started = std::time::Instant::now();
        match copy_verified(&src, &dest, &hash, rec.size, &label, Publish::Replace, &mut on) {
            Ok(fingerprint) => {
                rec.fingerprint = Some(fingerprint);
                rec.accessible = true;
                rec.verified_unix = Some(manifest::now_unix());
                out.repaired += 1;
                on(BackupEvent::FileDone {
                    label,
                    secs: started.elapsed().as_secs_f32(),
                });
            }
            Err(e) => {
                out.unrepairable.push(rel);
                on(BackupEvent::Failed {
                    label,
                    error: format!("{e:#}"),
                });
            }
        }
    }
    man.generated_unix = manifest::now_unix();
    Ok(out)
}

/// Re-hash every file a root's manifest records and compare against the
/// stored identities. Updates `verified_unix` on matches. The caller
/// persists the manifest.
pub fn verify(
    man: &mut RootManifest,
    mut on: impl FnMut(BackupEvent),
) -> Result<VerifyReport> {
    if !man.root.path.is_dir() {
        bail!("{} is offline — nothing to verify against", man.root.path.display());
    }
    let mut report = VerifyReport::default();
    for f in &mut man.files {
        let Some(expected) = f.sha256.clone() else {
            report.unhashed += 1;
            continue;
        };
        let label = f
            .name
            .clone()
            .unwrap_or_else(|| f.rel_path.display().to_string());
        if manifest::sanitize_rel(&f.rel_path).is_err() {
            report.mismatched.push(f.rel_path.clone());
            on(BackupEvent::Failed {
                label,
                error: "unsafe path in manifest — not part of this root".into(),
            });
            continue;
        }
        let abs = man.root.path.join(&f.rel_path);
        if !abs.is_file() {
            report.missing.push(f.rel_path.clone());
            on(BackupEvent::Failed {
                label,
                error: "missing from disk".into(),
            });
            continue;
        }
        on(BackupEvent::FileStart {
            label: label.clone(),
            size: f.size,
        });
        let started = std::time::Instant::now();
        let mut last = 0u64;
        let actual = identity::sha256_file(&abs, |done, total| {
            if done - last >= 64 * 1024 * 1024 {
                last = done;
                on(BackupEvent::FileProgress {
                    label: label.clone(),
                    phase: "verify",
                    done,
                    total,
                });
            }
        })?;
        if actual == expected {
            report.ok += 1;
            f.verified_unix = Some(manifest::now_unix());
            on(BackupEvent::FileDone {
                label,
                secs: started.elapsed().as_secs_f32(),
            });
        } else {
            report.mismatched.push(f.rel_path.clone());
            on(BackupEvent::Failed {
                label,
                error: format!("hash mismatch: bytes on disk are not {}", crate::core::format::short_hash(&expected)),
            });
        }
    }
    man.generated_unix = manifest::now_unix();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gguf::tests::synthetic_gguf;
    use crate::core::manifest::{build_root_manifest, merge};

    #[test]
    fn log_lines_mirror_the_cli_wording() {
        // Both frontends log these words verbatim — changing them changes
        // user-visible output in two places at once, on purpose.
        let done = BackupEvent::FileDone { label: "m".into(), secs: 3.2 };
        assert_eq!(done.log_line().as_deref(), Some("verified m in 3s"));
        let skip = BackupEvent::Skipped { label: "m".into(), reason: "offline".into() };
        assert_eq!(skip.log_line().as_deref(), Some("skipped m: offline"));
        // Transient ticks are never logged durably.
        let start = BackupEvent::FileStart { label: "m".into(), size: 1 };
        assert_eq!(start.log_line(), None);
    }

    fn shelf_with_model() -> (tempfile::TempDir, RootSpec, Inventory) {
        let shelf = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shelf.path().join("Fam")).unwrap();
        std::fs::write(
            shelf.path().join("Fam/model.gguf"),
            synthetic_gguf("llama", 4096, 15),
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
        let inv = merge(&[man]);
        (shelf, spec, inv)
    }

    fn target_spec(dir: &Path) -> RootSpec {
        RootSpec {
            id: "ext-target01".into(),
            kind: RootKind::Removable,
            path: dir.to_path_buf(),
            label: Some("backup-drive".into()),
        }
    }

    #[test]
    fn backs_up_verifies_and_is_idempotent() {
        let (_shelf, _spec, inv) = shelf_with_model();
        let target = tempfile::tempdir().unwrap();
        let tspec = target_spec(target.path());

        let (man, report) = backup(&inv, &tspec, None, |_| {}).unwrap();
        assert_eq!(report.copied, 1);
        assert_eq!(report.failed, 0);
        let dest = target.path().join("Fam/model.gguf");
        assert!(dest.is_file(), "human-readable layout preserved");
        assert_eq!(man.files.len(), 1);
        assert!(man.files[0].verified_unix.is_some());
        assert!(!dest.with_extension("gguf.partial").exists());

        // Second run: the manifest knows this content — nothing recopied.
        manifest::save_json(&man, &target_manifest_path(target.path())).unwrap();
        let (_man2, report2) = backup(&inv, &tspec, None, |_| {}).unwrap();
        assert_eq!(report2.copied, 0);
        assert_eq!(report2.skipped_already, 1);
    }

    #[test]
    fn a_stale_catalog_hash_fails_the_copy_and_leaves_nothing() {
        let (shelf, _spec, inv) = shelf_with_model();
        // Rewrite the source AFTER cataloging: expected hash is now stale.
        std::fs::write(
            shelf.path().join("Fam/model.gguf"),
            synthetic_gguf("qwen3", 8192, 17),
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();
        let (_, report) = backup(&inv, &target_spec(target.path()), None, |_| {}).unwrap();
        assert_eq!(report.copied, 0);
        assert_eq!(report.failed, 1);
        assert!(
            std::fs::read_dir(target.path()).unwrap().flatten().all(|e| {
                let name = e.file_name();
                name == ".modelwarden" || !e.path().is_file()
            }),
            "no partials, no bad copies"
        );
    }

    #[test]
    fn verify_catches_corrupted_target_bytes() {
        let (_shelf, _spec, inv) = shelf_with_model();
        let target = tempfile::tempdir().unwrap();
        let tspec = target_spec(target.path());
        let (mut man, _) = backup(&inv, &tspec, None, |_| {}).unwrap();

        let ok = verify(&mut man, |_| {}).unwrap();
        assert_eq!(ok.ok, 1);
        assert!(ok.mismatched.is_empty());

        // Bit-rot the backup copy.
        std::fs::write(target.path().join("Fam/model.gguf"), b"rotten").unwrap();
        let bad = verify(&mut man, |_| {}).unwrap();
        assert_eq!(bad.ok, 0);
        assert_eq!(bad.mismatched, vec![PathBuf::from("Fam/model.gguf")]);
    }

    #[test]
    fn repair_replaces_corrupt_backup_copies_from_a_live_source() {
        let (_shelf, _spec, inv) = shelf_with_model();
        let target = tempfile::tempdir().unwrap();
        let tspec = target_spec(target.path());
        let (mut man, _) = backup(&inv, &tspec, None, |_| {}).unwrap();

        // Bit-rot the backup copy; verify flags it; repair heals it.
        std::fs::write(target.path().join("Fam/model.gguf"), b"rotten").unwrap();
        let bad = verify(&mut man, |_| {}).unwrap();
        assert_eq!(bad.mismatched.len(), 1);
        let rep = repair(&inv, &mut man, &bad, |_| {}).unwrap();
        assert_eq!(rep.repaired, 1);
        assert!(rep.unrepairable.is_empty());
        let again = verify(&mut man, |_| {}).unwrap();
        assert_eq!(again.ok, 1);
        assert!(again.mismatched.is_empty());

        // A missing file heals the same way.
        std::fs::remove_file(target.path().join("Fam/model.gguf")).unwrap();
        let gone = verify(&mut man, |_| {}).unwrap();
        assert_eq!(gone.missing.len(), 1);
        let rep2 = repair(&inv, &mut man, &gone, |_| {}).unwrap();
        assert_eq!(rep2.repaired, 1);

        // Content with no live source anywhere: reported, not silently lost.
        man.files.push(FileRecord {
            rel_path: "Fam/ghost.gguf".into(),
            size: 5,
            fingerprint: None,
            sha256: Some("no-such-content".into()),
            name: None,
            meta: None,
            accessible: true,
            verified_unix: None,
        });
        let gone2 = verify(&mut man, |_| {}).unwrap();
        let rep3 = repair(&inv, &mut man, &gone2, |_| {}).unwrap();
        assert_eq!(rep3.unrepairable, vec![PathBuf::from("Fam/ghost.gguf")]);
    }

    #[test]
    fn selection_backs_up_the_bundle_not_the_world() {
        use crate::core::identity;
        use crate::core::manifest::{build_root_manifest, merge};
        let shelf = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shelf.path().join("Vision")).unwrap();
        std::fs::create_dir_all(shelf.path().join("Other")).unwrap();
        std::fs::write(
            shelf.path().join("Vision/model-Q4_K_M.gguf"),
            crate::core::gguf::tests::synthetic_gguf("qwen3", 4096, 15),
        )
        .unwrap();
        std::fs::write(
            shelf.path().join("Vision/mmproj-F16.gguf"),
            crate::core::gguf::tests::synthetic_gguf("clip", 0, 1),
        )
        .unwrap();
        std::fs::write(
            shelf.path().join("Other/unrelated.gguf"),
            crate::core::gguf::tests::synthetic_gguf("llama", 1024, 1),
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
            f.sha256 =
                Some(identity::sha256_file(&spec.path.join(&f.rel_path), |_, _| {}).unwrap());
        }
        let inv = merge(&[man]);
        let model_key = inv
            .models
            .iter()
            .find(|(_, e)| e.display_name == "model-Q4_K_M")
            .map(|(k, _)| k.clone())
            .unwrap();

        let target = tempfile::tempdir().unwrap();
        let (_, report) = backup(
            &inv,
            &target_spec(target.path()),
            Some(&[model_key]),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.copied, 2, "the model AND its projector");
        assert!(target.path().join("Vision/model-Q4_K_M.gguf").is_file());
        assert!(
            target.path().join("Vision/mmproj-F16.gguf").is_file(),
            "vision projector rides along"
        );
        assert!(
            !target.path().join("Other/unrelated.gguf").exists(),
            "selection stays a selection"
        );
    }

    #[test]
    fn hf_bundles_keep_their_snapshot_layout_on_the_target() {
        use crate::core::manifest::{build_root_manifest, merge};
        let hub = tempfile::tempdir().unwrap();
        let snap = hub.path().join("models--org--Embed/snapshots/rev1");
        std::fs::create_dir_all(snap.join("1_Pooling")).unwrap();
        std::fs::write(snap.join("model.safetensors"), b"weights").unwrap();
        std::fs::write(snap.join("config.json"), b"{}").unwrap();
        std::fs::write(snap.join("1_Pooling/config.json"), b"{-}").unwrap();
        let spec = RootSpec {
            id: "hf-test".into(),
            kind: RootKind::HfHub,
            path: hub.path().to_path_buf(),
            label: None,
        };
        let mut man = build_root_manifest(&spec, None);
        for f in &mut man.files {
            f.sha256 = Some(
                identity::sha256_file(&spec.path.join(&f.rel_path), |_, _| {}).unwrap(),
            );
        }
        let inv = merge(&[man]);
        let target = tempfile::tempdir().unwrap();
        let (_, report) = backup(&inv, &target_spec(target.path()), None, |_| {}).unwrap();
        assert_eq!(report.copied, 3);
        assert_eq!(report.failed, 0, "the two config.json must not collide");
        assert!(target.path().join("Embed/model.safetensors").is_file());
        assert!(target.path().join("Embed/config.json").is_file());
        assert!(
            target.path().join("Embed/1_Pooling/config.json").is_file(),
            "subdir layout preserved"
        );
    }

    #[test]
    fn refuses_foreign_targets() {
        let (_shelf, _spec, inv) = shelf_with_model();
        let target = tempfile::tempdir().unwrap();
        let mut tspec = target_spec(target.path());
        tspec.kind = RootKind::HfHub;
        assert!(backup(&inv, &tspec, None, |_| {}).is_err());
    }

    #[test]
    fn repair_refuses_to_write_outside_its_root() {
        // Defence in depth: even if a poisoned manifest reaches this far
        // (it should be scrubbed at load), the write sink must refuse.
        // A live source IS available, so nothing but the guard stops it.
        use crate::core::gguf::tests::synthetic_gguf;
        let world = tempfile::tempdir().unwrap();
        let shelf = world.path().join("shelf");
        let drive = world.path().join("drive");
        std::fs::create_dir_all(&shelf).unwrap();
        std::fs::create_dir_all(&drive).unwrap();
        let bytes = synthetic_gguf("llama", 8192, 15);
        std::fs::write(shelf.join("m.gguf"), &bytes).unwrap();
        let shelf_spec = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.clone(),
            label: None,
        };
        let mut shelf_man = manifest::build_root_manifest(&shelf_spec, None);
        let hash = identity::sha256_file(&shelf.join("m.gguf"), |_, _| {}).unwrap();
        shelf_man.files[0].sha256 = Some(hash.clone());
        let inv = manifest::merge(&[shelf_man]);

        let mut man = RootManifest {
            schema_version: SCHEMA_VERSION,
            root: RootSpec {
                id: "ext-x".into(),
                kind: RootKind::Removable,
                path: drive.clone(),
                label: None,
            },
            generated_unix: 0,
            files: vec![FileRecord {
                rel_path: PathBuf::from("../escape.gguf"),
                size: bytes.len() as u64,
                fingerprint: None,
                sha256: Some(hash),
                name: None,
                meta: None,
                accessible: true,
                verified_unix: None,
            }],
        };
        let report = VerifyReport {
            mismatched: vec![PathBuf::from("../escape.gguf")],
            ..Default::default()
        };
        let out = repair(&inv, &mut man, &report, |_| {}).unwrap();
        assert_eq!(out.repaired, 0, "must not write outside the root");
        assert_eq!(out.unrepairable.len(), 1);
        assert!(
            !world.path().join("escape.gguf").exists(),
            "escaped the root!"
        );
    }
}
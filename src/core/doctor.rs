//! Store health: the problems spike 3 found in the wild, turned into a
//! report — and, since M10, a remedy per finding.
//!
//! Warden still never writes inside a foreign store itself. Cleanup routes
//! through the owning tool's own CLI (`hf cache rm`, `ollama rm`) — the
//! owner mutates its own store; warden only asks, and only on explicit user
//! action (`doctor --fix`, the GUI's Clean up button). The one narrow
//! exception: `*.incomplete` download debris, which no owner command
//! targets — warden may remove those files itself, guarded against active
//! downloads. Everything else without an owner command gets the exact
//! manual command instead.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A ref names a revision with no snapshot directory — the "file on
    /// disk, invisible to tooling" incident, or a fully pruned repo.
    DanglingRef,
    /// A repo directory with refs but no snapshots and no blobs at all:
    /// pruned content left a husk behind.
    PrunedHusk,
    /// A blob no snapshot references — often the remains of a superseded
    /// revision; reclaimable once confirmed.
    OrphanBlob,
    /// A `*.incomplete` blob: an interrupted download.
    IncompleteDownload,
    /// A snapshot symlink whose blob is gone.
    DanglingSnapshotLink,
    /// An Ollama manifest layer whose blob file is missing.
    MissingOllamaBlob,
}

impl FindingKind {
    pub fn label(self) -> &'static str {
        match self {
            FindingKind::DanglingRef => "dangling ref",
            FindingKind::PrunedHusk => "pruned husk",
            FindingKind::OrphanBlob => "orphan blob",
            FindingKind::IncompleteDownload => "incomplete download",
            FindingKind::DanglingSnapshotLink => "dangling snapshot link",
            FindingKind::MissingOllamaBlob => "missing ollama blob",
        }
    }

    /// What this class of problem *is*, for a reader who doesn't live in
    /// the store's internals.
    pub fn explanation(self) -> &'static str {
        match self {
            FindingKind::DanglingRef => {
                "A branch pointer (refs file) names a revision that no longer \
                 exists on disk. Tools resolving the ref see nothing, even if \
                 other revisions still hold files."
            }
            FindingKind::PrunedHusk => {
                "The repo folder still exists but every byte of content was \
                 pruned earlier — only the refs skeleton remains. It holds no \
                 data and only confuses tooling."
            }
            FindingKind::OrphanBlob => {
                "A content file no snapshot references — usually left behind \
                 when a newer revision superseded it. Real bytes, reachable by \
                 nothing."
            }
            FindingKind::IncompleteDownload => {
                "A temp file from an interrupted download. Not a valid model; \
                 the downloader would resume or restart it."
            }
            FindingKind::DanglingSnapshotLink => {
                "A snapshot entry pointing at a content blob that was pruned. \
                 The file appears to exist but its bytes are gone."
            }
            FindingKind::MissingOllamaBlob => {
                "An Ollama model's manifest names a weights blob that is \
                 missing from the blob store. The model is registered but \
                 cannot run."
            }
        }
    }

    /// What fixing it costs — stated so consent is informed.
    pub fn loss(self) -> &'static str {
        match self {
            FindingKind::DanglingRef => "nothing — the content it points to is already gone",
            FindingKind::PrunedHusk => "nothing — no bytes exist inside",
            FindingKind::OrphanBlob => {
                "the unreferenced bytes themselves; nothing uses them, but this IS real data"
            }
            FindingKind::IncompleteDownload => {
                "resuming that download later — a redownload starts from zero"
            }
            FindingKind::DanglingSnapshotLink => "nothing — it points at bytes already gone",
            FindingKind::MissingOllamaBlob => {
                "the model's registration in ollama (its bytes are already missing)"
            }
        }
    }
}

/// How a finding gets fixed. Owner commands are executed by warden on
/// explicit user action; debris files are the one thing warden removes
/// itself; everything else is a command for the human.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Remedy {
    /// The owning tool cleans its own store; warden only asks.
    OwnerCommand { program: String, args: Vec<String> },
    /// `*.incomplete` download debris — no owner command exists; warden may
    /// remove the file itself (guarded against active downloads).
    DebrisFile { path: PathBuf },
    /// No safe automated path: the exact command, for the human to run.
    Manual { command: String },
}

impl Remedy {
    pub fn display(&self) -> String {
        match self {
            Remedy::OwnerCommand { program, args } => {
                format!("{program} {}", args.join(" "))
            }
            Remedy::DebrisFile { path } => format!("remove {}", path.display()),
            Remedy::Manual { command } => format!("manual: {command}"),
        }
    }

    /// Whether `warden doctor --fix` / the GUI button can execute this.
    pub fn executable(&self) -> bool {
        !matches!(self, Remedy::Manual { .. })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    /// The repo or model the finding is about.
    pub subject: String,
    pub detail: String,
    /// Bytes involved (orphan/incomplete sizes); 0 when size isn't the point.
    pub bytes: u64,
    pub remedy: Remedy,
}

/// Which owner CLIs are on this machine — remedies prefer them and fall
/// back to manual commands.
#[derive(Debug, Clone, Copy, Default)]
pub struct OwnerTools {
    pub hf: bool,
    pub ollama: bool,
}

impl OwnerTools {
    pub fn detect() -> Self {
        Self {
            hf: cli_available("hf"),
            ollama: cli_available("ollama"),
        }
    }
}

fn cli_available(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let p = dir.join(name);
        std::fs::metadata(&p).is_ok_and(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        })
    })
}

/// Check every store. Unreadable directories contribute nothing — degrade,
/// never fail the report.
pub fn check(ollama_stores: &[PathBuf], hf_hub: Option<&Path>) -> Vec<Finding> {
    check_with_tools(ollama_stores, hf_hub, OwnerTools::detect())
}

pub fn check_with_tools(
    ollama_stores: &[PathBuf],
    hf_hub: Option<&Path>,
    tools: OwnerTools,
) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(hub) = hf_hub {
        check_hf_hub(hub, tools, &mut out);
    }
    for store in ollama_stores {
        check_ollama(store, tools, &mut out);
    }
    out
}

/// Execute a remedy on explicit user request. Owner commands run the
/// owning tool; debris files are removed after an active-download guard.
/// Manual remedies are never executed — the error carries the command.
pub fn apply(remedy: &Remedy) -> Result<String> {
    apply_with_min_debris_age(remedy, std::time::Duration::from_secs(15 * 60))
}

fn apply_with_min_debris_age(remedy: &Remedy, min_age: std::time::Duration) -> Result<String> {
    match remedy {
        Remedy::OwnerCommand { program, args } => {
            let output = std::process::Command::new(program)
                .args(args)
                .output()
                .with_context(|| format!("running {program}"))?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() {
                bail!("{program} failed ({}): {}", output.status, stderr.trim());
            }
            Ok(format!(
                "{} {} — done",
                program,
                args.join(" ")
            ))
        }
        Remedy::DebrisFile { path } => {
            if !path
                .file_name()
                .is_some_and(|f| f.to_string_lossy().ends_with(".incomplete"))
            {
                bail!("refusing: {} is not *.incomplete debris", path.display());
            }
            let md = std::fs::metadata(path)
                .with_context(|| format!("checking {}", path.display()))?;
            let age = md
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .unwrap_or_default();
            if age < min_age {
                bail!(
                    "{} was written recently — a download may be running; retry later",
                    path.display()
                );
            }
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            Ok(format!("removed {}", path.display()))
        }
        Remedy::Manual { command } => {
            bail!("manual remedy — run yourself: {command}")
        }
    }
}

fn hf_repo_remedy(tools: OwnerTools, repo: &str, repo_dir: &Path) -> Remedy {
    if tools.hf {
        Remedy::OwnerCommand {
            program: "hf".into(),
            args: vec!["cache".into(), "rm".into(), repo.into(), "-y".into()],
        }
    } else {
        Remedy::Manual {
            command: format!("rm -r '{}'", repo_dir.display()),
        }
    }
}

fn check_hf_hub(hub: &Path, tools: OwnerTools, out: &mut Vec<Finding>) {
    let Ok(repos) = std::fs::read_dir(hub) else {
        return;
    };
    for repo_dir in repos.flatten() {
        let dirname = repo_dir.file_name().to_string_lossy().into_owned();
        let Some(rest) = dirname.strip_prefix("models--") else {
            continue;
        };
        let repo = rest.replace("--", "/");
        let path = repo_dir.path();

        // Revisions that actually exist on disk.
        let snapshot_revs: BTreeSet<String> = std::fs::read_dir(path.join("snapshots"))
            .map(|revs| {
                revs.flatten()
                    .filter(|r| r.path().is_dir())
                    .map(|r| r.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();

        // Blobs on disk, split into finished and interrupted.
        let mut blobs: BTreeSet<String> = BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(path.join("blobs")) {
            for b in entries.flatten() {
                let name = b.file_name().to_string_lossy().into_owned();
                let size = b.metadata().map(|m| m.len()).unwrap_or(0);
                if name.ends_with(".incomplete") {
                    out.push(Finding {
                        kind: FindingKind::IncompleteDownload,
                        subject: repo.clone(),
                        detail: format!("blobs/{name}"),
                        bytes: size,
                        remedy: Remedy::DebrisFile { path: b.path() },
                    });
                } else {
                    blobs.insert(name);
                }
            }
        }

        // A husk: refs survive but every snapshot and finished blob is gone.
        let refs = read_refs(&path);
        if snapshot_revs.is_empty() && blobs.is_empty() && !refs.is_empty() {
            out.push(Finding {
                kind: FindingKind::PrunedHusk,
                subject: repo.clone(),
                detail: format!(
                    "refs remain ({}) but all content is pruned",
                    refs.iter()
                        .map(|(n, r)| format!("{n} → {}", &r[..r.len().min(12)]))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                bytes: 0,
                remedy: hf_repo_remedy(tools, &repo, &path),
            });
        } else {
            for (name, rev) in &refs {
                if !snapshot_revs.contains(rev) {
                    out.push(Finding {
                        kind: FindingKind::DanglingRef,
                        subject: repo.clone(),
                        detail: format!(
                            "refs/{name} → {} has no snapshot",
                            &rev[..rev.len().min(12)]
                        ),
                        bytes: 0,
                        remedy: Remedy::Manual {
                            command: format!("rm '{}'", path.join("refs").join(name).display()),
                        },
                    });
                }
            }
        }

        // Which blobs do snapshots actually reference? And are any links dead?
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        for rev in &snapshot_revs {
            walk_links(&path.join("snapshots").join(rev), 0, &repo, &mut referenced, out);
        }
        for blob in blobs.difference(&referenced) {
            let blob_path = path.join("blobs").join(blob);
            let size = std::fs::metadata(&blob_path).map(|m| m.len()).unwrap_or(0);
            out.push(Finding {
                kind: FindingKind::OrphanBlob,
                subject: repo.clone(),
                detail: format!(
                    "blobs/{} referenced by no snapshot",
                    &blob[..blob.len().min(16)]
                ),
                bytes: size,
                remedy: Remedy::Manual {
                    command: format!("rm '{}'", blob_path.display()),
                },
            });
        }
    }
}

fn read_refs(repo_path: &Path) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo_path.join("refs")) {
        for r in entries.flatten() {
            if r.path().is_file()
                && let Ok(rev) = std::fs::read_to_string(r.path())
            {
                refs.push((
                    r.file_name().to_string_lossy().into_owned(),
                    rev.trim().to_string(),
                ));
            }
        }
    }
    refs
}

fn walk_links(
    dir: &Path,
    depth: usize,
    repo: &str,
    referenced: &mut BTreeSet<String>,
    out: &mut Vec<Finding>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for f in entries.flatten() {
        let p = f.path();
        if p.is_dir() {
            walk_links(&p, depth + 1, repo, referenced, out);
        } else if let Ok(target) = std::fs::canonicalize(&p) {
            if let Some(base) = target.file_name() {
                referenced.insert(base.to_string_lossy().into_owned());
            }
        } else if p.is_symlink() {
            out.push(Finding {
                kind: FindingKind::DanglingSnapshotLink,
                subject: repo.to_string(),
                detail: format!("{} → pruned blob", p.display()),
                bytes: 0,
                remedy: Remedy::Manual {
                    command: format!("rm '{}'", p.display()),
                },
            });
        }
    }
}

fn check_ollama(store: &Path, tools: OwnerTools, out: &mut Vec<Finding>) {
    let manifests_root = store.join("manifests");
    let mut stack = vec![manifests_root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            // manifests/<host>/<namespace>/<name>/<tag> → the name ollama
            // itself knows, so `ollama rm` can act on it.
            let rel_parts: Vec<String> = path
                .strip_prefix(&manifests_root)
                .map(|r| r.iter().map(|c| c.to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            let name = match rel_parts.as_slice() {
                [_host, ns, name, tag] if ns == "library" => format!("{name}:{tag}"),
                [_host, ns, name, tag] => format!("{ns}/{name}:{tag}"),
                _ => continue,
            };
            let Some(layers) = json.get("layers").and_then(|l| l.as_array()) else {
                continue;
            };
            for layer in layers {
                let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) else {
                    continue;
                };
                let blob = store.join("blobs").join(digest.replace(':', "-"));
                if !blob.is_file() {
                    let remedy = if tools.ollama {
                        Remedy::OwnerCommand {
                            program: "ollama".into(),
                            args: vec!["rm".into(), name.clone()],
                        }
                    } else {
                        Remedy::Manual {
                            command: format!("rm '{}'", path.display()),
                        }
                    };
                    out.push(Finding {
                        kind: FindingKind::MissingOllamaBlob,
                        subject: name.clone(),
                        detail: format!("layer {digest} has no blob"),
                        bytes: layer.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                        remedy,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TOOLS: OwnerTools = OwnerTools {
        hf: true,
        ollama: true,
    };
    const NO_TOOLS: OwnerTools = OwnerTools {
        hf: false,
        ollama: false,
    };

    fn kinds(findings: &[Finding]) -> Vec<FindingKind> {
        findings.iter().map(|f| f.kind).collect()
    }

    #[test]
    fn healthy_repo_reports_nothing() {
        let hub = tempfile::tempdir().unwrap();
        let repo = hub.path().join("models--org--Good");
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        std::fs::create_dir_all(repo.join("snapshots/rev1")).unwrap();
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("blobs/aabb"), b"model bytes").unwrap();
        std::os::unix::fs::symlink(repo.join("blobs/aabb"), repo.join("snapshots/rev1/m.gguf"))
            .unwrap();
        std::fs::write(repo.join("refs/main"), "rev1").unwrap();
        assert!(check_with_tools(&[], Some(hub.path()), ALL_TOOLS).is_empty());
    }

    #[test]
    fn finds_every_hf_problem_class() {
        let hub = tempfile::tempdir().unwrap();

        // Husk: refs, nothing else.
        let husk = hub.path().join("models--org--Husk");
        std::fs::create_dir_all(husk.join("refs")).unwrap();
        std::fs::write(husk.join("refs/main"), "deadbeef00").unwrap();

        // Repo with an orphan, an incomplete, a dangling ref, and a dangling link.
        let repo = hub.path().join("models--org--Messy");
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        std::fs::create_dir_all(repo.join("snapshots/rev1")).unwrap();
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("blobs/used"), b"referenced").unwrap();
        std::fs::write(repo.join("blobs/orphan"), b"nobody points here").unwrap();
        std::fs::write(repo.join("blobs/dl.incomplete"), b"partial").unwrap();
        std::os::unix::fs::symlink(repo.join("blobs/used"), repo.join("snapshots/rev1/m.gguf"))
            .unwrap();
        std::os::unix::fs::symlink(
            repo.join("blobs/pruned-away"),
            repo.join("snapshots/rev1/gone.gguf"),
        )
        .unwrap();
        std::fs::write(repo.join("refs/main"), "rev1").unwrap();
        std::fs::write(repo.join("refs/old"), "rev0gone").unwrap();

        let findings = check_with_tools(&[], Some(hub.path()), ALL_TOOLS);
        let ks = kinds(&findings);
        assert!(ks.contains(&FindingKind::PrunedHusk));
        assert!(ks.contains(&FindingKind::OrphanBlob));
        assert!(ks.contains(&FindingKind::IncompleteDownload));
        assert!(ks.contains(&FindingKind::DanglingRef));
        assert!(ks.contains(&FindingKind::DanglingSnapshotLink));
        // The healthy parts stay quiet: exactly one of each problem.
        assert_eq!(findings.len(), 5, "{findings:?}");
    }

    #[test]
    fn remedies_prefer_owner_tools_and_fall_back_to_manual() {
        let hub = tempfile::tempdir().unwrap();
        let husk = hub.path().join("models--org--Husk");
        std::fs::create_dir_all(husk.join("refs")).unwrap();
        std::fs::write(husk.join("refs/main"), "deadbeef00").unwrap();

        let with = check_with_tools(&[], Some(hub.path()), ALL_TOOLS);
        assert_eq!(
            with[0].remedy,
            Remedy::OwnerCommand {
                program: "hf".into(),
                args: vec!["cache".into(), "rm".into(), "org/Husk".into(), "-y".into()],
            }
        );
        let without = check_with_tools(&[], Some(hub.path()), NO_TOOLS);
        match &without[0].remedy {
            Remedy::Manual { command } => assert!(command.starts_with("rm -r ")),
            other => panic!("expected manual fallback, got {other:?}"),
        }
    }

    #[test]
    fn missing_ollama_blob_remedy_is_ollama_rm_by_model_name() {
        let store = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(store.path().join("blobs")).unwrap();
        let mdir = store.path().join("manifests/registry.ollama.ai/library/ghost");
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("latest"),
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:gone","size":42}]}"#,
        )
        .unwrap();
        let findings = check_with_tools(&[store.path().to_path_buf()], None, ALL_TOOLS);
        assert_eq!(kinds(&findings), vec![FindingKind::MissingOllamaBlob]);
        assert_eq!(findings[0].subject, "ghost:latest");
        assert_eq!(
            findings[0].remedy,
            Remedy::OwnerCommand {
                program: "ollama".into(),
                args: vec!["rm".into(), "ghost:latest".into()],
            }
        );
    }

    #[test]
    fn debris_removal_is_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let debris = dir.path().join("dl.incomplete");
        std::fs::write(&debris, b"partial").unwrap();

        // A freshly-written file might be an active download: refused.
        let err = apply_with_min_debris_age(
            &Remedy::DebrisFile {
                path: debris.clone(),
            },
            std::time::Duration::from_secs(3600),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("recently"));
        assert!(debris.exists());

        // Old enough: removed.
        apply_with_min_debris_age(
            &Remedy::DebrisFile {
                path: debris.clone(),
            },
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert!(!debris.exists());

        // Never anything that isn't *.incomplete.
        let real = dir.path().join("model.gguf");
        std::fs::write(&real, b"bytes").unwrap();
        let err = apply_with_min_debris_age(
            &Remedy::DebrisFile { path: real.clone() },
            std::time::Duration::ZERO,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not *.incomplete"));
        assert!(real.exists());
    }

    #[test]
    fn manual_remedies_are_never_executed() {
        let err = apply(&Remedy::Manual {
            command: "rm /something".into(),
        })
        .unwrap_err();
        assert!(format!("{err}").contains("run yourself"));
    }
}

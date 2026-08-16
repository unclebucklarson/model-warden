//! Store health: the problems spike 3 found in the wild, turned into a
//! report. Read-only — doctor never fixes anything, it names what's wrong
//! so the user (or a later warden feature) can act deliberately.
//!
//! Real findings from this machine's HF cache on day one: 4 pruned husk
//! repos with dangling refs, 2 interrupted downloads, a 349 MiB orphan blob.

use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

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
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    /// The repo or model the finding is about.
    pub subject: String,
    pub detail: String,
    /// Bytes involved (orphan/incomplete sizes); 0 when size isn't the point.
    pub bytes: u64,
}

/// Check every store. Unreadable directories contribute nothing — degrade,
/// never fail the report.
pub fn check(ollama_stores: &[std::path::PathBuf], hf_hub: Option<&Path>) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(hub) = hf_hub {
        check_hf_hub(hub, &mut out);
    }
    for store in ollama_stores {
        check_ollama(store, &mut out);
    }
    out
}

fn check_hf_hub(hub: &Path, out: &mut Vec<Finding>) {
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
            });
        } else {
            for (name, rev) in &refs {
                if !snapshot_revs.contains(rev) {
                    out.push(Finding {
                        kind: FindingKind::DanglingRef,
                        subject: repo.clone(),
                        detail: format!("refs/{name} → {} has no snapshot", &rev[..rev.len().min(12)]),
                        bytes: 0,
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
            let size = std::fs::metadata(path.join("blobs").join(blob))
                .map(|m| m.len())
                .unwrap_or(0);
            out.push(Finding {
                kind: FindingKind::OrphanBlob,
                subject: repo.clone(),
                detail: format!("blobs/{} referenced by no snapshot", &blob[..blob.len().min(16)]),
                bytes: size,
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
            });
        }
    }
}

fn check_ollama(store: &Path, out: &mut Vec<Finding>) {
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
            let name = path
                .strip_prefix(&manifests_root)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| path.display().to_string());
            let Some(layers) = json.get("layers").and_then(|l| l.as_array()) else {
                continue;
            };
            for layer in layers {
                let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) else {
                    continue;
                };
                let blob = store.join("blobs").join(digest.replace(':', "-"));
                if !blob.is_file() {
                    out.push(Finding {
                        kind: FindingKind::MissingOllamaBlob,
                        subject: name.clone(),
                        detail: format!("layer {digest} has no blob"),
                        bytes: layer.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(check(&[], Some(hub.path())).is_empty());
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

        let findings = check(&[], Some(hub.path()));
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
    fn finds_missing_ollama_blob() {
        let store = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(store.path().join("blobs")).unwrap();
        let mdir = store.path().join("manifests/registry.ollama.ai/library/ghost");
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("latest"),
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:gone","size":42}]}"#,
        )
        .unwrap();
        let findings = check(&[store.path().to_path_buf()], None);
        assert_eq!(kinds(&findings), vec![FindingKind::MissingOllamaBlob]);
        assert_eq!(findings[0].bytes, 42);
    }
}

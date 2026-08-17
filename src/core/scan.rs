//! Store scanners: one view of every GGUF this machine has, wherever it
//! lives — shelf directories, Ollama's blob store (manifest → blob), and the
//! HuggingFace hub cache.
//!
//! Harvested from llamacppCodeConf (src/core/library.rs), minus its
//! serving-side helpers, with three warden-specific changes the M0 spikes
//! drove:
//!
//! * **Every snapshot is enumerated**, and files inside snapshot
//!   subdirectories are found too (split-quant repos keep GGUFs in
//!   per-quant subfolders).
//! * **mmproj projectors are inventoried.** The serving-side sibling hides
//!   them because they aren't servable alone; warden's job is storage truth,
//!   and their bytes count.
//! * **Inaccessible files stay visible.** A snapshot symlink whose blob was
//!   pruned (hub cache GC — the "file on disk, invisible to tooling"
//!   incident) is listed with `accessible: false` instead of vanishing.

use crate::core::gguf::{self, GgufMeta};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Found under a configured scan directory.
    Shelf,
    /// A weights blob in Ollama's store; `name` is the `model:tag` Ollama
    /// knows it by. The blob is a raw GGUF.
    Ollama { name: String },
    /// A GGUF in the HuggingFace hub cache, downloaded by llama-server's
    /// `-hf`, unsloth studio, or the hf CLI. `repo` is "org/name".
    HfHub { repo: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelFile {
    pub path: PathBuf,
    pub file_size: u64,
    pub source: Source,
    /// `false` when the bytes aren't reachable: a dangling snapshot symlink
    /// (pruned blob) or a manifest naming a blob that's gone. The entry is
    /// still listed — a missing file must be visible, not invisible.
    pub accessible: bool,
    /// `None` when the header couldn't be read; the file is still listed so
    /// a broken download is visible rather than invisible.
    pub meta: Option<GgufMeta>,
}

impl ModelFile {
    /// The label a row leads with: Ollama's name, the HF repo + file, or
    /// the file stem, in that order of preference.
    pub fn display_name(&self) -> String {
        let stem = || {
            self.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.display().to_string())
        };
        match &self.source {
            Source::Ollama { name } => name.clone(),
            Source::HfHub { repo } => format!("{repo} — {}", stem()),
            Source::Shelf => stem(),
        }
    }
}

/// Ollama store locations worth probing, most specific first: the
/// `OLLAMA_MODELS` env var, the per-user store, then the system service's
/// store. Only existing directories are returned.
pub fn default_ollama_stores() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(env_store) = std::env::var("OLLAMA_MODELS") {
        candidates.push(PathBuf::from(env_store));
    }
    if let Some(home) = std::env::home_dir() {
        candidates.push(home.join(".ollama/models"));
    }
    candidates.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
    candidates.retain(|p| p.join("manifests").is_dir());
    candidates
}

/// The HuggingFace hub cache, when present.
pub fn default_hf_hub() -> Option<PathBuf> {
    let hub = std::env::var_os("HF_HOME")
        .map(|h| PathBuf::from(h).join("hub"))
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".cache/huggingface/hub")
        });
    hub.is_dir().then_some(hub)
}

/// GGUFs in the HF hub cache: `hub/models--org--name/snapshots/<rev>/**.gguf`
/// — every revision, subdirectories included.
pub fn hf_hub_models(hub: &Path) -> Vec<ModelFile> {
    let mut out = Vec::new();
    let Ok(repos) = std::fs::read_dir(hub) else {
        return out;
    };
    for repo_dir in repos.flatten() {
        let dirname = repo_dir.file_name().to_string_lossy().into_owned();
        let Some(rest) = dirname.strip_prefix("models--") else {
            continue;
        };
        let repo = rest.replace("--", "/");
        let snapshots = repo_dir.path().join("snapshots");
        let Ok(revs) = std::fs::read_dir(&snapshots) else {
            continue;
        };
        for rev in revs.flatten() {
            walk_snapshot(&rev.path(), 0, &repo, &mut out);
        }
    }
    out
}

/// Collect `*.gguf` under one snapshot revision, a few levels deep
/// (split-quant repos use `<quant>/<file>.gguf` subfolders).
fn walk_snapshot(dir: &Path, depth: usize, repo: &str, out: &mut Vec<ModelFile>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for f in entries.flatten() {
        let path = f.path();
        if path.is_dir() {
            walk_snapshot(&path, depth + 1, repo, out);
        } else if path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("gguf"))
        {
            // Snapshot entries are usually symlinks into blobs/; metadata()
            // follows them, so it fails when the blob has been pruned even
            // though the symlink is still there. That file is listed as
            // inaccessible, not skipped.
            let accessible = std::fs::metadata(&path).is_ok();
            let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push(ModelFile {
                meta: gguf::read_meta(&path).ok(),
                path,
                file_size,
                source: Source::HfHub {
                    repo: repo.to_string(),
                },
                accessible,
            });
        }
    }
}

/// Scan shelf directories, the Ollama stores, and (when given) the HF hub
/// cache. Never errors as a whole — an unreadable directory contributes
/// nothing rather than sinking the scan. Duplicate paths (a shelf dir
/// listed twice, two store candidates that resolve to the same blob)
/// collapse to one entry.
pub fn scan(
    scan_dirs: &[PathBuf],
    ollama_stores: &[PathBuf],
    hf_hub: Option<&Path>,
) -> Vec<ModelFile> {
    let mut out = Vec::new();
    for dir in scan_dirs {
        walk_gguf(dir, 0, &mut out);
    }
    for store in ollama_stores {
        out.extend(ollama_models(store));
    }
    if let Some(hub) = hf_hub {
        out.extend(hf_hub_models(hub));
    }
    // Dedupe by inode, not path: an archived model hardlinked into the
    // shelf is the SAME file as its cache original and must appear once.
    // Shelf entries are pushed first, so the user-owned copy is the one
    // that survives. (Byte-identical copies with different inodes are the
    // hash worker's job, not this pass's.)
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| {
        use std::os::unix::fs::MetadataExt;
        let key = std::fs::metadata(&m.path)
            .map(|md| (md.dev(), md.ino()))
            .unwrap_or((0, 0));
        key == (0, 0) || seen.insert(key)
    });
    // Stable order: shelf, then Ollama, then HF hub — alphabetical within.
    out.sort_by(|a, b| {
        let rank = |m: &ModelFile| match m.source {
            Source::Shelf => 0u8,
            Source::Ollama { .. } => 1,
            Source::HfHub { .. } => 2,
        };
        (rank(a), a.display_name().to_lowercase()).cmp(&(rank(b), b.display_name().to_lowercase()))
    });
    out
}

/// GGUFs under one shelf directory (manifest generation scans roots
/// individually rather than through the deduping `scan()` view — per-root
/// truth lists a file in every root that has it).
pub fn shelf_models(dir: &Path) -> Vec<ModelFile> {
    let mut out = Vec::new();
    walk_gguf(dir, 0, &mut out);
    out
}

/// Depth-limited recursive walk collecting `*.gguf`. The shelf layout here
/// is `~/models/<ModelName>/<file>.gguf`, so a few levels is plenty.
fn walk_gguf(dir: &Path, depth: usize, out: &mut Vec<ModelFile>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk_gguf(&path, depth + 1, out);
        } else if path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("gguf"))
        {
            let file_size = e.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(ModelFile {
                meta: gguf::read_meta(&path).ok(),
                path,
                file_size,
                source: Source::Shelf,
                accessible: true,
            });
        }
    }
}

/// Enumerate Ollama's store by walking its manifests: each manifest is JSON
/// whose layer with mediaType `…image.model` names the weights blob.
///
/// Manifest paths look like
/// `manifests/registry.ollama.ai/library/<name>/<tag>`; the name shown is
/// `<name>:<tag>` (with the namespace kept when it isn't `library`).
pub fn ollama_models(store: &Path) -> Vec<ModelFile> {
    let mut out = Vec::new();
    let manifests = store.join("manifests");
    let mut stack = vec![manifests.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                ollama_models_from_manifest(store, &manifests, &path, &mut out);
            }
        }
    }
    out
}

fn ollama_models_from_manifest(
    store: &Path,
    manifests_root: &Path,
    manifest: &Path,
    out: &mut Vec<ModelFile>,
) {
    let Some(json) = std::fs::read_to_string(manifest)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return;
    };
    // manifests/<host>/<namespace>/<name>/<tag> → "name:tag" (or
    // "namespace/name:tag" for non-library namespaces).
    let Ok(rel) = manifest.strip_prefix(manifests_root) else {
        return;
    };
    let parts: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
    let name = match parts.as_slice() {
        [_host, ns, name, tag] if ns == "library" => format!("{name}:{tag}"),
        [_host, ns, name, tag] => format!("{ns}/{name}:{tag}"),
        _ => return,
    };
    let Some(layers) = json.get("layers").and_then(|l| l.as_array()) else {
        return;
    };
    // The weights blob plus, for vision models, the projector blob — both
    // are bytes the model needs to run, so both are inventory. The
    // projector's `+projector` name suffix ties it to its model (bundles
    // group by the base name).
    for (suffix, media_suffix) in [("", "image.model"), ("+projector", "image.projector")] {
        let Some(digest) = layers.iter().find_map(|l| {
            l.get("mediaType")?
                .as_str()?
                .ends_with(media_suffix)
                .then(|| l.get("digest")?.as_str().map(str::to_string))?
        }) else {
            continue;
        };
        let blob = store.join("blobs").join(digest.replace(':', "-"));
        // A manifest naming a blob that's gone is the same incident class
        // as a pruned HF snapshot: list it, honestly inaccessible.
        let accessible = blob.is_file();
        let file_size = std::fs::metadata(&blob).map(|m| m.len()).unwrap_or(0);
        out.push(ModelFile {
            meta: gguf::read_meta(&blob).ok(),
            path: blob,
            file_size,
            source: Source::Ollama {
                name: format!("{name}{suffix}"),
            },
            accessible,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gguf::tests::synthetic_gguf;

    fn shelf_with(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, bytes) in files {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        dir
    }

    #[test]
    fn finds_ggufs_in_nested_shelf_layout() {
        let shelf = shelf_with(&[
            (
                "Qwen3.6-27B/Qwen3.6-27B-UD-Q5_K_XL.gguf",
                &synthetic_gguf("qwen3", 262_144, 17)[..],
            ),
            ("Tiny/tiny.GGUF", &synthetic_gguf("llama", 4096, 15)[..]),
            ("notes/readme.txt", b"not a model"),
        ]);
        let models = scan(&[shelf.path().to_path_buf()], &[], None);
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| matches!(m.source, Source::Shelf)));
        let qwen = models
            .iter()
            .find(|m| m.display_name().starts_with("Qwen3.6"))
            .unwrap();
        assert_eq!(qwen.meta.as_ref().unwrap().context_length, Some(262_144));
        assert_eq!(
            qwen.meta.as_ref().unwrap().quantization.as_deref(),
            Some("Q5_K_M")
        );
    }

    #[test]
    fn broken_gguf_is_listed_without_meta() {
        let shelf = shelf_with(&[("Broken/broken.gguf", b"corrupt")]);
        let models = scan(&[shelf.path().to_path_buf()], &[], None);
        assert_eq!(models.len(), 1);
        assert!(models[0].meta.is_none(), "visible, but honestly meta-less");
        assert!(models[0].accessible, "bytes are there, header is not");
    }

    #[test]
    fn missing_scan_dir_contributes_nothing() {
        let models = scan(&[PathBuf::from("/nonexistent/nowhere")], &[], None);
        assert!(models.is_empty());
    }

    #[test]
    fn duplicate_scan_dirs_collapse_to_one_entry() {
        let shelf = shelf_with(&[("Tiny/tiny.gguf", &synthetic_gguf("llama", 4096, 15)[..])]);
        let dir = shelf.path().to_path_buf();
        let models = scan(&[dir.clone(), dir], &[], None);
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn reads_the_ollama_store_layout() {
        let store = tempfile::tempdir().unwrap();
        let blob_bytes = synthetic_gguf("gemma3", 131_072, 15);
        let digest = "sha256-abc123";
        std::fs::create_dir_all(store.path().join("blobs")).unwrap();
        std::fs::write(store.path().join("blobs").join(digest), &blob_bytes).unwrap();
        let mdir = store
            .path()
            .join("manifests/registry.ollama.ai/library/gemma4");
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("latest"),
            r#"{"layers":[
                {"mediaType":"application/vnd.ollama.image.template","digest":"sha256-zzz","size":10},
                {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:abc123","size":100}
            ]}"#,
        )
        .unwrap();

        let models = ollama_models(store.path());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name(), "gemma4:latest");
        assert!(models[0].accessible);
        assert_eq!(
            models[0].meta.as_ref().unwrap().context_length,
            Some(131_072)
        );
        assert!(models[0].path.ends_with("blobs/sha256-abc123"));
    }

    #[test]
    fn ollama_projector_layers_are_inventoried_with_tied_names() {
        let store = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(store.path().join("blobs")).unwrap();
        std::fs::write(
            store.path().join("blobs/sha256-model1"),
            synthetic_gguf("qwen3", 4096, 15),
        )
        .unwrap();
        std::fs::write(
            store.path().join("blobs/sha256-proj1"),
            synthetic_gguf("clip", 0, 1),
        )
        .unwrap();
        let mdir = store
            .path()
            .join("manifests/registry.ollama.ai/library/vision");
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("latest"),
            r#"{"layers":[
                {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:model1","size":100},
                {"mediaType":"application/vnd.ollama.image.projector","digest":"sha256:proj1","size":50}
            ]}"#,
        )
        .unwrap();
        let models = ollama_models(store.path());
        assert_eq!(models.len(), 2, "weights AND projector");
        assert!(models.iter().any(|m| m.display_name() == "vision:latest"));
        assert!(
            models
                .iter()
                .any(|m| m.display_name() == "vision:latest+projector")
        );
    }

    #[test]
    fn ollama_manifest_with_pruned_blob_is_listed_inaccessible() {
        let store = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(store.path().join("blobs")).unwrap();
        let mdir = store
            .path()
            .join("manifests/registry.ollama.ai/library/ghost");
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("latest"),
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:gone","size":100}]}"#,
        )
        .unwrap();
        let models = ollama_models(store.path());
        assert_eq!(models.len(), 1);
        assert!(!models[0].accessible);
        assert_eq!(models[0].file_size, 0);
    }

    #[test]
    fn hf_hub_enumerates_every_snapshot_and_subdirs() {
        let hub = tempfile::tempdir().unwrap();
        let repo = hub.path().join("models--unsloth--Test-GGUF");
        let bytes = synthetic_gguf("qwen3", 4096, 15);
        // Two revisions; the second keeps its GGUF in a split-quant subdir.
        let rev1 = repo.join("snapshots/aaaa");
        let rev2 = repo.join("snapshots/bbbb/UD-Q5_K_XL");
        std::fs::create_dir_all(&rev1).unwrap();
        std::fs::create_dir_all(&rev2).unwrap();
        std::fs::write(rev1.join("old.gguf"), &bytes).unwrap();
        std::fs::write(rev2.join("new-UD-Q5_K_XL.gguf"), &bytes).unwrap();

        let models = hf_hub_models(hub.path());
        assert_eq!(models.len(), 2, "both revisions, subdir included");
        assert!(
            models
                .iter()
                .all(|m| matches!(&m.source, Source::HfHub { repo } if repo == "unsloth/Test-GGUF"))
        );
    }

    #[test]
    fn mmproj_projectors_are_inventoried() {
        let hub = tempfile::tempdir().unwrap();
        let snap = hub.path().join("models--org--Vision-GGUF/snapshots/rev1");
        std::fs::create_dir_all(&snap).unwrap();
        let bytes = synthetic_gguf("clip", 0, 1);
        std::fs::write(snap.join("mmproj-F16.gguf"), &bytes).unwrap();
        let models = hf_hub_models(hub.path());
        assert_eq!(models.len(), 1, "projector bytes are inventory too");
    }

    #[test]
    fn dangling_snapshot_symlink_is_listed_inaccessible() {
        let hub = tempfile::tempdir().unwrap();
        let snap = hub.path().join("models--org--Pruned-GGUF/snapshots/rev1");
        std::fs::create_dir_all(&snap).unwrap();
        std::os::unix::fs::symlink(
            hub.path().join("models--org--Pruned-GGUF/blobs/gone"),
            snap.join("model-Q4_K_M.gguf"),
        )
        .unwrap();
        let models = hf_hub_models(hub.path());
        assert_eq!(models.len(), 1, "pruned ≠ invisible");
        assert!(!models[0].accessible);
        assert!(models[0].meta.is_none());
    }

    #[test]
    fn hardlinked_archive_and_original_dedupe_to_the_shelf_row() {
        let shelf = shelf_with(&[(
            "Archived/model.gguf",
            &synthetic_gguf("llama", 4096, 15)[..],
        )]);
        // Simulate the HF original as a hardlink of the archived file, in a
        // hub-layout directory that scan() reads via the hf_hub param.
        let hub = tempfile::tempdir().unwrap();
        let snapdir = hub.path().join("models--org--repo/snapshots/rev1");
        std::fs::create_dir_all(&snapdir).unwrap();
        std::fs::hard_link(
            shelf.path().join("Archived/model.gguf"),
            snapdir.join("model.gguf"),
        )
        .unwrap();

        let models = scan(&[shelf.path().to_path_buf()], &[], Some(hub.path()));
        assert_eq!(models.len(), 1, "one file, one row");
        assert!(matches!(models[0].source, Source::Shelf), "shelf wins");
    }
}

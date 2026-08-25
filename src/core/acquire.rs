//! Acquisition: download GGUFs from HuggingFace into the shelf — the
//! warden-owned tier, where no cache pruner can reach them. Deliberately
//! the last pillar built (per PLAN.md).
//!
//! Downloads stream to a `.partial` temp with Range resume, then rename.
//! After the rename the finished file is hashed and its provenance (repo,
//! revision, etag, when) is recorded in the state dir keyed by content —
//! provenance is known at download time and never again, so it is captured
//! here or lost.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct RemoteFile {
    pub filename: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub tool: String,
    pub repo: String,
    pub filename: String,
    pub revision: Option<String>,
    pub etag: Option<String>,
    pub fetched_unix: u64,
}

#[derive(Debug, Clone)]
pub enum FetchEvent {
    Start {
        label: String,
        total: Option<u64>,
        resumed_from: u64,
    },
    /// Every ~8 MiB.
    Progress { label: String, done: u64, total: Option<u64> },
    Hashing { label: String },
}

impl FetchEvent {
    /// The durable activity-log line, worded identically in both frontends;
    /// `None` for transient progress ticks, which are never logged.
    pub fn log_line(&self) -> Option<String> {
        use crate::core::format::human_size;
        match self {
            Self::Start { label, total, resumed_from } => Some(format!(
                "downloading {label} ({}){}",
                total.map(human_size).unwrap_or_else(|| "size unknown".into()),
                if *resumed_from > 0 {
                    format!(", resuming at {}", human_size(*resumed_from))
                } else {
                    String::new()
                }
            )),
            Self::Progress { .. } => None,
            Self::Hashing { label } => Some(format!("hashing {label}")),
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(60))
        .user_agent(concat!("modelwarden/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn with_auth(req: ureq::Request, token: Option<&str>) -> ureq::Request {
    match token {
        Some(t) => req.set("Authorization", &format!("Bearer {t}")),
        None => req,
    }
}

/// The HF token for gated repos, most explicit source first: caller's
/// (`--token` / the GUI field), warden's config, the env vars the HF
/// tooling uses, then the token file the `hf` CLI writes on login.
pub fn resolve_token(explicit: Option<String>, cfg: &crate::core::settings::AppConfig) -> Option<String> {
    explicit
        .filter(|t| !t.trim().is_empty())
        .or_else(|| cfg.hf_token.clone().filter(|t| !t.trim().is_empty()))
        .or_else(|| std::env::var("HF_TOKEN").ok())
        .or_else(|| std::env::var("HUGGING_FACE_HUB_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            let base = std::env::var_os("HF_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::home_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".cache/huggingface")
                });
            token_from_file(&base.join("token"))
        })
}

fn token_from_file(path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// llama.cpp split convention: `<name>-00001-of-00003.gguf`. Returns the
/// stem-with-set (`<name>`, part index, part count) when it matches.
pub(crate) fn split_parts(filename: &str) -> Option<(&str, u32, u32)> {
    let stem = filename.strip_suffix(".gguf")?;
    // <prefix>-NNNNN-of-NNNNN
    let (rest, count) = stem.rsplit_once("-of-")?;
    let (prefix, idx) = rest.rsplit_once('-')?;
    if idx.len() != 5 || count.len() != 5 {
        return None;
    }
    Some((prefix, idx.parse().ok()?, count.parse().ok()?))
}

/// The full download set a chosen file implies: every sibling part of its
/// split (in order), or just the file itself. Errors if the listing is
/// missing parts — a split with holes is not servable.
pub fn split_set(all: &[RemoteFile], chosen: &str) -> Result<Vec<String>> {
    let Some((prefix, _, count)) = split_parts(chosen) else {
        return Ok(vec![chosen.to_string()]);
    };
    let mut parts = Vec::new();
    for i in 1..=count {
        let want = format!("{prefix}-{i:05}-of-{count:05}.gguf");
        if !all.iter().any(|f| f.filename == want) {
            bail!("split set incomplete: {want} is missing from the repo listing");
        }
        parts.push(want);
    }
    Ok(parts)
}

/// Complete a download set with the repo's `mmproj` vision projectors —
/// the same rule the catalog's bundle_for applies: a projector rides
/// along with its model (it's required for images), but choosing the
/// projector alone never drags the model in. Mirrored here so the bundle
/// promise starts at download time, not at the first backup.
pub fn with_projectors(all: &[RemoteFile], mut set: Vec<String>) -> Vec<String> {
    let is_projector = |name: &str| {
        let base = name.rsplit('/').next().unwrap_or(name).to_lowercase();
        base.contains("mmproj") && base.ends_with(".gguf")
    };
    if set.iter().all(|f| is_projector(f)) {
        return set; // projector-only choice stays as chosen
    }
    for f in all {
        if is_projector(&f.filename) && !set.contains(&f.filename) {
            set.push(f.filename.clone());
        }
    }
    set
}

/// GGUF files a repo offers, with sizes when the API provides them.
///
/// HF answers **401 for unknown repo ids too** (it hides private repos), so
/// a 401 gets a diagnosis: close-match suggestions when the id looks
/// mistyped, token guidance when it doesn't.
pub fn list_files(repo: &str, token: Option<&str>) -> Result<Vec<RemoteFile>> {
    let json = repo_api_json(repo, token)?;
    files_from_siblings(repo, &json, true)
}

/// Every file a repo offers — the listing behind whole-snapshot downloads
/// of safetensors-style repos.
pub fn list_all_files(repo: &str, token: Option<&str>) -> Result<Vec<RemoteFile>> {
    let json = repo_api_json(repo, token)?;
    files_from_siblings(repo, &json, false)
}

fn repo_api_json(repo: &str, token: Option<&str>) -> Result<serde_json::Value> {
    let url = format!("https://huggingface.co/api/models/{repo}?blobs=true");
    let resp = match with_auth(agent().get(&url), token).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(401 | 404, _)) => {
            let suggestions = suggest_repos(repo, token);
            if suggestions.is_empty() {
                bail!(
                    "{repo}: not found or gated/private (HF hides unknown repos behind 401). \
                     Check the id (case-sensitive), or provide a token — GUI token field, \
                     --token, config hf_token, $HF_TOKEN, or `hf auth login`."
                );
            }
            bail!(
                "{repo}: not found (ids are case-sensitive) — did you mean: {}?",
                suggestions.join(", ")
            );
        }
        Err(e) => return Err(e).with_context(|| format!("querying {repo}")),
    };
    resp.into_json().context("parsing repo metadata")
}

fn files_from_siblings(
    repo: &str,
    json: &serde_json::Value,
    gguf_only: bool,
) -> Result<Vec<RemoteFile>> {
    let Some(siblings) = json.get("siblings").and_then(|s| s.as_array()) else {
        bail!("{repo}: no file list in API response");
    };
    let mut out = Vec::new();
    for s in siblings {
        let Some(name) = s.get("rfilename").and_then(|n| n.as_str()) else {
            continue;
        };
        if !gguf_only || name.to_lowercase().ends_with(".gguf") {
            out.push(RemoteFile {
                filename: name.to_string(),
                size: s.get("size").and_then(|z| z.as_u64()),
            });
        }
    }
    Ok(out)
}

/// The whole-snapshot download set: every listed file except git
/// housekeeping — any path with a dot-leading component, mirroring the
/// scanner, which never catalogs dotfiles.
pub fn snapshot_set(all: &[RemoteFile]) -> Vec<RemoteFile> {
    all.iter()
        .filter(|f| !f.filename.split('/').any(|c| c.starts_with('.')))
        .cloned()
        .collect()
}

/// Close matches for a repo id the API rejected — search scoped to the
/// author when the id has one. Best-effort: network errors yield nothing.
fn suggest_repos(repo: &str, token: Option<&str>) -> Vec<String> {
    let (author, name) = repo.split_once('/').unwrap_or(("", repo));
    let mut url = format!(
        "https://huggingface.co/api/models?search={}&limit=5",
        name.replace(' ', "+")
    );
    if !author.is_empty() {
        url.push_str(&format!("&author={author}"));
    }
    with_auth(agent().get(&url), token)
        .call()
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .and_then(|j| {
            j.as_array().map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Where a fetched file lands: `<shelf>/<repo-last-segment>/<filename>`,
/// with the remote's relative layout preserved. Rejects path tricks.
pub fn dest_for(shelf_root: &Path, repo: &str, filename: &str) -> Result<PathBuf> {
    let rel = Path::new(filename);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("refusing suspicious remote filename {filename}");
    }
    let family = repo.rsplit('/').next().unwrap_or("model");
    Ok(shelf_root.join(family).join(rel))
}

/// Temp name for an in-flight download, always `.partial`-suffixed so an
/// interrupted transfer can never be scanned as a model.
fn partial_path(dest: &Path) -> PathBuf {
    match dest.extension() {
        Some(e) => dest.with_extension(format!("{}.partial", e.to_string_lossy())),
        None => dest.with_extension("partial"),
    }
}

/// Download one file with Range resume. Returns (final path, provenance).
pub fn fetch(
    repo: &str,
    filename: &str,
    shelf_root: &Path,
    token: Option<&str>,
    mut on: impl FnMut(FetchEvent),
) -> Result<(PathBuf, Provenance)> {
    let dest = dest_for(shelf_root, repo, filename)?;
    if dest.exists() {
        bail!("{} already exists — refusing to overwrite", dest.display());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = partial_path(&dest);
    let mut resume_from = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);

    let url = format!("https://huggingface.co/{repo}/resolve/main/{filename}");
    let label = format!("{repo}/{filename}");
    let mut req = with_auth(agent().get(&url), token);
    if resume_from > 0 {
        req = req.set("Range", &format!("bytes={resume_from}-"));
    }
    let resp = req.call().with_context(|| format!("downloading {url}"))?;

    // A 200 to a Range request means the server ignored it: start over.
    let appending = resp.status() == 206;
    if resume_from > 0 && !appending {
        resume_from = 0;
    }
    // `x-repo-commit` is exact when present, but redirects (CDN, moved
    // repos) routinely eat it — the API's HEAD-of-main sha is the fallback.
    let revision = resp
        .header("x-repo-commit")
        .map(str::to_string)
        .or_else(|| {
            with_auth(
                agent().get(&format!("https://huggingface.co/api/models/{repo}")),
                token,
            )
                .call()
                .ok()
                .and_then(|r| r.into_json::<serde_json::Value>().ok())
                .and_then(|j| j.get("sha").and_then(|s| s.as_str()).map(str::to_string))
        });
    let etag = resp
        .header("x-linked-etag")
        .or_else(|| resp.header("etag"))
        .map(|e| e.trim_matches('"').to_string());
    let total = resp
        .header("content-length")
        .and_then(|l| l.parse::<u64>().ok())
        .map(|l| l + if appending { resume_from } else { 0 });

    on(FetchEvent::Start {
        label: label.clone(),
        total,
        resumed_from: resume_from,
    });

    let mut file = if appending {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&tmp)
            .with_context(|| format!("appending to {}", tmp.display()))?
    } else {
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?
    };
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = resume_from;
    let mut last = done;
    loop {
        let n = reader.read(&mut buf).context("network read")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", tmp.display()))?;
        done += n as u64;
        if done - last >= 8 * 1024 * 1024 {
            last = done;
            on(FetchEvent::Progress {
                label: label.clone(),
                done,
                total,
            });
        }
    }
    file.sync_all().ok();
    drop(file);
    if let Some(t) = total
        && done != t
    {
        // Keep the .partial — the next fetch resumes from here.
        bail!(
            "connection ended early at {done}/{t} bytes — rerun to resume ({})",
            tmp.display()
        );
    }
    std::fs::rename(&tmp, &dest).with_context(|| format!("finalizing {}", dest.display()))?;

    let prov = Provenance {
        tool: "warden-fetch".into(),
        repo: repo.to_string(),
        filename: filename.to_string(),
        revision,
        etag,
        fetched_unix: crate::core::manifest::now_unix(),
    };
    Ok((dest, prov))
}

/// Provenance store: `<state>/provenance.json`, keyed by sha256 — content
/// identity, like everything else.
pub fn provenance_path(state_dir: &Path) -> PathBuf {
    state_dir.join("provenance.json")
}

pub fn record_provenance(state_dir: &Path, sha256: &str, prov: &Provenance) -> Result<()> {
    let path = provenance_path(state_dir);
    let mut map: BTreeMap<String, Provenance> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(sha256.to_string(), prov.clone());
    crate::core::manifest::save_json(&map, &path)
}

pub fn load_provenance(state_dir: &Path) -> BTreeMap<String, Provenance> {
    std::fs::read_to_string(provenance_path(state_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_layout_keeps_remote_structure_and_rejects_tricks() {
        let shelf = Path::new("/shelf");
        assert_eq!(
            dest_for(shelf, "unsloth/Qwen3.8-27B-GGUF", "UD-Q4_K_XL/model.gguf").unwrap(),
            PathBuf::from("/shelf/Qwen3.8-27B-GGUF/UD-Q4_K_XL/model.gguf")
        );
        assert!(dest_for(shelf, "org/repo", "../escape.gguf").is_err());
        assert!(dest_for(shelf, "org/repo", "/abs.gguf").is_err());
    }

    #[test]
    fn split_sets_expand_and_demand_completeness() {
        let files = |names: &[&str]| -> Vec<RemoteFile> {
            names
                .iter()
                .map(|n| RemoteFile {
                    filename: n.to_string(),
                    size: None,
                })
                .collect()
        };
        // Not a split: passes through.
        let all = files(&["model-Q4_K_M.gguf"]);
        assert_eq!(
            split_set(&all, "model-Q4_K_M.gguf").unwrap(),
            vec!["model-Q4_K_M.gguf"]
        );
        // A split expands to every part, in order, from any chosen part.
        let all = files(&[
            "UD/big-00002-of-00003.gguf",
            "UD/big-00001-of-00003.gguf",
            "UD/big-00003-of-00003.gguf",
        ]);
        assert_eq!(
            split_set(&all, "UD/big-00002-of-00003.gguf").unwrap(),
            vec![
                "UD/big-00001-of-00003.gguf",
                "UD/big-00002-of-00003.gguf",
                "UD/big-00003-of-00003.gguf"
            ]
        );
        // A hole in the listing is an error, not a partial download.
        let all = files(&["big-00001-of-00003.gguf", "big-00003-of-00003.gguf"]);
        assert!(split_set(&all, "big-00001-of-00003.gguf").is_err());
        // Five-digit discipline: near-misses are not splits.
        assert!(split_parts("model-001-of-003.gguf").is_none());
        assert!(split_parts("model-00001-of-00003.txt").is_none());
    }

    #[test]
    fn sibling_listing_separates_gguf_from_snapshot_views() {
        let json: serde_json::Value = serde_json::json!({
            "siblings": [
                {"rfilename": "model.safetensors", "size": 133},
                {"rfilename": "config.json", "size": 7},
                {"rfilename": "1_Pooling/config.json", "size": 3},
                {"rfilename": ".gitattributes", "size": 1},
                {"rfilename": "sub/model-Q4.gguf", "size": 99},
            ]
        });
        let ggufs = files_from_siblings("org/repo", &json, true).unwrap();
        assert_eq!(ggufs.len(), 1);
        assert_eq!(ggufs[0].filename, "sub/model-Q4.gguf");

        let all = files_from_siblings("org/repo", &json, false).unwrap();
        assert_eq!(all.len(), 5, "the all-files view holds everything");

        // Snapshot set: everything except dotfile paths.
        let snap = snapshot_set(&all);
        let names: Vec<_> = snap.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(
            names,
            [
                "model.safetensors",
                "config.json",
                "1_Pooling/config.json",
                "sub/model-Q4.gguf"
            ]
        );

        // No siblings array at all is an error, not an empty repo.
        assert!(files_from_siblings("org/repo", &serde_json::json!({}), false).is_err());
    }

    #[test]
    fn projectors_ride_along_with_the_model_never_the_reverse() {
        let files = |names: &[&str]| -> Vec<RemoteFile> {
            names
                .iter()
                .map(|n| RemoteFile { filename: n.to_string(), size: None })
                .collect()
        };
        let all = files(&[
            "Qwen-Ridge-3.7bpw.gguf",
            "mmproj-Qwen-BF16.gguf",
            "README.md",
        ]);
        // Downloading the model pulls the projector too.
        assert_eq!(
            with_projectors(&all, vec!["Qwen-Ridge-3.7bpw.gguf".into()]),
            vec!["Qwen-Ridge-3.7bpw.gguf", "mmproj-Qwen-BF16.gguf"]
        );
        // Downloading just the projector stays just the projector.
        assert_eq!(
            with_projectors(&all, vec!["mmproj-Qwen-BF16.gguf".into()]),
            vec!["mmproj-Qwen-BF16.gguf"]
        );
        // No projector in the repo: set unchanged.
        let plain = files(&["model-Q4.gguf"]);
        assert_eq!(
            with_projectors(&plain, vec!["model-Q4.gguf".into()]),
            vec!["model-Q4.gguf"]
        );
    }

    #[test]
    fn log_lines_mirror_the_cli_wording() {
        let start = FetchEvent::Start {
            label: "org/repo/m.gguf".into(),
            total: Some(1024),
            resumed_from: 512,
        };
        assert_eq!(
            start.log_line().as_deref(),
            Some("downloading org/repo/m.gguf (1.0 KiB), resuming at 512 B")
        );
        let tick = FetchEvent::Progress { label: "x".into(), done: 1, total: None };
        assert_eq!(tick.log_line(), None);
    }

    #[test]
    fn partial_names_never_shadow_models() {
        assert_eq!(
            partial_path(Path::new("/s/r/model.gguf")),
            PathBuf::from("/s/r/model.gguf.partial")
        );
        assert_eq!(
            partial_path(Path::new("/s/r/config.json")),
            PathBuf::from("/s/r/config.json.partial")
        );
        // Extension-less files get a plain .partial, not a fake .gguf.
        assert_eq!(
            partial_path(Path::new("/s/r/LICENSE")),
            PathBuf::from("/s/r/LICENSE.partial")
        );
    }

    #[test]
    fn token_file_reading_trims_and_ignores_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        assert_eq!(token_from_file(&path), None, "missing file");
        std::fs::write(&path, "  hf_abc123\n").unwrap();
        assert_eq!(token_from_file(&path).as_deref(), Some("hf_abc123"));
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(token_from_file(&path), None, "blank file");
    }

    #[test]
    fn provenance_store_roundtrips_by_content() {
        let state = tempfile::tempdir().unwrap();
        let prov = Provenance {
            tool: "warden-fetch".into(),
            repo: "org/repo".into(),
            filename: "m.gguf".into(),
            revision: Some("abc123".into()),
            etag: None,
            fetched_unix: 42,
        };
        record_provenance(state.path(), "cafe01", &prov).unwrap();
        let map = load_provenance(state.path());
        assert_eq!(map.get("cafe01"), Some(&prov));
        // Second record for another content joins the first.
        record_provenance(state.path(), "beef02", &prov).unwrap();
        assert_eq!(load_provenance(state.path()).len(), 2);
    }
}

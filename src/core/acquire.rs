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

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(60))
        .user_agent(concat!("modelwarden/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// GGUF files a repo offers, with sizes when the API provides them.
pub fn list_files(repo: &str) -> Result<Vec<RemoteFile>> {
    let url = format!("https://huggingface.co/api/models/{repo}?blobs=true");
    let resp = agent()
        .get(&url)
        .call()
        .with_context(|| format!("querying {repo}"))?;
    let json: serde_json::Value = resp.into_json().context("parsing repo metadata")?;
    let Some(siblings) = json.get("siblings").and_then(|s| s.as_array()) else {
        bail!("{repo}: no file list in API response");
    };
    let mut out = Vec::new();
    for s in siblings {
        let Some(name) = s.get("rfilename").and_then(|n| n.as_str()) else {
            continue;
        };
        if name.to_lowercase().ends_with(".gguf") {
            out.push(RemoteFile {
                filename: name.to_string(),
                size: s.get("size").and_then(|z| z.as_u64()),
            });
        }
    }
    Ok(out)
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

/// Download one file with Range resume. Returns (final path, provenance).
pub fn fetch(
    repo: &str,
    filename: &str,
    shelf_root: &Path,
    mut on: impl FnMut(FetchEvent),
) -> Result<(PathBuf, Provenance)> {
    let dest = dest_for(shelf_root, repo, filename)?;
    if dest.exists() {
        bail!("{} already exists — refusing to overwrite", dest.display());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = dest.with_extension(format!(
        "{}.partial",
        dest.extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|| "gguf".into())
    ));
    let mut resume_from = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);

    let url = format!("https://huggingface.co/{repo}/resolve/main/{filename}");
    let label = format!("{repo}/{filename}");
    let mut req = agent().get(&url);
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
            agent()
                .get(&format!("https://huggingface.co/api/models/{repo}"))
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

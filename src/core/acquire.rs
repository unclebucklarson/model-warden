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
    /// The connection dropped mid-transfer; warden re-issues a Range
    /// request from `at` on its own — no rerun needed.
    Retry { label: String, at: u64, attempt: u32 },
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
            Self::Retry { label, at, attempt } => Some(format!(
                "connection dropped — resuming {label} at {} (attempt {attempt})",
                human_size(*at)
            )),
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
/// Percent-encode one URL path segment.
///
/// URLs were built by interpolating `repo` (from the user) and
/// `filename` (from the *server's* listing) straight into a format
/// string. A `?`, `#` or `%` in either silently re-partitions the URL —
/// a server-supplied filename could append a query or truncate the path.
/// The host is fixed before the interpolation so this was never SSRF,
/// but building URLs by concatenating remote input is the wrong default.
/// `/` is preserved: repo ids and hub filenames are multi-segment paths.
pub fn url_path(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One query-string value.
fn url_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A hub repo id: `org/name`, nothing else. Checked before the id is
/// put in a URL or used to build a local path.
pub fn valid_repo(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    match repo.split_once('/') {
        Some((org, name)) => ok(org) && ok(name) && !name.contains('/'),
        None => false,
    }
}

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

/// Read a token out of a file the user points at.
///
/// `--token hf_…` puts the credential in `argv`, where any local process
/// can read it from `/proc/*/cmdline` for the life of the download, and
/// in the shell's history file afterwards. A file the user controls the
/// mode of leaks neither.
pub fn token_from_path(path: &Path) -> Result<String> {
    token_from_file(path)
        .with_context(|| format!("no token in {}", path.display()))
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
        let base = name.rsplit('/').next().unwrap_or(name);
        crate::core::manifest::is_projector_name(base)
    };
    if set.iter().all(|f| is_projector(f)) {
        return set; // projector-only choice stays as chosen
    }
    let candidates: Vec<&RemoteFile> = all
        .iter()
        .filter(|f| is_projector(&f.filename) && !set.contains(&f.filename))
        .collect();
    // ONE projector suffices to run; repos often ship several precisions
    // (f16 + bf16 + q8) and pulling them all cost a real user ~2 GiB of
    // unasked-for transfer. Prefer plain f16 (llama.cpp's default
    // pairing), then bf16, else the first listed; the listing still
    // shows the rest for a deliberate manual pick.
    let pick = candidates
        .iter()
        .find(|f| {
            let b = f.filename.to_lowercase();
            b.contains("f16") && !b.contains("bf16")
        })
        .or_else(|| candidates.iter().find(|f| f.filename.to_lowercase().contains("bf16")))
        .or_else(|| candidates.first());
    if let Some(f) = pick {
        set.push(f.filename.clone());
    }
    set
}

/// One downloadable *model* in a repo listing: a plain file, or a whole
/// `-NNNNN-of-NNNNN` split set presented as one thing — because clicking
/// Download transfers the whole set, the row should say so.
#[derive(Debug, Clone)]
pub struct RemoteModel {
    /// What to show: the stem name, with a part count when split.
    pub display: String,
    /// The filename to hand to the download path (split_set expands it).
    pub first: String,
    /// Every file this row stands for, listing order.
    pub parts: Vec<RemoteFile>,
    /// Combined size; None if any part's size is unknown.
    pub total: Option<u64>,
}

/// Group a file listing into models: split parts collapse into one entry
/// (keyed by their stem), everything else passes through one-to-one.
/// Order follows first appearance in the listing.
pub fn group_listing(files: &[RemoteFile]) -> Vec<RemoteModel> {
    let mut out: Vec<RemoteModel> = Vec::new();
    let mut stem_index: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for f in files {
        match split_parts(&f.filename) {
            Some((prefix, _, count)) => {
                let key = format!("{prefix}-of-{count}");
                let idx = *stem_index.entry(key).or_insert_with(|| {
                    out.push(RemoteModel {
                        display: format!("{prefix}.gguf ({count} parts)"),
                        first: f.filename.clone(),
                        parts: Vec::new(),
                        total: Some(0),
                    });
                    out.len() - 1
                });
                let m = &mut out[idx];
                m.parts.push(f.clone());
                m.total = match (m.total, f.size) {
                    (Some(t), Some(s)) => Some(t + s),
                    _ => None,
                };
            }
            None => out.push(RemoteModel {
                display: f.filename.clone(),
                first: f.filename.clone(),
                parts: vec![f.clone()],
                total: f.size,
            }),
        }
    }
    out
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
    // One gate, on the way in: every listing and every download goes
    // through here, so nothing further down has to wonder whether the id
    // is a repo or a path fragment.
    if !valid_repo(repo) {
        bail!("{repo:?} is not a repo id — expected org/name");
    }
    let url = format!("https://huggingface.co/api/models/{}?blobs=true", url_path(repo));
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
        .filter(|f| {
            !f.filename
                .split('/')
                .any(crate::core::scan::is_dot_entry)
        })
        .cloned()
        .collect()
}

/// Close matches for a repo id the API rejected — search scoped to the
/// author when the id has one. Best-effort: network errors yield nothing.
fn suggest_repos(repo: &str, token: Option<&str>) -> Vec<String> {
    let (author, name) = repo.split_once('/').unwrap_or(("", repo));
    let mut url = format!(
        "https://huggingface.co/api/models?search={}&limit=5",
        url_query(name)
    );
    if !author.is_empty() {
        url.push_str(&format!("&author={}", url_query(author)));
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

/// HuggingFace serves LFS objects with the content's SHA-256 in
/// `x-linked-etag`. When the header is exactly 64 hex characters it IS a
/// checksum and warden verifies against it; anything else (a weak
/// validator, a plain MD5-style etag) is not a hash and must not be
/// treated as one.
pub fn etag_sha256(etag: Option<&str>) -> Option<String> {
    let raw = etag?.trim();
    if raw.starts_with("W/") {
        return None; // weak validator: not a content hash
    }
    let hex = raw.trim_matches('"');
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

/// The download's last gate: when the origin declared a checksum, the
/// bytes on disk must match it. No checksum available (a small non-LFS
/// file, an offline-ish origin) means the download proceeds unverified —
/// absence of a checksum must never fail a good download.
fn check_download(got: &str, declared: Option<&str>) -> Result<()> {
    match declared {
        Some(want) if got != want => bail!(
            "downloaded bytes do not match the checksum the server declared \
             (wanted {}…, got {}…)",
            crate::core::format::short_hash(want),
            crate::core::format::short_hash(got)
        ),
        _ => Ok(()),
    }
}

/// Ask the origin — not the CDN — for the content checksum.
///
/// `huggingface.co/.../resolve/...` answers 302 with `x-linked-etag`
/// (the object's SHA-256) and redirects to a CDN whose *own* `etag` is a
/// content-addressed id that is also 64 hex characters but is NOT the
/// file hash. Since ureq follows redirects, reading the header off the
/// final response silently yields the wrong value — so this asks with
/// redirects disabled and reads the authoritative header.
///
/// Best-effort: any failure yields `None`, which means "no checksum
/// available", never a failed download.
fn origin_checksum(repo: &str, filename: &str, token: Option<&str>) -> Option<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(15))
        .redirects(0)
        .user_agent(concat!("modelwarden/", env!("CARGO_PKG_VERSION")))
        .build();
    let url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        url_path(repo),
        url_path(filename)
    );
    let resp = match with_auth(agent.head(&url), token).call() {
        Ok(r) => r,
        // A 3xx with redirects(0) surfaces as Status — its headers are
        // exactly the ones we want.
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return None,
    };
    etag_sha256(resp.header("x-linked-etag"))
}

/// Temp name for an in-flight download, always `.partial`-suffixed so an
/// interrupted transfer can never be scanned as a model.
fn partial_path(dest: &Path) -> PathBuf {
    crate::core::fsx::temp_sibling(dest, "partial")
}

/// Download one file with Range resume. Returns (final path, provenance).
/// Returns (final path, provenance, sha256 of the bytes on disk). The
/// hash is computed once, before the rename, and handed back so callers
/// never re-read a 20 GiB file to learn what they just downloaded.
pub fn fetch(
    repo: &str,
    filename: &str,
    shelf_root: &Path,
    token: Option<&str>,
    mut on: impl FnMut(FetchEvent),
) -> Result<(PathBuf, Provenance, String)> {
    let dest = dest_for(shelf_root, repo, filename)?;
    if dest.exists() {
        bail!("{} already exists — refusing to overwrite", dest.display());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = partial_path(&dest);
    let url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        url_path(repo),
        url_path(filename)
    );
    let label = format!("{repo}/{filename}");

    // A dropped connection is normal on a 20 GiB transfer: warden retries
    // on its own, resuming from the .partial via a fresh Range request.
    // Only zero-progress attempts count against the budget, so a flaky
    // link that keeps inching forward never gives up.
    const MAX_STALLED_ATTEMPTS: u32 = 5;
    let mut stalled = 0u32;
    let mut attempt = 0u32;
    let mut started = false;
    let mut revision: Option<String> = None;
    let mut etag: Option<String> = None;
    loop {
        let mut resume_from = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        let pass_start = resume_from;
        let mut retry = |at: u64, on: &mut dyn FnMut(FetchEvent)| -> Result<()> {
            attempt += 1;
            stalled = if at > pass_start { 0 } else { stalled + 1 };
            if stalled >= MAX_STALLED_ATTEMPTS {
                bail!(
                    "download stalled at {} after {attempt} attempts — the partial is \
                     kept; rerun the same fetch to resume ({})",
                    at,
                    tmp.display()
                );
            }
            on(FetchEvent::Retry {
                label: label.clone(),
                at,
                attempt,
            });
            std::thread::sleep(std::time::Duration::from_secs(2));
            Ok(())
        };

        let mut req = with_auth(agent().get(&url), token);
        if resume_from > 0 {
            req = req.set("Range", &format!("bytes={resume_from}-"));
            // If the file changed on the server since the pass that set
            // `etag`, If-Range makes it answer 200-full instead of
            // appending bytes of a NEW revision onto our OLD partial —
            // the full response truncates and restarts cleanly below.
            if let Some(e) = &etag {
                req = req.set("If-Range", &format!("\"{e}\""));
            }
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(416, r)) if resume_from > 0 => {
                // Range unsatisfiable. If the partial is exactly the full
                // length (crash after the last byte, before rename), it's
                // complete — finalize it. Otherwise it's stale garbage:
                // discard warden's own temp and start over.
                let full = r
                    .header("content-range")
                    .and_then(|cr| cr.rsplit('/').next())
                    .and_then(|t| t.parse::<u64>().ok());
                if full == Some(resume_from) {
                    break;
                }
                let _ = std::fs::remove_file(&tmp);
                retry(0, &mut on).with_context(|| format!("downloading {url}"))?;
                continue;
            }
            Err(ureq::Error::Status(code, r)) => {
                return Err(ureq::Error::Status(code, r))
                    .with_context(|| format!("downloading {url}"));
            }
            Err(e) => {
                // Transport-level failure before any bytes: retry.
                retry(resume_from, &mut on)
                    .with_context(|| format!("downloading {url}: {e}"))?;
                continue;
            }
        };

        // A 200 to a Range request means the server ignored it: start over.
        let appending = resp.status() == 206;
        if resume_from > 0 && !appending {
            resume_from = 0;
        }
        if revision.is_none() {
            // `x-repo-commit` is exact when present, but redirects (CDN,
            // moved repos) routinely eat it — the API's sha is the fallback.
            revision = resp.header("x-repo-commit").map(str::to_string).or_else(|| {
                with_auth(
                    agent().get(&format!("https://huggingface.co/api/models/{}", url_path(repo))),
                    token,
                )
                .call()
                .ok()
                .and_then(|r| r.into_json::<serde_json::Value>().ok())
                .and_then(|j| j.get("sha").and_then(|s| s.as_str()).map(str::to_string))
            });
            etag = resp
                .header("x-linked-etag")
                .or_else(|| resp.header("etag"))
                .map(|e| e.trim_matches('"').to_string());
        }
        let total = resp
            .header("content-length")
            .and_then(|l| l.parse::<u64>().ok())
            .map(|l| l + if appending { resume_from } else { 0 });

        if !started {
            started = true;
            on(FetchEvent::Start {
                label: label.clone(),
                total,
                resumed_from: resume_from,
            });
        }

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
        let outcome: Result<()> = loop {
            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                // Mid-stream drop ("response body closed before all bytes
                // were read" and friends): resume, don't fail.
                Err(e) => break Err(e.into()),
            };
            if n == 0 {
                break Ok(());
            }
            // Disk errors are not retryable — fail immediately.
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
        };
        file.sync_all().ok();
        drop(file);

        match total {
            // More bytes than declared: never publish it — discard the
            // temp and start over. (The old `!= total` check refused
            // this; a rework briefly accepted it.)
            Some(t) if done > t => {
                let _ = std::fs::remove_file(&tmp);
                retry(0, &mut on)?;
                continue;
            }
            // Short — dropped mid-stream or clean-but-early EOF: resume.
            Some(t) if done < t => {
                retry(done, &mut on)?;
                continue;
            }
            // Exactly complete: success even if the connection closed
            // uncleanly after the last byte — retrying a finished
            // transfer just earns a 416.
            Some(_) => break,
            None => {
                if outcome.is_err() {
                    retry(done, &mut on)?;
                    continue;
                }
                break;
            }
        }
    }
    // Every other byte-movement in warden is hash-verified end to end;
    // downloads were the exception. Hash the finished temp BEFORE it is
    // published, and when the server gave us a content hash, refuse
    // anything that does not match it.
    on(FetchEvent::Hashing {
        label: label.clone(),
    });
    let got = crate::core::identity::sha256_file(&tmp, |_, _| {})
        .with_context(|| format!("hashing {}", tmp.display()))?;
    let declared = origin_checksum(repo, filename, token);
    if let Err(e) = check_download(&got, declared.as_deref()) {
        // The bytes are provably not what was asked for; resuming would
        // only re-derive the same wrong file.
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context("the partial was discarded"));
    }
    crate::core::fsx::rename_noreplace(&tmp, &dest)
        .with_context(|| format!("finalizing {}", dest.display()))?;

    let prov = Provenance {
        tool: "warden-fetch".into(),
        repo: repo.to_string(),
        filename: filename.to_string(),
        revision,
        // Prefer the origin's content hash; the CDN etag we may have
        // seen is a storage id, not a checksum.
        etag: declared.or(etag),
        fetched_unix: crate::core::manifest::now_unix(),
    };
    Ok((dest, prov, got))
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
    fn listings_group_split_sets_into_one_model() {
        let files = |names: &[(&str, Option<u64>)]| -> Vec<RemoteFile> {
            names
                .iter()
                .map(|(n, s)| RemoteFile { filename: n.to_string(), size: *s })
                .collect()
        };
        let all = files(&[
            ("UD/big-00001-of-00002.gguf", Some(40)),
            ("plain-Q4.gguf", Some(7)),
            ("UD/big-00002-of-00002.gguf", Some(10)),
        ]);
        let models = group_listing(&all);
        assert_eq!(models.len(), 2, "{models:?}");
        // The split set is one model: combined size, honest part count,
        // dispatchable via any member (first seen).
        assert_eq!(models[0].display, "UD/big.gguf (2 parts)");
        assert_eq!(models[0].total, Some(50));
        assert_eq!(models[0].parts.len(), 2);
        assert_eq!(models[0].first, "UD/big-00001-of-00002.gguf");
        // Plain files pass through untouched.
        assert_eq!(models[1].display, "plain-Q4.gguf");
        assert_eq!(models[1].total, Some(7));
        // An unknown part size makes the total honest: unknown.
        let all = files(&[("m-00001-of-00002.gguf", Some(1)), ("m-00002-of-00002.gguf", None)]);
        assert_eq!(group_listing(&all)[0].total, None);
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
        // Several projector precisions: ONE comes along (f16 preferred) —
        // a real user paid ~2 GiB for all of them before this rule.
        let multi = files(&[
            "big-Q4.gguf",
            "mmproj-big-bf16.gguf",
            "mmproj-big-f16.gguf",
            "mmproj-big-Q8_0.gguf",
        ]);
        assert_eq!(
            with_projectors(&multi, vec!["big-Q4.gguf".into()]),
            vec!["big-Q4.gguf", "mmproj-big-f16.gguf"]
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
        // A mid-transfer drop is reported as an automatic resume, not a
        // failure — both frontends surface this line durably.
        let retry = FetchEvent::Retry { label: "org/m.gguf".into(), at: 2048, attempt: 2 };
        assert_eq!(
            retry.log_line().as_deref(),
            Some("connection dropped — resuming org/m.gguf at 2.0 KiB (attempt 2)")
        );
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

    #[test]
    fn etag_is_recognised_as_a_checksum_only_when_it_is_one() {
        // HF serves LFS objects with the content SHA-256 in x-linked-etag.
        // warden already captured it as provenance and never checked it —
        // downloads were the one byte path with no verification.
        let sha = "a".repeat(64);
        assert_eq!(etag_sha256(Some(&sha)).as_deref(), Some(sha.as_str()));
        assert_eq!(etag_sha256(Some(&format!("\"{sha}\""))).as_deref(), Some(sha.as_str()));
        assert_eq!(
            etag_sha256(Some(&sha.to_uppercase())).as_deref(),
            Some(sha.as_str()),
            "normalised to lowercase for comparison"
        );
        // Non-LFS files get a plain MD5-ish etag; weak validators and
        // anything that isn't 64 hex chars must NOT be treated as a hash.
        assert_eq!(etag_sha256(Some("\"5d41402abc4b2a76b9719d911017c592\"")), None);
        assert_eq!(etag_sha256(Some(&format!("W/\"{sha}\""))), None);
        assert_eq!(etag_sha256(Some("")), None);
        assert_eq!(etag_sha256(None), None);
    }

    #[test]
    fn a_download_is_refused_when_it_contradicts_the_declared_checksum() {
        let good = "d".repeat(64);
        let other = "e".repeat(64);
        // Verified when the origin gave us a checksum.
        assert!(check_download(&good, Some(&good)).is_ok());
        let err = check_download(&other, Some(&good)).unwrap_err().to_string();
        assert!(err.contains("do not match"), "{err}");
        assert!(err.contains(&good[..12]) && err.contains(&other[..12]), "{err}");
        // No checksum offered (small non-LFS files carry a weak etag):
        // proceed rather than fail a perfectly good download.
        assert!(check_download(&good, None).is_ok());
    }

    #[test]
    fn remote_filenames_cannot_re_partition_the_url() {
        // `filename` comes from the SERVER's listing, so it is remote
        // input being interpolated into a URL. A `?` starts a query, a
        // `#` truncates the path at a fragment, a `%` can forge an
        // escape — none of which should be possible from a listing.
        assert_eq!(url_path("model.gguf"), "model.gguf");
        assert_eq!(url_path("sub/dir/model-Q4_K_M.gguf"), "sub/dir/model-Q4_K_M.gguf");
        assert_eq!(url_path("evil?x=1"), "evil%3Fx%3D1");
        assert_eq!(url_path("evil#frag"), "evil%23frag");
        assert_eq!(url_path("100%.gguf"), "100%25.gguf");
        assert_eq!(url_path("a b.gguf"), "a%20b.gguf");
        // Non-ASCII is encoded per UTF-8 byte.
        assert_eq!(url_path("é"), "%C3%A9");
    }

    #[test]
    fn a_repo_id_is_org_slash_name_and_nothing_else() {
        assert!(valid_repo("unsloth/Qwen3-30B-GGUF"));
        assert!(valid_repo("TheBloke/Llama-2-7B.Q4_K_M"));
        assert!(!valid_repo("noslash"));
        assert!(!valid_repo("too/many/slashes"));
        assert!(!valid_repo("/leading"));
        assert!(!valid_repo("trailing/"));
        assert!(!valid_repo("../../etc"));
        assert!(!valid_repo("org/name?x=1"));
        assert!(!valid_repo("org/name#f"));
    }
}

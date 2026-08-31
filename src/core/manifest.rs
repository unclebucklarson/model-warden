//! Per-root manifests and the merged inventory.
//!
//! One JSON manifest per storage root (schema proven in spike 2), all under
//! warden's state dir — never inside a foreign store. The merged inventory
//! groups every location by content identity and is what consumers will
//! eventually read (schema_version stays 0 until it's published at M6).
//!
//! Writes are atomic (temp + rename) and keep a `.bak` of the previous
//! version. A root whose path is missing keeps its manifest — offline is
//! not gone.

use crate::core::gguf::GgufMeta;
use crate::core::identity::Fingerprint;
use crate::core::roots::{RootKind, RootSpec};
use crate::core::scan::{self, ModelFile, Source};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Version 1 is the published contract (docs/inventory-schema.md) that
/// consumers like llamacppCodeConf read. Changes to the inventory shape
/// from here on require a version bump and a note in that document.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootManifest {
    pub schema_version: u32,
    pub root: RootSpec,
    pub generated_unix: u64,
    pub files: Vec<FileRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Relative to the root's path.
    pub rel_path: PathBuf,
    pub size: u64,
    /// `None` when the file was inaccessible at scan time.
    pub fingerprint: Option<Fingerprint>,
    /// The canonical identity, filled lazily by the hash worker.
    pub sha256: Option<String>,
    /// What the owning tool calls it (Ollama `model:tag`, HF `org/repo`).
    pub name: Option<String>,
    pub meta: Option<GgufMeta>,
    pub accessible: bool,
    /// When these bytes were last read end-to-end and matched `sha256`
    /// (set by backup and `warden verify`, carried while unchanged).
    #[serde(default)]
    pub verified_unix: Option<u64>,
}

/// Scan one root and reconcile with its previous manifest: a file whose
/// fingerprint is unchanged keeps its stored sha256; anything changed or new
/// starts unhashed.
pub fn build_root_manifest(spec: &RootSpec, previous: Option<&RootManifest>) -> RootManifest {
    let prior: BTreeMap<&Path, &FileRecord> = previous
        .map(|p| {
            p.files
                .iter()
                .map(|f| (f.rel_path.as_path(), f))
                .collect()
        })
        .unwrap_or_default();

    // Nothing about an unchanged file needs re-reading — not its hash,
    // and not its header either. The scanners ask this before opening
    // anything, so a settled catalog costs stats and no parses.
    let known = |abs: &Path, fp: Option<Fingerprint>| {
        let rel = abs.strip_prefix(&spec.path).unwrap_or(abs);
        let old = prior.get(rel)?;
        (old.fingerprint.is_some() && old.fingerprint == fp).then(|| old.meta.clone())
    };
    let models: Vec<ModelFile> = match spec.kind {
        RootKind::Shelf | RootKind::Removable => scan::shelf_models_cached(&spec.path, &known),
        RootKind::Ollama => scan::ollama_models_cached(&spec.path, &known),
        RootKind::HfHub => scan::hf_hub_models_cached(&spec.path, &known),
    };

    let files = models
        .iter()
        .map(|m| {
            let rel_path = m
                .path
                .strip_prefix(&spec.path)
                .unwrap_or(&m.path)
                .to_path_buf();
            // The scanner's one stat, not a second one.
            let fingerprint = m.fingerprint;
            let unchanged = prior
                .get(rel_path.as_path())
                .filter(|old| old.fingerprint.is_some() && old.fingerprint == fingerprint);
            let sha256 = unchanged.and_then(|old| old.sha256.clone());
            let verified_unix = unchanged.and_then(|old| old.verified_unix);
            let name = match &m.source {
                Source::Ollama { name } => Some(name.clone()),
                Source::HfHub { repo } => Some(repo.clone()),
                Source::Shelf => None,
            };
            FileRecord {
                rel_path,
                size: m.file_size,
                fingerprint,
                sha256,
                name,
                meta: m.meta.clone(),
                accessible: m.accessible,
                verified_unix,
            }
        })
        .collect();

    RootManifest {
        schema_version: SCHEMA_VERSION,
        root: spec.clone(),
        generated_unix: now_unix(),
        files,
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn manifest_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("roots")
}

pub fn manifest_path(state_dir: &Path, root_id: &str) -> PathBuf {
    manifest_dir(state_dir).join(format!("{root_id}.json"))
}

/// Remove a root's stored manifest (and the `.bak` that save_json keeps)
/// — the state-side half of forgetting a root. The `.bak` naming is
/// save_json's private detail; it stays inside this module.
pub fn remove_manifest(state_dir: &Path, root_id: &str) {
    let man = manifest_path(state_dir, root_id);
    let _ = std::fs::remove_file(&man);
    let _ = std::fs::remove_file(man.with_extension("json.bak"));
}

pub fn inventory_path(state_dir: &Path) -> PathBuf {
    state_dir.join("inventory.json")
}

pub fn load_manifest(path: &Path) -> Option<RootManifest> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Every stored manifest, whether or not its root is currently reachable —
/// this is exactly how offline media stay queryable.
pub fn load_all_manifests(state_dir: &Path) -> Vec<RootManifest> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(manifest_dir(state_dir)) else {
        return out;
    };
    for e in entries.flatten() {
        if e.path().extension().is_some_and(|x| x == "json")
            && let Some(m) = load_manifest(&e.path())
        {
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.root.id.cmp(&b.root.id));
    out
}

/// Validate a manifest-declared relative path before it is joined to a
/// root. Manifest records are untrusted the moment they come off
/// removable media — they are joined to a root and then read, written,
/// moved, and deleted — so every join site must pass through here.
/// Refuses absolute paths, any `..` component, and empty paths; strips
/// harmless `.` components.
pub fn sanitize_rel(rel: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("refusing unsafe manifest path {}", rel.display())
            }
        }
    }
    if out.as_os_str().is_empty() {
        anyhow::bail!("refusing empty manifest path");
    }
    Ok(out)
}

/// Scrub a manifest that came off media warden does not control (a
/// drive's carried `.modelwarden/manifest.json`) before any of it is
/// believed. A record survives only if its path is safe AND a real file
/// of exactly the recorded size sits there right now. Returns the
/// cleaned manifest and how many records were dropped.
///
/// This is deliberately a *hint filter*, not authentication: it defeats
/// path traversal and wholesale fabrication (including the forged
/// "already backed up" claims that would make `backup` copy nothing),
/// but a drive can still lie about the content of a file that really
/// exists at the right size. Only `warden verify` settles that.
pub fn sanitize_carried(mut man: RootManifest, root_path: &Path) -> (RootManifest, usize) {
    let before = man.files.len();
    man.files.retain_mut(|f| {
        let Ok(safe) = sanitize_rel(&f.rel_path) else {
            return false;
        };
        let Ok(md) = std::fs::metadata(root_path.join(&safe)) else {
            return false;
        };
        if !md.is_file() || md.len() != f.size {
            return false;
        }
        f.rel_path = safe;
        true
    });
    let dropped = before - man.files.len();
    (man, dropped)
}

/// Atomic, owner-only write, keeping a `.bak` of the previous version.
///
/// The backup is taken by **copying** the old file aside before the new
/// one is written — never by renaming the live file out of the way. The
/// old shape (rename target→bak, then rename tmp→target) left a window
/// with no manifest at `path`; a crash there — and the hash checkpoint
/// opens that window once per file — silently reverted a whole root to
/// "never catalogued".
pub fn save_json<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        crate::core::settings::create_private_dir(dir)?;
    }
    if path.exists() {
        let bak = path.with_extension("json.bak");
        // Best-effort: a failed backup must not block the real write.
        let _ = std::fs::copy(path, &bak);
        crate::core::settings::tighten(&bak);
    }
    crate::core::settings::write_private(path, serde_json::to_string_pretty(value)?.as_bytes())
}

// ---- merged inventory ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub schema_version: u32,
    pub generated_unix: u64,
    pub roots: Vec<RootSpec>,
    /// Keyed by identity: `sha256:<hex>` when hashed, `pending:<dev>:<ino>:<size>`
    /// while the hash worker hasn't reached it, `unknown:<root>:<rel>` when
    /// the bytes are unreachable.
    pub models: BTreeMap<String, ModelEntry>,
    /// Which roots are mounted, resolved once per loaded inventory.
    ///
    /// Deciding it per location meant a linear scan of `roots` plus a
    /// `stat` syscall, and the inventory row loop asks once per row on
    /// every repaint — measured at 1.1 ms per frame over 2,000 models,
    /// on top of everything else the frame does. It is deliberately not
    /// part of the inventory's value and is never serialized: plugging a
    /// drive in becomes visible on the next catalog update, which is the
    /// same point at which that drive's contents become known.
    #[serde(skip)]
    online: std::sync::OnceLock<BTreeMap<String, bool>>,
}

/// The liveness cache is not part of the value.
impl PartialEq for Inventory {
    fn eq(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.generated_unix == other.generated_unix
            && self.roots == other.roots
            && self.models == other.models
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub size: u64,
    pub display_name: String,
    /// GGUF header data from the first location that could read it.
    #[serde(default)]
    pub meta: Option<GgufMeta>,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub root_id: String,
    pub kind: RootKind,
    pub rel_path: PathBuf,
    pub accessible: bool,
    /// `(dev, ino)` — lets duplicate reporting tell hardlinks (one set of
    /// bytes) from real copies (reclaimable). (0,0) when unknown.
    pub dev: u64,
    pub ino: u64,
}

/// What a cataloged location is *right now* — warden's three
/// accessibility states, derived and never stored, because a stored flag
/// is only a claim about the last scan and drives move between scans.
///
/// Keeping these three apart is the whole point: readers that conflate
/// "the drive is unplugged" with "the file will not open" either drop
/// offline drives (bytes reported lost that are safe on a shelf) or
/// promise a restore from a copy that cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationState {
    /// The root is attached and the file was readable when last scanned.
    Present,
    /// The root is not attached. The bytes travel with it; they are fine.
    Offline,
    /// The root IS attached and the file still would not open: a pruned
    /// blob behind a snapshot symlink, a permission change, a deletion
    /// by another tool. The one state a backup promise must not count.
    Unreadable,
}

impl Inventory {
    pub fn root(&self, id: &str) -> Option<&RootSpec> {
        self.roots.iter().find(|r| r.id == id)
    }

    /// THE liveness question, asked in one place. Everything that cares
    /// whether bytes are reachable — coverage, dedup, usage, restore
    /// source selection — resolves through here, so two views of the
    /// same catalog can never describe different worlds.
    pub fn location_state(&self, loc: &Location) -> LocationState {
        // An unregistered root is treated as offline, not broken: the
        // same "missing is offline, not gone" rule roots follow.
        let online = *self
            .online
            .get_or_init(|| {
                self.roots
                    .iter()
                    .map(|r| (r.id.clone(), r.path.exists()))
                    .collect()
            })
            .get(&loc.root_id)
            .unwrap_or(&false);
        match (online, loc.accessible) {
            (false, _) => LocationState::Offline,
            (true, true) => LocationState::Present,
            (true, false) => LocationState::Unreadable,
        }
    }

    /// Can warden read these bytes right now?
    pub fn live_accessible(&self, loc: &Location) -> bool {
        self.location_state(loc) == LocationState::Present
    }
}

pub fn merge(manifests: &[RootManifest]) -> Inventory {
    let mut models: BTreeMap<String, ModelEntry> = BTreeMap::new();
    for m in manifests {
        let root_online = m.root.path.exists();
        for f in &m.files {
            let key = match (&f.sha256, &f.fingerprint) {
                (Some(h), _) => format!("sha256:{h}"),
                (None, Some(fp)) => format!("pending:{}:{}:{}", fp.dev, fp.ino, fp.size),
                (None, None) => format!("unknown:{}:{}", m.root.id, f.rel_path.display()),
            };
            let display_name = f.name.clone().unwrap_or_else(|| {
                f.rel_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| f.rel_path.display().to_string())
            });
            let entry = models.entry(key).or_insert_with(|| ModelEntry {
                size: f.size,
                display_name,
                meta: None,
                locations: Vec::new(),
            });
            if entry.meta.is_none() {
                entry.meta = f.meta.clone();
            }
            entry.locations.push(Location {
                root_id: m.root.id.clone(),
                kind: m.root.kind,
                rel_path: f.rel_path.clone(),
                accessible: f.accessible && root_online,
                dev: f.fingerprint.map(|fp| fp.dev).unwrap_or(0),
                ino: f.fingerprint.map(|fp| fp.ino).unwrap_or(0),
            });
        }
    }
    Inventory {
        schema_version: SCHEMA_VERSION,
        generated_unix: now_unix(),
        roots: manifests.iter().map(|m| m.root.clone()).collect(),
        models,
        online: Default::default(),
    }
}

/// Warden's core safety question, per model: is there a copy on a
/// registered drive that could actually be restored from? Offline
/// drives count — the bytes exist on that drive whether or not it is
/// plugged in right now — but a copy warden can see and cannot read
/// does not. Shared by `warden status` and the GUI's coverage display.
pub fn is_backed_up(inv: &Inventory, entry: &ModelEntry) -> bool {
    entry.locations.iter().any(|l| {
        l.kind == RootKind::Removable && inv.location_state(l) != LocationState::Unreadable
    })
}

/// The safety headline: (models with a drive copy, total models).
pub fn backup_coverage(inv: &Inventory) -> (usize, usize) {
    let backed = inv.models.values().filter(|e| is_backed_up(inv, e)).count();
    (backed, inv.models.len())
}

/// The companion relation, once: X is a companion of Y when X rides in
/// Y's bundle but Y does not ride in X's (mmproj projectors, Ollama
/// +projector blobs, safetensors tokenizer/config files). Returns
/// companion → the models that require it. Symmetric bundle members
/// (split parts) are peers, not companions.
pub fn companion_parents(inv: &Inventory) -> BTreeMap<String, Vec<String>> {
    // One index for the whole pass — it was being rebuilt implicitly,
    // as a full catalog scan, twice per model per bundle member.
    let idx = BundleIndex::of(inv);
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for k in inv.models.keys() {
        for m in bundle_for_indexed(inv, &idx, k) {
            if &m != k && !bundle_for_indexed(inv, &idx, &m).iter().any(|x| x == k) {
                out.entry(m).or_default().push(k.clone());
            }
        }
    }
    out
}

/// Display grouping for split models: maps every non-first part to its
/// part-1 sibling, so views can show ONE model row ("(N parts)",
/// combined size) instead of peer rows with misleading per-part sizes.
/// Split parts are symmetric bundle members — neither *requires* the
/// other — so this is presentation truth, distinct from the asymmetric
/// companion relation.
pub fn split_primary_of(inv: &Inventory) -> BTreeMap<String, String> {
    let filename_of = |key: &str| -> Option<String> {
        inv.models
            .get(key)?
            .locations
            .first()
            .and_then(|l| l.rel_path.file_name())
            .map(|f| f.to_string_lossy().into_owned())
    };
    // `idx` is taken here by the split part number; this is the bundle one.
    let bundles = BundleIndex::of(inv);
    let mut out = BTreeMap::new();
    for k in inv.models.keys() {
        let Some(name) = filename_of(k) else { continue };
        let Some((_, idx, _)) = crate::core::acquire::split_parts(&name) else {
            continue;
        };
        if idx == 1 {
            continue;
        }
        for m in bundle_for_indexed(inv, &bundles, k) {
            if m == *k {
                continue;
            }
            if let Some(f2) = filename_of(&m)
                && crate::core::acquire::split_parts(&f2).is_some_and(|(_, i2, _)| i2 == 1)
            {
                out.insert(k.clone(), m);
                break;
            }
        }
    }
    out
}

/// A selection expands to the union of its bundles, shared companions
/// once — the contract every multi-model operation (backup, demote,
/// delete) applies before moving anything.
pub fn bundle_union(inv: &Inventory, keys: &[String]) -> std::collections::BTreeSet<String> {
    let idx = BundleIndex::of(inv);
    keys.iter()
        .flat_map(|k| bundle_for_indexed(inv, &idx, k))
        .collect()
}

/// The one projector-filename rule, shared by the catalog (bundle_for),
/// acquisition (with_projectors), and trash restore — so the definitions
/// of "vision projector" can never drift apart.
pub fn is_projector_name(name: &str) -> bool {
    name.to_lowercase().contains("mmproj")
}

/// Progress reporting for `refresh` — both frontends render these; a 22 GiB
/// file takes ~35s, so byte-level progress matters.
#[derive(Debug, Clone)]
pub enum RefreshEvent {
    HashStart { label: String, size: u64 },
    /// Sent every ~64 MiB, not every read.
    HashProgress { label: String, done: u64, total: u64 },
    HashDone { label: String, secs: f32 },
    HashFailed { label: String, error: String },
}

impl RefreshEvent {
    /// The durable activity-log line, worded identically in both frontends;
    /// `None` for transient progress ticks, which are never logged.
    pub fn log_line(&self) -> Option<String> {
        match self {
            Self::HashStart { .. } | Self::HashProgress { .. } => None,
            Self::HashDone { label, secs } => Some(format!("hashed {label} in {secs:.0}s")),
            Self::HashFailed { label, error } => Some(format!("hash FAILED {label}: {error}")),
        }
    }
}

/// The whole M2 write path in one place: rescan the given roots, carry
/// forward hashes whose fingerprints still match, hash what's missing,
/// persist per-root manifests, and merge ALL stored manifests (offline roots
/// included) into the inventory. Returns the merged inventory.
/// Callers pass `roots::discover_roots(&cfg)`.
/// When to make hashing progress durable during a long run.
///
/// Checkpointing after every finished file made the resume guarantee
/// cost O(n²) bytes: a 600-file root serialised its entire manifest 600
/// times and did 1,800 renames, and every one of those writes was
/// another window in which the manifest could be caught half-written.
///
/// The guarantee that actually matters is "an interrupted hash resumes
/// near where it stopped", and an interval delivers it: any file that
/// takes longer than `INTERVAL` to hash — which is every file big
/// enough for its loss to hurt — checkpoints when it lands. Only work
/// done in the last few seconds, i.e. small files, can be repeated. The
/// first finished file always checkpoints, both so an early crash is
/// never a total loss and so an unwritable state dir is discovered at
/// the start of a long run rather than at the end of it. The caller
/// writes once more on completion regardless.
pub struct Checkpoint {
    last: Option<std::time::Instant>,
    since: usize,
}

impl Default for Checkpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl Checkpoint {
    /// Files that may accumulate between writes.
    pub const EVERY: usize = 32;
    /// Work that may be repeated after an interruption.
    pub const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn new() -> Self {
        Self { last: None, since: 0 }
    }

    /// Call once per finished file; `true` means "make it durable now".
    pub fn tick(&mut self) -> bool {
        self.tick_at(std::time::Instant::now())
    }

    pub fn tick_at(&mut self, now: std::time::Instant) -> bool {
        self.since += 1;
        let due = match self.last {
            None => true,
            Some(last) => self.since >= Self::EVERY || now.duration_since(last) >= Self::INTERVAL,
        };
        if due {
            self.last = Some(now);
            self.since = 0;
        }
        due
    }
}

pub fn refresh(
    specs: &[RootSpec],
    state: &Path,
    mut on: impl FnMut(RefreshEvent),
) -> Result<Inventory> {
    use crate::core::identity;

    let mut manifests = Vec::new();
    for spec in specs {
        // An offline root is skipped, NOT rebuilt: rebuilding against a
        // missing path would clobber its stored manifest with an empty one,
        // and offline is not gone. It still merges below.
        if !spec.path.exists() {
            continue;
        }
        // Carry-forward sources: this machine's stored manifest AND the
        // manifest the drive itself carries — so a drive written by backup/
        // demote (or by another machine entirely) re-catalogs with its
        // hashes intact instead of pending a rehash.
        let stored = load_manifest(&manifest_path(state, &spec.id));
        // A drive's carried manifest is untrusted input: scrub it before
        // any of it is believed (see sanitize_carried).
        let carried = spec
            .kind
            .owned()
            .then(|| load_manifest(&spec.path.join(".modelwarden/manifest.json")))
            .flatten()
            .map(|m| sanitize_carried(m, &spec.path).0);
        let previous = match (stored, carried) {
            (Some(mut s), Some(c)) => {
                let by_rel: BTreeMap<&Path, &FileRecord> =
                    c.files.iter().map(|f| (f.rel_path.as_path(), f)).collect();
                for f in &mut s.files {
                    if f.sha256.is_none()
                        && let Some(cf) = by_rel.get(f.rel_path.as_path())
                        && cf.fingerprint == f.fingerprint
                    {
                        f.sha256 = cf.sha256.clone();
                        f.verified_unix = cf.verified_unix;
                    }
                }
                let known: std::collections::HashSet<&Path> =
                    s.files.iter().map(|f| f.rel_path.as_path()).collect();
                let new: Vec<FileRecord> = c
                    .files
                    .iter()
                    .filter(|cf| !known.contains(cf.rel_path.as_path()))
                    .cloned()
                    .collect();
                s.files.extend(new);
                Some(s)
            }
            (s, c) => s.or(c),
        };
        manifests.push(build_root_manifest(spec, previous.as_ref()));
    }

    // Hash what's missing with a small worker pool. Hashing is one core
    // of SHA-256 per file (~700 MB/s) while NVMe reads several GB/s, so
    // a few files in flight cut the first-catalog wall time severalfold;
    // capped at 4 so spinning disks aren't seek-thrashed. Workers only
    // hash; the manifests (and the `on` callback) stay on this thread —
    // and each finished file is CHECKPOINTED to the state dir before its
    // completion is reported, so an interrupted first hash resumes via
    // fingerprint carry-forward instead of restarting from zero.
    struct HashJob {
        m_i: usize,
        f_i: usize,
        path: PathBuf,
        label: String,
        size: u64,
    }
    enum HashMsg {
        Start { label: String, size: u64 },
        Progress { label: String, done: u64, total: u64 },
        Done { m_i: usize, f_i: usize, hex: String, label: String, secs: f32 },
        Failed { label: String, error: String },
    }
    let mut jobs: Vec<HashJob> = Vec::new();
    for (m_i, m) in manifests.iter().enumerate() {
        for (f_i, f) in m.files.iter().enumerate() {
            if f.sha256.is_some() || !f.accessible {
                continue;
            }
            jobs.push(HashJob {
                m_i,
                f_i,
                path: m.root.path.join(&f.rel_path),
                label: f
                    .name
                    .clone()
                    .unwrap_or_else(|| f.rel_path.display().to_string()),
                size: f.size,
            });
        }
    }
    if !jobs.is_empty() {
        let threads = jobs
            .len()
            .min(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
            .min(4);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut checkpoint = Checkpoint::new();
        let (etx, erx) = std::sync::mpsc::channel::<HashMsg>();
        std::thread::scope(|s| {
            for _ in 0..threads {
                let etx = etx.clone();
                let jobs = &jobs;
                let next = &next;
                s.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        let _ = etx.send(HashMsg::Start {
                            label: job.label.clone(),
                            size: job.size,
                        });
                        let started = std::time::Instant::now();
                        let mut last = 0u64;
                        let result = identity::sha256_file(&job.path, |done, total| {
                            if done - last >= 64 * 1024 * 1024 || done == total {
                                last = done;
                                let _ = etx.send(HashMsg::Progress {
                                    label: job.label.clone(),
                                    done,
                                    total,
                                });
                            }
                        });
                        let msg = match result {
                            Ok(hex) => HashMsg::Done {
                                m_i: job.m_i,
                                f_i: job.f_i,
                                hex,
                                label: job.label.clone(),
                                secs: started.elapsed().as_secs_f32(),
                            },
                            Err(e) => HashMsg::Failed {
                                label: job.label.clone(),
                                error: e.to_string(),
                            },
                        };
                        let _ = etx.send(msg);
                    }
                });
            }
            drop(etx); // the loop below ends when the last worker exits
            for msg in erx {
                match msg {
                    HashMsg::Start { label, size } => on(RefreshEvent::HashStart { label, size }),
                    HashMsg::Progress { label, done, total } => {
                        on(RefreshEvent::HashProgress { label, done, total })
                    }
                    HashMsg::Done { m_i, f_i, hex, label, secs } => {
                        manifests[m_i].files[f_i].sha256 = Some(hex);
                        // Checkpoint first, report second: when this
                        // file is one the policy makes durable, the
                        // hash is on disk before its completion is
                        // announced.
                        if checkpoint.tick() {
                            let m = &manifests[m_i];
                            let _ = save_json(m, &manifest_path(state, &m.root.id));
                        }
                        on(RefreshEvent::HashDone { label, secs });
                    }
                    HashMsg::Failed { label, error } => {
                        on(RefreshEvent::HashFailed { label, error })
                    }
                }
            }
        });
    }

    for m in &manifests {
        save_json(m, &manifest_path(state, &m.root.id))
            .with_context(|| format!("saving manifest for {}", m.root.path.display()))?;
    }
    let inv = merge(&load_all_manifests(state));
    save_json(&inv, &inventory_path(state)).context("saving inventory")?;
    Ok(inv)
}

pub fn load_inventory(state: &Path) -> Option<Inventory> {
    serde_json::from_str(&std::fs::read_to_string(inventory_path(state)).ok()?).ok()
}

#[derive(Debug, Clone, Serialize)]
pub struct DupGroup {
    pub sha256: String,
    pub size: u64,
    pub display_name: String,
    pub locations: Vec<Location>,
    /// Bytes freeable by collapsing distinct inodes to one: `(inodes-1) * size`.
    pub reclaimable: u64,
}

/// Everything a selected model needs to run, as catalog keys: the model
/// itself, its split siblings (`-NNNNN-of-NNNNN` parts are useless alone),
/// any `mmproj` vision projector kept beside it, and — for Ollama — the
/// projector blob its manifest ties to it (`<name>+projector`). Every
/// operation that moves models moves the whole bundle.
///
/// Deliberately asymmetric: selecting a projector does NOT drag in every
/// quant that shares its directory.
pub fn bundle_for(inv: &Inventory, key: &str) -> Vec<String> {
    bundle_for_indexed(inv, &BundleIndex::of(inv), key)
}

/// Which models could possibly be in the same bundle, looked up instead
/// of searched for.
///
/// `bundle_for` used to compare the subject against every model in the
/// catalog and every one of its locations. Two models are only ever
/// bundled when they sit in the same directory on the same root, or
/// share an Ollama base name, so those are the only candidates worth
/// testing — and both are lookups. Measured on a synthetic 2,000-model
/// catalog, computing `companion_parents` + `split_primary_of` went from
/// 234 ms to a few ms; the GUI was doing that twice per repaint.
pub struct BundleIndex {
    by_container: BTreeMap<(String, PathBuf), Vec<String>>,
    by_ollama: BTreeMap<(String, String), Vec<String>>,
}

impl BundleIndex {
    pub fn of(inv: &Inventory) -> Self {
        let mut by_container: BTreeMap<(String, PathBuf), Vec<String>> = BTreeMap::new();
        let mut by_ollama: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for (k, e) in &inv.models {
            for l in &e.locations {
                if l.kind == RootKind::Ollama {
                    by_ollama
                        .entry((l.root_id.clone(), ollama_base(&e.display_name).to_string()))
                        .or_default()
                        .push(k.clone());
                } else {
                    by_container
                        .entry((l.root_id.clone(), container_of(l.kind, &l.rel_path)))
                        .or_default()
                        .push(k.clone());
                }
            }
        }
        Self { by_container, by_ollama }
    }

    fn in_container(&self, root_id: &str, container: &Path) -> &[String] {
        self.by_container
            .get(&(root_id.to_string(), container.to_path_buf()))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Every key under `container` on this root, the container itself
    /// included — the subtree a non-GGUF weights file claims. Paths sort
    /// component-wise, so a prefix's descendants are contiguous.
    fn under_container(&self, root_id: &str, container: &Path) -> Vec<&String> {
        let start = (root_id.to_string(), container.to_path_buf());
        let mut out = Vec::new();
        for ((r, c), keys) in self.by_container.range(start..) {
            if r != root_id || !c.starts_with(container) {
                break;
            }
            out.extend(keys.iter());
        }
        out
    }

    fn same_ollama_base(&self, root_id: &str, base: &str) -> &[String] {
        self.by_ollama
            .get(&(root_id.to_string(), base.to_string()))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

pub fn bundle_for_indexed(inv: &Inventory, idx: &BundleIndex, key: &str) -> Vec<String> {
    let mut keys = std::collections::BTreeSet::new();
    keys.insert(key.to_string());
    let Some(entry) = inv.models.get(key) else {
        return keys.into_iter().collect();
    };
    for loc in &entry.locations {
        match loc.kind {
            RootKind::Ollama => {
                let base = ollama_base(&entry.display_name);
                for k2 in idx.same_ollama_base(&loc.root_id, base) {
                    if k2 != key {
                        keys.insert(k2.clone());
                    }
                }
            }
            _ => {
                let container = container_of(loc.kind, &loc.rel_path);
                let fname = file_name_of(&loc.rel_path);
                let my_split =
                    crate::core::acquire::split_parts(&fname).map(|(p, _, c)| (p.to_string(), c));
                let i_am_projector = is_projector_name(&fname);
                // A non-GGUF weights file isn't self-contained: the model is
                // the whole container (tokenizer, configs, everything).
                let i_am_weights = crate::core::scan::is_weights_filename(&fname);
                // Only models in this same directory can be companions —
                // except for weights, which claim their whole subtree.
                let candidates: Vec<&String> = if i_am_weights {
                    if container.as_os_str().is_empty() {
                        Vec::new()
                    } else {
                        idx.under_container(&loc.root_id, &container)
                    }
                } else {
                    idx.in_container(&loc.root_id, &container).iter().collect()
                };
                for k2 in candidates {
                    if k2 == key {
                        continue;
                    }
                    let Some(e2) = inv.models.get(k2) else { continue };
                    let companion = e2.locations.iter().any(|l2| {
                        if l2.root_id != loc.root_id {
                            return false;
                        }
                        // Weights make the whole subtree the model — the
                        // same rule the shelf scanner applies — so subdir
                        // companions (1_Pooling/config.json) come too.
                        if i_am_weights {
                            return !container.as_os_str().is_empty()
                                && l2.rel_path.starts_with(&container);
                        }
                        if container_of(l2.kind, &l2.rel_path) != container {
                            return false;
                        }
                        let f2 = file_name_of(&l2.rel_path);
                        let same_split = my_split.as_ref().is_some_and(|(p, c)| {
                            crate::core::acquire::split_parts(&f2)
                                .is_some_and(|(p2, _, c2)| p2 == p && c2 == *c)
                        });
                        same_split
                            || (!i_am_projector && is_projector_name(&f2))
                    });
                    if companion {
                        keys.insert(k2.clone());
                    }
                }
            }
        }
    }
    keys.into_iter().collect()
}

pub(crate) fn ollama_base(name: &str) -> &str {
    name.strip_suffix("+projector").unwrap_or(name)
}

fn file_name_of(rel: &Path) -> String {
    rel.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The scope inside which files count as "kept beside each other": for the
/// HF hub cache, the snapshot revision (mmproj at snapshot root pairs with
/// a quant in a split subfolder); elsewhere, the parent directory.
fn container_of(kind: RootKind, rel: &Path) -> PathBuf {
    match kind {
        // models--org--name/snapshots/<rev>/...
        RootKind::HfHub => rel.components().take(3).collect(),
        _ => rel.parent().map(Path::to_path_buf).unwrap_or_default(),
    }
}

/// Disk usage grouped by model family — `<architecture> <size_label>` when
/// the GGUF header offers them, else the display name's leading token.
#[derive(Debug, Clone, Serialize)]
pub struct FamilyUsage {
    pub family: String,
    pub contents: usize,
    /// One copy of each distinct content.
    pub unique_bytes: u64,
    /// Every live location counted — the difference to `unique_bytes` is
    /// what duplication and backups cost.
    pub stored_bytes: u64,
}

/// What forgetting a root costs in *knowledge*: (models with a copy
/// there, models whose ONLY known copies are there, bytes of the latter).
/// Only-here models leave the catalog entirely once the root is forgotten.
pub fn root_impact(inv: &Inventory, root_id: &str) -> (usize, usize, u64) {
    let mut touched = 0usize;
    let mut only = 0usize;
    let mut only_bytes = 0u64;
    for e in inv.models.values() {
        if !e.locations.iter().any(|l| l.root_id == root_id) {
            continue;
        }
        touched += 1;
        if e.locations.iter().all(|l| l.root_id == root_id) {
            only += 1;
            only_bytes += e.size;
        }
    }
    (touched, only, only_bytes)
}

pub fn family_usage(inv: &Inventory) -> Vec<FamilyUsage> {
    let mut map: BTreeMap<String, FamilyUsage> = BTreeMap::new();
    for entry in inv.models.values() {
        let family = entry
            .meta
            .as_ref()
            .and_then(|g| {
                g.architecture.as_ref().map(|a| match &g.size_label {
                    Some(s) => format!("{a} {s}"),
                    None => a.clone(),
                })
            })
            .unwrap_or_else(|| {
                entry
                    .display_name
                    .split([' ', ':', '/'])
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            });
        let mut inodes: Vec<(u64, u64)> = entry
            .locations
            .iter()
            .filter(|l| inv.live_accessible(l))
            .map(|l| (l.dev, l.ino))
            .collect();
        inodes.sort();
        inodes.dedup();
        let u = map.entry(family.clone()).or_insert(FamilyUsage {
            family,
            contents: 0,
            unique_bytes: 0,
            stored_bytes: 0,
        });
        u.contents += 1;
        u.unique_bytes += entry.size;
        u.stored_bytes += entry.size * inodes.len().max(1) as u64;
    }
    let mut out: Vec<FamilyUsage> = map.into_values().collect();
    out.sort_by(|a, b| b.stored_bytes.cmp(&a.stored_bytes));
    out
}

/// Hash-identical content present as more than one set of bytes *on the
/// same filesystem*. Hardlinked paths count as ONE set — nothing to reclaim
/// there — and copies on different devices (e.g. a backup drive) are
/// intentional redundancy, not waste: hardlinks can't cross filesystems.
pub fn dup_groups(inv: &Inventory) -> Vec<DupGroup> {
    let mut out = Vec::new();
    for (key, entry) in &inv.models {
        let Some(hash) = key.strip_prefix("sha256:") else {
            continue;
        };
        let mut by_dev: BTreeMap<u64, std::collections::BTreeSet<u64>> = BTreeMap::new();
        // Live, not "was accessible at the last hash": reclaim is
        // hardlinking, which needs the filesystem mounted right now.
        for l in entry
            .locations
            .iter()
            .filter(|l| l.dev != 0 && inv.live_accessible(l))
        {
            by_dev.entry(l.dev).or_default().insert(l.ino);
        }
        let extra_inodes: u64 = by_dev
            .values()
            .map(|inos| inos.len() as u64 - 1)
            .sum();
        if extra_inodes > 0 {
            out.push(DupGroup {
                sha256: hash.to_string(),
                size: entry.size,
                display_name: entry.display_name.clone(),
                locations: entry.locations.clone(),
                reclaimable: extra_inodes * entry.size,
            });
        }
    }
    out.sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gguf::tests::synthetic_gguf;

    fn shelf_spec(path: &Path) -> RootSpec {
        RootSpec {
            id: "shelf-test".into(),
            kind: RootKind::Shelf,
            path: path.to_path_buf(),
            label: None,
        }
    }

    #[test]
    fn manifest_roundtrips_through_disk() {
        let shelf = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shelf.path().join("M")).unwrap();
        std::fs::write(
            shelf.path().join("M/m.gguf"),
            synthetic_gguf("llama", 4096, 15),
        )
        .unwrap();

        let m = build_root_manifest(&shelf_spec(shelf.path()), None);
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].rel_path, PathBuf::from("M/m.gguf"));
        assert!(m.files[0].fingerprint.is_some());
        assert!(m.files[0].sha256.is_none(), "hashing is lazy");

        let path = manifest_path(state.path(), &m.root.id);
        save_json(&m, &path).unwrap();
        let loaded = load_all_manifests(state.path());
        assert_eq!(loaded, vec![m]);
    }

    #[test]
    fn unchanged_fingerprint_carries_the_hash_forward() {
        let shelf = tempfile::tempdir().unwrap();
        std::fs::write(shelf.path().join("m.gguf"), synthetic_gguf("llama", 1, 1)).unwrap();
        let spec = shelf_spec(shelf.path());

        let mut first = build_root_manifest(&spec, None);
        first.files[0].sha256 = Some("cafe".into());
        let second = build_root_manifest(&spec, Some(&first));
        assert_eq!(second.files[0].sha256.as_deref(), Some("cafe"));

        // Rewriting the file invalidates the stored hash.
        std::fs::write(
            shelf.path().join("m.gguf"),
            synthetic_gguf("qwen3", 2048, 15),
        )
        .unwrap();
        let third = build_root_manifest(&spec, Some(&first));
        assert!(third.files[0].sha256.is_none(), "changed bytes → rehash");
    }

    #[test]
    fn save_is_atomic_and_keeps_a_bak() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("roots/x.json");
        save_json(&serde_json::json!({"v": 1}), &path).unwrap();
        save_json(&serde_json::json!({"v": 2}), &path).unwrap();
        let cur: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let bak: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path.with_extension("json.bak")).unwrap())
                .unwrap();
        assert_eq!(cur["v"], 2);
        assert_eq!(bak["v"], 1);
    }

    #[test]
    fn merge_keeps_offline_roots_and_groups_by_hash() {
        let online = tempfile::tempdir().unwrap();
        let mut a = RootManifest {
            schema_version: SCHEMA_VERSION,
            root: shelf_spec(online.path()),
            generated_unix: 1,
            files: vec![FileRecord {
                rel_path: "m.gguf".into(),
                size: 100,
                fingerprint: Some(Fingerprint {
                    size: 100,
                    mtime_s: 0,
                    mtime_nsec: 0,
                    dev: 1,
                    ino: 10,
                }),
                sha256: Some("aa".into()),
                name: None,
                meta: None,
                accessible: true,
                verified_unix: None,
            }],
        };
        // Same content recorded on an unplugged drive.
        let mut b = a.clone();
        b.root = RootSpec {
            id: "shelf-offline".into(),
            kind: RootKind::Shelf,
            path: "/media/nowhere/archive".into(),
            label: None,
        };
        b.files[0].fingerprint = None;
        b.files[0].rel_path = "backup/m.gguf".into();
        a.root.id = "shelf-online".into();

        let inv = merge(&[a, b]);
        assert_eq!(inv.models.len(), 1, "one content, two locations");
        let entry = &inv.models["sha256:aa"];
        assert_eq!(entry.locations.len(), 2);
        let offline = entry
            .locations
            .iter()
            .find(|l| l.root_id == "shelf-offline")
            .unwrap();
        assert!(!offline.accessible, "offline, not gone — and not dropped");
    }

    #[test]
    fn refresh_hashes_once_then_reuses_stored_hashes() {
        let shelf = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(
            shelf.path().join("m.gguf"),
            synthetic_gguf("llama", 4096, 15),
        )
        .unwrap();
        let specs = vec![shelf_spec(shelf.path())];

        let mut events = Vec::new();
        let inv = refresh(&specs, state.path(), |ev| {
            if matches!(ev, RefreshEvent::HashStart { .. }) {
                events.push(());
            }
        })
        .unwrap();
        assert_eq!(events.len(), 1, "one file hashed");
        assert_eq!(inv.models.len(), 1);
        assert!(inv.models.keys().next().unwrap().starts_with("sha256:"));
        assert!(inventory_path(state.path()).exists());

        // Unchanged file: second refresh hashes nothing, identity survives.
        let mut second_events = Vec::new();
        let inv2 = refresh(&specs, state.path(), |ev| {
            if matches!(ev, RefreshEvent::HashStart { .. }) {
                second_events.push(());
            }
        })
        .unwrap();
        assert!(second_events.is_empty(), "fingerprint match → no rehash");
        assert_eq!(
            inv.models.keys().collect::<Vec<_>>(),
            inv2.models.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn refresh_never_clobbers_an_offline_roots_manifest() {
        let state = tempfile::tempdir().unwrap();
        // A stored manifest for a drive that is not plugged in.
        let stored = RootManifest {
            schema_version: SCHEMA_VERSION,
            root: RootSpec {
                id: "ext-cafe0123".into(),
                kind: RootKind::Removable,
                path: "/media/nowhere/archive1".into(),
                label: Some("archive1".into()),
            },
            generated_unix: 42,
            files: vec![FileRecord {
                rel_path: "cold/m.gguf".into(),
                size: 1000,
                fingerprint: None,
                sha256: Some("dd".into()),
                name: None,
                meta: None,
                accessible: true,
                verified_unix: None,
            }],
        };
        save_json(&stored, &manifest_path(state.path(), "ext-cafe0123")).unwrap();

        let inv = refresh(&[stored.root.clone()], state.path(), |_| {}).unwrap();
        // The manifest survived untouched and the offline copy is still known.
        assert_eq!(
            load_manifest(&manifest_path(state.path(), "ext-cafe0123")),
            Some(stored)
        );
        let entry = &inv.models["sha256:dd"];
        assert_eq!(entry.locations.len(), 1);
        assert!(!entry.locations[0].accessible, "offline, not gone");
    }

    #[test]
    fn bundles_group_splits_mmproj_and_ollama_projectors() {
        let loc = |kind: RootKind, root: &str, rel: &str| Location {
            root_id: root.into(),
            kind,
            rel_path: rel.into(),
            accessible: true,
            dev: 1,
            ino: 1,
        };
        let entry = |name: &str, locs: Vec<Location>| ModelEntry {
            size: 1,
            display_name: name.into(),
            meta: None,
            locations: locs,
        };
        let mut models = BTreeMap::new();
        // HF snapshot: split quant in a subfolder + mmproj at snapshot root.
        models.insert(
            "sha256:part1".to_string(),
            entry("unsloth/Big-GGUF", vec![loc(
                RootKind::HfHub, "hf",
                "models--unsloth--Big-GGUF/snapshots/rev1/UD/big-00001-of-00002.gguf",
            )]),
        );
        models.insert(
            "sha256:part2".to_string(),
            entry("unsloth/Big-GGUF", vec![loc(
                RootKind::HfHub, "hf",
                "models--unsloth--Big-GGUF/snapshots/rev1/UD/big-00002-of-00002.gguf",
            )]),
        );
        models.insert(
            "sha256:proj".to_string(),
            entry("unsloth/Big-GGUF", vec![loc(
                RootKind::HfHub, "hf",
                "models--unsloth--Big-GGUF/snapshots/rev1/mmproj-F16.gguf",
            )]),
        );
        // A different quant in the same snapshot: NOT part of the bundle.
        models.insert(
            "sha256:otherquant".to_string(),
            entry("unsloth/Big-GGUF", vec![loc(
                RootKind::HfHub, "hf",
                "models--unsloth--Big-GGUF/snapshots/rev1/big-Q2_K.gguf",
            )]),
        );
        // Ollama model + projector tied by name.
        models.insert(
            "sha256:omodel".to_string(),
            entry("vision:latest", vec![loc(RootKind::Ollama, "ol", "blobs/sha256-m")]),
        );
        models.insert(
            "sha256:oproj".to_string(),
            entry("vision:latest+projector", vec![loc(RootKind::Ollama, "ol", "blobs/sha256-p")]),
        );
        let inv = Inventory {
            schema_version: SCHEMA_VERSION,
            generated_unix: 0,
            roots: vec![],
            models,
        online: Default::default(),
        };
        // Picking one split part pulls the other part and the projector,
        // but not the unrelated quant.
        let b = bundle_for(&inv, "sha256:part1");
        assert_eq!(b, vec!["sha256:part1", "sha256:part2", "sha256:proj"]);
        // Picking the projector does not drag every quant along.
        assert_eq!(bundle_for(&inv, "sha256:proj"), vec!["sha256:proj"]);
        // Ollama model pulls its projector (and vice versa).
        assert_eq!(
            bundle_for(&inv, "sha256:omodel"),
            vec!["sha256:omodel", "sha256:oproj"]
        );
        assert_eq!(
            bundle_for(&inv, "sha256:oproj"),
            vec!["sha256:omodel", "sha256:oproj"]
        );
    }

    #[test]
    fn a_weights_file_bundles_its_whole_container() {
        let loc = |rel: &str| Location {
            root_id: "hf".into(),
            kind: RootKind::HfHub,
            rel_path: rel.into(),
            accessible: true,
            dev: 1,
            ino: 1,
        };
        let entry = |locs: Vec<Location>| ModelEntry {
            size: 1,
            display_name: "org/Embed".into(),
            meta: None,
            locations: locs,
        };
        let mut models = BTreeMap::new();
        models.insert(
            "sha256:weights".to_string(),
            entry(vec![loc("models--org--Embed/snapshots/rev1/model.safetensors")]),
        );
        models.insert(
            "sha256:tok".to_string(),
            entry(vec![loc("models--org--Embed/snapshots/rev1/tokenizer.json")]),
        );
        models.insert(
            "sha256:pool".to_string(),
            entry(vec![loc("models--org--Embed/snapshots/rev1/1_Pooling/config.json")]),
        );
        // Another repo entirely: not part of the bundle.
        models.insert(
            "sha256:other".to_string(),
            entry(vec![loc("models--org--Other/snapshots/rev9/model.safetensors")]),
        );
        let inv = Inventory {
            schema_version: SCHEMA_VERSION,
            generated_unix: 0,
            roots: vec![],
            models,
            online: Default::default(),
        };
        assert_eq!(
            bundle_for(&inv, "sha256:weights"),
            vec!["sha256:pool", "sha256:tok", "sha256:weights"],
            "weights pull the whole snapshot, subdirs included"
        );
        // A companion alone stays alone (asymmetric, like mmproj).
        assert_eq!(bundle_for(&inv, "sha256:tok"), vec!["sha256:tok"]);
    }

    #[test]
    fn dup_groups_ignore_hardlinks_and_rank_by_reclaimable() {
        let mounted = tempfile::tempdir().unwrap();
        let mut models = BTreeMap::new();
        let loc = |root: &str, ino: u64| Location {
            root_id: root.into(),
            kind: RootKind::Shelf,
            rel_path: "x".into(),
            accessible: true,
            dev: 1,
            ino,
        };
        // Two locations, same inode: a hardlink, not a duplicate.
        models.insert(
            "sha256:linked".to_string(),
            ModelEntry {
                size: 500,
                display_name: "linked".into(),
                meta: None,
                locations: vec![loc("a", 1), loc("b", 1)],
            },
        );
        // Three locations, two inodes: one reclaimable copy.
        models.insert(
            "sha256:copied".to_string(),
            ModelEntry {
                size: 700,
                display_name: "copied".into(),
                meta: None,
                locations: vec![loc("a", 2), loc("a", 3), loc("b", 2)],
            },
        );
        // A copy on another filesystem (backup drive): intentional
        // redundancy, not reclaimable — hardlinks can't cross devices.
        models.insert(
            "sha256:backedup".to_string(),
            ModelEntry {
                size: 900,
                display_name: "backedup".into(),
                meta: None,
                locations: vec![
                    loc("a", 7),
                    Location {
                        root_id: "vault".into(),
                        kind: RootKind::Removable,
                        rel_path: "x".into(),
                        accessible: true,
                        dev: 2,
                        ino: 7,
                    },
                ],
            },
        );
        let inv = Inventory {
            schema_version: SCHEMA_VERSION,
            generated_unix: 0,
            roots: ["a", "b"]
                .into_iter()
                .map(|id| RootSpec {
                    id: id.into(),
                    kind: RootKind::Shelf,
                    path: mounted.path().to_path_buf(),
                    label: None,
                })
                .collect(),
            models,
            online: Default::default(),
        };
        let groups = dup_groups(&inv);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sha256, "copied");
        assert_eq!(groups[0].reclaimable, 700);
    }
    fn entry_with(kinds: &[(RootKind, bool)]) -> ModelEntry {
        ModelEntry {
            size: 1,
            display_name: "m".into(),
            meta: None,
            locations: kinds
                .iter()
                .enumerate()
                .map(|(i, (kind, accessible))| Location {
                    root_id: format!("r{i}"),
                    kind: *kind,
                    rel_path: PathBuf::from("m.gguf"),
                    accessible: *accessible,
                    dev: 0,
                    ino: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn backed_up_means_a_copy_on_a_registered_drive() {
        // No roots registered here: every drive reads as offline, which
        // is exactly the "unplugged drive still counts" case.
        let inv = inv_with(vec![], BTreeMap::new());
        // Only shelf/cache copies: one disk failure loses it.
        assert!(!is_backed_up(
            &inv,
            &entry_with(&[(RootKind::Shelf, true), (RootKind::HfHub, true)])
        ));
        // A drive copy counts — even offline: the bytes exist on that
        // drive whether or not it is plugged in right now.
        assert!(is_backed_up(
            &inv,
            &entry_with(&[(RootKind::Shelf, true), (RootKind::Removable, false)])
        ));
        assert!(is_backed_up(
            &inv,
            &entry_with(&[(RootKind::Removable, true)])
        ));
    }

    /// One location on a named root, so a test can say exactly which
    /// root each copy lives on.
    fn loc_on(root_id: &str, kind: RootKind, accessible: bool) -> Location {
        Location {
            root_id: root_id.into(),
            kind,
            rel_path: PathBuf::from("m.gguf"),
            accessible,
            dev: 0,
            ino: 0,
        }
    }

    fn inv_with(roots: Vec<RootSpec>, models: BTreeMap<String, ModelEntry>) -> Inventory {
        Inventory {
            schema_version: SCHEMA_VERSION,
            generated_unix: 0,
            roots,
            models,
            online: Default::default(),
        }
    }

    #[test]
    fn a_location_is_present_offline_or_unreadable_and_nothing_else() {
        let attached = tempfile::tempdir().unwrap();
        let inv = inv_with(
            vec![
                RootSpec { id: "on".into(), kind: RootKind::Removable, path: attached.path().into(), label: None },
                RootSpec { id: "off".into(), kind: RootKind::Removable, path: attached.path().join("unplugged"), label: None },
            ],
            BTreeMap::new(),
        );
        use LocationState::*;
        assert_eq!(inv.location_state(&loc_on("on", RootKind::Removable, true)), Present);
        assert_eq!(inv.location_state(&loc_on("on", RootKind::Removable, false)), Unreadable);
        // An unplugged drive is Offline whatever the stored flag says —
        // the flag describes the last scan, not now.
        assert_eq!(inv.location_state(&loc_on("off", RootKind::Removable, true)), Offline);
        assert_eq!(inv.location_state(&loc_on("off", RootKind::Removable, false)), Offline);
        // Liveness is that one state, not a second opinion.
        assert!(inv.live_accessible(&loc_on("on", RootKind::Removable, true)));
        assert!(!inv.live_accessible(&loc_on("off", RootKind::Removable, true)));
    }

    #[test]
    fn backup_coverage_never_counts_a_copy_it_knows_is_unreadable() {
        // The safety headline decided ✓ from `kind == Removable` alone,
        // so a drive copy that is attached and WILL NOT OPEN — pruned
        // blob, permission change, deleted behind warden's back —
        // promised a restore that cannot happen. An *offline* drive is a
        // different thing and must still count: the bytes travel with it.
        let attached = tempfile::tempdir().unwrap();
        let roots = vec![
            RootSpec { id: "here".into(), kind: RootKind::Removable, path: attached.path().into(), label: None },
            RootSpec { id: "away".into(), kind: RootKind::Removable, path: attached.path().join("unplugged"), label: None },
        ];
        let mut models = BTreeMap::new();
        let mk = |loc: Location| ModelEntry {
            size: 1,
            display_name: "m".into(),
            meta: None,
            locations: vec![loc_on("shelf", RootKind::Shelf, true), loc],
        };
        models.insert("sha256:present".into(), mk(loc_on("here", RootKind::Removable, true)));
        models.insert("sha256:offline".into(), mk(loc_on("away", RootKind::Removable, true)));
        models.insert("sha256:broken".into(), mk(loc_on("here", RootKind::Removable, false)));
        let inv = inv_with(roots, models);

        assert!(is_backed_up(&inv, &inv.models["sha256:present"]));
        assert!(is_backed_up(&inv, &inv.models["sha256:offline"]), "offline is not gone");
        assert!(
            !is_backed_up(&inv, &inv.models["sha256:broken"]),
            "an unreadable drive copy is not a backup"
        );
        assert_eq!(backup_coverage(&inv), (2, 3));
    }

    #[test]
    fn duplicate_reporting_and_family_usage_agree_about_liveness() {
        // The CLI reads a serialized inventory, so `accessible` is a
        // claim about the last hash. dup_groups trusted that stale flag
        // while family_usage re-checked the root: unplug the drive and
        // the two views described different worlds — one offering to
        // reclaim bytes on a filesystem that is not mounted.
        let attached = tempfile::tempdir().unwrap();
        let roots = vec![RootSpec {
            id: "away".into(),
            kind: RootKind::Removable,
            path: attached.path().join("unplugged"),
            label: None,
        }];
        let mut models = BTreeMap::new();
        let two_copies = |a: u64, b: u64| ModelEntry {
            size: 100,
            display_name: "m".into(),
            meta: None,
            locations: vec![
                Location { ino: a, dev: 9, ..loc_on("away", RootKind::Removable, true) },
                Location { ino: b, dev: 9, ..loc_on("away", RootKind::Removable, true) },
            ],
        };
        models.insert("sha256:copied".into(), two_copies(1, 2));
        let inv = inv_with(roots, models);

        assert!(
            dup_groups(&inv).is_empty(),
            "nothing is reclaimable on an unmounted drive"
        );
        assert_eq!(
            family_usage(&inv)[0].stored_bytes,
            100,
            "and usage counts no live copy either"
        );
    }

    #[test]
    fn backup_coverage_counts_the_safety_headline() {
        let mut inv = Inventory {
            schema_version: SCHEMA_VERSION,
            generated_unix: 0,
            roots: Vec::new(),
            models: BTreeMap::new(),
            online: Default::default(),
        };
        inv.models.insert(
            "sha256:aa".into(),
            entry_with(&[(RootKind::Shelf, true), (RootKind::Removable, true)]),
        );
        inv.models
            .insert("sha256:bb".into(), entry_with(&[(RootKind::Shelf, true)]));
        inv.models
            .insert("sha256:cc".into(), entry_with(&[(RootKind::Ollama, true)]));
        assert_eq!(backup_coverage(&inv), (1, 3));
    }
    /// Not an assertion — a measurement, printed with `--nocapture`,
    /// to size the relation cache honestly rather than by arithmetic.
    #[test]
    #[ignore]
    fn measure_relation_cost() {
        for n in [100usize, 500, 2000] {
            let mut models = BTreeMap::new();
            for i in 0..n {
                let dir = format!("fam{}", i / 4);
                models.insert(
                    format!("sha256:{i:04}"),
                    ModelEntry {
                        size: 1,
                        display_name: format!("m{i}"),
                        meta: None,
                        locations: vec![Location {
                            root_id: "shelf-1".into(),
                            kind: RootKind::Shelf,
                            rel_path: PathBuf::from(format!("{dir}/m{i}-00001-of-00002.gguf")),
                            accessible: true,
                            dev: 1,
                            ino: i as u64,
                        }],
                    },
                );
            }
            let inv = inv_with(vec![], models);
            let t = std::time::Instant::now();
            let _ = companion_parents(&inv);
            let _ = split_primary_of(&inv);
            println!("n={n}: relations {:?} per frame", t.elapsed());

            // E17: the liveness predicate stats the root filesystem for
            // every location, and the row loop asks it for every row.
            let mounted = tempfile::tempdir().unwrap();
            let live = inv_with(
                vec![RootSpec {
                    id: "shelf-1".into(),
                    kind: RootKind::Shelf,
                    path: mounted.path().to_path_buf(),
                    label: None,
                }],
                inv.models.clone(),
            );
            let t = std::time::Instant::now();
            let n_live = live
                .models
                .values()
                .flat_map(|e| &e.locations)
                .filter(|l| live.live_accessible(l))
                .count();
            println!("n={n}: liveness {:?} per frame ({n_live} live)", t.elapsed());
        }
    }

    #[test]
    fn an_unchanged_file_never_has_its_header_read_again() {
        // build_root_manifest carried sha256 and verified_unix forward
        // when the fingerprint was unchanged, then threw away the meta
        // it already had and re-derived it — which means opening and
        // walking the whole KV block of every GGUF, on every scan, for
        // the entire life of the catalog. Nothing about an unchanged
        // file needs re-reading.
        //
        // Staged by making the bytes unreadable while leaving the
        // fingerprint (size, mtime, dev, ino) untouched: a scan that
        // still parses headers loses the metadata, one that carries it
        // forward does not.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use crate::core::gguf::tests::synthetic_gguf;
        use std::os::unix::fs::PermissionsExt;
        let shelf = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let model = shelf.path().join("m.gguf");
        std::fs::write(&model, synthetic_gguf("llama", 8192, 15)).unwrap();
        let specs = [RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        }];

        let first = refresh(&specs, state.path(), |_| {}).unwrap();
        let arch = |inv: &Inventory| {
            inv.models
                .values()
                .next()
                .and_then(|e| e.meta.as_ref())
                .and_then(|m| m.architecture.clone())
        };
        assert_eq!(arch(&first).as_deref(), Some("llama"));
        let before = std::fs::metadata(&model).unwrap().modified().unwrap();

        std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o000)).unwrap();
        let again = refresh(&specs, state.path(), |_| {}).unwrap();
        std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            std::fs::metadata(&model).unwrap().modified().unwrap(),
            before,
            "the file itself did not change — the fingerprint must still match"
        );
        assert_eq!(
            arch(&again).as_deref(),
            Some("llama"),
            "the header was re-read instead of carried forward"
        );
        assert!(
            again.models.keys().all(|k| k.starts_with("sha256:")),
            "and the hash is still carried too"
        );
    }

    #[test]
    fn checkpoints_are_paced_by_time_not_taken_per_file() {
        let t0 = std::time::Instant::now();
        let mut cp = Checkpoint::new();
        // The first finished file is always made durable: an early
        // crash is never a total loss, and a state dir warden cannot
        // write to is discovered now, not after an hour of hashing.
        assert!(cp.tick_at(t0));
        // Small files that land in a burst do NOT each rewrite the
        // whole manifest — that was O(n²) bytes over a large root.
        for _ in 1..Checkpoint::EVERY {
            assert!(!cp.tick_at(t0));
        }
        assert!(cp.tick_at(t0), "a full batch is worth making durable");
        assert!(!cp.tick_at(t0), "and the count starts over");
        // A file slow enough for its loss to hurt checkpoints on time,
        // whatever the count: that is the resume guarantee.
        assert!(cp.tick_at(t0 + Checkpoint::INTERVAL));
    }

    #[test]
    fn hashing_checkpoints_each_file_and_reports_every_job() {
        use crate::core::gguf::tests::synthetic_gguf;
        let shelf = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        for i in 0..5 {
            let mut b = synthetic_gguf("llama", 8192, 15);
            b.push(i);
            std::fs::write(shelf.path().join(format!("m{i}.gguf")), b).unwrap();
        }
        let spec = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let specs = [spec];
        let mut starts = 0usize;
        let mut dones = 0usize;
        let mut checkpoint_seen = false;
        let inv = refresh(&specs, state.path(), |ev| match ev {
            RefreshEvent::HashStart { .. } => starts += 1,
            RefreshEvent::HashDone { .. } => {
                dones += 1;
                if !checkpoint_seen {
                    // The manifest on disk must already hold this hash
                    // BEFORE the completion is reported: an interrupted
                    // first hash resumes instead of restarting.
                    let m = load_manifest(&manifest_path(state.path(), "shelf-1"))
                        .expect("checkpoint manifest written before HashDone");
                    assert!(
                        m.files.iter().any(|f| f.sha256.is_some()),
                        "checkpoint holds at least the finished hash"
                    );
                    checkpoint_seen = true;
                }
            }
            RefreshEvent::HashFailed { label, error } => {
                panic!("unexpected failure {label}: {error}")
            }
            RefreshEvent::HashProgress { .. } => {}
        })
        .unwrap();
        // Every job reported exactly once, all hashes landed.
        assert_eq!(starts, 5);
        assert_eq!(dones, 5);
        assert!(checkpoint_seen);
        assert_eq!(inv.models.len(), 5);
        assert!(inv.models.keys().all(|k| k.starts_with("sha256:")));
    }
    #[test]
    fn split_parts_group_under_part_one_for_display() {
        use crate::core::gguf::tests::synthetic_gguf;
        let shelf = tempfile::tempdir().unwrap();
        let base = synthetic_gguf("llama", 8192, 15);
        let mut p2 = base.clone();
        p2.extend_from_slice(b"2");
        let mut proj = base.clone();
        proj.extend_from_slice(b"p");
        let mut other = base.clone();
        other.extend_from_slice(b"o");
        std::fs::write(shelf.path().join("big-00001-of-00002.gguf"), &base).unwrap();
        std::fs::write(shelf.path().join("big-00002-of-00002.gguf"), &p2).unwrap();
        std::fs::write(shelf.path().join("mmproj-F16.gguf"), &proj).unwrap();
        std::fs::write(shelf.path().join("other-Q4.gguf"), &other).unwrap();
        let spec = RootSpec {
            id: "shelf-1".into(),
            kind: RootKind::Shelf,
            path: shelf.path().to_path_buf(),
            label: None,
        };
        let inv = merge(&[build_root_manifest(&spec, None)]);
        let key_of = |frag: &str| {
            inv.models
                .iter()
                .find(|(_, e)| e.display_name.contains(frag))
                .map(|(k, _)| k.clone())
                .unwrap()
        };
        let map = split_primary_of(&inv);
        // Part 2 groups under part 1 — one model, one display row.
        assert_eq!(map.get(&key_of("big-00002-of")), Some(&key_of("big-00001-of")), "{map:?}");
        // Part 1 is the primary, never a child; non-splits stay out.
        assert!(!map.contains_key(&key_of("big-00001-of")));
        assert!(!map.contains_key(&key_of("mmproj")));
        assert!(!map.contains_key(&key_of("other")));
    }
    #[test]
    fn unsafe_relative_paths_are_refused() {
        // A manifest's rel_path is untrusted input the moment it comes
        // off removable media: it is joined to a root and then read,
        // written, moved, and deleted.
        assert!(sanitize_rel(Path::new("Vision/model.gguf")).is_ok());
        assert!(sanitize_rel(Path::new("a/b/c/model.gguf")).is_ok());
        for bad in [
            "../escape.gguf",
            "a/../../escape.gguf",
            "/etc/passwd",
            "",
            "..",
        ] {
            assert!(
                sanitize_rel(Path::new(bad)).is_err(),
                "must refuse {bad:?}"
            );
        }
        // A trailing/interior CurDir is harmless but normalized away.
        assert_eq!(
            sanitize_rel(Path::new("./a/./b.gguf")).unwrap(),
            PathBuf::from("a/b.gguf")
        );
    }

    #[test]
    fn a_carried_manifest_is_scrubbed_before_it_is_trusted() {
        use crate::core::gguf::tests::synthetic_gguf;
        let drive = tempfile::tempdir().unwrap();
        let real = synthetic_gguf("llama", 8192, 15);
        std::fs::write(drive.path().join("present.gguf"), &real).unwrap();
        let rec = |rel: &str, size: u64| FileRecord {
            rel_path: PathBuf::from(rel),
            size,
            fingerprint: None,
            sha256: Some("a".repeat(64)),
            name: None,
            meta: None,
            accessible: true,
            verified_unix: None,
        };
        let man = RootManifest {
            schema_version: SCHEMA_VERSION,
            root: RootSpec {
                id: "ext-x".into(),
                kind: RootKind::Removable,
                path: drive.path().to_path_buf(),
                label: None,
            },
            generated_unix: 0,
            files: vec![
                rec("present.gguf", real.len() as u64), // real: kept
                rec("../../../../etc/passwd", 10),      // traversal: dropped
                rec("/etc/shadow", 10),                 // absolute: dropped
                rec("ghost.gguf", 999),                 // fabricated: dropped
                rec("present.gguf.2", real.len() as u64), // fabricated: dropped
            ],
        };
        let (clean, dropped) = sanitize_carried(man, drive.path());
        assert_eq!(dropped, 4, "{:?}", clean.files);
        assert_eq!(clean.files.len(), 1);
        assert_eq!(clean.files[0].rel_path, PathBuf::from("present.gguf"));

        // A record that exists but lies about its size is dropped too —
        // that lie is what makes backup skip copying the real model.
        let man = RootManifest {
            files: vec![rec("present.gguf", 999_999)],
            ..clean.clone()
        };
        let (clean2, dropped2) = sanitize_carried(man, drive.path());
        assert_eq!(dropped2, 1);
        assert!(clean2.files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn save_json_never_leaves_the_target_missing_and_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roots/x.json");
        save_json(&vec![1, 2, 3], &path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().replace(['\n', ' '], ""),
            "[1,2,3]"
        );
        // The previous version is kept by COPYING it aside, never by
        // moving the live file out of the way — the old code renamed the
        // target to .bak first, so a crash in that window left no
        // manifest at all and the root silently reverted to unknown.
        save_json(&vec![4, 5], &path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().replace(['\n', ' '], ""),
            "[4,5]"
        );
        let bak = path.with_extension("json.bak");
        assert_eq!(
            std::fs::read_to_string(&bak).unwrap().replace(['\n', ' '], ""),
            "[1,2,3]"
        );
        // Manifests carry every model path on the machine.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700);
    }
}
# modelwarden — Plan

## North star

**The owner of 200GB+ of local model files can always answer: what do I have,
where is it, is it reachable, is it backed up, and which of these are the same
bytes — and nothing this tool does can ever lose bytes.**

Consequences:

1. Identity is content (SHA-256), never path. Paths are locations; a model can
   have several, including on drives that aren't plugged in.
2. Warden owns storage truth and acquisition. It never serves models and never
   edits inference-tool configs — that is llamacppCodeConf's side of the
   boundary.
3. Every write path is engineered to be non-destructive: refuse-overwrite,
   .partial temp names, verify-by-hash before any rename or link.
4. A missing storage root is *Offline*, not gone. Manifests outlive mounts.

## The incidents this tool prevents (all from one real session)

1. A 20GB model sat unknown in the HF cache until another tool surfaced it.
2. A downloaded quant became inaccessible because HF's `refs/main` moved to a
   different snapshot revision — file on disk, invisible to tooling.
3. An 18GB duplicate existed (manual download + cache copy of the same quant)
   and nothing noticed until serving aliases collided.

## Boundary contract (with llamacppCodeConf)

- **modelwarden owns**: what exists, where, content identity, provenance,
  backup state, archival tiers, acquisition (downloads).
- **llamacppCodeConf owns**: serving (llama-server router), measurement,
  opencode.json. It reads the same stores (and eventually warden's merged
  inventory) but never manages storage.
- Neither tool reaches across. The meeting points are the filesystem and
  warden's merged inventory manifest, read-only to consumers.

The user's earlier storage/config split (llm_forge + opencode_configuration_tool)
had a fuzzy seam and got merged. This split survives only if the seam stays crisp.

## Decisions (2026-08-16)

| Decision | Choice |
|---|---|
| Language / structure | Rust, edition 2024; single crate, core/ui split: headless testable lib + two bins (`warden` CLI, `warden-gui` egui). Core never depends on egui. |
| Frontends | Both from the start. GUI is a traditional desktop app (menu bar, dialogs, status bar). Read-only views land in the GUI in the same milestone as the CLI; write operations land CLI-first, GUI one milestone later. |
| Identity model | Two-tier: `(size, mtime, dev, ino)` fingerprint is a change-detector keyed by path; SHA-256 is the only canonical identity, computed lazily / in background; both stored. Fingerprint match ⇒ reuse stored hash; mismatch ⇒ rehash (safe, just slow). |
| Manifest storage | JSON, one manifest per storage root, plus a merged view cached at `~/.local/state/modelwarden/inventory.json` with `schema_version` from day one. Owned removable roots carry `<root>/.modelwarden/manifest.json` so unplugged drives stay self-describing. Manifests for foreign stores (Ollama, HF cache) live under `~/.local/state/modelwarden/roots/` — never inside the store. sqlite only if scale or concurrent writers ever materialize. |
| Removable media identity | Filesystem UUID via `/dev/disk/by-uuid`, with a `.modelwarden/root-id` marker-file fallback for filesystems with weak IDs (VFAT) or shared UUIDs (btrfs subvolumes, bind mounts). Accessibility states: Present / Offline / Pruned. Offline entries are never dropped. |
| Dedup scope | Report everywhere; hardlink reclaim only within/into warden-owned roots. Foreign stores (Ollama blobs, HF cache) are never rewritten — report-only, always. |
| Backup layout | Human-readable tree: `<target>/<family>/<file>.gguf` + a target-local manifest recording hashes and dates. A human can rescue files without warden. |
| Acquisition target | HF downloads land in the shelf (warden-owned), provenance (repo, revision, etag, when) recorded at download time. Warden does not write into the HF cache. |
| Safety invariants | Never delete model bytes — hardlinking is the only reclaim, after hash-verifying both files, via temp-link+rename. All copies go .partial → verify hash → rename. Refuse-overwrite everywhere. Manifest writes atomic (temp+rename). Never write inside a store another tool owns. |
| Foreign-store cleanup (2026-08-17) | Warden never deletes real bytes in a foreign store itself. Doctor remedies route through the owning tool's own CLI (`hf cache rm`, `ollama rm`), executed only on explicit user action (`doctor --fix`, GUI Clean up button). One narrow exception: `*.incomplete` download debris — no owner command targets it, so warden may remove those files itself, refusing anything not named `*.incomplete` and anything modified recently (active-download guard). Orphan blobs (real unreferenced bytes) stay manual, always. |
| Demotion refinement (2026-08-16) | A demote is a *verified move*: copy → read-back hash on cold storage → record in the drive's carried manifest → only then, and only with an explicit `--remove-source`, delete the shelf copy. Default keeps both. This is the one sanctioned deletion, because the bytes are provably preserved first. |
| Beyond GGUF (2026-08-17, M12) | Non-GGUF weights (`.safetensors/.bin/.pt/.pth/.onnx`) are not self-contained: the model is the *directory* — weights plus tokenizer/config companions. One weights file makes its whole container (HF snapshot revision / shelf directory subtree) a cataloged bundle; every operation moves it whole. Applies to all roots. Weights metadata comes from the adjacent `config.json` (model_type, context window). Acquisition of non-GGUF repos deliberately deferred. |
| Snapshot fetch (2026-08-20, M13) | Acquisition catches up with M12: `warden fetch --snapshot` (and the GUI's whole-snapshot button, offered automatically when a repo has no GGUFs) downloads a repo's entire snapshot into one shelf directory — dotfiles excluded, mirroring the scanner. GGUF repos keep the per-file/pattern flow; the snapshot is the unit only where the directory is the model. |
| Milestone ordering | Spikes before building; downloads (acquisition) deliberately last, after inventory/dedup/backup/archival mature. |

## Architecture

```
modelwarden/
  Cargo.toml                 edition 2024; anyhow, serde/serde_json, thiserror,
                             sha2, ureq (HF API/downloads), rfd (native file
                             dialogs), eframe = "=0.36.1" (pinned to match
                             harvest source), env_logger; dev: tempfile.
                             Hash-path crates build opt-level=3 even in dev
  src/
    lib.rs                   pub mod core;
    core.rs                  module list; rule: GUI-free and testable
    core/
      gguf.rs                GGUF header reader (arch, ctx, quant)
                             [harvest verbatim: llamacppCodeConf src/core/gguf.rs,
                              incl. tests::synthetic_gguf helper]
      scan.rs                store scanners: shelf/roots walk, Ollama
                             (manifest → blob), HF hub (ALL snapshots after
                             spike 3), inode dedupe
                             [harvest: llamacppCodeConf src/core/library.rs,
                              minus serving-side router_cache_id/alias_suggestion]
      identity.rs            Fingerprint{size,mtime,dev,ino} + lazy SHA-256;
                             background hash worker with mpsc progress
      lock.rs                single-instance write lock: pid file, stale-
                             steal; every write command takes it
      doctor.rs              store health: dangling refs, pruned husks,
                             orphan blobs, incomplete downloads, manifests
                             naming missing blobs — each finding explained
                             (what/loss) with a remedy: owner-tool command
                             (hf cache rm / ollama rm), *.incomplete debris
                             removal (guarded), or exact manual command
      manifest.rs            per-root manifest read/write (atomic temp+rename,
                             schema_version), merged Inventory view,
                             bundle_for() (split parts + mmproj/projector
                             companions), dup_groups, family_usage
      roots.rs               storage-root registry: kind (shelf/ollama/hf/
                             removable/nas), UUID identity, accessibility
      backup.rs              copy → verify-by-hash → record {target, hash, when}
      archive.rs             promote (archive-to-shelf), demote (verified
                             move to cold storage), restore (drive → shelf)
                             [harvest: archive_to_shelf from library.rs]
      dedup.rs               group by sha256, report; hardlink reclaim
                             (owned roots only)
      acquire.rs             HF downloads: Range-resume via .partial, split
                             sets, gated-repo tokens, 401 did-you-mean,
                             provenance recorded by content hash
      settings.rs            config: #[serde(default)] + infallible load
                             [harvest: llamacppCodeConf src/core/settings.rs,
                              minus overrides field; XDG helpers re-implemented]
  src/bin/warden.rs          thin CLI: scan/hash/status/dups/doctor[--fix]/
                             roots/where/backup/verify/archive/restore/dedup/
                             report/fetch; --json on all read commands
  src/bin/warden-gui/        egui shell [pattern from llamacppCodeConf src/ui.rs,
                             re-typed not copied]: Msg enum, spawn(label, job)
                             single-job worker, deferred row actions, menu bar,
                             status bar, activity log, tabs
```

Config at `~/.config/modelwarden/config.json`; state (manifests, inventory) at
`~/.local/state/modelwarden/`.

## Spikes first (verify before building on them)

> **Status 2026-08-16: all four spikes run and confirmed** — details in
> `docs/spikes.md`. Headlines: full-hash of ~217 GiB took ~5.7 min at 683 MB/s
> (background hashing tolerable, no partial-hash tier needed); JSON manifests
> proven at KB scale with offline roots merging cleanly; the real HF cache
> contained 4 pruned-husk repos with dangling refs, 1 multi-snapshot repo, 2
> interrupted downloads (→ scanner rules + `warden doctor`); fs-UUID confirmed
> as removable identity, with by-uuid only listing attached devices (manifest
> must persist the UUID). Bonus: hashing found a real 16.7 GiB shelf duplicate
> that inode comparison cannot see.

Throwaway scripts, read-only against the real stores. Verdicts recorded here and
in `docs/spikes.md`.

1. **Hashing 200GB.** Walk all stores, record fingerprints, full-SHA-256
   everything with per-file timing and aggregate MB/s, cold vs warm cache.
   Answers: is background full-hash tolerable, and does mtime hold up as a
   fingerprint component. Feeds: identity worker design; whether a partial-hash
   middle tier is needed at all.
2. **Manifest format.** Serialize a real scan to the proposed per-root JSON
   schema; simulate an offline drive by merging a manifest whose root path
   doesn't exist. Answers: does the schema express offline media, plural
   locations, and backup records cleanly. Feeds: manifest.rs + eventual
   schema_version 1.
3. **HF hub semantics.** Enumerate ALL `snapshots/<rev>` dirs across the real
   repos; map symlinks to `blobs/`; list orphaned blobs and refs pointing at
   missing snapshots (incident 2). Answers: exact enumeration rules; safe
   coexistence with the hf CLI (skip `.incomplete`/locks, tolerate dangling
   symlinks). Feeds: scan.rs HF scanner, which must go beyond the harvested
   refs/main-only logic.
4. **Removable media identity.** Map mounted path → block device →
   `/dev/disk/by-uuid`; verify stability across unmount/remount and distinctness
   across the machine's filesystems. Answers: is fs-UUID sufficient, and when
   the marker-file fallback is required. Feeds: roots.rs.

## Milestones

> **Status 2026-08-20: all milestones complete** — M0–M7 as planned below,
> plus M8 (restore, split-GGUF + gated downloads, write lock), M8.1 (HF
> token management, 401 did-you-mean), M9 (bundles — every operation moves
> everything a model needs to run; selective backup; native folder pickers),
> M10 (doctor remedies: owner-mediated cleanup via `hf cache rm` /
> `ollama rm`, full explanations + loss statements per finding), M11
> (scrub timer, verify --repair, provenance surfaced), M12 (safetensors
> model directories as whole-container bundles), and M13 (whole-snapshot
> fetch for non-GGUF repos). Details and per-milestone results in
> ROADMAP.md.

- **M0 — plan + scaffold + spikes.** This document, CLAUDE.md, ROADMAP.md, a
  compiling scaffold (both bins runnable), and the four spikes run with verdicts
  recorded.
- **M1 — inventory skeleton, both frontends.** Harvest gguf/scan/settings;
  fingerprint-only identity. `warden scan` prints the unified table (and
  `--json`). GUI: menu bar, read-only Inventory tab, status bar, activity log.
- **M2 — content identity + manifest.** Background SHA-256 worker; per-root +
  merged manifests persisted. `warden hash`, `warden status`, `warden dups`
  (report-only). GUI: hash progress in status bar, duplicate-group view.
- **M3 — roots + offline media.** Root registry, removable drives identified by
  UUID, accessibility states; manifests answer "what's on the unplugged drive
  labeled X". `warden roots add/list`, `warden where <model>`. GUI: roots
  dialog, greyed offline entries.
- **M4 — backup (CLI).** `warden backup <target>`: copy via .partial+rename,
  verify by hash, record. "Is this model safe?" answered per model. GUI shows a
  backup-state column, read-only.
- **M5 — archival + reclaim (CLI); backup (GUI).** Generalized archive-to-shelf
  and cold-storage demotion with manifest hand-off; `warden dedup --hardlink`
  restricted to owned roots. GUI gains the backup action with confirm dialog.
- **M6 — GUI write parity + polish.** Archive/demote/dedup in the GUI with
  confirmations; disk-usage view grouped by model family; README. Publish the
  merged-inventory schema (schema_version 1) for llamacppCodeConf.
- **M7 — acquisition.** `warden fetch <repo> [pattern]`: HF download with
  Range-resume into the shelf, provenance recorded at download time. GUI
  download dialog with progress.

Each milestone leaves something runnable in both bins; write operations are
usable from the CLI one milestone before the GUI exposes them.

## Known risks (watched, not blocking)

- mtime fragility on NAS mounts → fingerprint mismatch causes a rehash storm;
  safe but slow (measured ~680 MB/s cold, ~1.2 GB/s warm).
- HF cache concurrency: a pull running during a scan can present half-written
  blobs; the scanner must tolerate dangling symlinks and skip partial files.
- Committing schema_version 1 (M6) makes the merged inventory a contract;
  changes after that need versioning discipline.
- `/usr/share/ollama` may be unreadable as the user; degrade to
  present-but-unreadable, never fail the whole scan.

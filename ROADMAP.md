# modelwarden — Roadmap

Living tracker. Design authority is PLAN.md; this file records status. North
star: **always able to answer what do I have, where, is it reachable, is it
backed up, which are the same bytes — without ever losing bytes.**

## Done

- **M0** — plan + scaffold + all four spikes run against the real stores
  (verdicts in docs/spikes.md and PLAN.md). Hashing at 683 MB/s validated the
  two-tier identity; spikes found 4 pruned HF husks, 2 interrupted downloads,
  and a real 16.7 GiB shelf duplicate.
- **M1** — inventory skeleton, both frontends. gguf/scan/settings harvested
  (15 tests green) with warden-specific scanner changes: every HF snapshot +
  subdirs enumerated, mmproj inventoried, pruned/dangling files listed as
  MISSING instead of vanishing. `warden scan [--json]` prints the unified
  table (23 files / 284 GiB on the dev machine); GUI shows the same inventory
  read-only with menu bar, status bar, activity log.
- **M2** — content identity + manifests. `manifest::refresh()` is the one
  write path both frontends share: rescan roots, carry hashes forward on
  fingerprint match, hash the rest with byte progress, persist per-root
  manifests + merged inventory (atomic, `.bak`-keeping) under
  `~/.local/state/modelwarden/`. `warden hash/status/dups/doctor` +
  `--json`; GUI gains Duplicates and Health panes, Tools menu, live hash
  progress in the status bar. Doctor's first real run: 7 findings, 2.9 GiB
  of orphaned/partial blobs. Dups confirmed the 16.7 GiB Qwen3.8 duplicate.
  28 tests green.
- **M3** — roots + offline media. Registered roots (drive/NAS) with durable
  identity: fs UUID primary, `.modelwarden/root-id` marker fallback —
  re-adding a remounted drive keeps its id. `warden roots add/list`,
  `warden where` (searches the catalog incl. offline locations, with live
  root-presence checks so an unplug shows OFFLINE immediately). refresh()
  skips offline roots instead of clobbering their manifests. New config:
  `discover_stores: false` limits warden to scan_dirs + registered roots.
  GUI: Storage Roots dialog (list + register), catalog-only offline entries
  greyed in the inventory with their drive label. 31 tests green.

- **M4** — verified backup. `warden backup <path>` copies every hashed
  content to a target (auto-registered as a root) in a human-readable
  layout; a copy only counts after three-way verification (catalog hash =
  source-read hash = destination read-back hash), via .partial temp names.
  Target carries its own `.modelwarden/manifest.json` so the drive stays
  self-describing unplugged. `warden verify <path|id>` re-hashes a root
  against its manifest (bit-rot detection proven in test), updating
  `verified_unix`. Status answers "is this model safe?": N of M contents
  have a copy on a registered drive. Dup accounting fixed: cross-device
  copies (backups) are intentional redundancy, never "reclaimable" —
  hardlinks can't cross filesystems. 35 tests green.

## Later milestones (deliverables in PLAN.md)

- **M5** — archival + owned-root hardlink reclaim (CLI); backup reaches GUI.
- **M6** — GUI write parity, disk-usage view (CLI `warden report` too),
  publish inventory schema v1.
- **M7** — acquisition (`warden fetch`, HF downloads into the shelf).

## Smaller items (fold in opportunistically)

- `--json` output on every read command from the moment it exists (scan: done).
- Activity log lines mirror between CLI verbose mode and GUI panel.
- Single-instance lock file before the first write operation lands (M4) so
  two wardens can't race a backup/archive.
- Ollama projector layers: manifests can carry `image.projector` blobs
  alongside `image.model`; inventory them like HF mmproj files.
- Track mmproj ↔ parent-model companionship so archival/backup of a vision
  model brings its projector along.
- `warden backup --repair`: re-copy backup files that failed verify (today a
  corrupted target copy is reported but refuses overwrite; the user deletes
  it manually first).

## Sibling project: llamacppCodeConf (`~/src2/llamacppCodeConf`)

Serving (llama-server router), measurement, and opencode.json live there.
Boundary: **warden owns storage truth and acquisition; the sibling owns serving
+ OpenCode.** It will read warden's merged inventory (schema v1, M6) but never
manages storage; warden never serves or edits configs. Keep the seam crisp.

## Parked / ideas

- Hardlink dedup inside foreign stores (Ollama/HF) — decided report-only,
  revisit only with strong evidence it's safe.
- rsync/restic-style backup backends (current design: plain verified copies).
- Scheduled scrub: periodic re-verify of backups by hash (manual `warden
  verify` lands in M4).
- Model-family grouping heuristics beyond name prefixes (use GGUF metadata).
- Non-GGUF model files (safetensors in the HF cache, etc.): the hub cache
  holds them too. Inventory/backup could cover any large file; needs a scope
  decision from the user before widening past GGUF.

# modelwarden — project handoff

*Seeded 2026-08-16 from the llamacppCodeConf sessions. This document is the
context a fresh session needs to plan and build this tool. Nothing here is
implemented yet.*

## Name

**modelwarden** — a warden keeps the inventory, guards the collection, and
always knows where everything is and whether it's safe. Runner-ups
considered: Hoard, Curator, Modelarium, modelshelf.

## Mission

Local LLM model files are scattered across stores owned by different tools
with different lifecycle rules — Ollama's blob store (`ollama rm` deletes),
the HuggingFace hub cache (revision pointers move, caches get pruned),
llama.cpp's download cache, unsloth studio's pulls, and hand-downloaded
files. The owner of 200GB+ of GGUFs cannot today answer: *what do I have,
where is it, is it reachable, is it backed up, and which of these are the
same bytes?*

modelwarden is the answer: **inventory, backup, and archival for local model
files.** It owns storage truth. It never serves models and never edits
inference-tool configs.

## The boundary contract (learned the hard way)

The user's earlier projects (llm_forge / opencode_configuration_tool) split
storage-side vs config-side and ended up merged because the seam was fuzzy.
This split only works if the boundary is crisp:

- **modelwarden owns**: what exists, where, content identity, provenance,
  backup state, archival tiers, acquisition (downloads).
- **llamacppCodeConf owns**: serving (llama-server router), measurement,
  opencode.json. It *reads* the same stores (and eventually warden's
  manifest) but never manages storage.
- Neither tool reaches across. The meeting point is the filesystem and,
  later, warden's inventory manifest (read-only to consumers).

## Real incidents this tool exists to prevent (all from one session)

1. A 20GB model sat unknown in the HF cache until another tool surfaced it.
2. A downloaded quant became *inaccessible* because HF's `refs/main` moved
   to a different snapshot revision — file on disk, invisible to tooling.
3. An 18GB duplicate existed (manual download + cache copy of the same
   quant) and nothing noticed until serving aliases collided.

## Feature pillars

1. **Inventory by content, not path.** SHA-256 identity for every GGUF
   across every store + arbitrary roots (NAS, removable drives). Catches
   cross-store duplicates that inode comparison cannot. Per model:
   provenance (which tool fetched it, when), locations (plural), and
   accessibility (present / offline-media / pruned).
2. **Backups with verification.** Copy to targets, verify by hash, record
   what's backed up where and when. "Is this model safe?" gets an answer.
3. **Archival tiers.** Promote to owned storage (llamacppCodeConf already
   has a per-file "archive to shelf" — warden generalizes it), demote to
   cold storage with a manifest, so offline disks stay queryable.
4. **Dedup & reclaim.** Hash-identical copies → hardlink (same fs) or
   report; disk-usage view grouped by model family.
5. **Acquisition.** Download from HuggingFace by repo (this deliberately
   pulls roadmap item #7 out of llamacppCodeConf — downloads are
   storage-side).

## Harvest map (proven code, take it)

From `~/src2/llamacppCodeConf`:
- `src/core/gguf.rs` — GGUF header reader (arch, ctx, quant), tested.
- `src/core/library.rs` — scanner for shelf dirs, Ollama stores (manifest →
  blob mapping, both user and system locations), HF hub cache (snapshot
  layout, mmproj exclusion), inode dedupe, `archive_to_shelf`
  (hardlink-or-copy with temp-name safety).
- `src/core/settings.rs` — config pattern (defaults + partial-file
  tolerance).
- GUI shell pattern from `src/ui.rs` if a GUI is wanted (egui 0.36,
  worker-thread + mpsc, menu bar) — the user prefers traditional desktop
  apps with menus (see their memory notes).

## Spikes to run before building

1. **Hashing 200GB**: full SHA-256 of every file is minutes of I/O. Design
   incremental identity: (size, mtime, dev, ino) as cheap fingerprint,
   full hash lazily / on first sight / in background; store both.
2. **Manifest format**: single JSON? sqlite? Must survive offline media
   (manifest describes drives that aren't mounted). Consider one manifest
   per storage root + a merged view.
3. **HF hub semantics**: enumerate ALL snapshots (not just refs/main), map
   blobs↔snapshots links, detect orphaned blobs; confirm safe read-only
   behavior alongside hf CLI.
4. **Removable media**: how to identify a drive stably (filesystem UUID via
   /dev/disk/by-uuid) so "on the drive labeled X" survives remounts.

## Working conventions that served the sibling project well

Rust; core/ui split (core headless + testable); CLAUDE.md + PLAN.md +
ROADMAP.md from day one; spike before building on assumptions; milestones
that each leave something runnable; "measured, not guessed"; never destroy
user data (comment-out / refuse-overwrite / backup-before-write).

## Status

Empty repo. Next session: read this, write PLAN.md (mission → decisions →
spikes → milestones), run the spikes, build M1 (read-only inventory — the
scanner harvest + hashing), and only then consider backup/archival writes.

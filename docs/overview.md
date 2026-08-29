# modelwarden — project overview

*A one-page introduction to the project: what it is, the problems it solves,
how it's built, and what it deliberately doesn't do.*

## What it is

modelwarden is a desktop tool (Rust; CLI + native GUI) that manages the
storage side of running large language models locally: **inventory, backup,
and archival for model files**. On a machine used for local AI work, model
files — often 10–20 GB each — accumulate across several directories owned by
different tools with different lifecycle rules: Ollama's blob store (`ollama
rm` deletes), the HuggingFace hub cache (revision pointers move, caches get
pruned), and hand-downloaded files. modelwarden gives that collection a
single source of truth: what exists, where it is, whether it's reachable,
whether it's backed up, and which files are actually the same bytes.

It is a single-user desktop tool (Linux; macOS in beta) built for a real
machine with ~280 GB of models — not a server product. That scope is
deliberate. It ships as GitHub Release binaries and on crates.io
(`cargo install modelwarden`), dual-licensed MIT/Apache-2.0.

## The problems it addresses

All three of these happened on one real machine before the tool existed:

1. **Invisible storage.** A 20 GB model sat unknown in the HuggingFace cache
   — downloaded once, forgotten, discoverable only by accident.
2. **Silent inaccessibility.** A downloaded model became unreachable because
   the cache's revision pointer moved: the bytes were on disk, but no tool
   could see them.
3. **Undetected duplication.** The same 16.7 GB file existed twice (a manual
   download plus a cache copy) and nothing noticed, because the copies had
   different paths and different inodes — only their *contents* matched.

The common thread: **path is a bad identity for large files.** modelwarden's
core decision is that identity is the SHA-256 of the content, computed once
and cached against a cheap change-detecting fingerprint. Everything else —
duplicate detection, verified backup, "is this model safe?" — follows from
that.

## What it does

- **Catalog** every model across all stores, by content. Survives unplugged
  drives: an offline disk's contents stay queryable ("it's on the drive
  labeled archive-2").
- **Verified backup**: a copy only counts after three hashes agree — the
  catalog's, the source as read, and the destination read back after
  writing. `verify` re-checks a drive later, catching bit-rot.
- **Deduplication** by content, reclaiming space via hardlinks — only after
  re-hashing both files at reclaim time, only within one filesystem, never
  inside another tool's store.
- **Tiering**: promote cache-owned models to user-owned storage; demote to
  cold storage as a verified move (the copy is proven before the original
  can be removed, and only on an explicit flag).
- **Bundle awareness**: operations move *everything a model needs to run* —
  multi-part split files and vision-projector companions travel together,
  never fragments.
- **Store health**: detect dangling references, pruned husks, orphaned
  blobs, and interrupted downloads; each finding comes with a plain-language
  explanation, a statement of what fixing it would lose, and a remedy that
  routes through the owning tool's own CLI.
- **Acquisition**: resumable HuggingFace downloads with provenance (repo,
  revision, when) recorded at the only moment it's knowable.

## Engineering approach — and results on real data

- **Spike before building.** Four throwaway experiments against the real
  stores preceded the design: measuring full-hash throughput (~680 MB/s —
  proving lazy full hashing viable with no partial-hash tier), validating
  the manifest format against offline media, auditing real HuggingFace cache
  semantics, and testing removable-drive identity. Verdicts are recorded in
  `docs/spikes.md`; several assumptions died there instead of in code.
- **Safety as invariants, not intentions.** Never write inside a store
  another tool owns; destroy bytes only through the explicit two-stage
  trash (delete → restorable trash → `empty --yes`), plus one earlier
  documented exception: a
  verified move, on an explicit flag); every copy goes through a temp name
  and is re-read before it counts; every write operation takes a
  single-instance lock. These are written down in `PLAN.md` and enforced in
  one shared code path each.
- **Tested at two levels.** 56 unit tests over a headless core (the GUI and
  CLI are thin layers over the same library), plus end-to-end lifecycle
  tests in isolated environments: register → catalog → unplug → query →
  replug; backup → corrupt → detect.
- **Real use found a real bug — and the process worked.** The first
  real-data deduplication run exposed that Rust's `hard_link` doesn't follow
  symlinks, leaving one path dangling (no bytes lost; the operation's
  verification followed symlinks while the link step didn't). The fix came
  with a regression test that reproduces the exact on-disk shape and fails
  on the old code. That incident is documented, not hidden — it's the
  strongest argument in the repo for why the tool verifies everything.
- **Measured outcomes on the real machine**: a 16.7 GB duplicate found by
  content hashing that inode comparison could not see, and reclaimed; 2.5 GB
  of dead weight identified in the HF cache with one-command cleanup; a
  selective 47.8 GB verified backup to an external drive.

## Design decisions a reviewer might ask about

- **JSON manifests, not a database.** Per-root JSON files plus a merged
  view: entry counts are in the dozens, files are human-diffable, drives
  carry their own manifest (self-describing when unplugged), and a sibling
  tool consumes the merged inventory as a versioned, read-only contract.
  SQLite was considered and rejected until scale demands it.
- **A deliberate boundary.** A companion project handles model *serving*;
  modelwarden handles *storage*. The two meet only at the filesystem and one
  published JSON schema — a lesson from an earlier project pair that merged
  because their seam was fuzzy.
- **Cleanup by delegation.** When the health check finds problems inside
  Ollama's or HuggingFace's directories, warden doesn't delete there — it
  invokes the owning tool's own CLI (`ollama rm`, `hf cache rm`) with the
  user's explicit confirmation. The invariant holds; the user still gets a
  one-click fix.

## Honest limitations

- **Linux-only** (Unix metadata, `/dev/disk/by-uuid`, hardlink semantics)
  and **GGUF-only** for now — the HF cache also holds safetensors, which
  warden deliberately ignores pending a scope decision.
- **Single machine, single user.** The catalog is local state; there is no
  sync, no server, no multi-user story — by design, but a real boundary.
- Scheduled integrity checks (a cron'd `verify`) are planned but not built;
  today re-verification is manual.
- HF tokens are stored in plain text when saved — the same trade-off the
  official `hf` CLI makes, but worth knowing.

## By the numbers

~7,400 lines of Rust (≈4,750 headless core, ≈2,700 CLI + GUI), 56 tests,
Dozens of commits over an intensive build, each ending in a runnable
milestone, with per-push CI on Linux and macOS.
Stack: Rust (edition 2024), egui/eframe for the GUI, `sha2`, `ureq`, and
deliberately little else. Design history in `PLAN.md`, running status in
`ROADMAP.md`, empirical groundwork in `docs/spikes.md`.

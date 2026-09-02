# CLAUDE.md

This file provides guidance to Claude Code when working with code in this
repository.

## What this is

**modelwarden** — inventory, backup, and archival for local LLM model files
(GGUFs across Ollama's blob store, the HF hub cache, llama.cpp's cache,
hand-downloads, NAS/removable drives). It owns storage truth: content identity,
provenance, locations, accessibility, backup state, archival tiers, and
acquisition. It never serves models and never edits inference-tool configs.
**Read PLAN.md before making design decisions**; track work in ROADMAP.md and
update it when milestones land.

## Commands

- `cargo build` / `cargo test` — core is headless and fully testable.
- `cargo build --no-default-features` — core + CLI without the GUI stack
  (the `gui` feature gates eframe/rfd; this is what library consumers
  such as modellab get with `default-features = false`). CI builds and
  tests both ways.
- `cargo run` — opens the GUI (`default-run = "warden-gui"`).
- `cargo run --bin warden -- <scan|hash|status|dups|doctor|roots|where|backup|verify|scrub|archive|restore|dedup|report|fetch|delete|trash|journal>` — CLI; `--json` on all read commands.
- Config: `~/.config/modelwarden/config.json` (records only what the user changed).
- State (manifests, merged inventory): `~/.local/state/modelwarden/`.

## Development methodology: test-first

As of 2026-08-29 (user decision), warden is built **test-first**:

- **Write the failing test before the implementation** — unit tests for
  core logic, integration/E2E tests (isolated env: XDG overrides +
  `discover_stores:false`, fresh dirs each run) for behavior that spans
  modules or binaries. Red → green → refactor.
- **Design for testability**: decisions live in `src/core/` where they
  can be unit-tested; the bins render and dispatch. If GUI/CLI logic is
  hard to test, that's the signal to move the decision into core (this
  is how log_line, bundle_union, companion_parents, deletable_set came
  to exist — the pattern is proven here).
- **A bug fix starts with a regression test** that reproduces it and
  fails on the old code (see the dedup-symlink and split-delete
  inversion fixes for the standard).
- Tests assert *behavior and invariants* (bytes never lost, bundles move
  whole, refuse-overwrite), not implementation details.

## Architecture (big picture)

Single crate, strict core/ui split: `src/core/` is GUI-free and testable;
`src/bin/warden.rs` (CLI) and `src/bin/warden-gui/` (egui 0.36, traditional
menus) render over it and must never be dependencies of it.

Core modules: `gguf` (header reader — `read_meta` for the inventory's
fields, `read_fields` for any key typed as stored; the family's one GGUF
parser, modellab reads through it), `scan` (store scanners + inode dedupe),
`identity` (fingerprint + SHA-256 worker), `lock` (single-instance write
lock), `manifest` (per-root JSON + merged view + bundles), `roots`
(storage-root registry, removable-media identity), `backup`, `archive`
(promote/demote/restore), `dedup`, `doctor` (health + remedies), `acquire`
(HF downloads), `settings`.

Non-obvious constraints that shape the code:

- **SHA-256 is the only identity.** The `(size, mtime, dev, ino)` fingerprint
  only detects change; it is never identity across stores.
- **Never write inside a store another tool owns** (Ollama, HF cache,
  llama.cpp cache). Their manifests live under warden's state dir; dedup there
  is report-only, always. Doctor cleanup routes through the owning tool's own
  CLI (`hf cache rm`, `ollama rm`) on explicit user action, and owner-command
  success is verified (`expect_gone`), never trusted — hf exits 0 without
  acting on repos its scanner can't see. Two guarded direct-delete
  exceptions: `*.incomplete` download debris, and pruned husk directories
  (refs-only, re-verified to hold zero content bytes at apply time — the
  owner CLI provably cannot remove them).
- **Operations move bundles, not files.** Backup/archive/demote/restore carry
  everything a model needs to run: split `-NNNNN-of-NNNNN` parts, mmproj
  projectors beside the model, Ollama `+projector` blobs (`bundle_for`).
- **Bytes are destroyed only by `trash empty`.** Deletion is two-stage
  (`src/core/trash.rs`): `delete` renames a bundle into
  `<root>/.modelwarden/trash/` (restorable, nothing destroyed; companions
  another model needs are auto-kept; foreign copies get owner commands
  printed, never run); only the explicit `trash empty --yes` / GUI
  Empty-Trash confirm destroys bytes. Space reclaim otherwise is
  hardlinking, after hash-verifying both files, via temp-link+rename.
- **All copies go .partial → verify hash → rename.** A half-copy must never be
  scannable as a model. Refuse-overwrite everywhere.
- **A missing root is Offline, not gone.** Never drop offline entries from
  manifests; unplugged drives stay queryable. The one sanctioned
  exception: `roots forget` — the user explicitly declaring a drive truly
  gone (dead, reformatted), previewed with an impact statement and
  confirmed with --yes / a GUI dialog.
- **The merged inventory is a published read-only contract** (schema_version;
  llamacppCodeConf reads it from M6). Schema changes need versioning.
- **Spike before building on assumptions**; verdicts go in PLAN.md and
  `docs/spikes.md`. Spikes run read-only against the real stores.

## Environment facts (dev machine)

- Shelf: `~/models`. Ollama: `~/.ollama/models` (+ possible system store at
  `/usr/share/ollama/.ollama/models`, may be unreadable — degrade gracefully).
  HF cache: `~/.cache/huggingface/hub`. All populated; ~200GB total.
- Harvest source: `~/src2/llamacppCodeConf` (sibling project, serving-side —
  don't rewrite what it proved). `src/core/gguf.rs` verbatim;
  `src/core/library.rs` minus serving-side methods; `src/core/settings.rs`
  minus `overrides`; `src/ui.rs` as a pattern only. Pin `eframe = "=0.36.1"`
  so the harvested GUI shell compiles unchanged.
- Boundary contract with llamacppCodeConf is in PLAN.md — the seam must stay
  crisp; neither tool reaches across.
- **The inventory.json integration (ROADMAP next-candidate #1) is owned by
  the Claude instance working in `~/src2/llamacppCodeConf`** — it will pick
  the work up after its in-flight updates land. Do NOT start that
  integration from this repo; warden's side of the contract
  (docs/inventory-schema.md, schema v1) is published and frozen.

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
- `cargo run` — opens the GUI (`default-run = "warden-gui"`).
- `cargo run --bin warden -- <scan|hash|status|dups|roots|where|backup|archive|dedup|fetch>` — CLI.
- Config: `~/.config/modelwarden/config.json` (records only what the user changed).
- State (manifests, merged inventory): `~/.local/state/modelwarden/`.

## Architecture (big picture)

Single crate, strict core/ui split: `src/core/` is GUI-free and testable;
`src/bin/warden.rs` (CLI) and `src/bin/warden-gui/` (egui 0.36, traditional
menus) render over it and must never be dependencies of it.

Core modules: `gguf` (header reader), `scan` (store scanners + inode dedupe),
`identity` (fingerprint + SHA-256 worker), `manifest` (per-root JSON + merged
view), `roots` (storage-root registry, removable-media identity), `backup`,
`archive`, `dedup`, `acquire` (HF downloads, M7), `settings`.

Non-obvious constraints that shape the code:

- **SHA-256 is the only identity.** The `(size, mtime, dev, ino)` fingerprint
  only detects change; it is never identity across stores.
- **Never write inside a store another tool owns** (Ollama, HF cache,
  llama.cpp cache). Their manifests live under warden's state dir; dedup there
  is report-only, always.
- **Never delete model bytes.** The only reclaim is hardlinking, after
  hash-verifying both files, via temp-link+rename.
- **All copies go .partial → verify hash → rename.** A half-copy must never be
  scannable as a model. Refuse-overwrite everywhere.
- **A missing root is Offline, not gone.** Never drop offline entries from
  manifests; unplugged drives stay queryable.
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

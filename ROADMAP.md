# modelwarden — Roadmap

Living tracker. Design authority is PLAN.md; this file records status. North
star: **always able to answer what do I have, where, is it reachable, is it
backed up, which are the same bytes — without ever losing bytes.**

## Done

*(nothing landed yet)*

## M0 — plan + scaffold + spikes (IN PROGRESS)

1. ✔ **Convention files**: PLAN.md (north star, decisions, architecture,
   spikes, milestones), CLAUDE.md, ROADMAP.md.
2. ✔ **Scaffold**: compiling crate with core module stubs, `warden` CLI stub,
   `warden-gui` window with menu bar; `cargo test` green.
3. **Spike 1 — hashing 200GB**: time full SHA-256 over the real stores,
   cold/warm; check mtime stability. Verdict → PLAN.md.
4. **Spike 2 — manifest format**: serialize a real scan to per-root JSON;
   simulate an offline drive in the merged view.
5. **Spike 3 — HF hub semantics**: enumerate ALL snapshots, map blobs↔links,
   detect orphans and moved refs; confirm read-only coexistence with hf CLI.
6. **Spike 4 — removable media**: fs-UUID stability across remount; marker-file
   fallback rules.

## Next milestones (deliverables in PLAN.md)

- **M1** — inventory skeleton, both frontends (harvest gguf/scan/settings;
  `warden scan`; read-only GUI Inventory tab).
- **M2** — content identity + manifests (`warden hash/status/dups`).
- **M3** — roots + offline media (`warden roots`, `warden where`).
- **M4** — backup, CLI first.
- **M5** — archival + owned-root hardlink reclaim (CLI); backup reaches GUI.
- **M6** — GUI write parity, disk-usage view, publish inventory schema v1.
- **M7** — acquisition (`warden fetch`, HF downloads into the shelf).

## Smaller items (fold in opportunistically)

- `--json` output on every read command from the moment it exists.
- Activity log lines mirror between CLI verbose mode and GUI panel.

## Sibling project: llamacppCodeConf (`~/src2/llamacppCodeConf`)

Serving (llama-server router), measurement, and opencode.json live there.
Boundary: **warden owns storage truth and acquisition; the sibling owns serving
+ OpenCode.** It will read warden's merged inventory (schema v1, M6) but never
manages storage; warden never serves or edits configs. Keep the seam crisp.

## Parked / ideas

- Hardlink dedup inside foreign stores (Ollama/HF) — decided report-only,
  revisit only with strong evidence it's safe.
- rsync/restic-style backup backends (current design: plain verified copies).
- Scheduled scrub: periodic re-verify of backups by hash.
- Model-family grouping heuristics beyond name prefixes (use GGUF metadata).

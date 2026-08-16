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

## M2 — content identity + manifests (NEXT)

1. **Background SHA-256 worker**: fingerprint-gated (rehash only on
   fingerprint mismatch), per-file progress (a 22 GiB file takes ~35s —
   spinner isn't enough), throttle option.
2. **Per-root manifests + merged inventory** persisted to
   `~/.local/state/modelwarden/` (schema proven in spike 2), atomic writes.
3. **`warden hash` / `warden status` / `warden dups`** (report-only; dups
   groups by sha256 and shows reclaimable bytes — the 16.7 GiB Qwen3.8 dup is
   the acceptance test).
4. **`warden doctor`** (spike 3 fallout): store-health report — dangling HF
   refs, pruned husk repos, orphan blobs, `.incomplete` downloads, Ollama
   manifests naming missing blobs. Read-only.
5. GUI: hash progress in status bar, duplicate-groups view, doctor findings.
## Later milestones (deliverables in PLAN.md)

- **M3** — roots + offline media (`warden roots`, `warden where`).
- **M4** — backup, CLI first; plus `warden verify` (re-hash a backup target
  on demand — the manual form of the parked scheduled scrub).
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

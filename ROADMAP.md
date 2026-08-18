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

- **M5** — archival + reclaim. `warden archive <query>` promotes a
  cache-owned model to the shelf (hardlink same-fs, verified copy across);
  `warden archive demote <query> --to <root>` is a verified copy to cold
  storage that records into the drive's carried manifest — the shelf copy is
  deleted only with an explicit `--remove-source`, and only after the cold
  copy's read-back hash matched (a verified move never loses bytes).
  `warden dedup` dry-runs by default; `--hardlink` re-verifies both sides
  against the bytes as they are NOW, then collapses same-fs owned-root
  copies via temp-link + rename. Queries accept sha256 prefixes (two
  contents can share a name). refresh() now merges a drive's carried
  manifest into carry-forward, so drives written by backup/demote (or
  another machine) re-catalog with hashes intact. GUI: Tools → Back up…
  dialog with live progress. Real dry run: 16.7 GiB reclaimable. 41 tests.

- **M6** — GUI write parity + published schema. The GUI is catalog-driven
  now: Inventory rows carry Archive / Demote… actions (demote dialog with
  target-drive dropdown and an explicit remove-source checkbox), Duplicates
  gained a confirm-dialog Reclaim flow, and a Usage pane joins the tabs.
  CLI `warden report` groups disk usage by GGUF-architecture family with
  unique-vs-on-disk overhead. schema_version bumped to 1 and published in
  docs/inventory-schema.md as the read-only consumer contract
  (llamacppCodeConf reads inventory.json). README.md written. 41 tests.

- **M7** — acquisition. `warden fetch <org/repo> [pattern]` lists a repo's
  GGUFs (sizes included) and downloads a uniquely-matched file into the
  shelf: streaming to `.partial` with Range resume (a 200-to-Range restart
  is handled), refuse-overwrite, then hash + provenance (repo, revision
  from `x-repo-commit` or the API's sha, etag, when) recorded by content
  identity in `state/provenance.json`. GUI: Tools → Download from
  HuggingFace… (list + per-file download with live progress). Verified with
  real downloads. 43 tests green.

**All seven planned milestones are complete.** What remains lives below.
- **M8** — the three gaps a real-world day would hit. `warden restore
  <query>`: the return leg of backup/demote — verified copy from a drive
  back to the shelf, drive never modified, offline drives named in the
  refusal. Split-GGUF downloads: `fetch` expands any part to its full
  `-NNNNN-of-NNNNN` set (refusing sets with holes), skips already-present
  parts, and supports gated repos (--token / $HF_TOKEN / hf CLI login).
  Single-instance write lock: pid file under the state dir guards every
  write command in both frontends; live holders block with a clear message,
  stale locks from crashed runs are stolen. 49 tests green.
- **M8.1** — token management + 401 diagnosis (user-found: GUI listing hit
  a bare 401). HF answers 401 for unknown ids too, so the error now
  distinguishes: close-match "did you mean" suggestions from the search API
  for mistyped ids, token guidance otherwise. Tokens: GUI masked field with
  Remember-to-config, CLI --token --save-token, config `hf_token`, then
  env/hf-login fallbacks.

- **M9** — bundles, selective backup, native browse; dedup symlink fix.
  `bundle_for()` defines "everything the model needs to run": split
  `-NNNNN-of-NNNNN` siblings, `mmproj` vision projectors kept beside the
  model (same HF snapshot / same dir), and Ollama projector blobs (now
  inventoried, tied by `+projector` name). Backup, archive, demote, and
  restore all move whole bundles — never fragments; no tar needed, plain
  files keep the layout human-rescuable. `warden backup <path> [query…]`
  backs up selected models; GUI Back Up dialog gains Browse… (native
  picker via rfd), a model filter with live bundle/size preview, and a
  per-row Back up… action. Critical fix from the user's real run:
  dedup's relink hardlinked an HF snapshot SYMLINK instead of the bytes
  (hard_link doesn't follow symlinks), leaving a dangling shelf path —
  relink now canonicalizes the survivor; regression test proven to catch
  it; the damaged path was repaired in place. 53 tests green.

- **M10** — doctor remedies + owner-mediated cleanup (user-requested).
  Every finding now carries an explanation (what this is), a loss statement
  (what fixing costs), and a remedy: an owner-tool command (`hf cache rm
  <repo> -y`, `ollama rm <name:tag>`) that warden executes on explicit
  request, `*.incomplete` debris warden removes itself (the one narrow
  exception — guarded against active downloads and non-debris paths), or
  the exact manual command for what must stay human (orphan blobs = real
  bytes). CLI: `warden doctor --fix`; GUI: per-finding Clean up… button
  with a confirm dialog showing the command, what it means, and what it
  loses. Owner CLIs detected on PATH; manual fallbacks when absent.
  56 tests green.

## Next candidates (pick up here)

1. **llamacppCodeConf reads `inventory.json`** — the schema-v1 payoff; work
   happens in the sibling repo (`~/src2/llamacppCodeConf`), not here. The
   serving tool gets storage truth (incl. offline-drive locations) without
   scanning anything itself.
2. **Scheduled scrub** — a systemd timer running `warden verify` against the
   backup drive (exit 1 on mismatch makes it alert-friendly). Makes sense now
   that a real backup exists on "Archive 2".
3. **`warden backup --repair`** — re-copy backup files that failed verify
   (today a corrupted target copy is reported but refuses overwrite; the
   user deletes it manually first).
4. **Provenance surfacing** — fetch records repo/revision/etag by content
   hash in `state/provenance.json`; `where`/`status` don't display it yet.

## Smaller items (fold in opportunistically)

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
- Scheduled scrub: periodic re-verify of backups by hash (manual `warden
  verify` lands in M4).
- Model-family grouping heuristics beyond name prefixes (use GGUF metadata).
- Non-GGUF model files (safetensors in the HF cache, etc.): the hub cache
  holds them too. Inventory/backup could cover any large file; needs a scope
  decision from the user before widening past GGUF.

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

- **M11** — operations hardening: scrub, repair, provenance. `warden
  verify --all` covers every online owned root (offline drives noted, not
  failed); `--repair` re-copies mismatched/missing files from a live source
  elsewhere in the catalog — the corrupt copy is never deleted first, the
  verified replacement lands by atomic rename, and content with no live
  source anywhere is reported unrepairable, never silently dropped.
  `warden scrub install [--daily|--weekly|--monthly]` writes systemd user
  units running `hash && verify --all` (hash-first means legitimate edits
  re-hash and pass; bytes changed under an unchanged fingerprint — the
  bit-rot signature — fail the unit); units are written, never enabled —
  the enable command is printed. Provenance (repo/file@revision, fetch
  date) now shows in `warden where` and the GUI row tooltip. Proven
  end-to-end: corrupt → verify exit 1 → --repair → verify exit 0.
  58 tests green.

- **M12** — beyond GGUF (user decision: whole model dirs, all roots, fetch
  later). A weights file (`.safetensors/.bin/.pt/.pth/.onnx`) makes its
  whole container a model: HF snapshots with weights are cataloged whole
  (tokenizer/configs included, subdirs too); a shelf/drive directory holding
  weights is cataloged as one subtree. bundle_for: a weights file bundles
  everything under its container (matching the scanner's rule; companions
  alone stay alone, like mmproj). Backup/promote now preserve snapshot
  subpaths (two config.json in different subdirs no longer collide).
  Weights meta from the adjacent config.json (model_type → architecture,
  context window). GGUF-only snapshots don't drag READMEs in. Real machine:
  unsloth/bge-small-en-v1.5 (127 MiB + 10 companions) cataloged; E2E
  round-trip backup → demote --remove-source → restore, layout intact,
  proven. 63 tests green.

- **M13** — snapshot fetch (closes M12's deferred acquisition gap). `warden
  fetch <org/repo> --snapshot` downloads a repo's whole snapshot into one
  shelf directory — the M12 rule (weights make the directory the model)
  applied to acquisition: all files land together, dotfiles excluded
  (mirroring the scanner), each streamed via .partial + Range resume, hashed,
  provenance recorded. A GGUF-less repo now lists its files with sizes, a
  total, and the --snapshot hint instead of erroring; the GUI's HF dialog
  detects the same case and offers one "Download whole snapshot (N files,
  size)" button in place of per-file downloads. Extension-less files (LICENSE)
  now get a plain `.partial`, not a fake `.gguf.partial`. Proven live:
  prajjwal1/bert-tiny (4 files) fetched, cataloged whole, and a selective
  backup of just the weights pulled the entire bundle. 65 tests green.

- **M13.1** — activity-log mirror (the last backlog item). Event phrasing
  now lives in core (`log_line()` on RefreshEvent/BackupEvent/ReclaimEvent/
  FetchEvent, `core::format::human_size` shared by both bins): the CLI
  prints and the GUI activity panel logs the same words for the same event.
  The GUI's durable gap is closed — per-file "hashed/verified/relinked/
  downloading" lines now land in the activity panel (new `Msg::Activity`)
  instead of vanishing with the transient status bar; progress ticks stay
  status-bar-only in both frontends. 69 tests green.

- **M13.2** — doctor honesty fix (found in the field: `doctor --fix`
  "fixed" the same 4 pruned husks forever). Root cause: `hf cache rm`
  exits 0 saying "Nothing to delete" for snapshot-less repos — hf's own
  scanner cannot see husks, so the owner CLI provably cannot remove them,
  and apply() trusted the exit code. Now: owner-command success is
  verified (`expect_gone` — the target must actually be gone, or --fix
  reports failure with the tool's own output), and husks are warden's
  second guarded direct-delete exception (rationale as `*.incomplete`:
  no owner command can act) — re-verified at apply time to hold zero
  content bytes, refusal deletes nothing. Real machine: all 4 husks
  removed, doctor reports all stores healthy. 71 tests green.

- **M13.3** — release-readiness: the scrub-enable prompt (user question:
  "how will a GH-release user know to run the systemctl enable?"). Three
  legs: README gains a Setting up section (scan → hash → scrub install
  --enable → doctor); `warden scrub install --enable` collapses install+
  start into one consented step; and doctor now carries a machine-level
  advisory — `scrub timer off` — that keeps nagging (CLI and GUI Health
  pane) until the timer runs: not-installed hands over the one-liner,
  installed-but-disabled is --fix/button-executable (systemctl --user
  enable --now). Non-systemd machines stay quiet. 73 tests green.

- **M14 — release pipeline. DONE 2026-08-25**: repo pushed to GitHub
  (unclebucklarson/model-warden), CI workflow (build + test --locked on
  every push/PR, cargo-cached, green in ~2min), and a tag-triggered
  release workflow: `git tag vX.Y.Z && git push origin vX.Y.Z` tests,
  builds --release, and publishes a GitHub Release with a versioned
  x86_64-linux tarball (both binaries, licenses, README, User's Guide)
  plus sha256. **v0.1.0 is published**; the artifact was downloaded,
  checksum-verified, and run as a user would. Earlier chores below. Chores landed first (user
  decision: permissive license, "give back to the community"): dual
  **MIT OR Apache-2.0** (Rust convention; both texts in-repo, README
  License section with the standard contribution clause), Cargo.toml
  package metadata (description/license/authors), `warden --version` /
  `-V`, GUI title carries the version, and the fix-dialog now states who
  acts per remedy kind (`Remedy::actor_line()` — owner tool vs. warden
  itself vs. you). **Still to build: CI workflow (build+test) and the
  tag-triggered release workflow (GH Releases with packaged binaries);
  needs the repo pushed to GitHub — public/private is the user's call.**
  Also landed pre-release (user request, 2026-08-25): **docs/users-guide.md**
  — a comprehensive novice-facing guide (concepts with reasons, GUI tour,
  CLI reference, recipes, findings glossary, FAQ, glossary), linked from
  the README. Writing it surfaced a real gap, fixed alongside: `verify`
  and `demote --to` now accept a drive's registration **label**
  ("Archive 2") as well as id/path — labels existed but nothing accepted
  them. Proven E2E.

- **Post-release polish (2026-08-25, user-driven).** Storefront: CI /
  release / license badges in the README; GitHub repo description and
  topics set. GUI: Inventory columns are sortable — click a header to
  sort, again to reverse (default: name, ascending; previously rows sat
  in hash order). Fetch closes the last bundle gap (user question re
  vision models like Qwen3.8-Ridge + its mmproj): downloading a model now
  pulls the repo's `mmproj` projector along automatically — same
  asymmetric rule as the catalog's bundle_for (projector rides along,
  choosing the projector alone stays alone). Backup/archive/demote/
  restore already bundled projectors since M9. 74 tests green.

- **Inventory as models, not files (2026-08-25, user proposal) + crates.io
  prep.** GUI Inventory: filter box (name/quant/location/hash, matching a
  companion keeps its whole group visible) and companion grouping —
  contents that ride in another model's bundle while their own bundle
  stays alone (mmproj projectors, Ollama +projector blobs, safetensors
  tokenizer/config companions) render indented under the model that needs
  them with a "required by <models>" note; bundle_for is the single source
  of truth, no new relation invented. crates.io: Cargo.toml carries
  repository/readme/keywords/categories; `cargo publish --dry-run`
  validates. Publishing awaits the user's `cargo login` token; after
  first publish, add CARGO_REGISTRY_TOKEN to the release workflow.

- **Cold-storage workflow (2026-08-25, user refinement).** Collapsible
  companions (▸/+N toggle, collapsed by default; filter forces groups
  open). Bulk demote: CLI `archive demote <query…>` takes many queries;
  GUI **Tools → Move to Cold Storage…** — checkbox list with filter,
  target dropdown (any registered root: drive, NAS, or fixed dir),
  remove-after-verify checkbox, bundle-inclusive size total; shared
  companions move once (bundle union, deduped). Inventory **Active / All**
  toggle: Active (default) hides models whose every copy is on a
  registered cold root, with a "(N in cold storage hidden)" hint — cold
  models leave the view, never the catalog. Proven E2E (bulk demote of 2
  of 3 models, verified moves). 74 tests green. **Open discussion:
  deletes — a way to totally remove models; touches the never-delete
  invariant, options under discussion with the user.** → became M15, below.

- **M15 — two-stage deletion (2026-08-25; user decisions: Option B trash
  with GUI parity / foreign stores offered-never-executed / shared
  companions auto-kept / delete = everywhere in owned roots).** The
  never-delete invariant's one sanctioned amendment, recorded in PLAN.md
  and CLAUDE.md: `warden delete <query…>` renames bundles into
  `<root>/.modelwarden/trash/` (instant, free, restorable, invisible to
  scans); `warden trash [list|restore <q>|empty --yes]` completes the
  cycle — `empty` is warden's only irreversible act and requires --yes
  (CLI) / a count-and-size confirm (GUI). Companions still required by a
  surviving model are auto-kept via bundle asymmetry; foreign-store
  copies yield owner commands (printed CLI / copyable in the GUI dialog),
  never executed. GUI: Delete… row action with full preview, and a Trash
  tab (list, per-file Restore, Empty Trash…). No trash index — the
  filesystem is the record, human-rescuable with a file manager. No
  auto-empty, ever. Proven E2E: shared projector spared on single delete,
  taken when both users deleted; restore refuses overwrite; empty
  destroys only trash contents. 77 tests green.

- **Loss handling hardening (2026-08-26, user-driven — a real corrupt
  drive).** Three fixes from one afternoon of real use: automatic
  mid-download resume (Range retry from the .partial, zero-progress
  budget of 5; user's 20.7 GiB fetch had died on a drop); generated
  `hf cache rm` commands now use the typed id (`model/org/repo` — the
  bare form silently matches nothing on hf 1.26, exit 0); and
  `warden roots forget <id|label|path> --yes` + GUI Forget… button —
  un-register a truly-gone drive with an impact preview (N models here,
  M nowhere else leave the catalog), knowledge-only, no bytes touched.
  Delete dialog also now surfaces offline copies up front. Proven E2E.
  77 tests green.

## ⇒ PICK UP HERE (state as of 2026-08-26, v0.2.0)

**Everything through M15 + the use-in-anger wave is complete and
released**: v0.2.0 on GitHub Releases and crates.io (`cargo install
modelwarden`). 77 tests green, CI on every push, docs current. The
scrub timer is enabled on the dev machine. What remains:

1. **User's real-world task in progress:** the corrupt "Archive 2"
   drive — `warden roots forget "Archive 2" --yes`, reformat,
   re-register, re-run backups.
2. **Sibling integration** — llamacppCodeConf reads `inventory.json`
   (schema v1, frozen, docs/inventory-schema.md). **Owned by the Claude
   instance in `~/src2/llamacppCodeConf`.** Do NOT start from this repo.
3. **M16a — macOS beta: SHIPPED as v0.2.1 (2026-08-27).** Lock
   liveness via kill(pid,0); macos-latest permanently in CI (all 77
   tests green on the Mac runner); release builds Apple Silicon + Intel
   tarballs; **docs/qa-macos.md** is the requester's beta QA script.
   Awaiting their QA report — the last M16a checkbox. M16b (Windows)
   not started; plan in docs/portability.md.
4. Otherwise: **use-in-anger** — real usage keeps nominating the next
   work (it has produced every milestone since M13).

Release routine: bump Cargo.toml version → commit → `git tag vX.Y.Z &&
git push origin vX.Y.Z` (GH release builds itself) → `cargo publish`.

## M17 — usability & performance backlog (2026-08-29 assessment, user-approved)

Built test-first (see CLAUDE.md methodology). Priority order:

0. ~~**Integration test harness**~~ **DONE 2026-08-29**: `tests/e2e.rs`
   runs the real binary (`CARGO_BIN_EXE_warden` — never stale) in fully
   isolated envs; five whole-lifecycle proofs (hash carry-forward,
   backup→demote→restore by label, delete/trash/restore/empty with
   shared companions and the no-recatalog regression, roots-forget
   impact, dedup dry-run-then-hardlink) now run in cargo test and CI.
1. ~~**Backup-coverage visibility in the GUI**~~ **DONE 2026-08-29**
   (test-first): core is_backed_up/backup_coverage; status bar headline
   "N/M backed up to a drive"; sortable Backup column (✓/—).
2. ~~**GUI job queue**~~ **DONE 2026-08-29** (test-first): requests
   during a running job queue visibly and run in order; "(+N queued)" in
   the status bar. Retires "busy — ignored".
3. ~~**Parallel + checkpointed hashing**~~ **DONE 2026-08-29**
   (test-first: checkpoint-before-Done red test): worker pool (≤4, HDD-
   friendly cap), each finished file checkpointed to the state-dir
   manifest BEFORE its completion is reported — an interrupted first
   hash resumes via fingerprint carry. Measured: 2 GiB in 0.28 s at
   397% CPU (was single-core ~700 MB/s). CLI hash output switched to
   interleave-safe standalone lines.
4. ~~**Persistent operations journal**~~ **DONE 2026-08-29**
   (test-first): core `journal` module (append-only
   `<state>/journal.log`, plain text + readable UTC timestamps — cat
   works without warden); the GUI journals its whole durable activity
   stream via one hook, the CLI jots every result line; `warden journal
   [N|--all]` reads it back; E2E proves persistence across processes.
5. **Settings dialog** — shelf/scan_dirs editable in the GUI (last
   remaining hand-edit of config.json).
6. **Verification-freshness advisory** — doctor warns when a backup
   root's oldest `verified_unix` ages past a threshold ("Archive 2 last
   verified 94 days ago") — offline drives silently age out of trust.

## Smaller items (fold in opportunistically)

- (none — the activity-log mirror landed as M13.1)

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

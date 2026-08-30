# modelwarden — User's Guide

*For version 0.2.2+. Written for newcomers: every concept is explained, and
every feature comes with the reason it exists.*

---

## Table of contents

1. [What modelwarden is, and why you'd want it](#1-what-modelwarden-is-and-why-youd-want-it)
2. [The ideas behind it (read this once)](#2-the-ideas-behind-it-read-this-once)
3. [Installing and first run](#3-installing-and-first-run)
4. [A tour of the GUI](#4-a-tour-of-the-gui)
5. [The CLI, command by command](#5-the-cli-command-by-command)
6. [Everyday recipes](#6-everyday-recipes)
7. [Reading what warden tells you](#7-reading-what-warden-tells-you)
8. [Troubleshooting and FAQ](#8-troubleshooting-and-faq)
9. [Glossary](#9-glossary)

---

## 1. What modelwarden is, and why you'd want it

If you run large language models locally, you accumulate **model files** —
and they are enormous. A single quantized model is often 4–20 GiB; a
collection quickly grows past 200 GiB. Worse, those files end up scattered
across places you don't fully control:

- **Ollama** keeps models as anonymous blobs in `~/.ollama/models` —
  filenames are hashes, unreadable to humans.
- **Hugging Face tools** cache downloads in `~/.cache/huggingface/hub`, in a
  maze of `snapshots/`, `blobs/`, and symlinks — and cache-cleanup tools
  will happily **delete** things from it to save space.
- **llama.cpp** keeps its own cache.
- You download files by hand into a folder like `~/models`.
- You copy things to external drives and NAS shares, then forget what's
  where.

Three things go wrong with this, and each one is the reason a piece of
modelwarden exists:

1. **Silent loss.** A cache pruner deletes a 17 GiB download you meant to
   keep, because to the cache it was just old data. Warden's answer:
   a **shelf** you own that no pruner touches, **verified backups**, and a
   **doctor** that spots damage in the caches early.
2. **Invisible duplication.** The same bytes exist as an Ollama blob, an HF
   cache entry, and a hand-download — three copies of a 17 GiB file, and no
   filename tells you they're identical. Warden's answer: **content
   identity** (every file is fingerprinted by its actual bytes) and a
   **duplicates report** with safe, space-free reclaim.
3. **"Which drive was that on?"** Cold storage works until you can't
   remember — or the drive isn't plugged in and every tool pretends the
   files never existed. Warden's answer: a **catalog** that remembers
   offline drives, so an unplugged disk shows as *offline*, never *gone*.

**What warden deliberately does not do:** it never runs or serves models,
never edits another tool's configuration, and — this is the core promise —
**never destroys model bytes as a side effect of anything**. The only way
bytes stop existing is the deliberate, two-step trash flow (delete, then
empty the trash) described in 2.5. It is a librarian, not a janitor with a
shredder.

---

## 2. The ideas behind it (read this once)

Fifteen minutes here makes everything else in this guide obvious.

### 2.1 Content identity: bytes, not filenames

Two files named `model.gguf` in different folders might be the same model
or completely different ones. A file's *name* tells you nothing reliable.

Warden identifies every file by its **SHA-256 hash** — a 64-character
fingerprint computed from the file's actual bytes. Same hash = same bytes,
guaranteed, no matter what the file is called or where it lives. That's how
warden knows your Ollama blob `sha256-8601…` and your shelf file
`Qwen3.8-27B-UD-Q4_K_XL.gguf` are one model, and how it can promise a backup
is a *true* copy.

Hashing 300 GiB takes several minutes even on fast hardware, so warden
avoids re-hashing whenever it safely can: alongside the hash it records a
cheap **fingerprint** (file size, modification time, filesystem identity).
If the fingerprint hasn't changed, the bytes haven't changed, and the stored
hash is reused. If anything about the fingerprint changed, the file is
re-hashed. The fingerprint is only ever a *change detector* — identity is
always the hash.

### 2.2 Stores and roots: who owns what

Warden watches several kinds of location, called **roots**:

| Root kind | Example | Who owns it |
|---|---|---|
| **Shelf** | `~/models` | **You** (and warden manages it for you) |
| Ollama store | `~/.ollama/models` | Ollama |
| HF cache | `~/.cache/huggingface/hub` | Hugging Face tooling |
| **Registered drive** | `/media/you/Archive2` | **You** |

The distinction that drives everything: **owned vs. foreign**. The shelf
and your registered drives are *owned* — warden may write there. The Ollama
store and HF cache are *foreign* — warden reads and reports, but **never
writes inside them**. Cleaning up a foreign store happens through that
tool's own command (`ollama rm`, `hf cache rm`), run only when you say so.
This is why you can point warden at everything without fear: it cannot
corrupt another tool's world.

**Removable drives get a durable identity.** When you register a drive,
warden notes the filesystem's UUID (and drops a small marker file as a
fallback), so the same drive re-plugged next month — possibly at a different
mount point — is recognized as the same root. And a drive carries its own
catalog file at `<drive>/.modelwarden/manifest.json`, so it stays
*self-describing*: plug it into another machine running warden and the
contents are known without rescanning.

**Offline is not gone.** Unplug a registered drive and its models stay in
the catalog, marked offline. Warden can always answer "where is X?" with
"on the drive labeled Archive 2, currently unplugged" — which is exactly
what you need to hear at that moment.

### 2.3 The catalog

Everything warden learns is written into a **catalog** under
`~/.local/state/modelwarden/`:

- one manifest per root, and
- a merged view, `inventory.json`, keyed by content hash — every model,
  every place its bytes live, whether each place is reachable right now.

`inventory.json` is also a **published contract**: other tools can read it
to learn what models exist without doing any storage work themselves. It is
regenerated atomically on every catalog update — read it, never edit it.

### 2.4 Bundles: everything a model needs to run

A "model" is often more than one file:

- **Split GGUFs**: very large models ship as parts named
  `…-00001-of-00003.gguf` — one part alone is useless.
- **Vision projectors**: multimodal models pair the main GGUF with an
  `mmproj` file sitting beside it.
- **Safetensors directories**: a weights file (`.safetensors`, `.bin`,
  `.pt`, `.pth`, `.onnx`) is nothing without its neighbors — `config.json`,
  tokenizer files, sometimes whole subfolders. For these, the *directory*
  is the model.

Warden groups these into **bundles**, and every operation — backup, archive,
demote, restore, download — moves the whole bundle, never a fragment. Back
up "the model" and its projector and split parts come along automatically.
You cannot accidentally strand half a model.

Bundles are kept as plain files in a human-readable layout (no tar, no zip,
no proprietary container). The reasoning: if warden vanished tomorrow, a
human with a file manager could still rescue everything.

### 2.5 The safety rules

These invariants hold everywhere, in both the GUI and CLI:

1. **Bytes are destroyed only by emptying the trash.** Deleting a model
   (`warden delete`, or Delete… in the GUI) *moves* its bundle into the
   root's trash — a same-filesystem rename: instant, free, and fully
   restorable. Only the separate, explicit act of emptying the trash
   (`warden trash empty --yes`, or the GUI's Empty Trash confirmation)
   destroys bytes — two decisions, deliberately separated in time.
   Companions another model still needs (a shared vision projector) are
   automatically spared. Otherwise, space reclaim is *hardlinking* —
   making two directory entries share one copy of the bytes (see 2.6) —
   after re-hashing both files to prove they are still identical; and an
   *archive with move* (`demote --remove-source`) deletes the original
   **only after** the new copy's bytes have been read back from the
   destination and hash-verified.
2. **Every copy is verified end-to-end.** Copies are written to a temporary
   `.partial` name, hashed as written, read back from the destination and
   hashed *again*, and only then renamed into place. A power cut mid-copy
   leaves a `.partial` file — never a plausible-looking half-model. Warden
   also refuses to overwrite existing files, everywhere.
3. **Foreign stores are read-only** (2.2). Cleanup goes through the owning
   tool's own CLI, on your explicit action. Warden removes only two things
   itself, both guarded and both content-free: interrupted-download debris
   (`*.incomplete`) and pruned husk directories that provably contain zero
   model bytes.
4. **One writer at a time.** A lock file stops two wardens (say, the GUI
   and a scheduled scrub) from writing simultaneously. A crashed run's
   stale lock is detected and taken over automatically.

### 2.6 Duplicates and hardlinks

A **hardlink** makes two file names point at the same bytes on disk — the
data exists once, so a 17 GiB duplicate becomes 17 GiB reclaimed, and both
names keep working. Limits: both names must be on the *same filesystem*
(you can't hardlink across drives), which is also why copies on a backup
drive are never counted as "reclaimable" — cross-device copies are the
redundancy you *wanted*.

Warden's dedup: reports duplicates everywhere, reclaims (by hardlinking)
only within owned roots, and always re-verifies both files' bytes at the
moment of reclaim.

### 2.7 Bit rot and scrubbing

Disks fail quietly. A sector degrades, a byte flips, and a file you haven't
opened in a year is corrupt — you find out the day you need it. This is
**bit rot**, and backups are *more* exposed to it, because backup drives sit
unpowered and unread for months.

Warden's answer is the **scrub**: a scheduled background job that re-reads
every tracked byte and compares it against the catalog's hashes. A file you
legitimately edited re-hashes and passes; a file whose bytes changed while
its metadata claims otherwise is the bit-rot signature, and it fails
loudly. Found early — while a healthy copy still exists elsewhere — bit rot
is a non-event: `verify --repair` re-copies the damaged file from a good
source. Found late, it's a loss. That's why warden nags you (via the
doctor) until the scrub timer is running.

---

## 3. Installing and first run

### 3.1 Building from source

You need Rust (edition 2024). Then:

```
git clone <repo-url>
cd modelwarden
cargo build --release
```

The two programs land in `target/release/`:

- **`warden`** — the command-line tool
- **`warden-gui`** — the desktop app

Put them on your `PATH` (e.g. copy to `~/.local/bin/`). Everything below
assumes `warden` is runnable from any directory.

### 3.2 First run, in order

```
warden scan                    # 1. discover your stores; sanity-check the list
warden hash                    # 2. build the catalog (computes hashes; takes minutes)
warden scrub install --enable  # 3. schedule the weekly background re-verify
warden doctor                  # 4. health check; it will flag anything missing
```

**Step 1 — `scan`** finds your stores automatically (shelf, Ollama, HF
cache) and prints every model file it can see, without writing anything.
Look at the list: does it match reality? If your shelf lives somewhere
unusual, see 3.3.

**Step 2 — `hash`** is the first real catalog build. Expect it to take a
while the first time (it reads every byte of every model); it prints
per-file progress. Every later run is fast — only new or changed files are
re-hashed.

**Step 3 — the scrub** (concept in 2.7). This writes a systemd user timer
and starts it. On machines without systemd, skip this and run
`warden hash && warden verify --all` from cron instead.

**Step 4 — `doctor`** reports store health. On a machine that's been using
Ollama and HF tools for a while, expect a few findings — leftover debris
from interrupted downloads and cache pruning. Section 7.3 explains each
kind.

If you prefer clicking to typing, run `warden-gui` instead and use
**File → Update Catalog** — it's the same operation as `warden hash`.

### 3.3 Configuration (usually not needed)

Warden's config lives at `~/.config/modelwarden/config.json` and records
only what you've changed. You normally never edit it — the GUI dialogs and
CLI flags maintain it. The fields, for reference:

- `scan_dirs` — folders to treat as shelves. **The first entry is *the*
  shelf**: downloads, restores, and archived models land there.
- `roots` — your registered drives (managed by `warden roots add` / the GUI).
- `discover_stores` — set `false` to stop auto-detecting Ollama/HF stores
  and watch only `scan_dirs` + registered roots.
- `hf_token` — a Hugging Face token for gated downloads (see 6.5).

State (the catalog itself) lives at `~/.local/state/modelwarden/`. Deleting
that folder loses no models — it's all recomputable — but you'd lose
download provenance records and have to re-hash everything.

---

## 4. A tour of the GUI

Launch `warden-gui`. The window has a menu bar, four tabs, an activity log
(bottom panel), and a status bar.

### 4.1 The Inventory tab 📦

One row per model in the catalog, across *all* roots: name, quantization,
size, and where it lives. Click a **column heading** to sort by it (click
again to reverse), and use the **filter box** to narrow the list by name,
quant, location, or hash. Files that exist only to serve another model —
a vision `mmproj` projector, a safetensors model's tokenizer and config
companions — appear **indented under the model that needs them**, marked
"required by …", so the list reads as models, not as loose files; they're
collapsed behind a **▸** toggle by default. The **Active / All** switch
controls whether cold-stored models (every copy on a registered cold-
storage root) appear: Active shows your working set, All shows the whole
catalog. Rows for
models on an unplugged drive appear greyed with the drive's label —
offline, not gone. Hover a row for details, including **provenance** if
warden downloaded it (which repo, revision, and when).

Row actions — behind each row's **⋯** menu:

- **Keep on shelf** — copy a cache-owned model (e.g. one Ollama pulled)
  to your shelf, so no cache pruner can take it from you. Uses a
  hardlink when both are on the same filesystem, costing zero extra
  bytes. (The CLI verb for this is `warden archive`.)
- **Cold storage…** — move a model to a registered root (a drive, NAS
  mount, or any folder you registered). A dialog asks which target, and
  whether to remove the shelf copy afterwards — the removal happens only
  after the target's copy hash-verifies (see 2.5). For many models at
  once, use **Tools → Move to Cold Storage…**: check off models (a
  filter helps), pick the target, and one confirmation moves them all —
  required files included, shared companions moved once. (CLI:
  `warden archive demote`.)
- **Back up…** — back up this model (and everything it needs — its whole
  bundle) to a drive.
- **Delete…** — move the model's bundle to the trash (see the Trash tab
  below). The confirmation shows exactly what will move, what is kept
  because another model needs it, and — for copies in Ollama or the HF
  cache — the owning tool's command for you to run yourself (warden never
  touches foreign stores). Nothing is destroyed by this action.

### 4.2 The Duplicates tab 🔗

Groups of identical bytes (same hash) stored more than once, with the
reclaimable size per group. **Reclaim…** collapses a group into hardlinks —
after a confirmation dialog and a fresh re-verification of both copies.
Groups involving foreign stores are shown but marked untouchable
(report-only, per the safety rules). Copies on other drives aren't listed
at all — those are backups, not waste.

### 4.3 The Usage tab 📊

Disk usage grouped by model family — how much space each family occupies,
how much is unique bytes vs. duplicate copies. This is the "what's actually
eating my disk?" view.

### 4.4 The Health tab 🩺

The GUI face of the doctor. Run **Tools → Check store health**, and each
finding appears as a row: what kind of problem, where, how big, and the
remedy. Hover the problem label for a plain-English explanation plus what
fixing it would lose. Findings warden can fix get a **Clean up…** button —
it opens a confirmation showing exactly what will run and who is acting
(the owning tool's own command, or warden itself for the two guarded
exceptions), and nothing happens until you click **Run it**. Findings that
must stay manual show you the exact command to copy.

### 4.5 The Trash tab 🗑

Deleted bundles land here, intact — a delete is just a rename into
`<root>/.modelwarden/trash/`. Each file shows its size, root, and age, with
a per-file **Restore** that brings back the file's whole bundle — split
parts and projector included, exactly as delete took it (a rename back;
it refuses to overwrite anything that reappeared). **Empty Trash…** is warden's single irreversible act: a
confirmation states the exact count and size being destroyed, and only
that click makes bytes stop existing. There is no automatic emptying —
destruction never happens on a schedule.

### 4.6 Menus and dialogs

- **File → Update Catalog** — rescan all roots, hash new/changed files,
  rewrite the catalog. Same as `warden hash`. Progress shows in the status
  bar; per-file results land in the activity log.
- **File → Settings…** — shelf directories (add/remove, with the first
  marked as *the* shelf where downloads land) and the store-discovery
  toggle — the last settings that used to require editing config.json by
  hand. Nothing saves unless every path validates.
- **File → Storage Roots…** — see registered drives and register new ones
  (type a path or **Browse…**; optionally give the drive a label like
  "Archive 2"). Register a drive once; it's recognized forever after.
- **Tools → Back up…** — back up everything (or a filtered selection) to a
  target folder or drive. Type or **Browse…** to the destination, filter
  the model list, and watch the live bundle/size preview update before you
  commit.
- **Tools → Download from HuggingFace…** — see 6.5. Lists a repo's downloadable
  models — a split multi-part model shows as **one row** with its combined
  size and a "(N parts)" note (parts itemized on hover), because Download
  always transfers the whole set. A **filter box** narrows big
  listings by name or quantization (type `UD-Q3_K_XL` to cut a 31-file
  repo down to the one you want) — and it keeps its value across repos,
  so hunting the same quant through several repos is one keystroke per
  repo. Has a masked token field for gated repos with a "Remember"
  option.
- **Help → About** — version and license.

The **activity log** at the bottom keeps a durable line for everything that
happened this session — the same wording the CLI prints — while the status
bar shows only the current operation's live progress.

---

## 5. The CLI, command by command

Every read command accepts `--json` for machine-readable output. Commands
that write take the single-instance lock automatically.

### Seeing what you have

- **`warden scan`** — discover stores and list every model file found right
  now. Read-only; touches no state. Use it to sanity-check what warden can
  see.
- **`warden hash`** — the catalog builder. Rescans all roots, re-hashes
  only what changed, and rewrites the catalog. Run it (or the GUI's Update
  Catalog) after adding or moving models around. Every warden write
  operation also refreshes the catalog automatically.
- **`warden status`** — the big picture: each root with file count and
  size, how many contents are hashed vs. pending, and the safety headline:
  *"N of M contents have a copy on a registered drive."*
- **`warden where <query>`** — find a model by name fragment, path
  fragment, or hash prefix, and list *every* place its bytes live —
  including offline drives (marked OFFLINE) — plus download provenance if
  known. This is the "which drive was that on?" command.
- **`warden report`** — disk usage by model family; unique vs. on-disk
  bytes.
- **`warden dups`** — duplicate groups and what's reclaimable (see 2.6).

### Protecting it

- **`warden backup <path> [query…] [--label X]`** — verified copies to a
  target folder/drive. With no query, backs up everything hashed; with
  queries, just the matching models — each expanded to its full bundle.
  The target is registered as a root (label it with `--label`), gets a
  human-readable layout, and carries its own manifest so it stays
  self-describing unplugged. Files already on the target are skipped, so
  re-running a backup copies only what's new.
- **`warden verify <path|root-id> [--repair]`** and
  **`warden verify --all [--repair]`** — re-read a root (or every online
  owned root) and compare every byte against the catalog. Exit code 1 on
  any mismatch. With `--repair`, damaged or missing files are re-copied
  from a healthy source elsewhere in the catalog — the corrupt copy is
  never deleted first, and content with no live source is reported
  unrepairable, never silently dropped.
- **`warden scrub install [--daily|--weekly|--monthly] [--enable]`** —
  write the systemd user timer that runs `hash && verify --all` on a
  schedule (weekly by default) at idle I/O priority. Without `--enable` it
  only writes the units and prints the enable command; with it, the timer
  starts immediately.

### Organizing it

- **`warden archive <query>`** — *promote*: copy a cache-owned model to the
  shelf (hardlink when same-filesystem — zero extra bytes). The cache copy
  is untouched; you've just made sure a pruner can't take the model away.
- **`warden archive demote <query…> --to <path|root-id|label> [--remove-source]`**
  — *demote*: move one or many models to cold storage in one command. A verified copy lands on the
  drive and is recorded in the drive's carried manifest; the shelf copy is
  deleted only if you passed `--remove-source`, and only after the drive
  copy's read-back hash matched.
- **`warden restore <query>`** — the return leg: verified copy from a drive
  back to the shelf. The drive is never modified. If the drive is offline,
  the refusal names which drive to plug in.
- **`warden dedup [--hardlink]`** — duplicate reclaim. Default is a **dry
  run** that reports what would happen; `--hardlink` performs it (owned
  roots, same filesystem, both sides re-verified — see 2.6).

Queries in these commands accept a name fragment or a hash prefix — the
hash prefix matters when two different models share a name.

### Health and acquisition

- **`warden doctor [--fix]`** — store health. Each finding prints with an
  explanation, location, remedy, and what fixing loses. `--fix` executes
  the safe remedies (owner-tool commands and warden's two guarded
  exceptions) and lists what remains for you. Section 7.3 is the findings
  glossary.
- **`warden roots add <path> [--label X]`** / **`warden roots list`** —
  register and list drives (see 2.2).
- **`warden roots forget <id|label|path> --yes`** — for a drive that is
  *truly* gone: died, got reformatted, was given away. "Offline is not
  gone" holds right up until you know otherwise — this is how you tell
  warden. It removes only warden's knowledge (no bytes are touched
  anywhere); without `--yes` it previews the cost: how many models had
  copies there, and how many existed nowhere else and will leave the
  catalog. A working drive can always be re-registered and re-cataloged
  later. GUI: the **Forget…** button in File → Storage Roots….
- **`warden fetch <org/repo> [pattern] [--token T [--save-token]]`** —
  download from Hugging Face into the shelf. With just a repo, lists its
  GGUF files and sizes; with a pattern matching exactly one file, downloads
  it — automatically expanding split models to all their parts. Resumes
  interrupted downloads. Records provenance (repo, revision, date) at
  download time.
- **`warden fetch <org/repo> --snapshot`** — for repos with no GGUFs
  (safetensors-style): downloads the *whole snapshot* into one shelf
  directory, because for those models the directory is the model (see 2.4).
  Plain `fetch` on such a repo lists the files and suggests this.
- **`warden delete <query…>`** — stage 1 of deletion: each model's bundle
  moves into its root's trash (a rename — nothing destroyed, fully
  restorable). Companions another model still needs are kept
  automatically; copies in foreign stores get the owner command printed
  for you to run yourself.
- **`warden trash`** — list what the trash holds, where, and how old.
- **`warden trash restore <query>`** — bring matching models back. Each
  match expands to its full bundle in the trash — split parts and the
  projector return together, exactly as delete took them.
- **`warden trash empty --yes`** — stage 2: permanently destroy the
  trash's contents. Without `--yes` it only reports what would be
  destroyed. This is warden's one irreversible command.
- **`warden journal [N|--all]`** — the operations journal: every durable
  write-operation line (copied, demoted, trashed, destroyed, fetched,
  forgot…) is appended to `~/.local/state/modelwarden/journal.log` as it
  happens, from both the CLI and the GUI. "What did I do last Tuesday?"
  now has an answer after the session that did it is gone. Plain text
  with readable timestamps — `cat` works without warden; the file is
  yours to rotate or delete.
- **`warden version`** / **`warden help`** — what they say.

---

## 6. Everyday recipes

### 6.1 First inventory of a messy machine

```
warden scan          # look at the list — is anything missing?
warden hash          # build the catalog (be patient once)
warden status        # the headline numbers
warden dups          # find out how much space duplicates waste
warden doctor        # find out what's broken in the caches
```

Typical discoveries on a machine that's been running local models for a
year: a multi-GiB duplicate or two, a few gigabytes of interrupted-download
debris, and cache husks left by pruning. All of it visible in twenty
minutes, none of it touched until you say so.

### 6.2 Back up your keepers to an external drive

```
warden roots add /media/you/Archive2 --label "Archive 2"
warden backup /media/you/Archive2 qwen glm      # just these models
warden backup /media/you/Archive2               # …or everything
```

Or in the GUI: **Tools → Back up…**, Browse to the drive, filter, go.
Re-run any time — only new content is copied. Then make verification a
habit:

```
warden verify "Archive 2"     # by label, id, or path — full byte check
```

### 6.3 Free disk space, safely

```
warden dups                 # what's duplicated, what's reclaimable
warden dedup                # dry run — read the plan
warden dedup --hardlink     # reclaim (same bytes stay available at every path)
```

And for models you want to *keep but not keep here*:

```
warden archive demote gemma qwen glm --to "Archive 2" --remove-source
```

The shelf copy disappears only after the drive copy verifies. The model
stays in the catalog, findable by `warden where gemma`, listed as on
"Archive 2", offline whenever the drive is unplugged.

### 6.4 Get a model back from cold storage

```
warden where glm            # confirms: on "Archive 2" (OFFLINE)
# plug the drive in…
warden restore glm
```

The bundle is copied (and verified) back to the shelf; the drive keeps its
copy — restore never modifies the drive.

### 6.5 Download models

GGUF repos — list, then fetch by pattern:

```
warden fetch unsloth/Qwen3.8-27B-GGUF            # lists the files + sizes
warden fetch unsloth/Qwen3.8-27B-GGUF Q4_K_XL    # downloads the match
```

Split models expand automatically: match any part of a
`-00001-of-00003`-style set and all parts download together (a set with
missing parts is refused rather than half-downloaded). Vision models get
the same treatment: if the repo ships an `mmproj` projector (required for
image input), downloading the model pulls the projector along too — the
bundle promise (2.4) starts at download time. Dropped connections
resume automatically mid-download (a fresh Range request from the byte
where it stopped, retried until it stalls outright); a download that
still fails keeps its `.partial`, and re-running the same command
resumes from it.

Safetensors-style repos (no GGUFs):

```
warden fetch BAAI/bge-small-en-v1.5              # lists files, suggests --snapshot
warden fetch BAAI/bge-small-en-v1.5 --snapshot   # whole directory, one bundle
```

**Gated repos** (licenses you must accept, e.g. Llama-family) need a
Hugging Face token: pass `--token hf_…` once with `--save-token` to store
it, or set `$HF_TOKEN`, or log in with the `hf` CLI — warden finds any of
them. In the GUI, the download dialog has a masked token field with a
"Remember" checkbox. Note: Hugging Face answers *401* both for gated repos
and for repo ids that don't exist — warden tells you which it thinks it is,
with did-you-mean suggestions for likely typos.

### 6.6 The monthly five minutes

```
warden doctor        # anything new broken in the caches?
warden status        # is everything still backed up somewhere?
```

…and let the scrub timer do the byte-level checking for you
(`systemctl --user status modelwarden-scrub.timer` to see it run). If a
scrub ever fails, that's bit rot found *early*:

```
warden verify --all            # see what and where
warden verify "Archive 2" --repair    # re-copy damaged files from a good source
```

---

## 7. Reading what warden tells you

### 7.1 The status headline

*"12 of 14 contents have a copy on a registered drive."* — this is warden's
core safety metric: how much of your collection would survive this
machine's disk dying right now. The two unprotected contents are your
to-do list; `warden status --json` identifies them precisely.

### 7.2 Duplicates: reclaimable vs. redundancy

Only same-filesystem copies in owned roots count as *reclaimable* — those
are accidents. A copy on another device is *redundancy you chose* (that's
what a backup is), so it appears in `where`, but never in the dups report.

### 7.3 Doctor findings, translated

| Finding | What it means | Fixing loses |
|---|---|---|
| **incomplete download** | A `*.incomplete` temp file from an interrupted download. Not a valid model. | Only the ability to resume that download; a re-download starts over. |
| **pruned husk** | A cache repo folder whose content was pruned — only an empty skeleton remains. | Nothing; there are no bytes inside. Warden removes it itself (verified empty first) because the HF CLI can't even see it. |
| **dangling ref** | A cache branch pointer naming a revision that no longer exists on disk. | Nothing; what it points to is already gone. |
| **orphan blob** | A content file no snapshot references — real bytes reachable by nothing, usually left by an upgrade. | **The bytes themselves.** Nothing uses them, but this is real data — so it stays a manual decision, always. |
| **dangling snapshot link** | A cache entry that looks like a file but points at pruned bytes. | Nothing; the bytes are already gone. |
| **missing ollama blob** | An Ollama model whose manifest names a weights blob that isn't in the blob store — registered but can't run. | Just the registration; its bytes are already missing. |
| **scrub timer off** | Nothing is periodically re-reading your bytes; bit rot would go unnoticed (see 2.7). | Nothing — fixing *gains* you a weekly background check. |
| **stale verification** | A backup drive hasn't been byte-verified in over 90 days (or ever). The scrub only covers what's plugged in, so cold drives silently age out of trust — and the at-risk bytes are named. | Nothing — plug the drive in and run the printed `warden verify` command. |

`doctor --fix` (or the GUI's Clean up buttons) executes everything in this
table except **orphan blob**, which prints its exact removal command and
waits for you — warden never deletes real bytes in a foreign store, even
provably orphaned ones.

---

## 8. Troubleshooting and FAQ

**"another warden holds the write lock"** — some other warden process is
mid-write (the GUI, a scrub run, another terminal). Wait for it, or close
the GUI. If a previous run *crashed*, the leftover lock is detected as
stale and taken over automatically — you never need to delete it by hand.

**Hugging Face listing/download fails with 401** — three possibilities:
the repo id is mistyped (ids are case-sensitive; warden suggests close
matches), the repo is gated (supply a token — see 6.5), or the repo is
private. HF deliberately hides whether an id exists, so warden reports what
it can infer.

**A model shows "pending" instead of a hash** — it's been seen but not yet
hashed. Run `warden hash`.

**A model shows OFFLINE** — its only reachable copy is on an unplugged
registered drive. Plug the drive in; the catalog reconnects automatically
on the next operation. This is a feature: offline is remembered, not
forgotten.

**`verify` failed on a file** — the bytes on disk no longer match the
catalog hash. If you edited the file on purpose, run `warden hash` (it
re-fingerprints and re-hashes; the "failure" disappears). If you didn't,
that's bit rot or a bad copy: run `verify --repair` to re-copy from a
healthy source.

**How do I actually delete a model?** — `warden delete <name>` (or the
row's Delete… button), then, when you're sure, `warden trash empty --yes`
(or Empty Trash… in the Trash tab). The two steps are separate on purpose:
the first is a free, restorable rename; only the second destroys bytes.
Copies inside Ollama or the HF cache are never touched — warden prints the
owning tool's removal command for you to run yourself.

**The system Ollama store (`/usr/share/ollama/…`) shows nothing** — it's
often unreadable to your user account. Warden degrades gracefully and
scans what it can.

**A drive died or had to be reformatted** — tell warden it's truly gone:
`warden roots forget "<label>" --yes` (or Forget… in Storage Roots…). The
preview states exactly what knowledge is lost — models that existed only
there leave the catalog; models with other copies just drop that
location. After reformatting, the drive gets a fresh identity when you
re-register it.

**NAS quirks** — some network filesystems report unstable modification
times, which invalidates fingerprints and causes safe-but-slow re-hashing.
Identity is still the hash, so nothing breaks; it's just slower.

**Where does warden keep its own data?** — config:
`~/.config/modelwarden/config.json`; catalog and lock:
`~/.local/state/modelwarden/`; per-drive manifests: `.modelwarden/` on each
registered drive. To uninstall completely, delete the binaries and those
folders — your models are untouched (they were never warden's to move
without asking).

---

## 9. Glossary

- **Bundle** — a model plus everything it needs to run (split parts,
  projector, tokenizer/config companions). The unit every operation moves.
- **Catalog** — warden's stored knowledge: per-root manifests plus the
  merged `inventory.json`.
- **Content identity** — a file's SHA-256 hash; the only thing warden
  trusts to say two files are the same model.
- **Demote / restore** — verified move to cold storage / verified copy back
  to the shelf.
- **Fingerprint** — cheap change-detector (size, mtime, filesystem ids)
  used to skip re-hashing unchanged files. Never used as identity.
- **Foreign store** — a directory another tool owns (Ollama store, HF
  cache). Warden reads, reports, and never writes inside.
- **GGUF** — the single-file model format used by llama.cpp and Ollama;
  self-describing header (architecture, quantization, context length).
- **Hardlink** — two file names sharing one copy of bytes on the same
  filesystem. Warden's only space-reclaim mechanism.
- **Husk** — a cache repo folder left behind after pruning: structure
  without content.
- **Owned root** — the shelf and registered drives; places warden may write.
- **Promote (archive)** — copying a cache-owned model to the shelf so no
  pruner can take it.
- **Provenance** — where a download came from: repo, file, revision, date.
  Captured at download time (the only time it's knowable) and shown in
  `where` and the GUI.
- **Root** — any location warden watches; see 2.2.
- **Scrub** — the scheduled re-read of every tracked byte to catch bit rot.
- **Shelf** — your primary owned model directory (first entry of
  `scan_dirs`); where downloads, promotions, and restores land.
- **Snapshot (HF)** — one revision of a Hugging Face repo in the cache;
  `--snapshot` downloads a whole one as a bundle.
- **Split model** — a GGUF shipped as `-NNNNN-of-NNNNN` parts; warden
  always treats the set as one bundle.

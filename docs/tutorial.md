# The modelwarden Tutorial

*A hands-on course. In about 90 minutes you'll exercise every warden
concept — inventory, identity, backups, bit rot, bundles, cold storage,
safe deletion, health, and history — inside a **sandbox** that cannot
touch your real models. Every command here was executed for real while
this tutorial was written; the outputs you see are genuine.*

**Who this is for:** anyone who runs local LLMs. No storage knowledge is
assumed; every term is **bolded and defined** the first time it appears,
and they're all collected in the [glossary](#appendix-a--glossary).

**How to read it:** at a terminal, doing each step. Chapters end with a
✅ **Checkpoint** (what you should see) and 🧠 **Check yourself**
questions (answers in [Appendix C](#appendix-c--check-yourself-answers)).

Contents:
[1 The problem](#chapter-1--the-problem-you-already-have) ·
[2 Seeing](#chapter-2--seeing-what-you-have) ·
[3 Same bytes](#chapter-3--same-bytes-different-names) ·
[4 Drives & offline](#chapter-4--drives-and-the-meaning-of-offline) ·
[5 Real backups](#chapter-5--backups-you-can-actually-trust) ·
[6 Bit rot](#chapter-6--bit-rot-and-catching-it) ·
[7 Bundles](#chapter-7--models-are-more-than-one-file) ·
[8 Cold storage](#chapter-8--cold-storage) ·
[9 Deleting](#chapter-9--deleting-without-fear) ·
[10 Doctor & journal](#chapter-10--the-doctor-and-the-journal) ·
[11 Downloading](#chapter-11--getting-models-optional-needs-network) ·
[12 Graduation](#chapter-12--graduation-your-real-machine)

---

## Chapter 1 — The problem you already have

Run local models for a few months and your disk fills with **model
files**: multi-gigabyte blobs of neural-network weights. Most are
**GGUF** files — a single-file format used by llama.cpp and Ollama that
packs the weights *and* a self-describing header (architecture, context
size). Others are **safetensors**-style models, where a directory of
files — weights plus tokenizer and config — together form one model.
Each usually exists in several **quantizations** (compressed precisions
like `Q4_K_M` — smaller and faster at slight quality cost), which is why
one model becomes five files.

These files live in **stores** — directories with owners and rules:

- Ollama's store (`~/.ollama/models`): files named by hash, unreadable
  to humans, deleted by `ollama rm`.
- The Hugging Face **cache** (`~/.cache/huggingface/hub`): a maze of
  snapshots and symlinks, *designed to be pruned* — cache-cleanup tools
  will delete from it to save space.
- Your own downloads folder — the only store *you* own. Warden calls
  this the **shelf**.
- External drives and NAS mounts you copy things to and then forget.

Three failures follow from this sprawl, and each is a chapter of this
tutorial: files get **silently lost** (a pruner ate them), **silently
duplicated** (the same 17 GB exists three times under three names), and
**silently forgotten** ("which drive was that on?"). warden is the
librarian that prevents all three — and its core promise is that
*nothing it does can lose your bytes*.

### Exercise: build the sandbox

Everything in this tutorial happens in a disposable world under
`~/warden-tutorial` — your real stores are never touched, because we
point warden's **config** (its settings file) and **state** (its
database directory) somewhere private:

```sh
mkdir -p ~/warden-tutorial/{config/modelwarden,state,shelf}
cat > ~/warden-tutorial/config/modelwarden/config.json <<'EOF'
{"scan_dirs":["SHELF"],"discover_stores":false}
EOF
sed -i.bak "s|SHELF|$HOME/warden-tutorial/shelf|" ~/warden-tutorial/config/modelwarden/config.json
alias wt='XDG_CONFIG_HOME=$HOME/warden-tutorial/config XDG_STATE_HOME=$HOME/warden-tutorial/state warden'
```

(`wt` is "warden, tutorial edition" — the two environment variables
point it at the sandbox. `discover_stores:false` stops it from finding
your real Ollama/HF stores. This is a genuinely useful trick beyond the
tutorial: warden can be aimed at any directory tree this way.)

Now create three tiny fake models. This helper writes a *real* GGUF
header (the magic bytes and version any GGUF starts with) plus filler —
small enough to hash instantly, real enough for every feature to work:

```sh
mkgguf() { python3 -c "import struct,sys; open(sys.argv[1],'wb').write(b'GGUF'+struct.pack('<IQQ',3,0,0)+sys.argv[2].encode()*2000)" "$1" "$2"; }
mkgguf ~/warden-tutorial/shelf/tinyllama-Q4_K_M.gguf aaaa
mkgguf ~/warden-tutorial/shelf/tinyllama-Q8_0.gguf bbbb
mkgguf ~/warden-tutorial/shelf/nano-embed-Q4_K_M.gguf cccc
```

✅ **Checkpoint:** `ls ~/warden-tutorial/shelf` lists three `.gguf`
files.

🧠 **Check yourself:** (1) Which of your real directories are *foreign*
stores — ones warden will never write into? (2) Why does one model
often exist as five files?

---

## Chapter 2 — Seeing what you have

Two commands look at your models, and the difference between them *is*
warden's first big idea.

**`scan`** is a live view: walk the stores, list what's there right now.
It writes nothing:

```sh
wt scan
```

```
SOURCE  NAME                    QUANT      SIZE  STATE
shelf   nano-embed-Q4_K_M                7.8 KiB  present
shelf   tinyllama-Q4_K_M                 7.8 KiB  present
shelf   tinyllama-Q8_0                   7.8 KiB  present

3 files, 23.5 KiB total
```

**`hash`** builds the **catalog** — warden's persistent memory. For
every file it computes a **SHA-256** hash: a 64-character fingerprint of
the file's *actual bytes*. Same hash = same bytes, always, regardless of
filename. This is **content identity**, and it's the foundation under
everything else: names lie, bytes don't.

```sh
wt hash
```

```
  hashing tinyllama-Q8_0.gguf (7.8 KiB)
  hashed nano-embed-Q4_K_M.gguf in 0s
  hashed tinyllama-Q4_K_M.gguf in 0s
  hashed tinyllama-Q8_0.gguf in 0s
3 newly hashed; inventory: 3 distinct contents (3 hashed) → …/state/modelwarden/inventory.json
```

(Several files hash at once on real hardware — warden uses up to four
cores. On a real 300 GB collection this first run takes minutes; it also
*checkpoints* after every file, so interrupting it loses nothing.)

Now run `wt hash` again: **`0 newly hashed`**. Reading gigabytes to
recompute hashes every time would be absurd, so warden stores a cheap
**fingerprint** per file (size + modification time + filesystem
identity). Unchanged fingerprint → the stored hash is still valid.
Prove the mechanism:

```sh
touch ~/warden-tutorial/shelf/nano-embed-Q4_K_M.gguf   # change mtime only
wt hash
```

Exactly one file re-hashes. The fingerprint is only ever a *change
detector* — identity is always the hash.

What hashing produced: a **manifest** per store (what's in this root)
and the merged **inventory** — one entry per distinct content, listing
every place its bytes live. `wt status` is its summary; its last line —
`0 of 3 contents have a copy on a registered drive` — is warden's
safety headline, and the rest of this tutorial is about changing it.

✅ **Checkpoint:** second `hash` reports `0 newly hashed`; after
`touch`, exactly `1 newly hashed`.

🧠 **Check yourself:** (1) Why is a filename not identity? (2) What
does a fingerprint match allow warden to skip, and what is it never
allowed to decide?

---

## Chapter 3 — Same bytes, different names

Make the classic mistake on purpose:

```sh
cp ~/warden-tutorial/shelf/tinyllama-Q4_K_M.gguf ~/warden-tutorial/shelf/tinyllama-copy.gguf
wt hash && wt dups
```

```
f81cc974f445  tinyllama-copy  (7.8 KiB, 7.8 KiB reclaimable)
    [shelf-…] tinyllama-copy.gguf  (inode 45:21194)
    [shelf-…] tinyllama-Q4_K_M.gguf  (inode 45:21173)

1 duplicated contents, 7.8 KiB reclaimable — see `warden dedup`
```

Content identity found the **duplicate** instantly: two names, one
hash. On real collections this finds 17 GB copies you forgot you made.

The fix isn't deletion — it's a **hardlink**: a filesystem feature where
two names point at *one* copy of the bytes. Both names keep working;
the duplicate's space comes back; nothing is lost. The `inode` numbers
above are the filesystem's "which bytes" identifiers — different now,
same after:

```sh
wt dedup              # DRY RUN — reports, changes nothing
wt dedup --hardlink   # re-verifies both files' bytes, then relinks
ls -i ~/warden-tutorial/shelf/tinyllama-Q4_K_M.gguf ~/warden-tutorial/shelf/tinyllama-copy.gguf
```

`ls -i` now shows the **same inode** for both names. Note the safety
shape you'll see everywhere in warden: the default is a report; the
action needs an explicit flag; and both files were re-hashed *at relink
time* to prove they were still identical.

**Reclaimable** has a precise meaning: same bytes, same filesystem,
warden-owned root. A copy on another drive is *not* reclaimable — that's
a backup, redundancy you chose. And duplicates inside foreign stores are
reported but never touched.

✅ **Checkpoint:** `ls -i` shows one inode number for both names.

🧠 **Check yourself:** (1) Why is a copy on a second drive never
"reclaimable"? (2) What did warden do immediately before relinking, and
why?

---

## Chapter 4 — Drives and the meaning of "offline"

A **root** is any location warden watches. Your shelf and the foreign
stores are discovered automatically; drives and NAS mounts you
**register** — once — and warden gives them durable identity (a
filesystem UUID where available, plus a `.modelwarden/root-id` marker
file), so the same drive is recognized forever, whatever its mount
point. A **label** gives it a human name you'll use in commands.

Our sandbox "drive" is just a folder — warden treats any registered
directory identically:

```sh
mkdir -p ~/warden-tutorial/drive
wt roots add ~/warden-tutorial/drive --label "Practice Drive"
wt backup ~/warden-tutorial/drive tinyllama-Q4
```

Now the important experiment — unplug it:

```sh
mv ~/warden-tutorial/drive ~/warden-tutorial/drive-unplugged
wt where tinyllama
```

```
f81cc974f445  tinyllama-copy  (7.8 KiB)
    [Practice Drive] tinyllama-copy.gguf  — OFFLINE
    [shelf-…] tinyllama-copy.gguf  — present
    …
```

This is warden's **offline-not-gone** rule: an unreachable drive's
contents stay in the catalog, marked OFFLINE. Six months from now,
`where` still answers "it's on the drive labeled Practice Drive." Every
other tool forgets unplugged disks; warden remembers. (`wt where`
searches by name fragment, path, or hash prefix — it's the
"which drive was that on?" command.)

```sh
mv ~/warden-tutorial/drive-unplugged ~/warden-tutorial/drive   # replug
wt where tinyllama    # everything 'present' again — same root identity
```

The complement, for a drive that truly died: `wt roots forget
"<label>" --yes` removes warden's *knowledge* (never bytes), after a
preview stating exactly how many models exist nowhere else.

✅ **Checkpoint:** `where` showed OFFLINE while moved, `present` after.

🧠 **Check yourself:** (1) What two mechanisms give a registered drive
identity across remounts? (2) What's the difference between an offline
root and a forgotten one?

---

## Chapter 5 — Backups you can actually trust

You already ran `backup` — now understand what made it trustworthy. A
warden **verified copy** is never "copy and hope":

1. Bytes stream to a temporary **`.partial`** name — a half-finished
   copy can never be mistaken for a model.
2. The bytes are hashed *as read from the source* — must match the
   catalog.
3. The destination file is **read back** and hashed — must match again.
4. Only then does it get renamed into place. Existing files are never
   overwritten, anywhere (**refuse-overwrite**).

So when `backup` said `verified in 0s`, it meant: the catalog, the
source read, and the destination read-back all agreed. The target drive
also received its own `.modelwarden/manifest.json` — it's
**self-describing**, readable by warden on any machine, unplugged or
not.

Check the headline:

```sh
wt status
```

The last line now counts your backed-up model. **Backup coverage** — *N
of M contents have a copy on a registered drive* — is the number that
answers: "if this machine's disk died right now, what survives?" (In
the GUI it lives in the status bar, with a per-model ✓ column.)

✅ **Checkpoint:** status shows `1 of 4 contents have a copy on a
registered drive` (your counts may differ slightly).

🧠 **Check yourself:** (1) Name the three hash comparisons in a
verified copy. (2) Why copy to a `.partial` name first?

---

## Chapter 6 — Bit rot, and catching it

Disks decay. A sector weakens, one byte flips, and a file you haven't
opened in a year is silently corrupt — **bit rot**. Backup drives are
*most* at risk precisely because nothing ever reads them. Warden's
answer is **verify**: re-read every byte of a root and compare against
the catalog's hashes.

Cause bit rot yourself — flip one byte in the backup copy:

```sh
printf 'X' | dd of=~/warden-tutorial/drive/tinyllama-copy.gguf bs=1 seek=100 conv=notrunc status=none
wt verify "Practice Drive"
```

```
  FAILED tinyllama-copy.gguf: hash mismatch: bytes on disk are not f81cc974f445
ext-…: 0 ok, 1 mismatched, 0 missing, 0 unhashed
  MISMATCH: tinyllama-copy.gguf
```

One flipped byte in eight thousand — caught. Now heal it. **Repair**
re-copies the damaged file from any healthy copy elsewhere in the
catalog (your shelf still has one), replacing it atomically — the
corrupt copy is never deleted first:

```sh
wt verify "Practice Drive" --repair
wt verify "Practice Drive"     # 1 ok, 0 mismatched — healed
```

On a real machine you don't run verify by hand: `warden scrub install
--enable` schedules the **scrub** — a weekly background
`hash && verify --all` at idle disk priority. The ordering matters:
`hash` first means a file you *legitimately edited* just re-hashes and
passes; a file whose bytes changed while its fingerprint claims
otherwise is exactly the bit-rot signature, and it fails loudly.

✅ **Checkpoint:** verify failed with `1 mismatched`, then `--repair`
fixed it, then verify passed with exit 0.

🧠 **Check yourself:** (1) Why are backup drives more exposed to bit
rot than your working disk? (2) Where did `--repair` get the good bytes?

---

## Chapter 7 — Models are more than one file

Three shapes of multi-file model, one rule. **Split models** ship as
parts (`…-00001-of-00002.gguf`) — one part alone is useless. **Vision
projectors** (`mmproj-*.gguf`) sit beside multimodal models and are
required for image input. And safetensors models are whole directories
(**containers**) of weights + companions. Warden groups each into a
**bundle** — everything a model needs to run — and *every* operation
moves bundles, never fragments.

Build a split vision model, then back up "one" model:

```sh
mkdir -p ~/warden-tutorial/shelf/vision
mkgguf ~/warden-tutorial/shelf/vision/pixel-9B-00001-of-00002.gguf p1
mkgguf ~/warden-tutorial/shelf/vision/pixel-9B-00002-of-00002.gguf p2
mkgguf ~/warden-tutorial/shelf/vision/mmproj-pixel-F16.gguf pj
wt hash
wt backup ~/warden-tutorial/drive pixel-9B-00001
```

```
3 copied (11.8 KiB), 0 already on target, 0 failed → …/drive
```

You named one file; **three** traveled — both parts and the projector.
The relation is deliberately one-way: a projector is a **companion** of
its model (backing up the model brings it), but backing up *just* the
projector wouldn't drag the model along. In the GUI, companions appear
indented under their model with a "required by" note — the Inventory
shows models, not loose files. Downloads work the same way: fetching a
split part pulls the whole set plus one projector.

✅ **Checkpoint:** the backup reported `3 copied` from one name.

🧠 **Check yourself:** (1) Why must operations move bundles? (2) Which
direction does the model↔projector relation *not* work, and why is that
right?

---

## Chapter 8 — Cold storage

Models you want to *keep but not keep here*. **Demote** is a **verified
move** to a drive: verified copy first, recorded in the drive's carried
manifest, and only then — only because you said `--remove-source` — is
the shelf copy deleted. Deletion happens strictly *after* the bytes
provably exist elsewhere.

```sh
wt archive demote tinyllama-Q8 nano-embed --to "Practice Drive" --remove-source
wt where tinyllama-Q8
```

```
    [Practice Drive] tinyllama-Q8_0.gguf  — present
```

Both models left the shelf, but not the catalog — cold-stored, findable
forever, greyed as offline whenever the drive's unplugged. (In the GUI:
**Tools → Move to Cold Storage…** for checkbox bulk moves, and the
Inventory's **Active / All** toggle hides cold models from your working
view.) The return leg:

```sh
wt restore tinyllama-Q8
```

A verified copy back to the shelf; the drive keeps its copy — restore
never modifies the drive. The opposite direction, **promote**
(`wt archive <model>`), copies a *cache-owned* model onto the shelf so
no pruner can take it.

✅ **Checkpoint:** after demote, `where` showed only the drive; after
restore, shelf again too.

🧠 **Check yourself:** (1) What must happen before `--remove-source`
deletes anything? (2) Why does restore leave the drive untouched?

---

## Chapter 9 — Deleting without fear

Some models genuinely deserve to go. Warden's deletion is **two-stage**,
and the stages are separated on purpose:

**Stage 1 — `delete`:** the bundle is *renamed* into the root's
**trash** (`.modelwarden/trash/`) — instant even for 20 GB, fully
restorable, invisible to the catalog. Nothing is destroyed.

```sh
wt delete pixel-9B-00001
```

```
trashed pixel-9B-00002-of-00002 → …/shelf/.modelwarden/trash/vision/…
trashed pixel-9B-00001-of-00002 → …
trashed mmproj-pixel-F16 → …
6 files moved to trash (11.8 KiB). Nothing destroyed:
    undo:            warden trash restore <name>
    reclaim space:   warden trash empty --yes
```

Notice: the whole bundle went (both parts + projector), and *both* your
shelf and drive copies — delete means everywhere warden owns. Had
another surviving model needed that projector, it would have been
auto-kept and the output would say so. Foreign-store copies are never
touched; warden prints the owner's command for you instead.

```sh
wt trash                       # what's inside, sizes, ages
wt trash restore pixel         # the whole bundle comes back
wt delete nano-embed
wt trash empty --yes           # stage 2
```

**Stage 2 — `empty --yes`** is warden's *only* irreversible act: the
one place bytes stop existing, gated behind its own explicit flag (and
a count-and-size confirmation dialog in the GUI). Until you run it,
every delete is an undo away — and there is no auto-empty, ever;
destruction never happens on a schedule.

✅ **Checkpoint:** restore brought all six files back; `empty --yes`
reported `destroyed 1 files`.

🧠 **Check yourself:** (1) Why is stage 1 instant even for huge
models? (2) What is the only command in all of warden that destroys
bytes?

---

## Chapter 10 — The doctor and the journal

**`wt doctor`** is store health. Each **finding** comes with three
things: an explanation in plain words, *what fixing it loses*, and a
**remedy**. Remedies respect ownership: problems inside Ollama or the
HF cache are fixed by running *that tool's own command* (executed only
when you say `--fix`, and its success is verified rather than trusted);
a couple of provably byte-free cleanups warden does itself; real
orphaned bytes are always left to you, command printed.

Run it in the sandbox:

```sh
wt doctor
```

You'll likely see one finding: `stale verification` — the practice
drive holds files that were restored into it and never byte-verified
since. Doctor watches for exactly this: drives silently aging out of
trust (anything unverified for 90+ days gets named, with its at-risk
bytes and the verify command to run). Machine-level **advisories** like
this and the scrub-timer reminder make doctor the "am I actually
protected?" command, not just a cache-lint.

Now the memory. Everything you've done this tutorial was a **journal**
entry:

```sh
wt journal --all
```

Hashed, relinked, verified, FAILED, repaired, demoted, restored,
trashed, destroyed — the whole course, timestamped, in order. The
journal is plain text (`state/modelwarden/journal.log`), append-only,
written by both the CLI and GUI, and it survives every session: "what
did I do last Tuesday?" always has an answer. `cat` works; the file is
yours.

✅ **Checkpoint:** the journal reads like a diary of chapters 2–9.

🧠 **Check yourself:** (1) What three things does every doctor finding
carry? (2) Who fixes a problem inside the Ollama store, and who decides?

---

## Chapter 11 — Getting models (optional, needs network)

Warden also *acquires*. `fetch` downloads from a Hugging Face **repo**
(like `unsloth/GLM-4.5-Air-GGUF`) into your shelf, and everything you've
learned applies at download time: split sets download together
(refusing sets with holes), one projector rides along automatically,
dropped connections resume mid-transfer from the byte where they
stopped, and **provenance** — which repo, which **revision**, when — is
recorded at the only moment it's knowable. Try it with a genuinely tiny
real model (~17 MB total):

```sh
wt fetch prajjwal1/bert-tiny              # lists the repo's files
wt fetch prajjwal1/bert-tiny --snapshot   # safetensors-style: the directory is the model
wt where bert                             # note the 'origin:' provenance line
```

Two more pieces you'll meet in the wild: **gated repos** (license-walled
models) need a Hugging Face **token** — `--token` once with
`--save-token`, or `$HF_TOKEN`, or your `hf` CLI login, all honored —
and a 401 error means *either* "gated" or "no such repo id" (HF hides
which; warden suggests close matches for likely typos). In the GUI,
**Tools → Download from HuggingFace…** adds a filter box for 40-quant
repos and shows split models as one row with their true combined size.

✅ **Checkpoint:** `where bert` shows an `origin:` line naming repo and
revision.

---

## Chapter 12 — Graduation: your real machine

Delete the sandbox whenever you like — `rm -rf ~/warden-tutorial` (and
drop the `wt` alias). Everything transfers; only the training wheels
come off. On your real machine, plain `warden` uses your real config,
and the first-run sequence is:

```sh
warden scan                    # sanity-check what it can see
warden hash                    # the real catalog build — minutes, once
warden scrub install --enable  # the weekly bit-rot watchman
warden doctor                  # expect findings! a lived-in HF cache has debris
```

Three expectations for the real world: your first `doctor` on a
years-old cache *will* find things (interrupted downloads, pruned
leftovers — read the explanations; `--fix` handles the safe ones);
your `status` headline will probably say something uncomfortable about
backup coverage (that's the point — register a drive and fix it); and
from then on warden earns its keep in the background, with the scrub
verifying your bytes and the doctor nagging exactly when something
deserves it.

The habits that matter, in order: **register and label your backup
drive** → `warden backup` → glance at `status` occasionally → let
`doctor` tell you the rest. And the safety model you can now recite:
foreign stores are never written; every copy is verified end-to-end;
reclaim is hardlinking after re-verification; offline is not gone; and
bytes are destroyed only by `trash empty --yes`.

For day-to-day reference from here, the [User's Guide](users-guide.md)
covers every command and FAQ. Welcome aboard — and if anything in this
tutorial surprised you or went wrong, that's exactly what the
[issue forms](../../../issues/new/choose) are for.

---

## Appendix A — Glossary

- **Advisory** — a doctor finding about machine protection (scrub off,
  stale verification) rather than store damage.
- **Backup coverage** — how many models have a copy on a registered
  drive; `status`'s headline and the GUI's ✓ column.
- **Bit rot** — silent on-disk corruption, found by verify/scrub.
- **Bundle** — a model plus everything it needs to run (split parts,
  projector, companions); the unit every operation moves.
- **Cache** — a store another tool manages and may prune (HF hub).
- **Catalog** — warden's stored knowledge: manifests + inventory.
- **Companion** — a file that rides in a model's bundle (projector,
  tokenizer) without the reverse being true.
- **Container** — the directory that *is* a safetensors-style model.
- **Content identity / SHA-256** — a file's byte-hash; the only thing
  warden trusts to say two files are the same.
- **Demote / restore / promote** — verified move to cold storage /
  verified copy back / copy from a cache into the shelf.
- **Duplicate** — same hash, multiple names. **Reclaimable** when on
  the same filesystem in an owned root.
- **Fingerprint** — size+mtime+filesystem change-detector; never
  identity.
- **Finding / remedy** — a doctor problem + its explanation, loss
  statement, and fix.
- **Foreign store** — Ollama's store, the HF cache: read, reported,
  never written.
- **GGUF** — single-file model format with a self-describing header.
- **Hardlink** — two names, one set of bytes; warden's only space
  reclaim.
- **Inventory / manifest** — the merged catalog file / the per-root
  catalog files.
- **Journal** — the append-only, plain-text history of every write
  operation.
- **Label** — the human name you give a registered drive.
- **Offline-not-gone** — unreachable roots stay cataloged, marked
  OFFLINE.
- **`.partial`** — the temp name every in-flight copy uses.
- **Provenance** — where a download came from (repo, revision, when).
- **Quantization** — a model's compressed precision (Q4_K_M…).
- **Refuse-overwrite** — warden never replaces an existing file.
- **Registered root / root** — a drive/NAS you added with durable
  identity / any location warden watches.
- **Repo / revision / gated / token** — a HF model's home / its exact
  version / license-walled / your HF credential.
- **Scrub** — the scheduled background `hash && verify --all`.
- **Shelf** — the model directory *you* own; downloads land here.
- **Snapshot** — one revision of an HF repo; `--snapshot` downloads a
  whole one.
- **Split model** — a GGUF shipped in `-NNNNN-of-NNNNN` parts.
- **Store** — any directory where models live.
- **Trash / empty** — deletion stage 1 (restorable rename) / stage 2
  (the only irreversible act).
- **Verified copy / verify** — the three-way-hash copy discipline /
  re-reading a root against the catalog.

## Appendix B — Command ↔ chapter map

`scan` `hash` `status` (2) · `dups` `dedup` (3) · `roots add` `where`
`roots forget` (4) · `backup` (5, 7) · `verify [--repair]`
`scrub install` (6) · `archive demote` `restore` `archive` (8) ·
`delete` `trash` (9) · `doctor` `journal` (10) · `fetch` (11).

## Appendix C — Check-yourself answers

**Ch1:** (1) Ollama's store and the HF cache — owned by those tools,
warden never writes inside them. (2) Quantizations: one model, several
precision/size trade-offs. **Ch2:** (1) Two different files can share a
name and one file can be renamed; only the bytes are the model. (2)
Skip re-hashing; it may never declare two files identical. **Ch3:** (1)
Hardlinks can't cross filesystems — and a second-device copy is
intentional redundancy. (2) Re-hashed both files, because the catalog
could be stale. **Ch4:** (1) Filesystem UUID + the marker file. (2)
Offline is remembered and expected back; forgotten is knowledge
deliberately removed. **Ch5:** (1) Catalog vs. source-read; source vs.
destination read-back; (the third comparison is the read-back against
the catalog — all three must agree). (2) So a crash mid-copy can never
leave a plausible-looking model. **Ch6:** (1) They sit unpowered and
unread — no chance to notice decay. (2) From a healthy copy elsewhere
in the catalog (your shelf). **Ch7:** (1) A fragment (one split part, a
model without its projector) isn't a working model. (2)
Projector→model; you may legitimately want only the projector. **Ch8:**
(1) The drive copy's read-back hash must match the catalog. (2) The
drive is the backup — restore is a copy, not a move. **Ch9:** (1) It's
a same-filesystem rename, not a copy. (2) `trash empty --yes` (the
GUI's Empty Trash confirm is the same act). **Ch10:** (1) Explanation,
loss statement, remedy. (2) The owning tool's own CLI does the fixing;
you decide, explicitly, every time.

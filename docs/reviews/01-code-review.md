# modelwarden — code review (correctness & robustness)

*Reviewed at v0.4.2 (`98ede8e` + post-tag fixes), 12,261 lines across
`src/core`, both binaries, and `tests/`. Every finding below was verified
against the source; the ones marked **proven** were reproduced live on
this machine. Findings are ordered for execution — see
[Fix order](#fix-order) — not by discovery.*

**Verdict up front.** The safety *design* is genuinely strong: bundle
semantics, three-way verified copies, the two-stage trash, owner-mediated
cleanup, and the never-trust-exit-codes discipline are all better than
this category of tool usually manages. The *implementation* does not yet
live up to that design in four specific places: warden's atomic writes
are not atomic, its single-writer lock does not guarantee a single
writer, its refuse-overwrite guarantee is a racy check rather than an
atomic operation, and a corrupt or hostile manifest can crash it or
steer it. Those four are the review. Everything else is ordinary debt.

Severity key: **S1** = can lose or corrupt user data, or crash a
scheduled operation. **S2** = wrong results the user would act on.
**S3** = real but bounded. **S4** = hygiene.

---

## S1 — data loss, corruption, or crash

### C1. `save_json` is not atomic; the manifest can vanish
`src/core/manifest.rs:162-174`

```rust
let tmp = path.with_extension("json.tmp");
std::fs::write(&tmp, ...)?;
if path.exists() {
    std::fs::rename(path, &bak)?;   // ← window opens
}
std::fs::rename(&tmp, path)?;       // ← window closes
```

Between those two renames there is **no file at `path`**. A crash, a
power loss, or a kill in that window leaves the root's manifest missing.
`load_all_manifests` only reads `*.json`, so the `.bak` is not a
fallback — the root silently reverts to "never catalogued" and every file
in it is rehashed from zero. PLAN.md claims "Manifest writes atomic
(temp+rename)"; they are not.

This is not a theoretical window. The hash checkpoint
(`manifest.rs:528-535`) calls `save_json` **once per hashed file**, so a
600-file first catalog opens and closes this hole 600 times, each time
also churning the `.bak`.

The fix is to delete the `.bak` dance: `rename(tmp, path)` alone is
atomic and replaces the destination. If a backup copy is genuinely
wanted, copy the old file to `.bak` *before* writing `tmp`, never by
moving the live file out of the way. Also `fsync` the parent directory
after the rename if durability across power loss is meant to be real.

### C2. The write lock does not guarantee a single writer
`src/core/lock.rs:28-63`

Two independent races:

1. **Stale-steal collision.** On finding an existing lock whose pid is
   dead, the code does an unconditional `remove_file(&path)` and loops to
   `create_new`. Two wardens that both observe the same stale lock will
   both remove *whatever is at that path* — including the lock the other
   one just successfully created — and both will then create their own.
   Both believe they hold it.
2. **Create-then-write gap.** `create_new` and `writeln!(f, "{pid}")` are
   separate syscalls (`lock.rs:34-36`). A second warden that reads the
   file in between sees empty content, parses `None`, takes the `_ =>`
   branch, and deletes a **live** lock.

Compounding both: `Drop` (`lock.rs:78-82`) removes the path
unconditionally, so a process whose lock was stolen deletes the thief's
lock on exit.

Consequence: a backup and a `dedup --hardlink` can run concurrently over
the same catalog. Given the whole point of the lock, this is the most
serious structural defect in the codebase.

The fix is the standard one: `O_CREAT|O_EXCL` a file that already
contains the pid (write to `warden.lock.<pid>` then `link()`/`rename`
into place), and — better — take an `flock(LOCK_EX|LOCK_NB)` on the
opened descriptor so the kernel arbitrates and death releases it
automatically. `flock` alone removes the entire stale-detection problem,
the pid-reuse problem, and both races.

### C3. Panic on a short or non-ASCII hash string
`src/core/backup.rs:460`, `src/bin/warden.rs:302,561,609`,
`src/bin/warden-gui/main.rs:1170,1251`

```rust
error: format!("hash mismatch: bytes on disk are not {}", &expected[..12])
```

`expected` comes from a manifest on disk — either warden's state dir or,
for a removable drive, **a file the drive itself carries**. Byte-slicing
a `str` at a fixed index panics when the string is shorter than 12 bytes
*or* when byte 12 is not a UTF-8 character boundary. A hand-edited,
truncated, or corrupted manifest containing `"sha256": "abc"` turns
`warden verify` — the command the weekly scrub timer runs — into a
panic.

Every one of these sites should use `expected.get(..12).unwrap_or(&expected)`
or `chars().take(12)`. Note `warden.rs:572` already does this correctly
(`[..12.min(r.len())]`), which shows the pattern was known and applied
inconsistently.

### C4. A corrupt config is silently replaced by defaults — and then saved
`src/core/settings.rs:57-62`

```rust
std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
```

A single malformed byte in `config.json` is indistinguishable from "no
config": warden silently continues with `scan_dirs = ~/models`,
`discover_stores = true`, and **`roots = []`**. Every registered drive is
forgotten. The next operation that calls `cfg.save()` — `roots add`, a
backup that registers its target, the Settings dialog, `--save-token` —
then **writes the defaults back over the user's real config**,
destroying the registration list permanently.

`save()` (`settings.rs:64-69`) is also a bare `fs::write`: a crash
mid-write truncates the file and produces exactly the corrupt state that
triggers the above.

Parse failure must be distinguished from absence: fail loudly, refuse to
save over a file that failed to parse, and write config through the same
atomic helper as manifests.

### C5. Symlinked shelf models are recorded with the symlink's size — **proven**
`src/core/scan.rs:291-320` vs `src/core/identity.rs:28-37`

`walk_gguf` takes `file_size` from `DirEntry::metadata()`, which does
**not** follow symlinks. `Fingerprint::of` uses `std::fs::metadata`,
which does. `sha256_file` hashes the target. The same `FileRecord`
therefore describes two different files. Reproduced here with a 5 MB
model symlinked into a shelf:

```
FileRecord.size   = 114          ← the symlink inode
fingerprint.size  = 5000024      ← the actual model
warden scan       → "linked  114 B  present"
warden delete     → "1 files moved to trash (114 B)"
```

Every size the user sees for such a model is wrong by four orders of
magnitude: `status` totals, `report` usage, the backup dialog's size
preview, the delete confirmation, the trash's "reclaimed" figure, and the
stale-verification advisory's "at stake" bytes. `hf_entry`
(`scan.rs:177-183`) uses following-metadata, so the shelf and the HF
cache disagree about the same question.

Pick one policy and apply it in all three scanners. Following is the
right choice (the model's bytes are what matter), plus a symlink-loop
guard.

### C6. One I/O error aborts an entire verify
`src/core/backup.rs:424-450`

`let actual = identity::sha256_file(&abs, ...)?;` — the `?` propagates
out of `verify`, discarding the whole `VerifyReport` and every
`verified_unix` update for the files already checked. A single
permission-denied file or one bad sector on a backup drive means the
scrub reports nothing useful and re-reads everything next week.

An unreadable file is a *finding*, not a reason to stop: record it
(a third bucket alongside `missing`/`mismatched`) and continue.

---

## S2 — wrong answers

### C7. "Refuse-overwrite" is a TOCTOU check, not a guarantee
`src/core/backup.rs:129` + `:293`, `src/core/acquire.rs:367`,
`src/core/trash.rs:286`

Every one of these does `if dest.exists() { refuse }` and later
`std::fs::rename(tmp, dest)` — and `rename` on Unix **silently replaces**
the destination. Anything that appears at `dest` between the check and
the rename is destroyed without a word. The stated invariant
("Refuse-overwrite everywhere") is enforced only against files that
existed when the operation started.

`renameat2(RENAME_NOREPLACE)` (Linux ≥3.15) makes the guarantee real;
`link()` + `unlink()` is the portable fallback. Given that
refuse-overwrite is one of the five advertised safety rules, it deserves
to be an actual atomic operation.

### C8. `with_extension` mangles temp names for non-GGUF files
`src/core/backup.rs:225`, `src/core/dedup.rs:206`

```rust
let tmp = dest.with_extension("gguf.partial");     // backup
let tmp = victim.with_extension("gguf.wardenlink"); // dedup
```

`with_extension` *replaces* the extension. Since M12 these paths handle
safetensors bundles, so `config.json` becomes `config.gguf.partial` and
`model.safetensors` becomes `model.gguf.partial`. Two files in one
directory that differ only by extension (`model.bin`, `model.json`) map
to the **same temp path**, and `File::create` truncates it without
complaint. A leftover `.wardenlink` from a crashed run permanently blocks
that file's dedup, since `hard_link` fails on an existing target with no
cleanup path.

`acquire::partial_path` (`acquire.rs:349-355`) already solves this
correctly. Promote it to a shared helper and use it in all three places.

### C9. `is_backed_up` ignores accessibility
`src/core/manifest.rs:274-280`

The backup-coverage headline — the number the whole GUI status bar and
`warden status` are built around — counts any `Removable` location
regardless of `accessible`. A file whose drive copy is present in the
manifest but unreadable (dangling link, permission change) still reports
✓. The docstring justifies counting *offline* drives, which is right;
counting *known-unreadable* files is not the same thing.

### C10. The snapshot scanner and the snapshot downloader disagree about dotfiles
`src/core/scan.rs:198-222` vs `src/core/acquire.rs:301-308`

`snapshot_set` documents itself as excluding dotfiles "mirroring the
scanner". `collect_snapshot` has no such filter — `walk_gguf` and
`emit_dir_as_model` skip dot-entries, but the HF snapshot walker does
not. A safetensors repo therefore catalogs `.gitattributes` and friends
when scanned, but a repo fetched with `--snapshot` does not contain
them. The same model has different contents depending on how it arrived.

### C11. `demote --remove-source` is the one deletion with no undo
`src/core/archive.rs:174`

`std::fs::remove_file(&src)` destroys the shelf copy outright. Since M15
warden has a restorable trash; routing the source through it would make
the "verified move" recoverable if the destination drive fails minutes
later, at essentially zero cost. Today `delete` is undoable and
`demote --remove-source` is not, which is the wrong way round given the
second one is advertised as the safe operation.
**Done, 2026-08-31.** The per-file trash move was extracted as
`trash::trash_one` and both paths use it; `DemoteOutcome::removed_source`
is now `trashed_source`, and the CLI, the GUI, the README, the users
guide and the tutorial all say where the copy went. `trash empty`
remains the only thing that destroys bytes.

---

## S3 — real but bounded

### C12. Symlink loops are walked repeatedly, inflating the trash listing
`src/core/trash.rs:200-221`, `src/core/scan.rs:372-386`

`trash::walk` recurses with no depth cap and follows symlinks via
`p.is_dir()`; `ollama_models` walks an explicit stack and follows them
too. A symlink pointing at an ancestor is therefore walked repeatedly.
`collect_snapshot`, `walk_gguf` and `emit_dir_as_model` are capped at
depth 3 and are safe.

**Correction (verified while fixing, 2026-08-31).** The heading and
severity here were revised. This was first written up as a stack overflow / unbounded loop. It is neither: the
kernel refuses to resolve a path with more than ~40 symlink components
(`ELOOP`), so the walk terminates on its own. The real defect is a
*wrong answer*, measured with the pre-fix walk against a trash directory
holding one file and one loop link: **42 entries listed for that one
file**, the deepest at `deep/loop/…/deep/kept.gguf`. That is 42× its
bytes in the reclaim figure the Empty-Trash confirmation shows, and 42
redundant manifest parses on the Ollama side. Downgrade the severity;
keep the fix.

### C13. The depth-3 scan cap hides models silently
`src/core/scan.rs:292`, `:322`, `:199`

A model nested four directories deep in a shelf is invisible, and nothing
tells the user. For a tool whose entire purpose is "know what you have",
silently not-knowing is the worst failure mode. Either raise the cap
substantially, or report directories that were skipped for depth.
**Done, 2026-08-31**: one `MAX_DEPTH = 16` shared by all three walkers.
Raised rather than reported — the deepest real shape warden handles is
four or five levels (an HF snapshot with per-quant subfolders and a
`1_Pooling/` companion), so at sixteen the cap only ever stops something
pathological, and a report about a limit nothing reaches is noise.

### C14. Wrong string's length in a slice bound
`src/core/roots.rs:119`

```rust
Some(u) => format!("ext-{}", &u.replace('-', "").to_lowercase()[..8.min(u.len())]),
```

The slice is on the *dash-stripped* string; the bound is the length of
the *original*. A UUID with many dashes (or any non-ASCII byte) panics.
`warden roots add` is the entry point.

### C15. `dup_groups` and `family_usage` disagree about liveness
`src/core/manifest.rs:749` vs `:713`

One filters on the stale stored `accessible` flag, the other on
`live_accessible()`. Since the CLI reads a serialized inventory, the
stored flag reflects the last `hash`, not now. Two views of the same
catalog will disagree about the same drive.

### C16. `exists()` treated as proof in the owner-command verifier
`src/core/doctor.rs:375-390`

`expect_gone.exists()` returns `false` when the path exists but is
unreadable, so an owner command that did nothing can still be reported
as success — the precise failure mode this check was added to prevent.
Use `symlink_metadata().is_err()` and distinguish `NotFound` from other
errors.

---

## S4 — hygiene

- **C17.** `thiserror` is declared in `Cargo.toml:25` and never used
  anywhere in `src/`. Dead dependency: compile time and audit surface for
  nothing. **Removed, 2026-08-31.**
- **C18.** *(Done, 2026-08-31 — one `log()` keeps the last ~500 lines.)*
  `src/bin/warden-gui/main.rs`: `self.activity` has 15 push
  sites and no cap. A long session grows it without bound and re-lays
  out every line every frame (see the efficiency review, E2).
- **C19.** `src/core/archive.rs:74` `unreachable!("guarded above")` — a
  panic keyed to a guard twenty lines away. Restructure so the compiler
  enforces it. **Done, 2026-08-31**: the arm asks the same question the
  guard asks and returns an error instead of aborting. The compiler
  cannot prove the relationship here without restructuring the location
  search itself, so the fix is to stop punishing a reader's memory with a
  panic, not to pretend the invariant is type-level.
- **C20.** No `[profile.release]` in `Cargo.toml`: no LTO, no
  `codegen-units = 1`, no `strip`. Shipped binaries carry symbols
  (measured: `warden` 6.33 MB → 4.92 MB stripped; `warden-gui`
  34.43 MB → 25.72 MB). **Done, 2026-08-31** — see efficiency E13 for the
  after-the-fact numbers (27% and 33%).

---

## What is genuinely good

Worth recording so the above is read in proportion:

- `copy_verified` (`backup.rs:213-294`) does hash-as-read, `sync_all`,
  read-back, and only then renames. That is a real verified copy, and the
  `sync_all` is not something most implementations remember.
- The owner-command `expect_gone` discipline, and the refusal to trust an
  exit code, is unusually honest engineering.
- `verify_husk` (`doctor.rs:437`) re-proves emptiness at apply time and
  refuses symlinks — textbook.
- `dedup::relink` canonicalises the survivor before linking, with the
  reason recorded in a comment. That bug is easy to ship and hard to find.
- `acquire::dest_for` is the one path-validation in the codebase and it
  is correct. The problem is that it is the *only* one (see the security
  review).
- 101 tests, whole-binary E2E in CI on two OSes, and a genuinely
  test-first recent history.

---

## Fix order

Ordered so that shared foundations land before their dependants; each
group is independently shippable.

**Group 1 — primitives (unblocks 5 findings).**
1. **C1** atomic `save_json` (single rename, optional pre-copy backup,
   dir fsync). Also the prerequisite for fixing **C4**'s save path.
2. **C7** a `rename_noreplace` helper (`renameat2`, `link`+`unlink`
   fallback). Used by backup, acquire, trash restore, dedup.
3. **C8** promote `acquire::partial_path` to a shared `temp_sibling`
   helper; consume it in `backup::copy_verified` and `dedup::relink`.
   Depends on nothing; do it alongside C7 since both touch the same lines.

**Group 2 — the lock (independent, highest structural risk).**
4. **C2** replace the pid-file protocol with `flock` on an
   `O_CREAT|O_EXCL`-opened file. This deletes the stale-detection code
   entirely, so do it before anyone builds more on top of it.

**Group 3 — crash and config safety (cheap, high value).**
5. **C3** every `[..12]` on a hash → checked slice. Mechanical; do it in
   one pass with a grep.
6. **C4** config: distinguish parse-failure from absence; refuse to save
   over an unparsed config; route through the Group-1 atomic writer.
7. **C14** the `roots.rs:119` slice bound.

**Group 4 — truth about sizes and state.**
8. **C5** one symlink policy across all three scanners (follow +
   loop guard), then re-hash to correct existing manifests.
9. **C9**, **C15** make `is_backed_up`/`dup_groups`/`family_usage` agree
   on one liveness predicate. Do after C5 so all three read the same
   corrected data.
10. **C10** dotfile filter in `collect_snapshot`.

**Group 5 — robustness.**
11. **C6** verify continues past I/O errors (new `unreadable` bucket).
12. **C12** depth cap, and stop descending through symlinked
    directories, in `trash::walk` and `ollama_models`. (A visited-inode
    set was the original proposal; refusing to follow linked directories
    at all is simpler and is the right policy for these two trees —
    warden's own trash and another tool's own store.)
13. **C13** raise or report the scan depth limit.
14. **C16** `exists()` → `symlink_metadata()`.

**Group 6 — design and hygiene.**
15. **C11** route `demote --remove-source` through the trash (a user
    decision as much as a code one).
16. **C17**, **C19**, **C20**, **C18**.

Groups 1–3 are the ones that matter. Everything from Group 4 down is
quality-of-implementation; Groups 1–3 are the difference between
warden's promises being true and being intended.

# modelwarden — efficiency review

*Reviewed at v0.4.2. Measurements below were taken on this machine
(44 catalogued contents, 419.8 GiB, NVMe + a 24-thread CPU) with the
release binary unless stated. Complexity claims were read out of the
source, not guessed; projections say so explicitly.*

**Verdict up front.** Nothing here is slow *today* — 44 models is small
enough to hide every algorithmic sin in the codebase, and the one place
real work happens (hashing) was already parallelised and is close to
disk-bound. But the catalog is the product, and the catalog grows: a
single safetensors repo contributes a dozen entries (`bge-small` alone
added 11), so a user with a normal HF cache and a few hundred models
lands at 1,000–3,000 entries. At that size three specific things become
unusable, and they are all the same root cause: **`bundle_for` is O(n·L)
and it is called inside O(n) loops, one of which runs on every GUI
frame.** Fix that one function's call pattern and most of this document
disappears.

Measured baseline, so later claims are anchored:

| Operation | Wall | User CPU | Syscalls |
|---|---|---|---|
| `warden scan` (release) | 0.553 s | 0.526 s | ~356 |
| `warden scan` (debug) | 1.985 s | 1.904 s | — |
| `warden status` | 0.004 s | 0.002 s | — |

Note the shape: `scan` spends **95% of its time in userspace with 27 ms
of kernel time**. This is not an I/O-bound scanner; it is a CPU-bound one
(E4 explains why), and that CPU is spent again on every write command
(E3).

Severity: **P1** = becomes a wall at realistic catalog sizes. **P2** =
measurable waste now. **P3** = tidy-up.

---

## P1 — scaling cliffs

### E1. The companion/split relations are recomputed on every frame, at O(n²·L) each
`src/bin/warden-gui/main.rs:812,816` → `src/core/manifest.rs:292-341`,
`:575-637`

```rust
let parents_of = manifest::companion_parents(inv);   // every frame
let split_of  = manifest::split_primary_of(inv);     // every frame
```

`bundle_for` (`manifest.rs:575`) iterates **every model and every one of
its locations** for each call — O(n·L). `companion_parents` calls it once
per model *and then again for every member of every bundle*, so it is
O(n²·L) with a large constant (string compares, `PathBuf` joins,
`to_lowercase` inside `is_projector_name`). `split_primary_of` repeats
the pattern.

egui is immediate-mode: this runs on every repaint, which means every
mouse move over the window, every scroll tick, every keystroke in the
filter box, and continuously at 4 Hz while any job is running
(`request_repaint_after(250ms)`). Precisely when responsiveness matters.

Order of magnitude, with L ≈ 1.5 locations per model:

| Catalog size | Location comparisons per frame |
|---|---|
| 44 (today) | ~10⁴ — invisible |
| 500 | ~10⁷ — visible stutter while scrolling |
| 2,000 | ~10⁹ — the tab is unusable |

**Measured, 2026-08-31** (synthetic catalog, split-part filenames, four
models per directory; `companion_parents` + `split_primary_of`, the pair
the inventory tab wanted on every repaint):

| Catalog size | before | after the index |
|---|---|---|
| 100 | 0.69 ms | 0.28 ms |
| 500 | 14.7 ms | 0.99 ms |
| 2,000 | 233.7 ms | 4.5 ms |

The 500-model figure is the interesting one: 14.7 ms per frame is
already over a 60 Hz frame budget on its own, so the stutter predicted
below starts well before the catalog gets large. Both fixes landed —
the index, and the cache — because 4.5 ms per repaint would still be
the most expensive thing in the frame.

**Fix:** compute both relations once when the inventory changes
(`set_inventory`, `main.rs:214-228`, which already does `dup_groups` and
`family_usage` there) and cache them beside `self.inv`. That is a
five-line change and it is the single highest-value item in this
document. The deeper fix — worth doing at the same time — is to give
`bundle_for` an index: build `container → [keys]` and `ollama_base →
[keys]` maps once per inventory, turning each `bundle_for` from O(n·L)
into O(bundle size).

### E2. No virtualisation anywhere; the activity log is unbounded
`src/bin/warden-gui/main.rs:975` (inventory), `:1702` (trash), `:1330`
(health), `:2728` (activity), and every `ScrollArea::…::show`

Every `egui::Grid` and every `ScrollArea` in the GUI uses `.show()`,
which lays out **all** children every frame. egui provides
`ScrollArea::show_rows` for exactly this. At 44 rows it costs nothing; at
2,000 rows (see above — one busy HF cache) the Inventory tab is laying
out 2,000 rows × 7 cells with per-row string formatting on every repaint,
on top of E1.

Independently, `self.activity` is pushed to from 15 sites and **trimmed
by none**. A long-running session (a snapshot fetch, a large hash) grows
it without limit and re-lays out every historical line every frame. The
journal is the durable record now, so the in-memory log can safely keep
the last ~500 lines.

**Status, 2026-08-31.** The activity cap landed: every one of the fifteen
push sites now goes through one `log()` that keeps the last ~500 lines,
and the journal remains the durable record.

**The grid virtualisation is deliberately deferred.** `ScrollArea::
show_rows` wants a uniform row height and a flat row count; the
inventory is an `egui::Grid` whose column widths adapt to the content
actually laid out, so feeding it only the visible rows makes the columns
jitter as you scroll. Doing it properly means `egui_extras::TableBuilder`
— a new dependency and a rewrite of the pane — and the result is a visual
change that no test here can verify. It is worth doing when someone can
look at the window; it should not be done blind. Real cost today: 57
rows. The threshold where it matters is the same few-hundred-row mark as
E1, which is now fixed.

**Fix:** `show_rows` for the inventory and trash grids; cap `activity` at
a few hundred entries with a `VecDeque`.

### E3. Every write command triggers a full rescan of every root
`src/bin/warden.rs:765-771` (`rerun_hash_quietly`, 9 call sites);
`src/bin/warden-gui/main.rs:251-262` (`refresh_catalog`, on every job)

Deleting one model, restoring one file, or emptying the trash re-walks
the shelf, the whole Ollama store, and the entire HF cache, and re-parses
the GGUF header of every file found (E4). Measured cost of that scan on
this machine: **0.53 s of pure CPU for 44 models**, and it is called
after *every* write.

The information needed is usually tiny — "this path is gone", "this path
is new". A targeted update (patch the affected root's manifest, re-merge)
would be milliseconds. Failing that, the rescan should at least be
incremental per root rather than global: `warden delete` only ever
touches owned roots, yet it re-scans the HF cache too.

**Closed by E4, 2026-08-31 — not fixed on its own terms, and that is
deliberate.** The premise above was the 0.53 s. `rerun_hash_quietly` and
the GUI's `refresh_catalog` both call `manifest::refresh`, which is the
path E4 fixed: the full post-write rescan of all five roots on this
machine now costs **0.02 s**, measured. Targeted manifest patching would
buy back at most those 20 ms and would introduce a class of bug warden
cannot afford — a catalog that disagrees with the disk because an update
was scoped too narrowly. The cheap half of the suggestion (refresh only
owned roots after a delete) has the same hazard for the same ~15 ms: it
would build the merged inventory from a partially refreshed set. Left
alone on purpose.

### E4. GGUF headers are re-parsed on every scan, forever
`src/core/scan.rs:307` and `:349`, consumed by
`manifest.rs:56-101`

`build_root_manifest` carefully carries `sha256` and `verified_unix`
forward when the fingerprint is unchanged — and then throws away the
`meta` it already had and re-derives it by opening and parsing every
GGUF header again. That parse is not free: it walks the whole KV block,
skipping tokenizer arrays 8 KiB at a time (`gguf.rs:160-168`), which is
why `scan` burns 0.53 s of CPU on 44 files (~12 ms each) with almost no
kernel time.

**Fix:** carry `meta` forward exactly like `sha256`. It is not three
lines: the metadata is produced by the *scanner*, which has already
opened the file by the time `build_root_manifest` sees it, so the prior
manifest has to reach the scanners. They now take a `MetaCache` — "do
you already know this file's header?" — that `build_root_manifest`
answers from the fingerprint it was going to check anyway.

**Measured, 2026-08-31**, on this machine's real catalog (57 files, 576
GiB across five roots, warm cache, release build, alternating binaries):

| | `warden hash` on an unchanged catalog |
|---|---|
| before | 0.60 s real, 0.54 s user |
| after | 0.02 s real, 0.00 s user |

That is the path every write takes on the way back (the GUI rescans
after each operation) and the first half of the weekly scrub. **The
claim above about `warden scan` was wrong**: `scan` is a standalone
scanner with no manifest to carry anything forward, by design, and it is
unchanged at 0.57 s. E3's sting is reduced for the same reason the
refresh got faster, not because `scan` improved.

### E5. `verify` is single-threaded while `hash` uses four cores
`src/core/backup.rs:424-448` vs `manifest.rs:470-545`

`refresh` builds a worker pool; `verify` re-reads every byte on one
thread. The weekly scrub is `hash && verify --all`, so the scheduled job
that exists to re-read the entire collection runs the second half at ¼
speed. On 420 GiB at ~700 MB/s single-core that is ~10 minutes of
avoidable wall time per scrub.

**Fix:** reuse the pool shape from `refresh` — the job list is already a
flat `Vec<FileRecord>`; only the result write-back needs to stay on the
calling thread. (Note E5 must land *after* code-review C6, which changes
the error handling in the same loop.)

---

## P2 — measurable waste today

### E6. The hash checkpoint rewrites the entire manifest per file
`src/core/manifest.rs:528-535`

Serialising the whole root manifest to pretty JSON, writing it, renaming
the previous one to `.bak`, and renaming the temp into place — once per
hashed file. For a 600-file root that is 600 full serialisations, 1,800
renames, and O(n²) bytes written over the run. It also multiplies
exposure to the atomicity hole in code-review C1 by the file count.

**Fix:** checkpoint on a timer (every ~5 s) or every N files, and always
on completion. The resume guarantee is unchanged in practice.

### E7. O(n·m) membership scan when merging a carried manifest
`src/core/manifest.rs:423-425`

```rust
let known: Vec<PathBuf> = s.files.iter().map(|f| f.rel_path.clone()).collect();
s.files.extend(c.files.into_iter().filter(|cf| !known.contains(&cf.rel_path)));
```

`Vec::contains` with `PathBuf` comparison, inside a filter over the
carried manifest. Quadratic in the number of files on the drive, plus a
full clone of every path to build `known`. A `HashSet<&Path>` over
borrowed paths removes both.

### E8. Redundant `stat` calls throughout the scanners
`src/core/scan.rs:177-183`, `:251-259`, `:317`, `:294`, `:301`

- `hf_entry` calls `std::fs::metadata(path)` **twice** — once for
  `accessible`, once for `file_size`. One call answers both.
- `scan()`'s inode-dedupe pass (`:249-259`) stats **every model again**,
  after the walkers already had `DirEntry` metadata in hand.
- `path.is_dir()` / `p.is_file()` inside the walk loops are extra stats;
  `DirEntry::file_type()` is free on Linux (`readdir` already returned
  `d_type`).

Individually small; collectively this is the difference between one stat
per file and three or four, on a tree that can hold tens of thousands of
entries (an HF cache with blobs and snapshots).

**Fixed and measured, 2026-08-31.** `ModelFile` now carries the
`Fingerprint` from the single stat the walker already did, so the
inode-dedupe pass and `build_root_manifest` both read it instead of
stat'ing again. Counted with `strace -c` on this machine's real stores:
`warden scan` 251 → 224 stat calls, `warden hash` 365 → 287. Wall time
is unchanged (0.56 s → 0.55 s on `scan`), exactly as "individually
small" predicts; the value is at HF-cache scale, not here.

**Not done: `DirEntry::file_type()` in the walk loops.** `file_type()`
comes from `readdir` and does not follow symlinks, while `path.is_dir()`
does — and a shelf of symlinks into a big drive is a layout warden
supports (see code-review C5). Swapping them would silently stop the
shelf walker descending through a symlinked directory. Saving one stat
is not worth changing what gets found.

### E9. The journal opens, writes, and closes a file per line — on the UI thread
`src/bin/warden-gui/main.rs:575,596,604` → `src/core/journal.rs:25-35`

Every durable activity line does `create_dir_all` + `OpenOptions::open` +
`writeln!` + close, synchronously inside `drain_messages`, which runs on
the render thread. A 600-file hash is 600 of those cycles interleaved
with frame rendering.

**Fix:** hold the appender open for the session (or batch per drain),
and drop the `create_dir_all` after the first success.

### E10. Sort comparators allocate
`src/bin/warden-gui/main.rs:800-830` (inventory sort),
`src/core/scan.rs:260-269` (scan sort)

The inventory comparator calls `quant_of` (clones a `String`) and
`where_of` (**builds a `Vec`, sorts it, dedups it, and joins it into a
new `String`**) for *each comparison* — O(n log n) allocations per sort,
and the sort itself runs every frame. `scan`'s comparator calls
`display_name()` (allocates) and `.to_lowercase()` (allocates) twice per
comparison.

**Fix:** decorate-sort-undecorate — build the key tuple once per row,
sort the keys.

### E11. Double buffering in the hasher
`src/core/identity.rs:46-49`

A 4 MiB `BufReader` wrapping the file *and* a separate 4 MiB read buffer.
`BufReader` bypasses its own buffer for reads ≥ capacity, so the 4 MiB
allocation is pure overhead — 8 MiB resident per hashing thread, 32 MiB
across the pool, for no benefit. Read directly from the `File`.

### E12. `restore_set` re-walks the entire trash per restored file
`src/core/trash.rs:231-260` (called per match in
`src/bin/warden.rs:1250-1262` and per click in the GUI)

Restoring a 3-part bundle walks the whole trash tree three times, then
`trash::restore` prunes empty dirs with another full walk. Walk once,
pass the listing.

### E13. No release profile
`Cargo.toml` — no `[profile.release]` section

Defaults mean no LTO, `codegen-units = 16`, and **no symbol stripping**.
Measured on the shipped binaries:

| Binary | As shipped | Stripped | Waste |
|---|---|---|---|
| `warden` | 6.33 MB | 4.92 MB | 22% |
| `warden-gui` | 34.43 MB | 25.72 MB | 25% |

Every release tarball and every `cargo install` carries ~9 MB of debug
symbols nobody asked for. Adding `lto = "thin"`, `codegen-units = 1`, and
`strip = "symbols"` typically also buys 5–15% on the CPU-bound paths
(header parsing, JSON serialisation) for free.

**Measured after the change, 2026-08-31.** The size win is real and
slightly better than predicted — `warden` 6.45 MB → 4.70 MB (27%),
`warden-gui` 34.46 MB → 23.11 MB (33%). **The CPU claim is not borne
out**: `warden scan`, the most header-parse-heavy path there is, went
0.57 s → 0.565 s, which is noise. Keep the profile for the size; do not
credit it with speed.

---

## P3 — tidy-up

- **E14.** Hex encoding via `format!("{b:02x}")` in a loop — 64 `String`
  allocations per hash — implemented **twice**
  (`identity.rs:62-67`, `backup.rs:297-303`). One shared function writing
  into a pre-sized `String` with `write!`, or a 512-byte lookup table.
- **E15.** `backup()` scans `man.files` linearly for each catalog entry
  (`backup.rs:106`) — O(n·m) on a drive with many files. Build a
  `HashSet<&str>` of hashes once.
- **E16.** The GUI's job queue (`VecDeque<Box<dyn FnOnce>>`) and the
  `Msg` channel are both unbounded. Fifty impatient clicks on "Update
  Catalog" queue fifty full rescans; a fast hash of many small files can
  outrun the 4 Hz drain and pile up messages. Coalesce duplicate job
  labels; drop `Progress` messages older than the newest.
- **E17.** `Inventory::live_accessible` calls `self.root()` (a linear
  scan of `roots`) and then `path.exists()` (**a syscall**) for every
  location, and it is called inside per-frame loops and inside
  `family_usage`/`dedup`. At 2,000 models that is thousands of `stat`
  calls per frame. Cache root liveness once per inventory load.
  **Measured 2026-08-31**: asking the predicate for every location cost
  1.11 ms per frame at n=2,000; with the cache, 42 µs. Landed as a
  `OnceLock` on `Inventory`, deliberately outside the value (`PartialEq`
  is hand-written) and never serialized.

---

## Fix order

Grouped so shared work lands once; each group is independently
shippable and testable.

**Group 1 — the scan path (biggest measured win, no dependencies).**
1. **E4** carry `meta` forward on unchanged fingerprints. Alone this
   should take `scan` from 0.53 s of CPU to near-zero on a warm catalog.
2. **E8** collapse the redundant `stat`s (`hf_entry`, the dedupe pass,
   `file_type()` in the walkers).
3. **E3** make the post-write refresh targeted, or at minimum per-root.
   Do it after E4 so the remaining cost is honest.

**Group 2 — the GUI frame budget (the scaling cliff).**
4. **E1** cache `companion_parents` / `split_primary_of` alongside
   `self.inv`; index `bundle_for`. Everything else in the GUI is noise
   until this is done.
5. **E17** cache root liveness per inventory (same cache object as #4).
6. **E2** `show_rows` for the inventory and trash grids; cap `activity`.
7. **E10** decorate-sort-undecorate for the inventory comparator.
8. **E9** keep the journal appender open.

**Group 3 — the long-running operations.**
9. **E6** checkpoint on an interval instead of per file. *Do this after
   code-review C1* (atomic `save_json`) so the interval change lands on a
   correct writer.
10. **E5** parallelise `verify`. *After code-review C6*, which rewrites
    the same loop's error handling.
11. **E7** `HashSet` in the carried-manifest merge; **E15** the same in
    `backup()`; **E12** walk the trash once.

**Group 4 — free wins.**
12. **E13** add `[profile.release]` (`lto = "thin"`, `codegen-units = 1`,
    `strip = "symbols"`). One commit, applies to every future release.
13. **E11** drop the double buffer; **E14** one hex helper;
    **E16** bound the queue and channel.

If only three things are done: **E4, E1, E13.** Those are, in order, the
largest CPU win, the only true scaling cliff, and the cheapest.

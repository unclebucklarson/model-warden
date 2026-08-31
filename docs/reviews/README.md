# modelwarden reviews — index and combined fix order

Three independent passes over v0.4.2, written to be acted on rather than
admired. Each document stands alone and carries its own prioritised fix
order; this page exists because several findings share a root cause
across documents, and fixing them in the wrong order means doing the work
twice.

| Document | Scope | Headline |
|---|---|---|
| [01 — Code review](01-code-review.md) | correctness, robustness | Atomic writes aren't atomic; the single-writer lock doesn't guarantee a single writer; a corrupt manifest can panic the scrub. |
| [02 — Efficiency review](02-efficiency-review.md) | CPU, I/O, memory, scaling | Nothing is slow at 44 models; three things become unusable at ~2,000, all from one function's call pattern. |
| [03 — Security review](03-security-review.md) | trust boundaries | Removable media is trusted for path construction and backup completeness; the HF token is world-readable. |

Counts: 20 code findings, 17 efficiency findings, 13 security findings.
Nothing here is a reason to stop shipping — warden's *design* is sound and
its test discipline is real. These are the places where the
implementation does not yet keep the promises the documentation makes.

## Status — all six phases complete (2026-08-31)

Every one of the 50 findings has been acted on, and each document carries
a dated note under the finding saying what happened. Fixed in six pushes,
test-first: 118 unit tests plus a 10-test whole-binary suite, green on
Linux and macOS.

| Phase | Findings | Outcome |
|---|---|---|
| 1 — write primitives | C1, C7, C8, H1, H2 | Atomic `save_json`; `fsx::rename_noreplace` via `renameat2`; a hostile drive manifest is sanitised before any of it is believed. |
| 2 — the lock | C2 | pid-file protocol replaced with `flock`; the kernel arbitrates. |
| 3 — secrets and config | C4, H3, H4, L1c, L1e | `0600`/`0700` everywhere and self-healing at startup; downloads verified against the origin's `x-linked-etag`. |
| 4 — truth and robustness | C3, C5, C6, C9, C10, C12, C13, C14, C15, C16, E5, E6 | One symlink policy, one liveness predicate (`LocationState`), no unchecked slices, verify survives a bad sector and uses the cores. |
| 5 — performance | E1–E4, E7–E17 | `hash` on an unchanged catalog 0.60 s → 0.02 s; the GUI's per-frame relation cost 234 ms → 4.5 ms at n=2,000, then out of the frame entirely. |
| 6 — remaining | C11, C17–C20, M1–M5, L1a–L1f | A verified move is now undoable; no shell in the systemd unit; URLs encoded; `--token-file`. |

**Four findings were wrong, and are corrected in place rather than
quietly dropped** — the same standard the reviews were written to:

- **C12** was written up as a stack overflow. It is not: the kernel's
  `ELOOP` limit bounds the walk. The real defect was a 42× inflated
  trash listing, measured. Severity downgraded, fix kept.
- **E4** predicted `warden scan` would get faster. It cannot — `scan` has
  no manifest to carry anything forward, by design. The win is in the
  refresh path, and it is large.
- **E11** claimed 32 MiB of resident memory across the hashing pool.
  Measured: none of it was resident. The allocation was virtual.
- **E13** predicted 5–15% CPU from the release profile. Measured: noise.
  The 27–33% size reduction is real; the speed claim was not.

**Three findings were deliberately not fixed**, each with the reasoning
recorded next to it: the grid virtualisation in E2 (needs
`egui_extras::TableBuilder` and a visual check no test here can do), the
`DirEntry::file_type()` half of E8 (would silently stop the shelf walker
descending through symlinked directories), and the fs-UUID precedence in
M2 (would silently re-id existing drives — a security nicety bought with
a catalog that has lost track of real backups).

## Findings that share a root cause

Fix once, close several:

- **Unvalidated `rel_path` from manifests** → security H1 + H2, and it is
  what makes code-review C5's size confusion dangerous rather than merely
  wrong. One `sanitize_rel()` helper closes all of it.
- **Non-atomic writes** → code-review C1 (manifests) and C4 (config) are
  the same missing primitive; efficiency E6 (checkpoint frequency) should
  land on the fixed writer, not the broken one.
- **`with_extension` for temp names** → code-review C8 hits `backup` and
  `dedup`; `acquire::partial_path` is the existing correct implementation.
- **`bundle_for`'s cost** → efficiency E1 (per-frame relations) and part
  of E3 (post-write refresh). One cached, indexed relation fixes both.
- **File modes** → security H3 (token), L1c (journal paths), L1e (trash
  root). One `0600`/`0700` change.
- **Liveness predicates** → code-review C9 and C15 both come from three
  call sites disagreeing about what "accessible" means.

## Combined execution order

Ordered so prerequisites land first. Each phase is independently
shippable and independently testable — and per CLAUDE.md, each starts
with a failing test.

**Phase 1 — trust boundary and secrets (security first, no dependencies).**
1. `sanitize_rel()` + stop persisting drive-declared records → **H1, H2**
   (also neutralises M2's impact and L1e).
2. File modes `0600`/`0700` with repair-on-load → **H3, L1c**.
3. Verify downloads against `x-linked-etag` → **H4**.

**Phase 2 — the write primitives (blocks Phase 4).**
4. Atomic `save_json`, no `.bak` shuffle, parent-dir fsync → **C1**.
5. `rename_noreplace` helper (`renameat2`) → **C7**; adopt in backup,
   acquire, trash.
6. Shared `temp_sibling()` from `acquire::partial_path` → **C8**.
7. Config: distinguish parse failure from absence, save atomically →
   **C4**.

**Phase 3 — the lock (independent, highest structural risk).**
8. Replace the pid-file protocol with `flock` → **C2**. Do this before
   anything else builds on the current protocol; it deletes the
   stale-detection code entirely.

**Phase 4 — truth and robustness (depends on Phase 2).**
9. Checked hash slicing everywhere → **C3**; `roots.rs:119` bound →
   **C14**.
10. One symlink policy across all three scanners → **C5**; then one
    liveness predicate → **C9, C15**.
11. `verify` continues past I/O errors → **C6**; *then* parallelise it →
    **E5**.
12. Checkpoint on an interval → **E6** (needs Phase 2's writer).
13. Depth caps and symlink-loop guards → **C12**; dotfile parity →
    **C10**.

**Phase 5 — performance (independent of everything above).**
14. Carry `meta` forward on unchanged fingerprints → **E4** *(largest
    measured win: 0.53 s of CPU per scan → ~0)*.
15. Cache the companion/split relations and index `bundle_for` → **E1**
    *(the only true scaling cliff)*; cache root liveness → **E17**.
16. `[profile.release]` with LTO and strip → **E13** *(free; ~9 MB per
    binary)*.
17. Targeted post-write refresh → **E3**; redundant `stat`s → **E8**;
    `show_rows` and a bounded activity log → **E2**.

**Phase 6 — remaining hardening and hygiene.**
18. **M1** (no shell in the systemd unit), **M3** (URL encoding),
    **M5** (incremental GGUF allocation), **M4** (`--token` exposure),
    **L1a**, **L1b**, **L1d**.
19. **C11** (route `demote --remove-source` through the trash — a product
    decision as much as a code one), **C13**, **C16**, **C17**, **C18**,
    **C19**, **C20**, and efficiency P3.

## If you only do one phase

**Phase 1.** It is the only group where warden's behaviour is
meaningfully worse than a reasonable user would infer from the README,
and two of its three items are the difference between "a backup exists"
being true and being asserted.

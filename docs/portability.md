# Portability: macOS and Windows

Status: **analysis complete, build not started** — awaiting the
requesting user's platform to pick the first ship target. Facts below
come from the portability spike (2026-08-26): `cargo check` + `cargo
test` on real `macos-latest` and `windows-latest` runners
(`.github/workflows/portability-spike.yml`, manual dispatch).

## Spike verdict (the headline)

- **macOS: 76 of 77 tests pass today.** The one failure is the write
  lock's liveness check (`/proc/<pid>` doesn't exist → a *live* lock is
  judged stale and stolen). Everything else — scanner, manifests,
  bundles, backup, trash, dedup, doctor logic — already behaves.
- **Windows: does not compile.** Every error is the same species:
  `std::os::unix` metadata (dev/ino/mode) in seven files. No
  architectural problem surfaced; the safety invariants are all
  platform-neutral (rename, hardlink, hash-verify all exist on NTFS).

## Support tiers (the vocabulary for "runnable")

- **Tier 1 — full**: everything works, scheduled scrub included. Linux
  today.
- **Tier 2 — supported with named degradations**: all operations work;
  the degradations are documented, and doctor stays honest about them.
- **Tier 3 — compiles, untested on real stores**: not claimed as
  supported.

Proposed first ship: macOS at Tier 2, then Windows at Tier 2.

## The seam table

One row per platform seam. "Sev" = blocker for Tier 2 (B) or acceptable
degradation (D).

| Seam | Linux today | macOS plan | Windows plan | Sev |
|---|---|---|---|---|
| Write-lock liveness (`lock.rs`) | `/proc/<pid>` exists-check | `kill(pid, 0)` via libc (spike-confirmed: the one test failure) | `OpenProcess` probe (or `sysinfo` crate for both) | B — silently stealing a live lock breaks single-writer |
| Fingerprint identity (`identity.rs`) | unix `dev`/`ino` + mtime | same as Linux (unix) | `MetadataExt::volume_serial_number`/`file_index` (Win, stabilized subset) or size+mtime-only fingerprint (weaker ⇒ just re-hashes more; always safe) | B (compile) |
| Inode dedupe + hardlink detection (`scan.rs`, `dedup.rs`) | `dev`/`ino` pairs | unchanged | same file-index approach as fingerprint; hardlinks themselves work on NTFS | B (compile) |
| Drive identity (`roots.rs`) | fs UUID via `/dev/disk/by-uuid` + marker fallback | marker-only (or `diskutil` later) | marker-only (or volume serial later) | D — marker file was built for exactly this |
| Owner-tool detection exec-bit (`doctor.rs`) | `PermissionsExt::mode() & 0o111` | unchanged | extension-based (`.exe`/`.cmd`) or just try-spawn | B (compile) |
| Scheduled scrub (`scrub.rs`) | systemd user units | launchd plist (later); ship with manual guidance | Task Scheduler (later); ship with manual guidance | D — doctor advisory already goes silent off-systemd; extend that silence + docs |
| Archive hardlink path (`archive.rs`) | unix metadata for same-fs test | unchanged | same-volume test via file-index metadata | B (compile) |
| HF cache layout (`scan.rs`) | symlinked snapshots | same | HF duplicates files when symlinks unavailable (no Dev Mode) — scanner must treat copies-not-links correctly; needs a real-machine test | B — verify, likely already works |
| Config/state paths (`settings.rs`) | XDG (`~/.config`, `~/.local/state`) | works (dotfolders); idiomatic `~/Library` later | works under `%USERPROFILE%`; idiomatic `%APPDATA%` later | D |
| Store discovery | `~/.ollama`, `~/.cache/huggingface` | identical paths on macOS | identical under `%USERPROFILE%` | D — verify on real machines |
| GUI (egui/eframe, rfd) | works | cross-platform by construction | cross-platform by construction | — |
| Release pipeline | x86_64-linux tarball | add `aarch64-apple-darwin` job | add `x86_64-pc-windows-msvc` job (zip) | B for shipping |

Compile-error inventory from the spike (Windows): `identity.rs:27`,
`scan.rs:254,622`, `roots.rs:144`, `archive.rs:287,319`,
`dedup.rs:262,281,296,308`, `doctor.rs:215–216,642,665,667` — all
`std::os::unix` / `mode()`.

## Ship checklists

### M16a — macOS (small)

- [ ] Lock liveness via `kill(pid, 0)` (fixes the one failing test)
- [ ] CI: add `macos-latest` to the build+test matrix (promote from spike)
- [ ] Release: `aarch64-apple-darwin` artifact in release.yml
- [ ] Docs: named degradations (marker-only drive identity for external
      drives without the by-uuid path; no scheduled scrub — manual
      `hash && verify --all` guidance, e.g. cron/launchd by hand)
- [ ] Real-machine pass: scan a real Ollama + HF cache on macOS
- Later (Tier 1): launchd scrub units; `diskutil` UUID identity

### M16b — Windows (moderate)

- [ ] Platform-abstract the metadata seam: one `platform` module giving
      (volume-id, file-id, is-executable) with unix and windows arms —
      fixes every compile error in one shape
- [ ] Lock liveness via process-handle probe (or `sysinfo` for all three)
- [ ] Verify HF cache copies-not-symlinks layout against the scanner
- [ ] CI: add `windows-latest` to the matrix
- [ ] Release: `x86_64-pc-windows-msvc` zip artifact
- [ ] Docs: named degradations (as macOS, plus FAT mtime granularity note)
- [ ] Real-machine pass on Windows (the part CI cannot do)
- Later (Tier 1): Task Scheduler scrub; volume-serial drive identity

## Verification strategy

- **CI proves**: compilation and the unit/E2E test suite per OS (the
  spike workflow is the template; promote per-OS into `ci.yml` as each
  tier lands so regressions stay caught).
- **Only a real machine proves**: removable-drive identity across
  remounts, the Windows HF cache layout, owner-CLI interop (`ollama`,
  `hf` on that OS), and GUI look/feel.
- The spike workflow stays available for re-running the full matrix on
  demand before either milestone starts.

# modelwarden on macOS — beta QA guide

Thank you for testing! You are the first real macOS user, and this guide
is a complete script: install, a safe read-only pass over your actual
model stores, an optional deeper pass, and exactly what to report back.

**Safety, up front:** everything in Phase 1 is read-only — warden never
modifies the Ollama or Hugging Face stores it scans (that's a core design
rule on every platform), and read commands don't write anything at all
beyond warden's own catalog under `~/.local/state/modelwarden/`. You can
stop at any point with nothing changed.

## Known macOS limitations in this beta (expected, not bugs)

- **No scheduled scrub yet.** On Linux warden installs a systemd timer
  that periodically re-verifies every byte; the macOS (launchd)
  equivalent isn't built yet. `warden scrub install` will fail or do
  nothing useful — skip it. Manual check: `warden hash && warden verify
  --all` whenever you like.
- **External drives are identified by a marker file only** (Linux also
  uses filesystem UUIDs). Registered drives still keep their identity
  across remounts via `.modelwarden/root-id`; this is a degraded detail,
  not a missing feature.
- Paths are Linux-style (`~/.config/modelwarden`,
  `~/.local/state/modelwarden`) rather than `~/Library/…`. Cosmetic.

## 1. Install

**Option A — release tarball.** From the GitHub Releases page, download
the build for your chip:

- Apple Silicon (M1/M2/M3/M4): `…-aarch64-apple-darwin.tar.gz`
- Intel: `…-x86_64-apple-darwin.tar.gz`

(Not sure? Apple menu → About This Mac: "Apple M…" = Apple Silicon.)

```
tar xzf modelwarden-*-apple-darwin.tar.gz
cd modelwarden-*-apple-darwin
xattr -d com.apple.quarantine warden warden-gui 2>/dev/null || true
./warden version
```

The `xattr` line matters: macOS quarantines downloaded binaries, and
these are not (yet) notarized with Apple — without it, Gatekeeper shows
"cannot be opened because the developer cannot be verified". If a dialog
still appears, right-click the binary → Open once, or allow it under
System Settings → Privacy & Security. Then put `warden` and `warden-gui`
somewhere on your `PATH` (e.g. `/usr/local/bin` or `~/bin`).

**Option B — build from source** (needs Rust; no Gatekeeper friction):

```
cargo install modelwarden
```

**First checkpoint:** `warden version` prints a version → report ✅ or
the exact error.

## 2. Phase 1 — read-only pass (10 minutes, changes nothing)

Run each command; note anything that errors, hangs, or looks wrong.

```
warden scan
```

Should list your models across the Ollama store (`~/.ollama/models`) and
Hugging Face cache (`~/.cache/huggingface/hub`) if you have them.
**Check:** does the list match what you believe you have? Are sizes
sensible? Anything missing?

```
warden hash
```

The first catalog build — reads every byte of every model, so expect
minutes for large collections, with per-file progress. **Check:** does it
finish? Note the speed it settles at.

```
warden status
warden where <some-model-name-fragment>
warden dups
warden report
```

**Check:** status shows your roots and counts; `where` finds a model you
know you have; dups/report produce plausible numbers (empty is fine).

```
warden doctor
```

Store health. Findings about your HF cache (interrupted downloads, pruned
leftovers) are likely *true positives* — note them, don't fix anything
yet. **Expected on macOS:** it should NOT nag about the scrub timer
(that advisory is Linux-only); if it does, that's a bug to report.

```
warden-gui
```

**Check:** window opens (Gatekeeper may need the same right-click → Open
once); Inventory shows your models with sorting/filtering; tabs switch;
nothing panics. Cosmetic oddities (fonts, spacing) are worth a screenshot.

## 3. Phase 2 — optional write pass (touches only a folder you choose)

These exercise warden's write paths against a scratch folder — your
model stores are still never written to.

```
mkdir -p ~/warden-qa-backup
warden backup ~/warden-qa-backup <a-small-model-name> --label "QA"
warden verify "QA"
```

**Check:** the copy completes and verifies ("N ok, 0 mismatched"). Then,
if you have a USB drive handy, the interesting macOS-specific test:
register it (`warden roots add /Volumes/<YourDrive> --label "Test"`),
back something up to it, eject, re-plug, and run `warden roots list` —
**does the drive keep its identity** (same root id, shown online again)?

Clean up when done: `rm -rf ~/warden-qa-backup` and, if you registered a
test drive, `warden roots forget "Test" --yes`.

## 4. Phase 3 — optional download test

```
warden fetch prajjwal1/bert-tiny --snapshot
```

A tiny (~17 MB) real model into your shelf. **Check:** downloads, hashes,
appears in `warden scan`. Delete it afterwards if unwanted:
`warden delete bert-tiny` then `warden trash empty --yes`.

## 5. What to report

Open a GitHub issue (or just send the text) with:

```
macOS version:        (e.g. 15.3)
Chip:                 (Apple Silicon / Intel)
Install method:       (tarball / cargo install)
warden version:       (output of `warden version`)
Stores present:       (Ollama? HF cache? rough total size)

Phase 1: each command — OK / output pasted for anything odd
Phase 2: OK / output          (if attempted)
Phase 3: OK / output          (if attempted)
GUI: opened? usable? screenshots welcome
Surprises: anything that felt wrong, slow, or confusing — UX notes are
as valuable as crashes.
```

For any error, the exact command plus the full output is the most useful
thing you can send. Thank you again — this pass is what promotes macOS
from "compiles and tests green" to *supported*.

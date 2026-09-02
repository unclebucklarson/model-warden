# QA + tasks for Scott — written 2026-09-02, for when you're back

Everything the projects need from a human, in priority order, with exact
commands and what to record. Total ~40 minutes plus drive-spin time.
Nothing here is urgent: the scrub timer fires Mon 2026-09-07 while you're
away and will run fine in its current form — task 1 just makes the next
one better.

Record findings in the **Feedback** section at the bottom of this file
(edit it directly and commit, or just tell the next session "read
docs/qa/2026-09-02-away-tasks.md and here's what I found").

---

## Task 1 — Install warden v0.5.0 properly and regenerate the scrub unit (~5 min)

**Why:** your live systemd unit still has the pre-remediation shell form
(`ExecStart=/bin/sh -c '…'` — the injection shape security finding M1
removed) and points at `target/debug/warden` inside the dev tree — a
debug build hashes ~10× slower than release, and a `cargo clean` would
silently break the timer. Installing to `~/.cargo/bin` fixes all three.

```sh
cargo install modelwarden            # pulls the published 0.5.0, release build
~/.cargo/bin/warden scrub install --weekly --enable
systemctl --user daemon-reload
```

**Important:** run `scrub install` via `~/.cargo/bin/warden` exactly as
written — the unit records the path of the binary that ran the command,
and you want it pointing at the installed copy, not the dev tree.

**Verify (paste results into Feedback A):**

```sh
cat ~/.config/systemd/user/modelwarden-scrub.service
systemctl --user list-timers | grep modelwarden
```

Expected: **two** `ExecStart=` lines (`… hash` and `… verify --all`),
**no `/bin/sh` anywhere**, path `…/.cargo/bin/warden` in quotes, and the
timer showing next Monday.

---

## Task 2 — Plug in both archive drives; exercise the new liveness + parallel verify (~15 min)

**Why:** the remediation replaced four disagreeing "is this reachable"
checks with one three-state answer (Present / Offline / Unreadable), and
made `verify` read four files at once. Both drives have been offline for
every test run — this is the first real exercise with them attached.

With **both** drives plugged in:

```sh
warden status
warden roots list
warden verify "Archive 2"        # and the other drive, by its label or id
```

Expected and what to record (Feedback B):

- `warden status` coverage headline should still read **6 of 47** (or
  whatever it said before you left). **If the number DROPS with the
  drives attached, that is the new detection working** — it found a
  drive copy that exists in the catalog but won't open (permissions,
  failing media). That's a finding, not a bug: note which model.
- `verify` output: one named line per file (e.g.
  `  Muse-Glimmer… verified in 20s`), files completing out of order is
  normal now (parallel). Record the summary line —
  `N ok, 0 mismatched, 0 missing, 0 unreadable, 0 unhashed` — and
  roughly how long each drive took.
- Anything `UNREADABLE` is worth reporting verbatim.

---

## Task 3 — GUI smoke pass (~10 min)

**Why:** the remediation rewrote the inventory tab's internals (row
decoration, sorting, filtering, the companion index, the activity-log
cap, duplicate-click coalescing). All 130 tests pass, but nobody with
eyes has watched the window since. `cargo run` from the repo, then:

- [ ] Inventory renders; row count and sizes look right.
- [ ] Click **every column header** to sort (Name, Quant, Size, Where,
      Backup, State), each twice (reverse). Order sane each time?
- [ ] Filter box: type part of a model name, a quant (`Q4`), a location
      (`ollama`) — matches appear, clearing restores everything.
- [ ] Expand/collapse a model with companions (mmproj / split parts) —
      indented rows appear with "required by" / "part of" notes.
- [ ] Backup column ✓s match reality (drives unplugged: your backed-up
      models should STILL show ✓ — offline is not gone).
- [ ] File → Update Catalog, then **click it again immediately** — the
      activity log should say `… is already queued — ignoring the
      repeat`, not queue a second full rescan.
- [ ] Cold storage dialog opens, lists eligible models and targets.
- [ ] Trash tab lists correctly; empty-trash confirm shows a sane byte
      total.
- [ ] Scrolling the inventory with your ~47 rows: any stutter? (This
      answers whether the deferred grid-virtualisation work matters at
      your catalog size.)

Record anything odd in Feedback C — screenshots welcome.

---

## Task 4 (optional) — Demote round-trip with the new trash behavior (~5 min)

**Why:** `demote --remove-source` now moves the shelf copy to the trash
instead of deleting it (review finding C11 — a verified move should be
at least as undoable as a delete). Worth one real-life pass:

```sh
warden archive demote <your-smallest-model> --to <drive-label> --remove-source
warden trash                      # the shelf copy should be listed
warden trash restore <model>      # bring it back
warden verify <drive-label>       # drive copy still fine
```

Does the wording at each step tell you where your bytes are? (Feedback D)

---

## Task 5 — Nudge the macOS beta tester

M16a's last open checkbox is their QA report against `docs/qa-macos.md`.
A v0.5.0 note to them: the macOS tarballs on the v0.5.0 release are
fresh if they'd rather test current.

---

## Task 6 — When you're back: start the modellab usability review

modellab's M1 closes with a CLI usability review run by a **fresh
session** (Shepard's protocol — the reviewer must not be the session
that built it). Open a new Claude session in `~/src2/modellab` and say:

> Run the CLI usability review that closes M1, per ROADMAP.md.

Everything else in modellab (M2, the fit calculator) is unblocked and
waiting behind that review.

---

# Feedback — fill in what you found

## A. Scrub unit regenerated (Task 1)
- Service file contents after regen:
- Timer line:
- Anything odd:

## B. Drives-attached verify (Task 2)
- Coverage headline with drives attached:
- Drive 1 (label, summary line, rough duration):
- Drive 2 (label, summary line, rough duration):
- Any MISMATCH / MISSING / UNREADABLE lines (verbatim):

## C. GUI pass (Task 3)
- Checkboxes all fine? List any that weren't, with what you saw:
- Scrolling feel at your catalog size (informs deferred virtualisation):

## D. Demote round-trip wording (Task 4, optional)
- Did each step's message tell you where your bytes were?
- Any doc/tutorial wording that now reads wrong:

## E. Decisions when you're back
1. **modellab GitHub repo**: create `unclebucklarson/modellab` (public,
   like model-warden)? yes / no / different name:
2. **Priority order**: modellab M2 (fit calculator) vs warden backlog vs
   waiting on ModelShepard's inventory integration — your call:
3. Anything from A–D worth turning into roadmap items:

## F. Anything else broken, confusing, or annoying
-

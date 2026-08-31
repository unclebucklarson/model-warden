# modelwarden — security review

*Reviewed at v0.4.2. Findings verified against source; those marked
**proven** were reproduced on this machine. Ordered for execution — see
[Fix order](#fix-order).*

## Threat model

warden is a single-user desktop tool that runs with the invoking user's
privileges and holds no secrets of its own beyond a Hugging Face token.
There is no privilege boundary inside it, so "warden can write files as
you" is not by itself a finding. What matters is where **untrusted input
crosses into privileged action**, and warden has four such surfaces —
three of them are its core purpose:

- **T1 — removable media.** warden exists to have drives plugged into
  it, and it reads a metadata file (`.modelwarden/manifest.json`) off
  those drives and acts on its contents. This is the largest surface and
  the least defended.
- **T2 — remote content.** HTTPS to huggingface.co: repo listings
  (server-chosen filenames), file bytes, redirects to a CDN.
- **T3 — local file content.** GGUF headers are parsed from files that
  arrived from anywhere.
- **T4 — other local users.** Multi-user machines, shared groups, backup
  systems that read `$HOME`.

Explicitly out of scope: an attacker who already runs code as the user.

**Verdict up front.** The remote surface (T2) and the parser (T3) are in
decent shape — bounded allocations, no shell, TLS by default, auth
correctly *not* forwarded across redirects. The removable-media surface
(T1) is not defended at all: a drive's own manifest is trusted for path
construction, for backup-completeness decisions, and for drive identity,
and one path (`verify --repair`) turns that trust into an arbitrary file
write. The token is stored world-readable. Those are the review.

Severity: **H** = exploitable to write/destroy files or leak a
credential. **M** = integrity or disclosure with preconditions.
**L** = defence-in-depth.

---

## H1 — Arbitrary file write from a hostile removable drive
`src/bin/warden.rs:1545-1565` → `src/core/backup.rs:328-400`

`warden verify <path> --repair` on a drive that is not a registered root
falls back to reading the drive's **own** `.modelwarden/manifest.json`
(`warden.rs:1556`), sets `root.path` to the drive, and keeps every
`rel_path` the file declares. `verify` then reads
`man.root.path.join(&f.rel_path)`; `repair` **writes** to the same
expression (`backup.rs:373`).

Nothing anywhere validates that a manifest `rel_path` is relative or free
of `..`. A manifest containing:

```json
{"rel_path": "../../../../home/victim/.config/autostart/x.desktop",
 "sha256": "<hash of any file already in the victim's catalog>", …}
```

causes `verify --repair` to report a mismatch and then write that
catalogued content to that path. Destination is attacker-chosen; content
is attacker-*selected* from files the victim already has. Overwriting
dotfiles, shell profiles, or systemd user units is straightforward
destruction; code execution requires the victim to happen to hold a file
whose bytes are useful, which is a real constraint but not a guarantee.

Preconditions: the user plugs in a prepared drive and runs
`verify … --repair` against it — a plausible request ("can you check this
disk for me?") for a tool whose whole job is checking disks.

**Fix:** one `sanitize_rel(path) -> Result<PathBuf>` in core, rejecting
absolute paths, any `ParentDir` component, and (on principle) symlinked
intermediate components; apply it at every point where a manifest
`rel_path` is joined to a root — `backup.rs:373`, `:120`, `verify`'s
`:426`, `archive.rs:53,60,174`, `trash.rs:95,96,285`, `dedup.rs:213`.
`acquire::dest_for` (`acquire.rs:336-346`) already does exactly this for
remote filenames and is the model to copy.

## H2 — A drive's manifest is trusted, persisted, and merged into the catalog
`src/core/backup.rs:80-90` → `src/bin/warden.rs:1475-1485`

`backup()` loads the target drive's manifest, appends to it, and
`cmd_backup` writes the result **into warden's state dir**
(`manifest_path(&state, &tspec.id)`) and then merges it into
`inventory.json`. Whatever the drive asserted — paths, sizes, hashes,
names — is now warden's own catalog, and every later command
(`where`, `verify`, `repair`, `delete`, `dedup`, `restore`) operates on
it. H1's traversal therefore does not need `--repair` on the drive: one
`warden backup /media/hostile` is enough to plant the paths, and a later
`delete` will move the victim's real files into a trash directory.

Two more consequences of the same trust:

- **Silent backup forgery.** `backup()` decides "already on the target"
  by looking for the hash in the drive's manifest (`backup.rs:106`). A
  manifest that claims every hash makes `warden backup` copy **nothing**
  and report success. The user believes they have a backup; the drive is
  empty. This defeats the product's central promise silently.
- **Coverage forgery.** Those fabricated entries are `Removable`
  locations, so `is_backed_up` (`manifest.rs:270`) counts them and the
  "N/M backed up to a drive" headline reports protection that does not
  exist.

**Fix:** treat a carried manifest as a *hint*, never as truth. Accept it
only for hash carry-forward on files that were actually scanned on the
drive (which `build_root_manifest` already does correctly), and never let
its records enter warden's state dir unverified. The skip-decision in
`backup()` should be based on the target's *scanned* contents, not its
self-declaration.

## H3 — The Hugging Face token is stored world-readable — **proven**
`src/core/settings.rs:64-69`; `src/core/settings.rs:22-26`

```
$ stat -c '%a %n' ~/.config/modelwarden/config.json
664 /home/buck/.config/modelwarden/config.json
$ stat -c '%a %n' ~/.local/state/modelwarden
775 /home/buck/.local/state/modelwarden
```

`AppConfig::save` uses `std::fs::write`, which creates at `0666 & ~umask`
— `0664` under the common `umask 002`. The file contains `hf_token` in
plaintext. Every local user can read the token; every member of the
user's primary group can **rewrite the config** (and, at `0775` on the
state directory, the manifests too — which is the input H1 trusts).

The docstring says the token is stored "plainly, like the hf CLI's own
`~/.cache/huggingface/token`" — but `huggingface_hub` writes that file
`0600`. warden is strictly less careful than the tool it cites.

**Fix:** create the config with `0600` (and the state dir `0700`) via
`OpenOptions::mode()`; repair permissions on load if they are looser.
Consider not persisting the token at all by default — `$HF_TOKEN` and the
`hf` login file already work.

## H4 — Downloaded bytes are never verified against anything *(FIXED — see note)*
`src/core/acquire.rs:356-530`, `src/bin/warden.rs:1096-1112`

Every other byte-movement in warden is hash-verified end to end. Downloads
are the exception: `fetch` streams to `.partial`, compares only
**Content-Length**, renames into place, and *then* hashes the result —
purely to key the provenance record. The hash is never compared to
anything authoritative.

Hugging Face supplies the content hash: for LFS objects the
`x-linked-etag` **is** the SHA-256, and warden already captures it
(`acquire.rs:437-441`) and stores it as provenance. It just never checks
it. A corrupted transfer that happens to match the declared length, a
compromised or misbehaving CDN edge, or a TLS-terminating middlebox with
a trusted cert all yield a file warden accepts and catalogs as
authoritative — and which then propagates to backups and cold storage
under warden's own "verified" branding.

**Fix:** when the etag looks like a SHA-256, compare the computed hash
against it before the rename and refuse on mismatch. This closes the last
unverified byte path in the product and costs nothing — the hash is
already computed.

**Implementation note (2026-08-31), and a finding the fix uncovered:**
the obvious implementation is wrong. `resolve/` answers **302** with
`x-linked-etag` (the object's SHA-256) and redirects to a CDN whose own
`etag` is a Xet content-addressed id — also 64 hex characters, but *not*
the file hash. ureq follows the redirect, so reading the header off the
final response silently yields the wrong value: verification rejected a
perfectly good 17 MB download on the first live test (wanted
`05526191ad0e…`, got `dab2c2bddcfb…` — the *second* being the truth).
This also means the `etag` warden has been recording as provenance all
along was, for Xet-backed files, a storage id rather than a checksum.
The fix asks the origin with redirects disabled
(`acquire::origin_checksum`) and records that value instead.

---

## M1 — Shell metacharacter injection into the generated systemd unit
`src/core/scrub.rs:35-47`

```rust
ExecStart=/bin/sh -c '{bin} hash && {bin} verify --all'
```

`bin` is `std::env::current_exe()`, interpolated into a single-quoted
shell string with no escaping. A path containing an apostrophe breaks the
unit; a path crafted as `…/x'; curl … | sh; '/warden` executes arbitrary
commands **on a timer, as the user**. Realistically this needs the binary
to sit at an attacker-influenced path (a shared `/tmp` build, a
downloaded folder) — but the apostrophe case alone is a real bug for
anyone named O'Brien.

**Fix:** drop the shell. `Type=oneshot` accepts multiple `ExecStart=`
lines, so use two, and quote the path per systemd's own escaping rules.
**Done, 2026-08-31.** Two `ExecStart=` lines, no shell anywhere, the
path double-quoted with `"` and `\` escaped. `Type=oneshot` runs them in
order and stops at the first failure, which is what the `&&` meant:
verify must not run against a catalog that failed to refresh.

## M2 — Drive identity is a plain text file on the drive
`src/core/roots.rs:161-165`

`read_marker` accepts `.modelwarden/root-id` from the drive as that
drive's identity. A drive carrying a copy of another drive's marker
presents as that drive. Combined with H2, a prepared disk mounted at the
expected path can impersonate the user's real backup drive and assert its
contents.

`register_root` refuses ids that are already registered, which limits
this to impersonation-at-the-same-mount-point rather than silent
takeover, but the trust assumption should be documented and the fs-UUID
check should be preferred over the marker when both are available (today
the marker wins unconditionally, `roots.rs:115-125`).

## M3 — URL path segments are interpolated without encoding
`src/core/acquire.rs:374`, `:253`, `:308`

```rust
let url = format!("https://huggingface.co/{repo}/resolve/main/{filename}");
```

`repo` comes from the user, `filename` from the **server's** listing
(`rfilename`). Neither is percent-encoded, so `?`, `#`, or `%` in either
silently re-partitions the URL — a server-supplied filename can append a
query string or truncate the path. The host cannot be changed (it
precedes the interpolation), so this is request manipulation rather than
SSRF, and `dest_for` independently protects the local filesystem. Still,
constructing URLs by string concatenation from remote input is the wrong
default.

**Fix:** percent-encode each path segment, and validate `repo` against
`^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$` before use. **Done, 2026-08-31.**
Both. `url_path` encodes everything outside the unreserved set (keeping
`/`, since repo ids and hub filenames are multi-segment), `url_query`
covers the two search parameters, and `valid_repo` gates every listing
and download at `repo_api_json`, the one door they all pass through.

## M4 — `--token` on the command line is visible to every local process
`src/bin/warden.rs:951-955`

`warden fetch … --token hf_xxx` puts the credential in `argv`, readable
by any user via `/proc/*/cmdline` for the process's lifetime, and into
shell history. The GUI's masked field and `$HF_TOKEN` are both fine; the
CLI flag should be documented as the insecure option, and ideally offer
`--token-file` or a stdin prompt as the recommended path.

## M5 — Unbounded allocation from a crafted GGUF header
`src/core/gguf.rs:126-134`

`read_string` reads a length, checks `len > 64 MiB`, then allocates
`vec![0u8; len]` **before** reading. A 40-byte file declaring a 64 MiB
string causes a 64 MiB allocation that is then thrown away on EOF. With
`kv_count` up to 100,000 and the 256 MiB header cap checked only *between*
KV pairs, a hostile file can force repeated large allocations. Bounded
and not exploitable beyond memory pressure, but the fix is one line:
allocate incrementally (`take(len).read_to_end`) so the allocation is
paid only for bytes that actually exist. **Done, 2026-08-31** — and the
short read is now its own error rather than a silently truncated
string.

Note the surrounding parser is otherwise well defended: magic and version
checks, a KV-count cap, `saturating_mul` on array sizes, refusal of
nested arrays, and a global header ceiling. This is the one gap.

---

## L1 — Reporting and hygiene

- **L1a.** `expect_gone.exists()` (`doctor.rs:380`) returns `false` for a
  path that exists but is unreadable, so an owner command that did
  nothing can be reported as success — the exact failure the check was
  written to catch. Use `symlink_metadata()` and distinguish `NotFound`.
- **L1b.** Owner commands resolve `hf`/`ollama` through `PATH` at
  execution time (`doctor.rs:223-233` checks, `:231` executes). A
  writable directory earlier in `PATH` substitutes the binary. No shell
  is used and arguments are passed as a vector, so there is **no
  injection** — this is ordinary `PATH` trust, worth documenting rather
  than fixing.
- **L1c.** The journal (`journal.rs`) records full filesystem paths of
  every model, world-readable at `0664`. On a shared machine that is a
  complete inventory of what the user has and where. Same `0600` fix as
  H3.
- **L1d.** TLS trust comes from `webpki-roots` bundled in the binary
  rather than the system store, so enterprise MITM proxies fail and root
  updates require a warden release. Correct default for reproducibility;
  worth a documented `--ca-bundle` escape hatch if users complain.
- **L1e.** `trash::empty` (`trash.rs:310`) calls `remove_dir_all` on a
  path derived from a root in the config. With the config group-writable
  (H3), another group member can point a root at a directory whose
  `.modelwarden/trash` subtree they want deleted. Low impact, but it
  disappears entirely once H3 tightens permissions.
- **L1f.** `RUSTSEC-2026-0192` (`ttf-parser` unmaintained) is ignored in
  `.cargo/audit.toml` with a reason and a revisit trigger. Correct
  handling; no action beyond honouring the trigger at the next `eframe`
  bump. `cargo audit` otherwise reports **zero** vulnerabilities across
  471 dependencies.

---

## What is genuinely well built

- **Auth is not forwarded across redirects.** ureq 2.12's default is
  `RedirectAuthHeaders::Never`, so the HF token is not leaked to the CDN
  the resolve endpoint redirects to. This is the classic mistake in this
  exact code shape, and warden avoids it (by default rather than by
  design, but it holds — pin the behaviour with a comment so a future
  ureq upgrade does not silently change it).
- **No shell anywhere.** Every subprocess uses `Command::args`. The one
  shell string in the codebase is the systemd unit (M1).
- **`acquire::dest_for`** is a correct, complete path-traversal guard.
- **`verify_husk`** re-proves emptiness at apply time, refuses symlinks,
  and caps file sizes before deleting a directory tree.
- **The `.incomplete` debris deleter** checks the filename, uses an age
  guard, and `remove_file` on a symlink removes the link, not the target.
- **Owner commands are verified, not trusted** (`expect_gone`) — a rare
  and correct instinct.

---

## Fix order

Dependencies noted; H1 and H2 share one fix, so they are one work item.

**Group 1 — close the removable-media surface (do together).**
1. **H1 + H2** — add `sanitize_rel()` in core and apply it at all
   manifest-join sites; stop persisting a drive's self-declared records
   into the state dir; base `backup()`'s skip decision on scanned
   contents. This single change eliminates the arbitrary write, the
   catalog poisoning, the silent backup forgery, and the coverage
   forgery, and it also removes the impact half of **M2** and **L1e**.
   *Prerequisite for nothing; blocks nothing. Do it first.*
2. **M2** — prefer fs-UUID over the marker file when both exist; document
   the marker as a convenience, not an authentication.

**Group 2 — secrets and permissions (independent, one commit).**
3. **H3** — `0600` config, `0700` state dir, repair on load. Fixes
   **L1c** and neutralises **L1e** at the same time.
4. **M4** — document `--token`'s exposure; add `--token-file`/stdin.

**Group 3 — close the unverified byte path.**
5. **H4** — compare the computed hash to `x-linked-etag` before the
   rename. *Best done after code-review C7 (`rename_noreplace`), since it
   touches the same few lines in `fetch`.*

**Group 4 — remote and parser hardening.**
6. **M3** — percent-encode URL segments; validate `repo`.
7. **M5** — incremental allocation in `read_string`.
8. **L1d** — pin the redirect-auth behaviour explicitly rather than
   relying on a crate default.

**Group 5 — the rest.**
9. **M1** — drop `/bin/sh` from the generated unit.
10. **L1a** — `symlink_metadata()` in the owner-command verifier.
11. **L1b** — document the `PATH` trust assumption in CLAUDE.md's safety
    section.

If only two things are done: **H1+H2** (the removable-media trust
boundary) and **H3** (the token's file mode). Those are the two places
where warden's actual behaviour is meaningfully worse than a user would
reasonably assume from its documentation.

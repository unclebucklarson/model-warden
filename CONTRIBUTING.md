# Contributing to modelwarden

Thanks for looking under the hood. Testers, bug reporters, and patch
authors are all welcome — this page is the map.

## Building and running

Rust **1.88+** (edition 2024). Then:

```
cargo build            # both binaries: warden (CLI), warden-gui
cargo test             # unit + GUI-logic + whole-binary e2e suites
cargo run              # opens the GUI
```

The e2e suite (`tests/e2e.rs`) drives the real `warden` binary in fully
isolated environments — Cargo builds it fresh via `CARGO_BIN_EXE_warden`,
so it never tests a stale build. Nothing in the test suite touches your
real model stores.

## How this project is built: test-first

The methodology is law here (see CLAUDE.md for the long form):

- **Failing test before implementation.** Unit tests for core logic;
  e2e tests for behavior spanning modules or binaries.
- **Bug fixes start with a regression test** that reproduces the bug and
  fails on the old code.
- **Decisions live in `src/core/`** (headless, testable); the two
  binaries render and dispatch. If UI logic is hard to test, that's the
  signal to move the decision into core.
- Tests assert behavior and invariants (bytes never lost, bundles move
  whole, refuse-overwrite), not implementation details.

PRs are expected to follow this: a change without a test needs a reason.

## The rules that shape every change

Read the **Safety model** in the README and the constraints in CLAUDE.md
before touching write paths. The short form: never write inside a store
another tool owns; every copy is verified end-to-end; offline is not
gone; and bytes are destroyed only by `trash empty`. Design authority is
PLAN.md — decisions there carry their reasoning and their dates.

## Docs map

- `docs/tutorial.md` — hands-on course (sandboxed).
- `docs/users-guide.md` — the reference.
- `docs/portability.md` — macOS/Windows seams and status.
- `docs/qa-macos.md` — the macOS beta test script.
- `PLAN.md` / `ROADMAP.md` — design authority / status tracker.

## Releasing (maintainers)

1. Ensure `main` is green and the tree is clean.
2. Bump `version` in `Cargo.toml`; commit.
3. `git tag vX.Y.Z && git push origin main vX.Y.Z` — the Release
   workflow builds and publishes the GitHub Release (Linux + both macOS
   targets, tarballs + sha256, generated notes).
4. `cargo publish` — from the tagged state, so the crate matches the
   release artifacts byte-for-byte.
5. The build id (git hash) embeds automatically via `build.rs`; crates
   builds pick up the packaged sha.

## License

Dual MIT OR Apache-2.0. Unless you state otherwise, your contributions
are dual-licensed the same way (the standard Apache-2.0 §5 rule quoted
in the README).

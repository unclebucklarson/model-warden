# modelwarden

**Inventory, backup, and archival for local LLM model files.** The owner of
200GB+ of GGUFs — scattered across Ollama's blob store, the HuggingFace hub
cache, shelf directories, NAS mounts, and removable drives — can always
answer: *what do I have, where is it, is it reachable, is it backed up, and
which of these are the same bytes?* And nothing warden does can lose bytes.

Identity is content: SHA-256, never path. A cheap `(size, mtime, dev, ino)`
fingerprint detects change; hashes are computed lazily by a background
worker (~680 MB/s measured; ~8 min for 300 GiB, then near-instant reruns).

## The two programs

- **`warden`** — the CLI. Every read command takes `--json`.
- **`warden-gui`** (`cargo run`) — a traditional desktop app: menu bar,
  status bar, activity log; Inventory / Duplicates / Usage / Health tabs.

## Commands

```
warden scan              live view of every store (no writes)
warden hash              update the catalog: rescan, hash new/changed
warden status            roots, identity coverage, duplicates, backup state
warden dups              hash-identical duplicates + reclaimable bytes
warden doctor            store health: dangling refs, orphans, partial downloads
warden roots add|list    register drives/NAS by fs UUID (+ marker file)
warden where <query>     locate by name, path, or sha256 prefix — incl. offline drives
warden backup <path>     verified copy of every content to a target drive
warden verify <path|id>  re-hash a root against its manifest (bit-rot check)
warden archive <query>   promote a cache-owned model to the shelf
warden archive demote <query> --to <root> [--remove-source]
warden restore <query>   verified copy from a drive back to the shelf
warden dedup [--hardlink]  collapse same-fs duplicates (dry run by default)
warden report            disk usage by model family
warden fetch <org/repo> [pattern] [--token T]
                         download from HF: split sets fetched together,
                         Range resume, gated repos via --token/$HF_TOKEN/
                         the hf CLI's saved login
```

Write operations take a single-instance lock (`state/warden.lock`) so two
wardens can't race a backup or reclaim; stale locks from crashed runs are
detected and stolen.

## Safety model

- **Never write inside a store another tool owns** (Ollama, HF cache):
  those are scanned and reported, never touched.
- **Every copy is verified**: `.partial` temp name → source-read hash must
  match the catalog → destination read back and re-hashed → rename.
- **The only reclaim is hardlinking** (same filesystem, owned roots only),
  and both sides are re-hashed against the bytes on disk first.
- **The one sanctioned deletion**: `archive demote --remove-source`, after
  the cold copy verified — a provably completed move.
- **Offline is not gone**: unplugged drives keep their manifests; the
  catalog answers "it's on the drive labeled archive1".

## For other tools

`~/.local/state/modelwarden/inventory.json` is a published, read-only,
versioned contract — see [docs/inventory-schema.md](docs/inventory-schema.md).
Drives carry their own `.modelwarden/manifest.json`, so a backup drive is
self-describing on any machine.

## Building

Rust, edition 2024. `cargo build` / `cargo test`; `cargo run` opens the GUI.
The hash-path crates are optimized even in dev profiles (see Cargo.toml).

Design authority: [PLAN.md](PLAN.md). Status: [ROADMAP.md](ROADMAP.md).
Spike results that shaped the design: [docs/spikes.md](docs/spikes.md).

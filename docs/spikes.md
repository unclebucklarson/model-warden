# M0 spike verdicts (run 2026-08-16, read-only against the real stores)

## Spike 1 — hashing 200GB

Full SHA-256 of every model file across shelf + HF hub + Ollama system store:
**304.1 GiB / 58 files in 8.0 minutes, 679 MB/s aggregate** (NVMe, mostly
cold cache — the data far exceeds RAM). Zero unreadable files — the system
Ollama store at `/usr/share/ollama` reads fine as the user on this machine.

**Verdict:** background full-hash is tolerable. Two-tier identity stands:
`(size, mtime, dev, ino)` fingerprint detects change, SHA-256 is the only
identity, computed lazily by a background worker. **No partial-hash middle
tier needed.** Largest single file (22.2 GiB) took 35s — per-file progress
reporting matters, a spinner does not.

Bonus finding, immediately actionable: `bee238bb…` (Qwen3.8-27B-UD-Q4_K_XL,
16.7 GiB) exists as **three paths, two inodes, one content** — the HF blob is
hardlinked to one shelf copy (the sibling's archive feature), but
`~/models/Qwen3.8-27B-UD-Q4_K_XL.gguf` is a second, independent copy.
16.7 GiB reclaimable by hardlink, both copies on the same filesystem. Inode
comparison could never catch this; content hashing did. (Incident 3, again.)

## Spike 2 — manifest format

Serialized a real scan (shelf + HF hub) plus a simulated offline removable
root to the proposed per-root JSON schema; merged view assembled trivially.

**Verdict: JSON per root + merged view works.** Real manifests are 1–7 KB;
the merged inventory ~5 KB for 17 models — scale is nowhere near sqlite
territory. The offline root merged cleanly with its path absent, and "what's
on the unplugged drive labeled X" was answerable from the manifest alone.
The merge also surfaced a real cross-store hardlink (shelf ↔ HF blob) as one
model with two locations. Schema fields proven: root {id, kind, path,
fs_uuid, label}, file {rel_path, size, fingerprint{size,mtime_ns,dev,ino},
sha256|null, gguf|null, provenance|null}.

## Spike 3 — HF hub semantics

Enumerated all 13 real repos: 10 snapshots total, **1 repo with two
snapshots** (refs/main points at only one — scanning refs/main alone misses
files), **4 repos that are empty husks** (refs/main names a revision with no
snapshots/ dir at all — pruned content, dangling ref: incident 2's exact
shape), **2 interrupted downloads** (`.incomplete` blob + orphan), and **1
genuine orphan blob** (349 MiB, superseded revision, reclaimable).

**Verdict:** the scanner must enumerate ALL `snapshots/<rev>` dirs (never
refs), recurse into snapshot subdirs (split-quant layouts), skip
`*.incomplete`, and treat dangling symlinks as present-but-inaccessible
rather than invisible. Store-health findings (dangling refs, orphan blobs,
incomplete downloads) are their own feature — `warden doctor` (added to
roadmap). Pure reads coexist fine with the hf CLI.

## Spike 4 — removable media identity

Machine has 4 filesystems with `/dev/disk/by-uuid` entries, including two
unmounted archive disks (4TB IronWolf ext4 `53b9be4e…`, NTFS "Archive 2"
`7ABC31DD…`).

**Verdict:** fs-UUID (udev `ID_FS_UUID`, a superblock property, stable
across remounts) is the primary identity. Caveats confirmed on this machine:
labels are unreliable (the ext4 archive disk has none), `lsblk RM` is
useless (0 for these disks), VFAT UUIDs are short/weak, and `by-uuid` only
lists *attached* devices — so the manifest must persist the UUID; an
unplugged drive is identified by its stored manifest, not by probing.
Fallback for weak-UUID filesystems: `.modelwarden/root-id` marker file on
owned removable roots.

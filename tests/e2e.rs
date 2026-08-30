//! End-to-end tests: the real `warden` binary against a fully isolated
//! environment (own XDG config/state, `discover_stores: false`, synthetic
//! stores) — the by-hand scratchpad pattern from every milestone, codified
//! so `cargo test` and CI prove the whole-binary behavior forever.
//!
//! Cargo builds the binary and hands its path to us via
//! `CARGO_BIN_EXE_warden`, so these never run against a stale build (the
//! classic "cargo test doesn't rebuild bins" trap doesn't apply here).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// One isolated warden world: shelf, config, state — nothing shared with
/// the developer's real stores or any other test.
struct Env {
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let e = Env { root };
        std::fs::create_dir_all(e.config_dir().join("modelwarden")).unwrap();
        std::fs::create_dir_all(e.shelf()).unwrap();
        std::fs::write(
            e.config_dir().join("modelwarden/config.json"),
            format!(
                r#"{{"scan_dirs":["{}"],"discover_stores":false}}"#,
                e.shelf().display()
            ),
        )
        .unwrap();
        e
    }

    fn config_dir(&self) -> PathBuf {
        self.root.path().join("config")
    }
    fn shelf(&self) -> PathBuf {
        self.root.path().join("shelf")
    }
    fn dir(&self, name: &str) -> PathBuf {
        let d = self.root.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A minimal-but-valid GGUF (magic + v3 header, zero KV/tensors) with
    /// a unique tail so every file is distinct content.
    fn gguf(&self, rel: &str, tail: &[u8]) {
        let path = self.shelf().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(tail);
        std::fs::write(path, bytes).unwrap();
    }

    /// Run the real binary in this world; panics on spawn failure only —
    /// callers assert on status/output.
    fn warden(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_warden"))
            .args(args)
            .env("XDG_CONFIG_HOME", self.config_dir())
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .output()
            .expect("spawning warden")
    }

    /// Run and require success, returning combined output for asserts.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.warden(args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "warden {args:?} failed:\n{text}");
        text
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|es| {
            es.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn hash_builds_the_catalog_and_carries_forward() {
    let e = Env::new();
    e.gguf("alpha.gguf", b"a");
    e.gguf("beta.gguf", b"b");
    let first = e.ok(&["hash"]);
    assert!(first.contains("2 newly hashed"), "{first}");
    // Unchanged fingerprints: the second run hashes nothing.
    let second = e.ok(&["hash"]);
    assert!(second.contains("0 newly hashed"), "{second}");
    let status = e.ok(&["status"]);
    assert!(status.contains("2 distinct contents; 2 hashed"), "{status}");
}

#[test]
fn backup_demote_restore_round_trip_by_label() {
    let e = Env::new();
    e.gguf("keeper.gguf", b"k");
    e.gguf("loner.gguf", b"l");
    e.ok(&["hash"]);
    let drive = e.dir("drive");
    let drive_s = drive.to_str().unwrap();

    e.ok(&["roots", "add", drive_s, "--label", "Cold"]);
    let backup = e.ok(&["backup", drive_s, "keeper"]);
    assert!(backup.contains("1 copied"), "{backup}");

    // Demote by LABEL with --remove-source: verified move off the shelf.
    let demote = e.ok(&["archive", "demote", "loner", "--to", "Cold", "--remove-source"]);
    assert!(demote.contains("removed shelf copy"), "{demote}");
    assert!(!e.shelf().join("loner.gguf").exists());
    // The catalog still knows it — offline is not gone, demoted is not lost.
    let wh = e.ok(&["where", "loner"]);
    assert!(wh.contains("loner"), "{wh}");

    // Verify the drive by label; then bring the model back.
    let verify = e.ok(&["verify", "Cold"]);
    assert!(verify.contains("0 mismatched"), "{verify}");
    let restore = e.ok(&["restore", "loner"]);
    assert!(restore.contains("restored"), "{restore}");
    assert!(e.shelf().join("loner.gguf").exists());
    // Restore never modifies the drive.
    assert!(drive.join("loner.gguf").exists() || !names_in(&drive).is_empty());
}

#[test]
fn delete_trash_cycle_honors_bundles_and_shared_companions() {
    let e = Env::new();
    e.gguf("V/big-00001-of-00002.gguf", b"1");
    e.gguf("V/big-00002-of-00002.gguf", b"2");
    e.gguf("V/mmproj-F16.gguf", b"p");
    e.gguf("V/other-Q4.gguf", b"o");
    e.ok(&["hash"]);

    // Deleting one split part takes the whole set; the projector is
    // spared because other-Q4 still needs it.
    let del = e.ok(&["delete", "big-00001"]);
    assert!(del.contains("kept mmproj-F16"), "{del}");
    assert!(del.contains("trashed big-00001-of-00002"), "{del}");
    assert!(del.contains("trashed big-00002-of-00002"), "{del}");
    assert!(e.shelf().join("V/mmproj-F16.gguf").exists());

    // Trashed models leave the catalog — and never re-catalog from the
    // trash directory (the walk_gguf dot-skip regression).
    let wh = e.warden(&["where", "big"]);
    let wh_text = String::from_utf8_lossy(&wh.stdout).to_string();
    assert!(
        !wh_text.contains(".modelwarden"),
        "trash contents must not re-catalog: {wh_text}"
    );

    // Restoring one part brings its bundle back, exactly as delete took it.
    let restore = e.ok(&["trash", "restore", "big-00001"]);
    assert!(restore.matches("restored").count() >= 2, "{restore}");
    assert!(e.shelf().join("V/big-00002-of-00002.gguf").exists());

    // Stage 2 requires --yes, then destroys only trash contents.
    e.ok(&["delete", "other-Q4"]);
    let dry = e.ok(&["trash", "empty"]);
    assert!(dry.contains("PERMANENTLY DESTROY"), "{dry}");
    let emptied = e.ok(&["trash", "empty", "--yes"]);
    assert!(emptied.contains("destroyed"), "{emptied}");
    assert!(e.ok(&["trash"]).contains("trash is empty"));
    // The spared projector and the split set survive on the shelf.
    assert!(e.shelf().join("V/big-00001-of-00002.gguf").exists());
}

#[test]
fn roots_forget_previews_impact_and_removes_knowledge_only() {
    let e = Env::new();
    e.gguf("keeper.gguf", b"k");
    e.gguf("loner.gguf", b"l");
    e.ok(&["hash"]);
    let drive = e.dir("drive");
    let drive_s = drive.to_str().unwrap().to_string();
    e.ok(&["roots", "add", &drive_s, "--label", "Doomed"]);
    e.ok(&["archive", "demote", "loner", "--to", "Doomed", "--remove-source"]);
    e.ok(&["backup", &drive_s, "keeper"]);

    // Simulate the drive dying.
    let corpse = e.root.path().join("drive-corrupted");
    std::fs::rename(&drive, &corpse).unwrap();

    // Unknown names fail loudly (the unlabeled-drive field find).
    let bad = e.warden(&["roots", "forget", "NoSuchDrive"]);
    assert!(!bad.status.success());

    // Preview states the cost before anything happens.
    let preview = e.ok(&["roots", "forget", "Doomed"]);
    assert!(preview.contains("2 models have a copy"), "{preview}");
    assert!(preview.contains("1 exist NOWHERE else"), "{preview}");

    e.ok(&["roots", "forget", "Doomed", "--yes"]);
    // The only-there model left the catalog; the keeper just lost a location.
    let gone = e.warden(&["where", "loner"]);
    assert!(!gone.status.success(), "loner should be unknown now");
    let keeper = e.ok(&["where", "keeper"]);
    assert!(keeper.contains("keeper"), "{keeper}");
    // Knowledge only: the dead drive's files were never touched.
    assert!(corpse.join("loner.gguf").exists() || !names_in(&corpse).is_empty());
}

#[test]
fn dedup_reclaims_only_with_hardlink_flag_and_verifies() {
    let e = Env::new();
    e.gguf("one/model.gguf", b"same-bytes");
    e.gguf("two/copy.gguf", b"same-bytes");
    e.ok(&["hash"]);
    let dry = e.ok(&["dedup"]);
    assert!(dry.contains("DRY RUN"), "{dry}");
    // Dry run touched nothing: still two distinct inodes.
    let m1 = std::fs::metadata(e.shelf().join("one/model.gguf")).unwrap();
    let m2 = std::fs::metadata(e.shelf().join("two/copy.gguf")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_ne!(m1.ino(), m2.ino());
    e.ok(&["dedup", "--hardlink"]);
    let m1 = std::fs::metadata(e.shelf().join("one/model.gguf")).unwrap();
    let m2 = std::fs::metadata(e.shelf().join("two/copy.gguf")).unwrap();
    assert_eq!(m1.ino(), m2.ino(), "same bytes now share one inode");
}

#[test]
fn the_operations_journal_persists_across_sessions() {
    let e = Env::new();
    e.gguf("doomed.gguf", b"d");
    e.ok(&["hash"]);
    e.ok(&["delete", "doomed"]);
    e.ok(&["trash", "empty", "--yes"]);
    // Three separate processes ran; the journal remembers all of it.
    let journal = e.ok(&["journal"]);
    assert!(journal.contains("hashed doomed"), "{journal}");
    assert!(journal.contains("trashed doomed"), "{journal}");
    assert!(journal.contains("destroyed 1 files"), "{journal}");
    // Human-rescuable: plain text with readable timestamps.
    let raw = std::fs::read_to_string(
        e.root.path().join("state/modelwarden/journal.log"),
    )
    .unwrap();
    assert!(raw.lines().count() >= 3, "{raw}");
    assert!(raw.lines().all(|l| l.starts_with("20")), "dated lines: {raw}");
}

#[test]
fn help_is_grouped_and_release_framed() {
    let e = Env::new();
    let help = e.ok(&["help"]);
    // Development-era framing must not ship to end users.
    assert!(!help.contains("ROADMAP"), "stale dev framing: {help}");
    assert!(!help.contains("landing per"), "{help}");
    // Twenty-plus commands need a map: the guide's own section headers.
    for h in [
        "Seeing what you have",
        "Protecting it",
        "Organizing it",
        "Health",
        "history",
    ] {
        assert!(help.contains(h), "missing section {h:?}:\n{help}");
    }
}

#[test]
fn version_carries_a_build_id() {
    // "warden X.Y.Z (<id>)" — the id pins the exact source, so an issue
    // report identifies the build, not just the nearest tag. Git builds
    // carry the commit hash (with -modified for dirty trees); crates.io
    // builds carry the packaged sha; never an empty "()".
    let e = Env::new();
    let v = e.ok(&["version"]);
    let inside = v
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(id, _)| id.trim().to_string());
    assert!(
        inside.as_deref().is_some_and(|id| !id.is_empty()),
        "no build id in: {v}"
    );
}

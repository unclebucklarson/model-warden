//! Embed a build id so `warden version` and the About dialog pin the
//! exact source, not just the nearest tag. Three tiers:
//!   1. a git checkout: short commit hash, plus "-modified" when the
//!      tree is dirty (the case that most needs disambiguating);
//!   2. a crates.io build (no .git): the sha packaged in
//!      .cargo_vcs_info.json at publish time;
//!   3. anything else: "unknown" — never a lie, never empty.
//! Deliberately NO build timestamp: it would break reproducible builds
//! and adds nothing the hash doesn't.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    let id = git_id()
        .or_else(vcs_info_id)
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=WARDEN_BUILD_ID={id}");
}

fn git_id() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if hash.is_empty() {
        return None;
    }
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());
    Some(if dirty { format!("{hash}-modified") } else { hash })
}

fn vcs_info_id() -> Option<String> {
    let text = std::fs::read_to_string(".cargo_vcs_info.json").ok()?;
    // {"git":{"sha1":"<hex>"}, ...} — a full parser would be overkill.
    let sha = text.split("\"sha1\"").nth(1)?.split('"').nth(1)?;
    (sha.len() >= 9).then(|| sha[..9].to_string())
}

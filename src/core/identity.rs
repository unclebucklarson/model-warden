//! Content identity: the cheap fingerprint and the canonical hash.
//!
//! The `(size, mtime, dev, ino)` fingerprint is a change-detector keyed by
//! path — a fingerprint match means a stored SHA-256 can be reused; a
//! mismatch means rehash (safe, just slow). It is never identity across
//! stores: SHA-256 is the only identity. Spike 1 measured full hashing at
//! ~680 MB/s on this machine (~8 min for 300 GiB), so lazy full hashes with
//! no partial-hash middle tier is the design.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_s: i64,
    pub mtime_nsec: i64,
    pub dev: u64,
    pub ino: u64,
}

impl Fingerprint {
    pub fn of(path: &Path) -> Result<Self> {
        let md = std::fs::metadata(path)
            .with_context(|| format!("fingerprinting {}", path.display()))?;
        Ok(Self::from_metadata(&md))
    }

    /// From a stat the caller already did — the scanners hold one for
    /// every file they list.
    pub fn from_metadata(md: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            size: md.len(),
            mtime_s: md.mtime(),
            mtime_nsec: md.mtime_nsec(),
            dev: md.dev(),
            ino: md.ino(),
        }
    }
}

/// Full SHA-256 of a file, reporting (bytes_done, bytes_total) as it goes —
/// a 22 GiB file takes ~35s, which is too long for a silent spinner.
pub fn sha256_file(path: &Path, mut progress: impl FnMut(u64, u64)) -> Result<String> {
    let md = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let total = md.len();
    // Read straight from the file: a BufReader here wrapped a read
    // buffer of its own size, and BufReader bypasses its buffer entirely
    // for reads at or above its capacity — so the second 4 MiB was pure
    // resident memory, 8 MiB per hashing thread and 32 MiB across the
    // pool, bought nothing.
    let mut reader =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    Ok(crate::core::format::hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_known_vector_and_report_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"abc").unwrap();
        let mut calls = Vec::new();
        let h = sha256_file(&path, |d, t| calls.push((d, t))).unwrap();
        // SHA-256("abc"), the classic test vector.
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(calls, vec![(3, 3)]);
    }

    #[test]
    fn fingerprint_changes_when_content_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"one").unwrap();
        let fp1 = Fingerprint::of(&path).unwrap();
        assert_eq!(fp1, Fingerprint::of(&path).unwrap(), "stable at rest");
        std::fs::write(&path, b"longer content").unwrap();
        assert_ne!(fp1, Fingerprint::of(&path).unwrap());
    }
}

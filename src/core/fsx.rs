//! Filesystem primitives warden's safety rules depend on.
//!
//! Two things every write path here needs and the standard library does
//! not offer directly: a rename that *refuses* to replace (the
//! "refuse-overwrite everywhere" rule was a `exists()` check over a
//! `rename` that silently replaces — a check, not a guarantee), and a
//! temp name that keeps a file's real extension.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Rename `from` → `to`, failing if `to` already exists.
///
/// `std::fs::rename` silently replaces the destination on Unix, so an
/// `exists()` check before it is a TOCTOU window: anything that appears
/// in between is destroyed. This closes that window where the kernel
/// lets it (`renameat2(RENAME_NOREPLACE)`), and degrades in documented
/// steps everywhere else.
pub fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        match renameat2_noreplace(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => match e.raw_os_error() {
                // The destination exists: that is the answer, not a
                // reason to try something weaker.
                Some(libc::EEXIST) | Some(libc::ENOTEMPTY) => {
                    bail!("{} already exists — refusing to overwrite", to.display())
                }
                // Old kernel or a filesystem without RENAME_NOREPLACE
                // (notably some FUSE and network mounts): fall through.
                Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP) => {}
                _ => {
                    return Err(e).with_context(|| {
                        format!("renaming {} → {}", from.display(), to.display())
                    });
                }
            },
        }
    }
    // Portable fallback: link() refuses an existing destination, so the
    // guarantee survives. Filesystems without hardlinks (exFAT, FAT32 —
    // real backup drives) reject it, and only there do we fall back to
    // the racy check.
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            std::fs::remove_file(from)
                .with_context(|| format!("removing {}", from.display()))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("{} already exists — refusing to overwrite", to.display())
        }
        Err(_) => {
            if to.exists() {
                bail!("{} already exists — refusing to overwrite", to.display());
            }
            std::fs::rename(from, to)
                .with_context(|| format!("renaming {} → {}", from.display(), to.display()))
        }
    }
}

#[cfg(target_os = "linux")]
fn renameat2_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let f = CString::new(from.as_os_str().as_bytes())?;
    let t = CString::new(to.as_os_str().as_bytes())?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A temp name beside `dest` that keeps the real extension:
/// `model.gguf` → `model.gguf.partial`, `config.json` →
/// `config.json.partial`, `LICENSE` → `LICENSE.partial`.
///
/// `Path::with_extension` *replaces* the extension, so the old
/// `with_extension("gguf.partial")` turned `config.json` into
/// `config.gguf.partial` and — worse — gave two files in one directory
/// that differ only by extension the same temp path.
pub fn temp_sibling(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    name.push('.');
    name.push_str(suffix);
    dest.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_noreplace_never_destroys_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"precious").unwrap();

        let err = rename_noreplace(&src, &dst).unwrap_err().to_string();
        assert!(err.contains("refusing to overwrite"), "{err}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"precious");
        assert!(src.is_file(), "the source must survive a refusal");

        // A free destination still works, and the source is gone.
        let free = dir.path().join("free");
        rename_noreplace(&src, &free).unwrap();
        assert_eq!(std::fs::read(&free).unwrap(), b"new");
        assert!(!src.exists());
    }

    #[test]
    fn temp_names_keep_the_real_extension() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            temp_sibling(&p("/a/model.gguf"), "partial"),
            p("/a/model.gguf.partial")
        );
        // The old with_extension form collapsed these two onto one name.
        assert_eq!(
            temp_sibling(&p("/a/model.safetensors"), "partial"),
            p("/a/model.safetensors.partial")
        );
        assert_eq!(
            temp_sibling(&p("/a/model.bin"), "partial"),
            p("/a/model.bin.partial")
        );
        assert_eq!(temp_sibling(&p("/a/LICENSE"), "partial"), p("/a/LICENSE.partial"));
    }
}

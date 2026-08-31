//! Single-instance write lock: two wardens must never race a backup,
//! reclaim, or catalog rewrite.
//!
//! An advisory lock (`flock(LOCK_EX|LOCK_NB)`) on a file under the state
//! dir. The kernel arbitrates, so there is nothing to race and nothing
//! to clean up: a holder that crashes releases the lock when its
//! descriptor closes, and a leftover lock *file* means nothing on its
//! own. The pid is still written inside, purely so the refusal message
//! can name who is holding it.
//!
//! This replaces a pid-file protocol with two defects: it decided
//! staleness by reading the file and then unconditionally deleting it
//! (two wardens could each delete the other's fresh lock and both
//! proceed), and it created the file before writing the pid (a reader in
//! that window saw an empty file and stole a *live* lock). Read commands
//! never lock — only operations that write.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct WriteLock {
    path: PathBuf,
    /// Holding the descriptor open IS the lock; dropping it releases.
    file: std::fs::File,
}

pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("warden.lock")
}

impl WriteLock {
    /// Take the write lock or explain who holds it.
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        let path = lock_path(state_dir);
        crate::core::settings::create_private_dir(state_dir)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        if !try_lock(&file)? {
            let holder = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            match holder {
                Some(pid) => bail!(
                    "another warden (pid {pid}) is writing — retry when it finishes"
                ),
                None => bail!("another warden is writing — retry when it finishes"),
            }
        }
        // Ours now: record who, for the other side's error message.
        use std::io::{Seek, Write};
        let mut f = &file;
        let _ = f.set_len(0);
        let _ = f.rewind();
        let _ = writeln!(f, "{}", std::process::id());
        let _ = f.flush();
        Ok(Self { path, file })
    }
}

/// `true` when the lock was taken, `false` when someone else holds it.
#[cfg(unix)]
fn try_lock(file: &std::fs::File) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(false),
        _ => Err(err).context("locking the state directory"),
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        // Unlink first, then release: a waiter that wins the lock on a
        // fresh open never finds itself holding a file we then delete.
        let _ = std::fs::remove_file(&self.path);
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_held_then_succeeds_after_drop() {
        let state = tempfile::tempdir().unwrap();
        let lock = WriteLock::acquire(state.path()).unwrap();
        let err = WriteLock::acquire(state.path()).unwrap_err();
        assert!(format!("{err}").contains("another warden"));
        drop(lock);
        assert!(WriteLock::acquire(state.path()).is_ok());
    }

    #[test]
    fn a_stale_lock_from_a_dead_process_is_stolen() {
        let state = tempfile::tempdir().unwrap();
        // A crashed holder leaves the file behind; nothing holds it.
        std::fs::write(lock_path(state.path()), "999999999\n").unwrap();
        let lock = WriteLock::acquire(state.path()).unwrap();
        drop(lock);
        assert!(!lock_path(state.path()).exists());
    }

    #[test]
    fn garbage_lock_files_are_stolen_too() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(lock_path(state.path()), "not a pid").unwrap();
        assert!(WriteLock::acquire(state.path()).is_ok());
    }

    #[test]
    fn a_lock_file_nobody_holds_is_not_an_obstacle() {
        // The old protocol decided liveness by parsing a pid and asking
        // whether *some* process with that number exists. After a crash
        // and pid reuse that answer is wrong in the dangerous direction:
        // warden refuses every write forever. Worse, deciding staleness
        // by reading the file raced with the writer of a brand-new lock.
        // With an advisory lock on the descriptor, "held" is a fact the
        // kernel answers, and death releases it.
        let state = tempfile::tempdir().unwrap();
        // A leftover file naming a live pid — this process — held by nobody.
        std::fs::write(
            lock_path(state.path()),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        let lock = WriteLock::acquire(state.path())
            .expect("an unheld lock file must never block a write");
        // And while it IS held, a second acquisition is refused.
        assert!(WriteLock::acquire(state.path()).is_err());
        drop(lock);
        assert!(WriteLock::acquire(state.path()).is_ok());
    }
}
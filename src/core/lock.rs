//! Single-instance write lock: two wardens must never race a backup,
//! reclaim, or catalog rewrite.
//!
//! A pid file under the state dir, dependency-free: created with
//! `create_new` (atomic on Linux), holder's pid inside. A lock whose pid no
//! longer exists in /proc is stale (crashed warden) and is stolen. Read
//! commands never lock — only operations that write.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct WriteLock {
    path: PathBuf,
}

pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("warden.lock")
}

impl WriteLock {
    /// Take the write lock or explain who holds it. The guard removes the
    /// lock file on drop (including unwind).
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        let path = lock_path(state_dir);
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok());
                    match holder {
                        Some(pid) if process_alive(pid) => {
                            bail!(
                                "another warden (pid {pid}) is writing — retry when it finishes \
                                 (or remove {} if that pid is not warden)",
                                path.display()
                            );
                        }
                        _ => {
                            // Stale (crashed holder) or unreadable: steal.
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
    }
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
        // No such pid on any sane system.
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
}

//! Scheduled scrub: a systemd user timer that periodically re-reads every
//! byte warden tracks and compares it against the catalog.
//!
//! The service runs `warden hash && warden verify --all`. The order is the
//! design: `hash` refreshes manifests first, so a file the user legitimately
//! changed (new mtime → new fingerprint) is re-hashed and passes; a file
//! whose bytes changed *without* its fingerprint changing — the bit-rot
//! signature — is exactly what `verify --all` then catches. Exit 1 on any
//! mismatch makes the unit's failure state the alert.
//!
//! Warden writes the two unit files (an explicit, user-invoked config
//! change) but does not enable them — starting services is the user's call,
//! and the command to run is printed.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const SERVICE_NAME: &str = "modelwarden-scrub.service";
pub const TIMER_NAME: &str = "modelwarden-scrub.timer";

/// systemd user-unit directory: `$XDG_CONFIG_HOME/systemd/user`.
pub fn unit_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("systemd/user")
}

/// The two unit files, generated from the running binary's path so the
/// timer survives however warden was installed.
pub fn unit_files(warden_bin: &Path, calendar: &str) -> (String, String) {
    let service = format!(
        "[Unit]\n\
         Description=modelwarden scrub: refresh catalog, re-verify every tracked byte\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=/bin/sh -c '{bin} hash && {bin} verify --all'\n\
         Nice=10\n\
         IOSchedulingClass=idle\n",
        bin = warden_bin.display()
    );
    let timer = format!(
        "[Unit]\n\
         Description=Run the modelwarden scrub {calendar}\n\
         \n\
         [Timer]\n\
         OnCalendar={calendar}\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    );
    (service, timer)
}

/// Write both units. Returns the paths written and the enable command the
/// user still has to run themselves.
pub fn install(calendar: &str) -> Result<(Vec<PathBuf>, String)> {
    let bin = std::env::current_exe().context("resolving the warden binary path")?;
    let (service, timer) = unit_files(&bin, calendar);
    let dir = unit_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let spath = dir.join(SERVICE_NAME);
    let tpath = dir.join(TIMER_NAME);
    std::fs::write(&spath, service).with_context(|| format!("writing {}", spath.display()))?;
    std::fs::write(&tpath, timer).with_context(|| format!("writing {}", tpath.display()))?;
    Ok((
        vec![spath, tpath],
        format!("systemctl --user enable --now {TIMER_NAME}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_wire_the_binary_and_calendar_through() {
        let (service, timer) = unit_files(Path::new("/opt/warden"), "weekly");
        assert!(service.contains("ExecStart=/bin/sh -c '/opt/warden hash && /opt/warden verify --all'"));
        assert!(service.contains("Type=oneshot"));
        assert!(
            service.contains("IOSchedulingClass=idle"),
            "a scrub must never contend with real work for the disk"
        );
        assert!(timer.contains("OnCalendar=weekly"));
        assert!(timer.contains("Persistent=true"), "missed runs catch up");
        assert!(timer.contains("WantedBy=timers.target"));
    }
}

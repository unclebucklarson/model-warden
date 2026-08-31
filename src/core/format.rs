//! Shared display formatting, so both frontends speak identically: the CLI
//! prints and the GUI activity panel logs the same words for the same
//! event (`log_line()` on each event enum lives beside its type and uses
//! these helpers).

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Relative-time label ("3 min ago") — one copy, so the CLI, the GUI
/// activity timestamps, and the Trash tab can never drift apart.
pub fn ago(unix: u64) -> String {
    let now = crate::core::manifest::now_unix();
    let d = now.saturating_sub(unix);
    match d {
        0..=90 => format!("{d}s ago"),
        91..=5400 => format!("{} min ago", d / 60),
        5401..=172_800 => format!("{} hours ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

/// The first 12 characters of an identity, for display. Never panics:
/// hashes are read from manifests on disk (including a drive's own), so
/// a corrupt or hand-edited one can hold anything at all — and
/// `&hash[..12]` on a short or non-ASCII value aborted the process,
/// inside `verify`, which is what the scrub timer runs.
pub fn short_hash(hash: &str) -> &str {
    match hash.char_indices().nth(12) {
        Some((byte_idx, _)) => &hash[..byte_idx],
        None => hash,
    }
}

/// Unix seconds → "YYYY-MM-DD HH:MM:SS" (UTC), no dependencies —
/// Howard Hinnant's civil-from-days algorithm. Journal lines must stay
/// readable to a human with cat, forever.
pub fn utc_datetime(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // civil_from_days (public-domain algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_datetimes_render_correctly() {
        assert_eq!(utc_datetime(0), "1970-01-01 00:00:00");
        // 2026-08-16 20:30:00 UTC
        assert_eq!(utc_datetime(1_786_912_200), "2026-08-16 20:30:00");
        // Leap-year February.
        assert_eq!(utc_datetime(1_709_164_800), "2024-02-29 00:00:00");
    }

    #[test]
    fn short_hashes_never_panic_on_junk() {
        assert_eq!(short_hash(&"a".repeat(64)), "aaaaaaaaaaaa");
        assert_eq!(short_hash("abc"), "abc", "shorter than 12");
        assert_eq!(short_hash(""), "");
        // Multi-byte characters: byte 12 is mid-character here, which is
        // precisely what used to abort.
        // Multi-byte characters: byte 12 lands mid-character here, which
        // is precisely what used to abort.
        assert_eq!(short_hash("日本語日本語日本語"), "日本語日本語日本語");
        assert_eq!(short_hash("日本語日本語日本語日本語日本語").chars().count(), 12);
    }

    #[test]
    fn human_sizes_render_stably() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(17_924_717_632), "16.7 GiB");
    }
}

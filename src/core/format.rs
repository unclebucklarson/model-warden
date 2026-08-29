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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes_render_stably() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(17_924_717_632), "16.7 GiB");
    }
}

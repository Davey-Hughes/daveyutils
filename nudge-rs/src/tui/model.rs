//! Dashboard state and its pure helpers.

/// Format a signed seconds delta as a compact countdown: `2h 14m`, `6d 3h`,
/// `45m`, `12s`. A non-positive delta (the job's time is here or past) renders
/// as `now` — the daemon fires within its grace window, and a dashboard that
/// showed a negative countdown would look broken.
pub fn human_countdown(delta_secs: i64) -> String {
    if delta_secs <= 0 {
        return "now".to_string();
    }
    let d = delta_secs / 86_400;
    let h = (delta_secs % 86_400) / 3_600;
    let m = (delta_secs % 3_600) / 60;
    let s = delta_secs % 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_formats_by_largest_unit() {
        assert_eq!(human_countdown(45 * 60), "45m");
        assert_eq!(human_countdown(2 * 3600 + 14 * 60), "2h 14m");
        assert_eq!(human_countdown(6 * 86400 + 3 * 3600), "6d 3h");
        assert_eq!(human_countdown(12), "12s");
    }

    #[test]
    fn a_past_or_zero_delta_reads_now_not_a_negative() {
        assert_eq!(human_countdown(0), "now");
        assert_eq!(human_countdown(-500), "now");
    }
}

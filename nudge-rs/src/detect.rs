//! Detect an AI-CLI rate-limit banner in captured pane text and compute the
//! absolute reset time (with safety padding).

use jiff::{ToSpan, Zoned};
use regex::Regex;

use crate::timespec::parse_timespec;

/// Padding added to every detected reset time to absorb scheduler latency.
const PADDING_MINUTES: i64 = 3;

/// Built-in clock-shape banner alternation, optionally extended by the user's
/// `NUDGE_CLOCK_PATTERN`.
fn clock_re(ext: Option<&str>) -> Regex {
    build_re(r"(?:session limit|current session).*resets", ext)
}

/// Built-in duration-shape banner alternation, optionally extended by
/// `NUDGE_DURATION_PATTERN`.
fn duration_re(ext: Option<&str>) -> Regex {
    build_re(r"quota reached", ext)
}

fn build_re(base: &str, ext: Option<&str>) -> Regex {
    let pattern = match ext {
        Some(e) if !e.is_empty() => format!("(?i)(?:{base}|{e})"),
        _ => format!("(?i)(?:{base})"),
    };
    Regex::new(&pattern).expect("valid built-in banner regex")
}

/// Which banner shape a match came from — it decides how the countdown token
/// after the banner is read, not which banner wins.
#[derive(Clone, Copy)]
enum Shape {
    /// "... Resets in 1h30m / 45m", or a fully custom NUDGE_DURATION_PATTERN
    /// banner (e.g. "out of credits, back in 20m") whose countdown need not
    /// follow the literal word "resets".
    Duration,
    /// "... resets 3:00pm" / "... try again at 4pm".
    Clock,
}

/// Returns the padded absolute reset time, or `None` if no banner is present.
pub fn detect_reset(
    pane_text: &str,
    now: &Zoned,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
) -> Option<Zoned> {
    let clean = strip_ansi_escapes::strip_str(pane_text);

    // A captured pane is chronological top-to-bottom, so the banner *lowest* on
    // screen is the live one. `Regex::find` is leftmost, and checking one shape
    // before the other imposes an order unrelated to the pane — either way a
    // superseded banner still on screen would beat the current one and the
    // nudge would fire hours early. Collect every banner of both shapes and
    // walk them bottom-up instead, so recency alone decides.
    let mut banners: Vec<(usize, usize, Shape)> = Vec::new();
    banners.extend(
        duration_re(dur_ext)
            .find_iter(&clean)
            .map(|m| (m.start(), m.end(), Shape::Duration)),
    );
    banners.extend(
        clock_re(clock_ext)
            .find_iter(&clean)
            .map(|m| (m.start(), m.end(), Shape::Clock)),
    );
    // Bottom-up. sort_by_key is stable, so two shapes matching at the same
    // offset keep the duration-first order callers have always seen.
    banners.sort_by_key(|b| std::cmp::Reverse(b.0));

    for (_, end, shape) in banners {
        // Scan only the text *after* the banner match: a captured pane includes
        // scrollback, and an unrelated duration-shaped substring earlier in the
        // pane (e.g. "16 minutes ago" in a shell prompt) must not be mistaken
        // for the banner's own countdown.
        let rest = &clean[end..];
        let token = match shape {
            Shape::Duration => find_duration_token(rest),
            Shape::Clock => find_clock_token(rest),
        };
        if let Some(token) = token {
            if let Ok(z) = parse_timespec(&token, now) {
                return z.checked_add(PADDING_MINUTES.minutes()).ok();
            }
        }
        // This banner carried no parseable countdown (a bare "quota reached"
        // with the time on a line the capture cut off). Fall back to the next
        // one up rather than giving up on the pane entirely.
    }

    None
}

/// Extract the first "3pm" / "3:00 PM" / "14:30" token from the text.
fn find_clock_token(text: &str) -> Option<String> {
    let re = Regex::new(r"(?i)\b(\d{1,2}(?::\d{2})?\s*(?:am|pm)|\d{1,2}:\d{2})\b").unwrap();
    re.find(text).map(|m| m.as_str().to_string())
}

/// Extract the first duration-shaped token ("1h30m", "45m", "20m", ...) from
/// the text. Not anchored to a literal "resets in" prefix so a fully custom
/// `NUDGE_DURATION_PATTERN` banner can phrase its countdown however it likes
/// (e.g. "back in 20m"), as long as the countdown itself looks like a
/// duration `parse_timespec` understands.
fn find_duration_token(text: &str) -> Option<String> {
    let re = Regex::new(
        r"(?i)\b(\d+\s*h(?:ours?|rs?)?(?:\s*\d+\s*m(?:in(?:ute)?s?)?)?|\d+\s*m(?:in(?:ute)?s?)?)\b",
    )
    .unwrap();
    re.find(text).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::{civil::date, tz::TimeZone};

    fn now() -> jiff::Zoned {
        date(2026, 7, 13)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap()
    }

    #[test]
    fn detects_claude_clock_banner_with_padding() {
        let pane = "Approaching usage limit — current session resets 3:00pm";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        // 15:00 + 3 minutes padding.
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    #[test]
    fn detects_agy_duration_banner_with_padding() {
        let pane = "quota reached. Resets in 1h30m";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        // now 10:00 + 1h30m + 3m padding = 11:33.
        assert_eq!((z.hour(), z.minute()), (11, 33));
    }

    #[test]
    fn duration_is_case_insensitive() {
        let pane = "QUOTA REACHED — RESETS IN 45M";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        assert_eq!((z.hour(), z.minute()), (10, 48));
    }

    #[test]
    fn ignores_ansi_colour_codes() {
        let pane = "\x1b[31mquota reached\x1b[0m Resets in 45m";
        assert!(detect_reset(pane, &now(), None, None).is_some());
    }

    #[test]
    fn custom_patterns_extend_detection() {
        let clock = "codex is rate limited — try again at 4pm";
        assert!(detect_reset(clock, &now(), Some("rate limited"), None).is_some());

        let dur = "out of credits, back in 20m";
        assert!(detect_reset(dur, &now(), None, Some("out of credits")).is_some());
    }

    #[test]
    fn no_banner_returns_none() {
        assert!(detect_reset("all good here", &now(), None, None).is_none());
    }

    #[test]
    fn duration_token_bound_to_banner_not_scrollback() {
        // A scrollback line with a duration-shaped phrase ABOVE the real banner
        // must not hijack the reset time.
        let pane = "commit abc123 16 minutes ago\nquota reached. Resets in 45m";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        // now 10:00 + 45m + 3m padding = 10:48, NOT 10:00 + 16m + 3m.
        assert_eq!((z.hour(), z.minute()), (10, 48));
    }

    #[test]
    fn clock_token_bound_to_banner_not_scrollback() {
        // A scrollback clock time ABOVE the real banner must not hijack the
        // reset time either.
        let pane = "started at 9:15\ncurrent session resets 3:00pm";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        // 15:00 + 3m padding = 15:03, NOT derived from the 9:15 scrollback time.
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    #[test]
    fn newest_of_two_duration_banners_wins() {
        // A pane is chronological top-to-bottom: the stale 45m banner scrolled
        // up the screen is superseded by the live 3h one below it.
        let pane =
            "quota reached. Resets in 45m\n... hours of work ...\nquota reached. Resets in 3h";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        // now 10:00 + 3h + 3m padding = 13:03, NOT the stale banner's 10:48.
        assert_eq!((z.hour(), z.minute()), (13, 3));
    }

    #[test]
    fn later_clock_banner_beats_earlier_duration_banner() {
        // Shape must not decide precedence: the clock banner sits lower on
        // screen, so it is the live one even though the duration branch used to
        // run first unconditionally.
        let pane = "quota reached. Resets in 45m\nlater output\ncurrent session resets 3:00pm";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        // 15:00 + 3m padding = 15:03, NOT the stale duration banner's 10:48.
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    #[test]
    fn later_duration_banner_beats_earlier_clock_banner() {
        // The mirror case, so "last banner wins" is not accidentally satisfied
        // by simply flipping the hardcoded branch order.
        let pane = "current session resets 3:00pm\nlater output\nquota reached. Resets in 45m";
        let z = detect_reset(pane, &now(), None, None).unwrap();
        // now 10:00 + 45m + 3m padding = 10:48, NOT the stale clock banner's 15:03.
        assert_eq!((z.hour(), z.minute()), (10, 48));
    }
}

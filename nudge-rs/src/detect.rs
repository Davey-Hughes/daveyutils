//! Detect an AI-CLI rate-limit banner in captured pane text and compute the
//! absolute reset time (with safety padding).

use jiff::{ToSpan, Zoned};
use regex::Regex;

use crate::timespec::parse_timespec;

/// Padding added to every detected reset time to absorb scheduler latency.
const PADDING_MINUTES: i64 = 3;

/// The built-in clock-shape banner alternation.
const CLOCK_BASE: &str = r"(?:session limit|current session).*resets";

/// The built-in duration-shape banner alternation.
const DURATION_BASE: &str = r"quota reached";

/// Built-in clock-shape banner alternation, optionally extended by the user's
/// `NUDGE_CLOCK_PATTERN`.
fn clock_re(ext: Option<&str>) -> Regex {
    build_re("NUDGE_CLOCK_PATTERN", CLOCK_BASE, ext)
}

/// Built-in duration-shape banner alternation, optionally extended by
/// `NUDGE_DURATION_PATTERN`.
fn duration_re(ext: Option<&str>) -> Regex {
    build_re("NUDGE_DURATION_PATTERN", DURATION_BASE, ext)
}

/// The one place a `base`/`ext` pair becomes a regex, so that what
/// [`validate_patterns`] checks is exactly what [`build_re`] will build.
fn compile(base: &str, ext: &str) -> Result<Regex, regex::Error> {
    Regex::new(&format!("(?i)(?:{base}|{ext})"))
}

/// Reject a `NUDGE_*_PATTERN` the CLI cannot use, naming the variable.
///
/// [`build_re`] must never fail — it runs on the daemon's scheduler thread,
/// where a panic kills every pending job — so it degrades to the built-in
/// pattern and warns. But `init_tracing()` only runs in daemon mode, so on the
/// CLI path there is no subscriber to carry that warning and it is discarded;
/// in the auto-started daemon stderr is /dev/null. The typo was therefore
/// silently ignored, and all the user saw was `no rate-limit banner detected in
/// <pane>` — pointing at the pane, when the fault is in their environment.
///
/// So the CLI, which has a user to talk to, checks up front and says so. The
/// daemon keeps the warn-and-degrade: a bad pattern still must not kill it.
pub fn validate_patterns(clock_ext: Option<&str>, dur_ext: Option<&str>) -> anyhow::Result<()> {
    for (var, base, ext) in [
        ("NUDGE_CLOCK_PATTERN", CLOCK_BASE, clock_ext),
        ("NUDGE_DURATION_PATTERN", DURATION_BASE, dur_ext),
    ] {
        // Unset and empty both mean "no extension", which is the common case.
        if let Some(e) = ext.filter(|e| !e.is_empty()) {
            if let Err(err) = compile(base, e) {
                anyhow::bail!("invalid {var}={e:?}: {err}");
            }
        }
    }
    Ok(())
}

/// The built-in `base` alone. `base` is a literal in this module, so this is
/// the one compile that genuinely cannot fail.
fn builtin_re(base: &str) -> Regex {
    Regex::new(&format!("(?i)(?:{base})")).expect("valid built-in banner regex")
}

/// `base`, extended with the user's `ext` pattern from `var` when it compiles.
///
/// `ext` is raw env input interpolated into a regex, so it is routinely
/// invalid: `(`, `*` and `a[b` all read as plain text to someone writing a
/// banner phrase. Nothing user-supplied may panic here — detect_reset runs on
/// the daemon's scheduler thread, where a panic takes down every pending job.
/// An unusable extension degrades to the built-in banner with a warning naming
/// the variable to fix.
fn build_re(var: &str, base: &str, ext: Option<&str>) -> Regex {
    let Some(e) = ext.filter(|e| !e.is_empty()) else {
        return builtin_re(base);
    };
    match compile(base, e) {
        Ok(re) => re,
        Err(err) => {
            tracing::warn!(
                "nudge: ignoring invalid {var}={e:?} ({err}); \
                 falling back to the built-in banner pattern"
            );
            builtin_re(base)
        }
    }
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

/// What `detect_reset` concluded about a pane.
///
/// `Option<Zoned>` cannot express the third outcome. A weekly banner whose reset
/// day we cannot read is not "no banner" — reporting it as such prints `no
/// rate-limit banner detected in <pane>`, blaming the pane for a gap in this
/// parser. It is a distinct answer with a distinct remedy, so it gets a variant.
#[derive(Debug)]
pub enum Detection {
    /// No rate-limit banner in the pane.
    None,
    /// A banner, and the padded absolute reset time it names.
    Reset(Zoned),
    /// A weekly banner whose reset day this parser does not understand.
    /// Carries the offending text verbatim so the report *is* the bug capture.
    Unreadable { banner: String, gap: String },
}

/// Returns the padded absolute reset time, or `Detection::None` if no banner is
/// present.
pub fn detect_reset(
    pane_text: &str,
    now: &Zoned,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
) -> Detection {
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
                if let Ok(padded) = z.checked_add(PADDING_MINUTES.minutes()) {
                    return Detection::Reset(padded);
                }
            }
        }
        // This banner carried no parseable countdown (a bare "quota reached"
        // with the time on a line the capture cut off). Fall back to the next
        // one up rather than giving up on the pane entirely.
    }

    Detection::None
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

    /// Unwrap a `Detection::Reset`, or fail naming what came back instead.
    fn reset_of(d: Detection) -> jiff::Zoned {
        match d {
            Detection::Reset(z) => z,
            other => panic!("expected Detection::Reset, got {other:?}"),
        }
    }

    #[test]
    fn detects_claude_clock_banner_with_padding() {
        let pane = "Approaching usage limit — current session resets 3:00pm";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // 15:00 + 3 minutes padding.
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    #[test]
    fn detects_agy_duration_banner_with_padding() {
        let pane = "quota reached. Resets in 1h30m";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // now 10:00 + 1h30m + 3m padding = 11:33.
        assert_eq!((z.hour(), z.minute()), (11, 33));
    }

    #[test]
    fn duration_is_case_insensitive() {
        let pane = "QUOTA REACHED — RESETS IN 45M";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        assert_eq!((z.hour(), z.minute()), (10, 48));
    }

    #[test]
    fn ignores_ansi_colour_codes() {
        let pane = "\x1b[31mquota reached\x1b[0m Resets in 45m";
        assert!(matches!(
            detect_reset(pane, &now(), None, None),
            Detection::Reset(_)
        ));
    }

    #[test]
    fn custom_patterns_extend_detection() {
        let clock = "codex is rate limited — try again at 4pm";
        assert!(matches!(
            detect_reset(clock, &now(), Some("rate limited"), None),
            Detection::Reset(_)
        ));

        let dur = "out of credits, back in 20m";
        assert!(matches!(
            detect_reset(dur, &now(), None, Some("out of credits")),
            Detection::Reset(_)
        ));
    }

    #[test]
    fn no_banner_returns_none() {
        assert!(matches!(
            detect_reset("all good here", &now(), None, None),
            Detection::None
        ));
    }

    #[test]
    fn duration_token_bound_to_banner_not_scrollback() {
        // A scrollback line with a duration-shaped phrase ABOVE the real banner
        // must not hijack the reset time.
        let pane = "commit abc123 16 minutes ago\nquota reached. Resets in 45m";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // now 10:00 + 45m + 3m padding = 10:48, NOT 10:00 + 16m + 3m.
        assert_eq!((z.hour(), z.minute()), (10, 48));
    }

    #[test]
    fn clock_token_bound_to_banner_not_scrollback() {
        // A scrollback clock time ABOVE the real banner must not hijack the
        // reset time either.
        let pane = "started at 9:15\ncurrent session resets 3:00pm";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // 15:00 + 3m padding = 15:03, NOT derived from the 9:15 scrollback time.
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    #[test]
    fn validate_patterns_names_the_variable_that_is_wrong() {
        // The daemon must degrade, but the CLI has a user standing right there
        // and should say so. Same metacharacter typos as the fallback test.
        for bad in ["codex (", "*", "a[b"] {
            let e = validate_patterns(Some(bad), None).unwrap_err().to_string();
            assert!(
                e.contains("invalid NUDGE_CLOCK_PATTERN"),
                "clock ext {bad:?} must be reported against its own variable, got: {e}"
            );

            let e = validate_patterns(None, Some(bad)).unwrap_err().to_string();
            assert!(
                e.contains("invalid NUDGE_DURATION_PATTERN"),
                "duration ext {bad:?} must be reported against its own variable, got: {e}"
            );
        }
    }

    #[test]
    fn validate_patterns_accepts_what_detect_reset_accepts() {
        // Whatever this passes, `build_re` must actually be able to use -- and
        // unset/empty is the overwhelmingly common case and not an error.
        assert!(validate_patterns(None, None).is_ok());
        assert!(validate_patterns(Some(""), Some("")).is_ok());
        assert!(validate_patterns(Some("rate limited"), Some("out of credits")).is_ok());
    }

    #[test]
    fn invalid_extension_pattern_falls_back_to_the_builtin() {
        // A user writing a banner phrase reasonably types regex metacharacters
        // as plain text. None of these may panic: detect_reset runs in the CLI
        // *and* on the daemon's scheduler thread, where a panic kills every
        // pending job.
        for bad in ["codex (", "*", "a[b"] {
            let z = reset_of(detect_reset(
                "current session resets 3:00pm",
                &now(),
                Some(bad),
                None,
            ));
            assert_eq!(
                (z.hour(), z.minute()),
                (15, 3),
                "clock ext {bad:?} must fall back to the built-in banner"
            );

            let d = reset_of(detect_reset(
                "quota reached. Resets in 45m",
                &now(),
                None,
                Some(bad),
            ));
            assert_eq!(
                (d.hour(), d.minute()),
                (10, 48),
                "duration ext {bad:?} must fall back to the built-in banner"
            );
        }
    }

    #[test]
    fn newest_of_two_duration_banners_wins() {
        // A pane is chronological top-to-bottom: the stale 45m banner scrolled
        // up the screen is superseded by the live 3h one below it.
        let pane =
            "quota reached. Resets in 45m\n... hours of work ...\nquota reached. Resets in 3h";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // now 10:00 + 3h + 3m padding = 13:03, NOT the stale banner's 10:48.
        assert_eq!((z.hour(), z.minute()), (13, 3));
    }

    #[test]
    fn later_clock_banner_beats_earlier_duration_banner() {
        // Shape must not decide precedence: the clock banner sits lower on
        // screen, so it is the live one even though the duration branch used to
        // run first unconditionally.
        let pane = "quota reached. Resets in 45m\nlater output\ncurrent session resets 3:00pm";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // 15:00 + 3m padding = 15:03, NOT the stale duration banner's 10:48.
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    #[test]
    fn later_duration_banner_beats_earlier_clock_banner() {
        // The mirror case, so "last banner wins" is not accidentally satisfied
        // by simply flipping the hardcoded branch order.
        let pane = "current session resets 3:00pm\nlater output\nquota reached. Resets in 45m";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // now 10:00 + 45m + 3m padding = 10:48, NOT the stale clock banner's 15:03.
        assert_eq!((z.hour(), z.minute()), (10, 48));
    }

    /// The enum's whole point: "no banner" and "a banner I can't read" are
    /// different answers, and a caller must be able to tell them apart.
    #[test]
    fn detection_distinguishes_absent_from_unreadable() {
        assert!(matches!(
            detect_reset("all good here", &now(), None, None),
            Detection::None
        ));
        assert!(matches!(
            detect_reset("current session resets 3:00pm", &now(), None, None),
            Detection::Reset(_)
        ));
        // The third answer, Detection::Unreadable, gets real coverage when the
        // weekly shape lands and detect_reset actually produces it (Task 4). No
        // production path constructs it yet, so asserting on a hand-built value
        // here would only re-check the compiler; left to the shape that earns it.
    }
}

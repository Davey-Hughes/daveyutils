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

/// The built-in weekly-shape banner alternation.
const WEEKLY_BASE: &str = r"weekly limit.*resets";

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

/// Built-in weekly-shape banner alternation, optionally extended by
/// `NUDGE_WEEKLY_PATTERN`.
fn weekly_re(ext: Option<&str>) -> Regex {
    build_re("NUDGE_WEEKLY_PATTERN", WEEKLY_BASE, ext)
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
        (
            "NUDGE_WEEKLY_PATTERN",
            WEEKLY_BASE,
            std::env::var("NUDGE_WEEKLY_PATTERN").ok().as_deref(),
        ),
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
    /// "You've hit your weekly limit · resets 8am (America/Los_Angeles)".
    /// The only shape whose reset may be days away, so the only one that must
    /// read a day out of the gap before its clock token means anything.
    Weekly,
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
    // Pushed BEFORE Clock: the sort below is stable, so insertion order breaks
    // an exact-offset tie. A user whose NUDGE_CLOCK_PATTERN is "weekly limit"
    // makes both shapes match at the same offset, and the Clock shape reading a
    // weekly banner schedules up to six days early -- the exact bug this shape
    // exists to prevent. Weekly must win.
    banners.extend(
        weekly_re(std::env::var("NUDGE_WEEKLY_PATTERN").ok().as_deref())
            .find_iter(&clean)
            .map(|m| (m.start(), m.end(), Shape::Weekly)),
    );
    banners.extend(
        clock_re(clock_ext)
            .find_iter(&clean)
            .map(|m| (m.start(), m.end(), Shape::Clock)),
    );
    // Bottom-up. sort_by_key is stable, so two shapes matching at the same
    // offset keep the duration-first order callers have always seen.
    banners.sort_by_key(|b| std::cmp::Reverse(b.0));

    for (start, end, shape) in banners {
        // Scan only the text *after* the banner match: a captured pane includes
        // scrollback, and an unrelated duration-shaped substring earlier in the
        // pane (e.g. "16 minutes ago" in a shell prompt) must not be mistaken
        // for the banner's own countdown.
        let rest = &clean[end..];
        // The zone belongs to the banner's line. `rest` runs to the end of the
        // capture, and a "(Region/City)" further down the scrollback is not this
        // banner's zone -- the same reasoning that binds the token search here.
        let line_rest = rest.split('\n').next().unwrap_or("");
        let now = &now_in_zone(now, find_zone_token(line_rest).as_deref());

        if let Shape::Weekly = shape {
            // The weekly banner is the only one whose reset may be days away,
            // and the only one that names no day in its bare form. What sits
            // between the banner and the clock token is the entire signal.
            let Some(m) = find_clock_token_match(line_rest) else {
                continue; // no clock token on this line; try the banner above.
            };
            let gap = &line_rest[..m.start()];
            let token = m.as_str().to_string();
            let words = gap_words(gap);
            let day_words: Vec<&str> = words
                .iter()
                .map(|w| w.as_str())
                .filter(|w| !GAP_FILLER.contains(w))
                .collect();

            let resolved = match day_words.as_slice() {
                // Nothing but filler: the bare form. Per the design, this means
                // the reset is within 24h, which is exactly at_clock's rule.
                [] => parse_timespec(&token, now).ok(),
                // Exactly one word, and we know it.
                [d] => match crate::timespec::parse_day(d) {
                    Some(day) => crate::timespec::resolve_day_clock(now, day, &token),
                    None => None,
                },
                // Two or more words is a shape we have never seen.
                _ => None,
            };

            return match resolved.and_then(|z| z.checked_add(PADDING_MINUTES.minutes()).ok()) {
                Some(padded) => Detection::Reset(padded),
                // Refuse. Do NOT fall through to the banner above: this is the
                // newest banner on screen, and scheduling off a superseded one
                // is the misfire in a different costume.
                None => Detection::Unreadable {
                    banner: banner_line(&clean, start),
                    gap: gap.to_string(),
                },
            };
        }

        let token = match shape {
            Shape::Duration => find_duration_token(rest),
            Shape::Clock => find_clock_token(rest),
            Shape::Weekly => unreachable!("handled above"),
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

/// Extract an IANA zone name from a trailing `(America/Los_Angeles)`.
///
/// Requires the `Region/City` slash form, so it cannot mistake ordinary
/// parenthesised prose for a zone. Accepts multi-segment names
/// ("America/Argentina/Buenos_Aires").
fn find_zone_token(text: &str) -> Option<String> {
    let re = Regex::new(r"\(([A-Za-z]+(?:/[A-Za-z0-9_+-]+)+)\)").unwrap();
    re.captures(text).map(|c| c[1].to_string())
}

/// `now`, expressed in the banner's zone so that a civil "8am" resolves on the
/// clock the banner is quoting rather than the machine's.
///
/// The instant is unchanged — only the calendar it is read against moves — so
/// `at_clock`'s "is this hour already past?" asks the right question. An
/// unresolvable name warns and degrades to local; it must never panic, because
/// this runs on the daemon's scheduler thread where a panic kills every pending
/// job, and a banner is free to name a zone this build's tzdb has never heard of.
fn now_in_zone(now: &Zoned, zone: Option<&str>) -> Zoned {
    let Some(name) = zone else {
        return now.clone();
    };
    match now.timestamp().in_tz(name) {
        Ok(z) => z,
        Err(err) => {
            tracing::warn!(
                "nudge: ignoring unknown time zone {name:?} from the banner ({err}); \
                 resolving the reset in the local zone instead"
            );
            now.clone()
        }
    }
}

/// Extract the first "3pm" / "3:00 PM" / "14:30" token from the text.
fn find_clock_token(text: &str) -> Option<String> {
    // Delegate to the match-returning form so the clock-token regex lives in
    // exactly one place: the Weekly gap path reads its token through
    // find_clock_token_match and the Clock path through here, and two copies of
    // the pattern could silently diverge under a later edit.
    find_clock_token_match(text).map(|m| m.as_str().to_string())
}

/// `find_clock_token`, but keeping the match position so the caller can see what
/// preceded it.
fn find_clock_token_match(text: &str) -> Option<regex::Match<'_>> {
    let re = Regex::new(r"(?i)\b(\d{1,2}(?::\d{2})?\s*(?:am|pm)|\d{1,2}:\d{2})\b").unwrap();
    re.find(text)
}

/// Words that may sit between the banner and its clock token without naming a
/// day. Anything else in the gap is either a day or a refusal.
const GAP_FILLER: &[&str] = &["at", "on"];

/// The gap's significant words: lowercased, stripped of surrounding punctuation,
/// with pure-punctuation tokens ("·", "-") dropped entirely.
fn gap_words(gap: &str) -> Vec<String> {
    gap.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// The whole line `offset` falls on, trimmed — the banner as the user saw it.
fn banner_line(clean: &str, offset: usize) -> String {
    let start = clean[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = clean[offset..]
        .find('\n')
        .map_or(clean.len(), |i| offset + i);
    clean[start..end].trim().to_string()
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

    /// A banner naming a zone must resolve in THAT zone, not the machine's.
    /// now() is 10:00 UTC; 3pm in New York is 19:00 UTC, so a local-zone reading
    /// would say 15:03 and be four hours wrong.
    #[test]
    fn a_stated_zone_is_honored_over_the_local_one() {
        let pane = "current session resets 3:00pm (America/New_York)";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // 15:00 New York + 3m padding == 19:03 UTC.
        assert_eq!(z.timestamp().to_string(), "2026-07-13T19:03:00Z");
    }

    /// No zone in the banner: unchanged behavior, resolved in now()'s zone.
    #[test]
    fn a_banner_without_a_zone_still_uses_the_local_one() {
        let z = reset_of(detect_reset(
            "current session resets 3:00pm",
            &now(),
            None,
            None,
        ));
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    /// An unresolvable zone must degrade to local, never panic: this runs on the
    /// daemon's scheduler thread, where a panic kills every pending job.
    #[test]
    fn an_unknown_zone_falls_back_to_local_without_panicking() {
        let pane = "current session resets 3:00pm (Mars/Olympus_Mons)";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    /// The zone must come from the banner's own line, not from anywhere in the
    /// scrollback below it -- the same discipline the token search already has.
    #[test]
    fn a_zone_on_a_later_line_is_not_the_banners() {
        let pane = "current session resets 3:00pm\nTZ set to (America/New_York)";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        assert_eq!((z.hour(), z.minute()), (15, 3));
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

    /// The captured banner, verbatim, from a live pane on 2026-07-15. now() is
    /// 10:00 UTC == 03:00 in Los Angeles, so 8am LA is later the same day.
    #[test]
    fn detects_the_captured_weekly_banner() {
        let pane = "You've hit your weekly limit · resets 8am (America/Los_Angeles)";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        // 08:00 America/Los_Angeles + 3m padding == 15:03 UTC.
        assert_eq!(z.timestamp().to_string(), "2026-07-13T15:03:00Z");
    }

    /// A bare gap, and gaps that carry only filler, all mean "the next such
    /// hour" -- there is no day to read.
    #[test]
    fn a_filler_only_gap_reads_as_the_bare_form() {
        for pane in [
            "You've hit your weekly limit · resets 3:00pm",
            "You've hit your weekly limit · resets at 3:00pm",
            "You've hit your weekly limit · resets on 3:00pm",
            "You've hit your weekly limit · resets · 3:00pm",
        ] {
            let z = reset_of(detect_reset(pane, &now(), None, None));
            assert_eq!(
                (z.hour(), z.minute()),
                (15, 3),
                "{pane:?} has no day in its gap and must read as the bare form"
            );
        }
    }

    /// A weekday in the gap moves the reset off "today or tomorrow" entirely.
    #[test]
    fn a_weekday_in_the_gap_sets_the_day() {
        // now() is Monday 2026-07-13 10:00 UTC.
        let pane = "You've hit your weekly limit · resets Wed 3:00pm";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        assert_eq!(z.date(), jiff::civil::date(2026, 7, 15));
        assert_eq!((z.hour(), z.minute()), (15, 3));

        let pane = "You've hit your weekly limit · resets Wednesday at 3:00pm";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        assert_eq!(z.date(), jiff::civil::date(2026, 7, 15));
    }

    /// The tripwire. An unrecognized gap must refuse -- and quote itself, so the
    /// report IS the capture needed to teach the parser this shape.
    #[test]
    fn an_unreadable_gap_refuses_and_quotes_the_text() {
        let pane = "You've hit your weekly limit · resets Jul 16, 8am";
        match detect_reset(pane, &now(), None, None) {
            Detection::Unreadable { banner, gap } => {
                assert!(gap.contains("Jul"), "the gap must be quoted: {gap:?}");
                assert!(
                    banner.contains("weekly limit"),
                    "the banner line must be quoted: {banner:?}"
                );
            }
            other => panic!(
                "an unreadable weekly gap must refuse, not guess a reset time; got {other:?}"
            ),
        }
    }

    /// A recognized day that cannot describe a future time is still a refusal,
    /// never a guess: "today 8am" at 10:00 is self-contradictory.
    #[test]
    fn a_recognized_day_that_resolves_to_nothing_still_refuses() {
        let pane = "You've hit your weekly limit · resets today 8am";
        assert!(matches!(
            detect_reset(pane, &now(), None, None),
            Detection::Unreadable { .. }
        ));
    }

    /// An unreadable weekly banner must NOT fall back to an older banner above
    /// it: that would schedule off stale scrollback.
    #[test]
    fn an_unreadable_weekly_banner_does_not_fall_back_to_an_older_one() {
        let pane =
            "current session resets 3:00pm\nYou've hit your weekly limit · resets Jul 16, 8am";
        assert!(
            matches!(
                detect_reset(pane, &now(), None, None),
                Detection::Unreadable { .. }
            ),
            "the newest banner is unreadable; falling back to the stale one above \
             would schedule from superseded information"
        );
    }

    /// But a weekly banner in the scrollback ABOVE a newer session banner is
    /// simply older, and the bottom-up walk never reaches it.
    #[test]
    fn a_newer_session_banner_below_a_weekly_one_still_wins() {
        let pane = "You've hit your weekly limit · resets Jul 16, 8am\nlater\ncurrent session resets 3:00pm";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    /// A weekly banner whose own line carries no clock token is not a refusal --
    /// there is nothing unreadable, just nothing to read here -- so the walk
    /// continues to the banner above. The newest banner is the weekly one (it is
    /// lowest), so this drives the Weekly `continue` branch specifically: if that
    /// branch instead returned None or refused, `reset_of` would panic rather
    /// than fall back to the older session banner's 3:00pm.
    #[test]
    fn a_weekly_banner_without_a_clock_token_falls_back_to_an_older_banner() {
        let pane = "current session resets 3:00pm\nYou've hit your weekly limit · resets";
        let z = reset_of(detect_reset(pane, &now(), None, None));
        assert_eq!((z.hour(), z.minute()), (15, 3));
    }

    /// Weekly is pushed before Clock so it wins an exact-offset tie. A user
    /// whose NUDGE_CLOCK_PATTERN is "weekly limit" makes both shapes match at
    /// the same offset, and the Clock shape reading this banner is precisely the
    /// six-day-early misfire.
    #[test]
    fn weekly_beats_clock_on_an_exact_offset_tie() {
        let pane = "You've hit your weekly limit · resets Jul 16, 8am";
        assert!(
            matches!(
                detect_reset(pane, &now(), Some("weekly limit"), None),
                Detection::Unreadable { .. }
            ),
            "the Clock shape would read '8am' and schedule six days early; \
             Weekly must win the tie and refuse"
        );
    }
}

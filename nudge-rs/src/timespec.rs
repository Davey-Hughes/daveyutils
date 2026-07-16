//! Parse user time-spec strings into an absolute `jiff::Zoned`.

use jiff::{Span, ToSpan, Zoned};
use regex::Regex;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimespecError {
    #[error("empty time spec")]
    Empty,
    #[error("unrecognized time spec: {0}")]
    Unrecognized(String),
}

/// Parse `input` relative to `now`. See module tests for the accepted forms.
pub fn parse_timespec(input: &str, now: &Zoned) -> Result<Zoned, TimespecError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(TimespecError::Empty);
    }
    if let Some(z) = parse_relative(s, now) {
        return Ok(z);
    }
    if let Some(z) = parse_named(s, now) {
        return Ok(z);
    }
    if let Some(z) = parse_clock(s, now) {
        return Ok(z);
    }
    Err(TimespecError::Unrecognized(s.to_string()))
}

/// "now + 45 min", "in 90m", "45m", "2h", "1h30m" -> now + span.
fn parse_relative(s: &str, now: &Zoned) -> Option<Zoned> {
    let lower = s.to_lowercase();
    // Normalize the "now +"/"in" prefixes away, then require a duration body.
    let body = lower
        .strip_prefix("now")
        .map(|r| r.trim_start().trim_start_matches('+').trim())
        .or_else(|| lower.strip_prefix("in ").map(str::trim))
        .unwrap_or(&lower)
        .trim();

    let re =
        Regex::new(r"^(?:(\d+)\s*h(?:ours?|rs?)?)?\s*(?:(\d+)\s*m(?:in(?:ute)?s?)?)?$").unwrap();
    let caps = re.captures(body)?;
    let hours: i64 = caps.get(1).map_or(0, |m| m.as_str().parse().unwrap_or(0));
    let mins: i64 = caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0));
    if hours == 0 && mins == 0 {
        return None;
    }
    let span: Span = hours.hours().checked_add(mins.minutes()).ok()?;
    now.checked_add(span).ok()
}

/// "noon" / "midnight".
fn parse_named(s: &str, now: &Zoned) -> Option<Zoned> {
    match s.to_lowercase().as_str() {
        "noon" => at_clock(now, 12, 0),
        "midnight" => at_clock(now, 0, 0),
        _ => None,
    }
}

/// 24h ("14:30") or 12h ("3pm", "3:00 PM", "11:59pm").
///
/// Anchored over the whole input, and deliberately so: a previous unanchored
/// search matched a meridiem anywhere in the string and grabbed the first digit
/// run anywhere else, so "spam 5" parsed as 05:00 -- "SPAM" supplied the AM.
/// Matching a meridiem also skipped the "must look like a 24h clock" guard, so
/// arbitrary text scheduled a real job at a time the user never asked for.
/// Callers pass an already-isolated token (`detect::find_clock_token` extracts
/// one; `--time` is a whole argument), so nothing legitimate needs the slack.
fn parse_clock(s: &str, now: &Zoned) -> Option<Zoned> {
    let (hour, minute) = clock_hm(s)?;
    at_clock(now, hour, minute)
}

/// The clock-token parse, without resolving it against a date. Returns
/// `(hour, minute)` on a 24-hour clock.
///
/// Split out of `parse_clock` because the weekly shape needs the fields to hang
/// on a day that is not necessarily today or tomorrow. Every guard below is
/// load-bearing and documented at its original site; do not relax them.
fn clock_hm(s: &str) -> Option<(i8, i8)> {
    let up = s.to_uppercase();
    let re = Regex::new(r"^\s*(\d{1,2})(?::(\d{2}))?\s*(AM|PM)?\s*$").unwrap();
    let caps = re.captures(&up)?;
    let mut hour: i8 = caps.get(1)?.as_str().parse().ok()?;
    let minute: i8 = caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0));

    match caps.get(3).map(|m| m.as_str()) {
        Some(meridiem) => {
            // A meridiem means a 12-hour clock, so the hour must be 1..=12.
            // Without this, "13pm" fell through every arm (`Some("PM") if hour
            // < 12` does not match 13) and the 0..=23 check below waved it
            // through as 13:00 -- a user typing `-m 13pm` meaning 1pm got a
            // silently wrong job.
            if !(1..=12).contains(&hour) {
                return None;
            }
            match meridiem {
                "PM" if hour < 12 => hour += 12,
                "AM" if hour == 12 => hour = 0,
                _ => {}
            }
        }
        // A bare number with no meridiem and no minutes ("5") is not a time.
        None if caps.get(2).is_none() => return None,
        None => {}
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    Some((hour, minute))
}

/// Build today's `hour:minute` in now's zone; if it's already past, roll to tomorrow.
fn at_clock(now: &Zoned, hour: i8, minute: i8) -> Option<Zoned> {
    let tz = now.time_zone().clone();
    let today = now
        .date()
        .at(hour, minute, 0, 0)
        .to_zoned(tz.clone())
        .ok()?;
    if &today <= now {
        now.date()
            .tomorrow()
            .ok()?
            .at(hour, minute, 0, 0)
            .to_zoned(tz)
            .ok()
    } else {
        Some(today)
    }
}

/// A day named by a banner, relative to "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaySpec {
    Today,
    Tomorrow,
    Weekday(jiff::civil::Weekday),
}

/// Parse a single day word. Weekday names (abbreviated or full), plus `today`
/// and `tomorrow`.
///
/// Deliberately does NOT accept month-day forms ("Jul 16"). No capture of such a
/// banner exists, and guessing at one is how you get a parser that is confidently
/// wrong. The caller's gap-guard turns anything unrecognized into a loud refusal
/// that quotes the text, which is how the next shape gets discovered.
pub fn parse_day(s: &str) -> Option<DaySpec> {
    use jiff::civil::Weekday;
    Some(match s.trim().to_lowercase().as_str() {
        "today" => DaySpec::Today,
        "tomorrow" => DaySpec::Tomorrow,
        "mon" | "monday" => DaySpec::Weekday(Weekday::Monday),
        "tue" | "tues" | "tuesday" => DaySpec::Weekday(Weekday::Tuesday),
        "wed" | "weds" | "wednesday" => DaySpec::Weekday(Weekday::Wednesday),
        "thu" | "thur" | "thurs" | "thursday" => DaySpec::Weekday(Weekday::Thursday),
        "fri" | "friday" => DaySpec::Weekday(Weekday::Friday),
        "sat" | "saturday" => DaySpec::Weekday(Weekday::Saturday),
        "sun" | "sunday" => DaySpec::Weekday(Weekday::Sunday),
        _ => return None,
    })
}

/// Resolve `day` + `clock_token` into an absolute time in `now`'s zone.
///
/// `None` means "these words do not describe a future time" — an unparseable
/// clock, or a self-contradictory `today` whose hour has already passed. It is
/// never "roll forward to something plausible": the caller refuses instead,
/// because a plausible-but-invented reset day is exactly the silent misfire this
/// design exists to prevent.
pub fn resolve_day_clock(now: &Zoned, day: DaySpec, clock_token: &str) -> Option<Zoned> {
    let (hour, minute) = clock_hm(clock_token)?;
    let tz = now.time_zone().clone();
    match day {
        DaySpec::Today => {
            let z = now.date().at(hour, minute, 0, 0).to_zoned(tz).ok()?;
            (&z > now).then_some(z)
        }
        DaySpec::Tomorrow => now
            .date()
            .tomorrow()
            .ok()?
            .at(hour, minute, 0, 0)
            .to_zoned(tz)
            .ok(),
        DaySpec::Weekday(target) => {
            let today_at = now.date().at(hour, minute, 0, 0).to_zoned(tz).ok()?;
            // Today counts only if its hour is still ahead; otherwise the reset
            // is a full week out, NOT tomorrow.
            if now.weekday() == target && &today_at > now {
                return Some(today_at);
            }
            // `nth_weekday(1, ..)` is strictly future and preserves time-of-day.
            today_at.nth_weekday(1, target).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::{civil::date, tz::TimeZone};

    // A fixed reference "now": 2026-07-13 10:00:00 in a fixed zone.
    fn now() -> jiff::Zoned {
        date(2026, 7, 13)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap()
    }

    fn hm(z: &jiff::Zoned) -> (i8, i8) {
        (z.hour(), z.minute())
    }

    #[test]
    fn parses_24h_clock_today() {
        let z = parse_timespec("14:30", &now()).unwrap();
        assert_eq!(hm(&z), (14, 30));
        assert_eq!(z.date(), date(2026, 7, 13));
    }

    #[test]
    fn parses_12h_bare_hour() {
        let z = parse_timespec("3pm", &now()).unwrap();
        assert_eq!(hm(&z), (15, 0));
    }

    #[test]
    fn parses_12h_with_minutes_and_space_and_case() {
        assert_eq!(hm(&parse_timespec("3:00pm", &now()).unwrap()), (15, 0));
        assert_eq!(hm(&parse_timespec("3:05 PM", &now()).unwrap()), (15, 5));
        assert_eq!(hm(&parse_timespec("11:59pm", &now()).unwrap()), (23, 59));
    }

    #[test]
    fn clock_already_past_rolls_to_tomorrow() {
        // 09:00 is before the 10:00 reference -> tomorrow.
        let z = parse_timespec("9am", &now()).unwrap();
        assert_eq!(z.date(), date(2026, 7, 14));
        assert_eq!(hm(&z), (9, 0));
    }

    #[test]
    fn parses_named_times() {
        assert_eq!(hm(&parse_timespec("noon", &now()).unwrap()), (12, 0));
        // midnight is past 10:00 -> tomorrow 00:00
        let mid = parse_timespec("midnight", &now()).unwrap();
        assert_eq!(hm(&mid), (0, 0));
        assert_eq!(mid.date(), date(2026, 7, 14));
    }

    #[test]
    fn parses_relative_offsets() {
        assert_eq!(
            hm(&parse_timespec("now + 45 min", &now()).unwrap()),
            (10, 45)
        );
        assert_eq!(hm(&parse_timespec("in 90m", &now()).unwrap()), (11, 30));
        assert_eq!(hm(&parse_timespec("45m", &now()).unwrap()), (10, 45));
        assert_eq!(hm(&parse_timespec("2h", &now()).unwrap()), (12, 0));
        assert_eq!(hm(&parse_timespec("1h30m", &now()).unwrap()), (11, 30));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_timespec("", &now()), Err(TimespecError::Empty));
        assert!(matches!(
            parse_timespec("banana", &now()),
            Err(TimespecError::Unrecognized(_))
        ));
    }

    #[test]
    fn rejects_an_out_of_range_meridiem_hour() {
        // The meridiem arms never checked hour ∈ 1..=12: `Some("PM") if hour <
        // 12` does not match 13, `Some(_) => {}` swallows it, and the 0..=23
        // check then waves it through. A user who types `nudge -m 13pm` meaning
        // 1pm silently gets a job at 13:00 -- no error, wrong time.
        for spec in ["13pm", "13PM", "0am", "24am", "99pm"] {
            assert!(
                matches!(
                    parse_timespec(spec, &now()),
                    Err(TimespecError::Unrecognized(_))
                ),
                "{spec:?} must be rejected, got {:?}",
                parse_timespec(spec, &now())
            );
        }
    }

    #[test]
    fn rejects_arbitrary_text_that_merely_contains_a_meridiem() {
        // The meridiem search was unanchored over the whole uppercased string
        // and matching one skipped the "must look like a 24h clock" guard, so
        // "SPAM" supplied the AM and the digit-run grabbed the 5: garbage that
        // should be Unrecognized instead scheduled a real job at a time the
        // user never asked for.
        for spec in ["spam 5", "3: 00", "spam 5 eggs", "eggs and ham 7"] {
            assert!(
                matches!(
                    parse_timespec(spec, &now()),
                    Err(TimespecError::Unrecognized(_))
                ),
                "{spec:?} must be rejected, got {:?}",
                parse_timespec(spec, &now())
            );
        }
    }

    #[test]
    fn the_legitimate_meridiem_forms_still_parse() {
        // Guards the anchoring against over-tightening: every spelling the help
        // text and README promise must survive.
        assert_eq!(hm(&parse_timespec("12am", &now()).unwrap()), (0, 0));
        assert_eq!(hm(&parse_timespec("12pm", &now()).unwrap()), (12, 0));
        assert_eq!(hm(&parse_timespec("1pm", &now()).unwrap()), (13, 0));
        assert_eq!(hm(&parse_timespec(" 3:05 pm ", &now()).unwrap()), (15, 5));
        assert_eq!(hm(&parse_timespec("00:30", &now()).unwrap()), (0, 30));
        assert_eq!(hm(&parse_timespec("23:59", &now()).unwrap()), (23, 59));
    }

    #[test]
    fn parses_every_weekday_spelling() {
        use jiff::civil::Weekday;
        assert_eq!(parse_day("Mon"), Some(DaySpec::Weekday(Weekday::Monday)));
        assert_eq!(parse_day("monday"), Some(DaySpec::Weekday(Weekday::Monday)));
        assert_eq!(
            parse_day("WEDS"),
            Some(DaySpec::Weekday(Weekday::Wednesday))
        );
        assert_eq!(
            parse_day("Wednesday"),
            Some(DaySpec::Weekday(Weekday::Wednesday))
        );
        assert_eq!(
            parse_day("thurs"),
            Some(DaySpec::Weekday(Weekday::Thursday))
        );
        assert_eq!(parse_day("sun"), Some(DaySpec::Weekday(Weekday::Sunday)));
        assert_eq!(parse_day("today"), Some(DaySpec::Today));
        assert_eq!(parse_day("tomorrow"), Some(DaySpec::Tomorrow));
        assert_eq!(parse_day("jul"), None);
        assert_eq!(parse_day("banana"), None);
    }

    #[test]
    fn resolves_a_weekday_later_this_week() {
        // now() is Monday 2026-07-13 10:00. Wednesday 8am is 2 days out.
        let z = resolve_day_clock(&now(), parse_day("Wed").unwrap(), "8am").unwrap();
        assert_eq!(z.date(), date(2026, 7, 15));
        assert_eq!(hm(&z), (8, 0));
    }

    #[test]
    fn resolves_today_s_weekday_when_the_time_is_still_ahead() {
        // Monday 10:00, banner says Monday 3pm -> today, not next week.
        let z = resolve_day_clock(&now(), parse_day("Monday").unwrap(), "3pm").unwrap();
        assert_eq!(z.date(), date(2026, 7, 13));
        assert_eq!(hm(&z), (15, 0));
    }

    #[test]
    fn resolves_today_s_weekday_to_next_week_when_the_time_has_passed() {
        // Monday 10:00, banner says Monday 8am -> this Monday's 8am is gone,
        // so the reset is a full week out. Rolling to "tomorrow" would be wrong
        // by six days, which is the entire bug this feature exists to avoid.
        let z = resolve_day_clock(&now(), parse_day("Monday").unwrap(), "8am").unwrap();
        assert_eq!(z.date(), date(2026, 7, 20));
        assert_eq!(hm(&z), (8, 0));
    }

    #[test]
    fn resolves_tomorrow_and_today() {
        let z = resolve_day_clock(&now(), DaySpec::Tomorrow, "8am").unwrap();
        assert_eq!(z.date(), date(2026, 7, 14));
        assert_eq!(hm(&z), (8, 0));

        let z = resolve_day_clock(&now(), DaySpec::Today, "3pm").unwrap();
        assert_eq!(z.date(), date(2026, 7, 13));
    }

    #[test]
    fn today_in_the_past_is_unresolvable_not_immediate() {
        // "resets today 8am" at 10:00 is self-contradictory. Returning a past
        // time would make the scheduler fire it at once; None lets the caller
        // refuse out loud instead.
        assert_eq!(resolve_day_clock(&now(), DaySpec::Today, "8am"), None);
    }

    #[test]
    fn a_day_with_an_unparseable_clock_is_unresolvable() {
        assert_eq!(resolve_day_clock(&now(), DaySpec::Tomorrow, "banana"), None);
    }
}

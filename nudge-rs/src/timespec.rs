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
    at_clock(now, hour, minute)
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
}

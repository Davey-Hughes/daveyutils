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

/// How far ahead a banner-named month/day is allowed to land.
///
/// A month/day names no year, so resolving it means choosing one — and a wrong
/// choice is wrong by a whole year, which is the silent misfire this module
/// exists to prevent. No rate-limit window is a month long, so anything past
/// this horizon means the year inference guessed and the caller must refuse
/// instead. It is what stops "resets Jul 28 at 8am", read at 10:00 on Jul 28,
/// from quietly becoming a nudge 365 days out.
const MONTH_DAY_HORIZON_DAYS: i64 = 31;

/// How far ahead a *weekly* reset can plausibly be.
///
/// A weekly window is at most seven days long, so a reset further out than this
/// is not one — which makes this a sharp instrument for telling "7/16" apart
/// from "16/7" when both are real dates. It only ever chooses between readings
/// that already cleared [`MONTH_DAY_HORIZON_DAYS`]; it never admits a date the
/// horizon rejected. The extra day is slack for zone shifts and padding.
const WEEKLY_WINDOW_DAYS: i64 = 8;

/// Which field a locale puts first in an all-numeric date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOrder {
    /// `M/D` — the US convention.
    MonthFirst,
    /// `D/M` — most everywhere else.
    DayFirst,
    /// The locale said nothing. Not the same as saying US.
    Unknown,
}

/// The date order implied by the machine's locale.
///
/// Follows the plain rule: the US writes the month first, everyone else writes
/// the day first. (A handful of other regions are month-first too; they are rare
/// enough beside the cost of a table that goes stale, and a wrong guess here is
/// only ever reached after the calendar and the weekly window have both failed
/// to decide.)
///
/// A `C`/`POSIX` or unset locale names no region and yields [`DateOrder::Unknown`]
/// — silence is not a vote for either convention, and an ambiguity that survives
/// this far is refused rather than guessed.
pub fn locale_date_order() -> DateOrder {
    // Ascending specificity, as POSIX defines it: LC_ALL overrides LC_TIME,
    // which overrides LANG. The decision itself lives in `order_for_locale` so
    // it can be tested without mutating the process environment, which no test
    // running beside others can safely do.
    for var in ["LC_ALL", "LC_TIME", "LANG"] {
        if let Some(order) = std::env::var(var)
            .ok()
            .as_deref()
            .and_then(order_for_locale)
        {
            return order;
        }
    }
    DateOrder::Unknown
}

/// The order a single POSIX locale name implies, or `None` when it names no
/// region at all.
fn order_for_locale(value: &str) -> Option<DateOrder> {
    let region = locale_region(value)?;
    Some(if region.eq_ignore_ascii_case("US") {
        DateOrder::MonthFirst
    } else {
        DateOrder::DayFirst
    })
}

/// The region of a POSIX locale name: `en_US.UTF-8` -> `US`.
fn locale_region(locale: &str) -> Option<&str> {
    let region = locale.split('_').nth(1)?.split(['.', '@']).next()?;
    (!region.is_empty()).then_some(region)
}

/// A day named by a banner, relative to "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaySpec {
    Today,
    Tomorrow,
    Weekday(jiff::civil::Weekday),
    /// A calendar date — "Jul 28", or "Jul 28, 2026". Banners seen so far state
    /// no year, so it is inferred at resolve time; see
    /// [`MONTH_DAY_HORIZON_DAYS`].
    MonthDay {
        month: i8,
        day: i8,
        year: Option<i16>,
    },
    /// An all-numeric date — "7/16" — whose own text does not say which field
    /// is the month. Both readings are carried to resolve time, where the
    /// calendar, then the weekly window, then the locale decide between them.
    Numeric {
        first: i8,
        second: i8,
        year: Option<i16>,
    },
}

/// Words that may sit around a banner's date without being part of it.
///
/// The banner's wording is not a stable interface — it has already moved from
/// "resets Jul 16, 8am" to "resets Jul 28 at 8am" — so the connectives a date
/// might be wrapped in are listed generously. Punctuation never reaches here:
/// the caller has already stripped it from the word edges.
const GAP_FILLER: &[&str] = &["at", "on", "by", "the", "of", "in", "next", "this"];

/// What the words around a banner's clock token said about its day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayWords {
    /// Nothing but connectives — the bare form, where the clock token alone
    /// decides and the reset is simply the next such hour.
    Unnamed,
    /// A day this parser understands.
    Named(DaySpec),
}

/// How to treat a word the scanner does not recognize.
#[derive(Clone, Copy)]
enum Unknown {
    /// Refuse the whole reading. For words sitting between "resets" and the
    /// time, where an unread word means the reset claim itself is not fully
    /// understood.
    Refuse,
    /// Skip it. For text trailing the time, which is routinely unrelated prose
    /// ("· /upgrade to increase your limits") and no reason to refuse.
    Ignore,
}

/// Read a day out of `words`, refusing anything not understood.
///
/// This is the strict reading, for the words a banner puts between "resets" and
/// its time. A word here that the parser cannot place means the banner is
/// saying something about the reset day that this code does not follow, and the
/// caller turns that into a loud refusal quoting the text — which is how the
/// next shape gets discovered, and how the dated one was.
pub fn parse_day_words<S: AsRef<str>>(words: &[S]) -> Option<DayWords> {
    scan_day_words(words, Unknown::Refuse)
}

/// Look for a day in `words`, ignoring everything that is not one.
///
/// This is the permissive reading, for text that trails the banner's time —
/// where a date may still be hiding ("resets at 8am on Jul 28") but unrelated
/// prose is the norm. `None` means "no day here", never "refuse".
pub fn find_day_words<S: AsRef<str>>(words: &[S]) -> Option<DaySpec> {
    match scan_day_words(words, Unknown::Ignore) {
        Some(DayWords::Named(day)) => Some(day),
        _ => None,
    }
}

/// Collect the date fields scattered through `words`, in any order.
///
/// Scanning rather than matching a fixed word count is what makes this tolerant
/// of the connectives and separators a banner drifts between. What it will not
/// tolerate is ambiguity: a field stated twice, or a date left incomplete, is
/// `None` rather than a guess.
fn scan_day_words<S: AsRef<str>>(words: &[S], unknown: Unknown) -> Option<DayWords> {
    let mut month: Option<i8> = None;
    let mut day: Option<i8> = None;
    let mut year: Option<i16> = None;
    let mut named: Option<DaySpec> = None;
    let mut date: Option<DaySpec> = None;

    for w in words.iter().map(AsRef::as_ref) {
        if GAP_FILLER.contains(&w) {
            continue;
        }
        // Months before years before days: the ranges do not overlap, but
        // pinning the order keeps "2026" out of the day slot by construction
        // rather than by luck. The numeric date goes first of all — it is the
        // only one of these that can contain a separator, so nothing else can
        // claim it.
        let refilled = if let Some(d) = parse_numeric_date(w) {
            date.replace(d).is_some()
        } else if let Some(m) = parse_month(w) {
            month.replace(m).is_some()
        } else if let Some(y) = parse_year(w) {
            year.replace(y).is_some()
        } else if let Some(d) = parse_day_number(w) {
            day.replace(d).is_some()
        } else if let Some(d) = parse_day(w) {
            named.replace(d).is_some()
        } else {
            match unknown {
                Unknown::Refuse => return None,
                Unknown::Ignore => continue,
            }
        };
        // The same field twice ("Jul ... Aug") is not a date, it is two.
        if refilled {
            return None;
        }
    }

    match (date, month, day, named) {
        // A whole date in one token. Spelled-out fields alongside it would be a
        // second date, not a clarification of this one.
        (Some(date), None, None, _) => Some(DayWords::Named(with_year(date, year)?)),
        // A calendar date is the most specific claim a banner can make, so it
        // wins over a weekday sitting beside it ("Tue, Jul 28"). The weekday
        // there only corroborates what the date already pins down exactly, and
        // preferring it would throw away precision to gain nothing.
        (None, Some(month), Some(day), _) => {
            Some(DayWords::Named(DaySpec::MonthDay { month, day, year }))
        }
        (None, None, None, Some(d)) if year.is_none() => Some(DayWords::Named(d)),
        (None, None, None, None) if year.is_none() => Some(DayWords::Unnamed),
        // A month with no day, a day with no month, a year on its own: the
        // banner named part of a date and this is not the half that decides it.
        _ => None,
    }
}

/// A four-digit calendar year.
fn parse_year(s: &str) -> Option<i16> {
    let n: i16 = s.parse().ok()?;
    (2000..=2100).contains(&n).then_some(n)
}

/// An all-numeric date token: `7/16`, `16-7`, `2026-07-28`, `7/28/2026`.
///
/// Only the ISO form settles its own field order. The two-field forms carry both
/// readings to resolve time rather than picking one here, because the pick needs
/// a calendar and a clock that this function does not have.
fn parse_numeric_date(s: &str) -> Option<DaySpec> {
    let parts: Vec<&str> = s.split(['/', '-']).collect();
    // Every field must be digits, or this is not a date — it keeps "24-7" out
    // on arithmetic rather than on hope, and "wed-8am" out entirely.
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let num = |p: &str| p.parse::<i8>().ok();
    match parts.as_slice() {
        [a, b] => Some(DaySpec::Numeric {
            first: num(a)?,
            second: num(b)?,
            year: None,
        }),
        // Year first is ISO, and ISO is year-month-day by definition: nothing
        // is left to guess.
        [y, a, b] if y.len() == 4 => Some(DaySpec::MonthDay {
            month: num(a)?,
            day: num(b)?,
            year: Some(parse_year(y)?),
        }),
        // Year last pins the year but not the order of the two fields before it.
        [a, b, y] if y.len() == 4 => Some(DaySpec::Numeric {
            first: num(a)?,
            second: num(b)?,
            year: Some(parse_year(y)?),
        }),
        // A two-digit year ("7/28/26") is a third ambiguous field, and three
        // ambiguous fields is not a date this parser will guess at.
        _ => None,
    }
}

/// Fold a separately-stated year into a date parsed from a single token.
///
/// Two different years stated two ways is a contradiction, not a date.
fn with_year(date: DaySpec, year: Option<i16>) -> Option<DaySpec> {
    let Some(stated) = year else {
        return Some(date);
    };
    match date {
        // The two-field numeric form is the only one that can arrive here
        // without a year of its own — the ISO form carries one by construction
        // — so a year stated a second time is a contradiction, not a
        // clarification, and is refused rather than reconciled.
        DaySpec::Numeric {
            first,
            second,
            year: None,
        } => Some(DaySpec::Numeric {
            first,
            second,
            year: Some(stated),
        }),
        _ => None,
    }
}

/// A month name, abbreviated or full.
fn parse_month(s: &str) -> Option<i8> {
    Some(match s.trim().to_lowercase().as_str() {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    })
}

/// A day-of-month number, with an optional ordinal suffix ("28", "28th", "1st").
///
/// The range check is a shape check, not a calendar one: whether day 31 exists
/// in the banner's month is [`jiff`]'s answer to give at resolve time.
fn parse_day_number(s: &str) -> Option<i8> {
    let s = s.trim().to_lowercase();
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    if !matches!(&s[digits.len()..], "" | "st" | "nd" | "rd" | "th") {
        return None;
    }
    let n: i8 = digits.parse().ok()?;
    (1..=31).contains(&n).then_some(n)
}

/// Parse a single day word. Weekday names (abbreviated or full), plus `today`
/// and `tomorrow`.
///
/// A month name alone is not a day — "jul" says nothing about which July day —
/// so it belongs to the two-word path in [`parse_day_words`], not here.
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
///
/// `order` breaks a tie between the two readings of an all-numeric date, and is
/// consulted only after the calendar and the weekly window have both failed to.
pub fn resolve_day_clock(
    now: &Zoned,
    day: DaySpec,
    clock_token: &str,
    order: DateOrder,
) -> Option<Zoned> {
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
        DaySpec::MonthDay { month, day, year } => month_day_at(now, month, day, year, hour, minute),
        DaySpec::Numeric {
            first,
            second,
            year,
        } => {
            // Nothing in "7/16" says which field is the month, so resolve both
            // readings and let them be ruled out rather than chosen between.
            let month_first = month_day_at(now, first, second, year, hour, minute);
            let day_first = month_day_at(now, second, first, year, hour, minute);
            match (month_first, day_first) {
                // The calendar settled it: a month of 16 is not a month, and a
                // date that has gone by or sits past the horizon is not this
                // reset. Most real dates die here, "7/16" among them.
                (Some(z), None) | (None, Some(z)) => Some(z),
                (None, None) => None,
                (Some(m), Some(d)) => {
                    let window = now.checked_add(WEEKLY_WINDOW_DAYS.days()).ok()?;
                    pick_reading(m, d, &window, order)
                }
            }
        }
    }
}

/// Choose between two readings of a numeric date that both name a real,
/// imminent instant.
///
/// In practice the `window` test settles every pair that differs at all. The two
/// readings of `a/b` land in month `a` and month `b`, so when `a != b` they sit
/// roughly a month apart — and two dates a month apart cannot both fall inside
/// one eight-day window. The locale arm below is therefore a backstop, live only
/// if [`WEEKLY_WINDOW_DAYS`] is ever widened past the length of a weekly limit.
fn pick_reading(
    month_first: Zoned,
    day_first: Zoned,
    window: &Zoned,
    order: DateOrder,
) -> Option<Zoned> {
    // "8/8" reads the same both ways. That is not an ambiguity to resolve, and
    // sending it to the locale would refuse a date nothing was ever unclear
    // about on a machine whose locale happens to be unset.
    if month_first == day_first {
        return Some(month_first);
    }
    // A weekly reset is at most a week out, so if only one reading is, it is
    // the one — whatever the local convention would have said.
    match (&month_first <= window, &day_first <= window) {
        (true, false) => Some(month_first),
        (false, true) => Some(day_first),
        // Two real dates, both plausible. Only convention is left, and where
        // there is no convention to read there is nothing left but a guess.
        _ => match order {
            DateOrder::MonthFirst => Some(month_first),
            DateOrder::DayFirst => Some(day_first),
            DateOrder::Unknown => None,
        },
    }
}

/// Resolve a calendar date at `hour:minute` in `now`'s zone, inferring the year
/// when the banner did not state one.
///
/// `None` for a date that does not exist, has already gone by, or lands past
/// [`MONTH_DAY_HORIZON_DAYS`] — the three ways a month/day fails to name this
/// reset, and the filter that lets [`DaySpec::Numeric`] rule out a field order
/// instead of guessing at one.
fn month_day_at(
    now: &Zoned,
    month: i8,
    day: i8,
    year: Option<i16>,
    hour: i8,
    minute: i8,
) -> Option<Zoned> {
    let tz = now.time_zone().clone();
    // With no year stated, take the first one that puts the date in the future:
    // this year normally, next year when the date has already gone by (a reset
    // named "Jan 2" read on Dec 30). A stated year is the banner's own answer
    // and the only one tried — but it still faces the horizon, because "a month
    // out" is not a rate limit however confidently the banner spells it.
    let horizon = now.checked_add(MONTH_DAY_HORIZON_DAYS.days()).ok()?;
    let inferred = [now.year(), now.year() + 1];
    let candidates = year.as_ref().map_or(&inferred[..], std::slice::from_ref);
    for &year in candidates {
        // Feb 29 in a common year is not a date; the next year may be.
        let Ok(d) = jiff::civil::Date::new(year, month, day) else {
            continue;
        };
        let Ok(z) = d.at(hour, minute, 0, 0).to_zoned(tz.clone()) else {
            continue;
        };
        if &z > now {
            // The first future candidate is the only one that can be right, so
            // a candidate beyond the horizon is not a reason to keep looking —
            // it is the refusal.
            return (z <= horizon).then_some(z);
        }
    }
    None
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

    /// A relative spec with no duration in it is not "now + nothing", it is not
    /// a time spec at all -- otherwise `-m now` would schedule a job in the past
    /// and the scheduler would fire it immediately.
    #[test]
    fn a_relative_prefix_with_no_duration_is_rejected() {
        for spec in ["now", "now +", "in "] {
            assert!(
                matches!(
                    parse_timespec(spec, &now()),
                    Err(TimespecError::Unrecognized(_))
                ),
                "{spec:?} names no duration and must be rejected"
            );
        }
    }

    /// The 24-hour range guard, which sits past the meridiem arms and catches
    /// what a colon-form clock can still get wrong.
    #[test]
    fn rejects_an_out_of_range_24h_clock() {
        for spec in ["25:00", "24:00", "99:00", "12:99", "23:60"] {
            assert!(
                matches!(
                    parse_timespec(spec, &now()),
                    Err(TimespecError::Unrecognized(_))
                ),
                "{spec:?} is not a time and must be rejected"
            );
        }
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
        let z = resolve_day_clock(&now(), parse_day("Wed").unwrap(), "8am", DateOrder::Unknown)
            .unwrap();
        assert_eq!(z.date(), date(2026, 7, 15));
        assert_eq!(hm(&z), (8, 0));
    }

    #[test]
    fn resolves_today_s_weekday_when_the_time_is_still_ahead() {
        // Monday 10:00, banner says Monday 3pm -> today, not next week.
        let z = resolve_day_clock(
            &now(),
            parse_day("Monday").unwrap(),
            "3pm",
            DateOrder::Unknown,
        )
        .unwrap();
        assert_eq!(z.date(), date(2026, 7, 13));
        assert_eq!(hm(&z), (15, 0));
    }

    #[test]
    fn resolves_today_s_weekday_to_next_week_when_the_time_has_passed() {
        // Monday 10:00, banner says Monday 8am -> this Monday's 8am is gone,
        // so the reset is a full week out. Rolling to "tomorrow" would be wrong
        // by six days, which is the entire bug this feature exists to avoid.
        let z = resolve_day_clock(
            &now(),
            parse_day("Monday").unwrap(),
            "8am",
            DateOrder::Unknown,
        )
        .unwrap();
        assert_eq!(z.date(), date(2026, 7, 20));
        assert_eq!(hm(&z), (8, 0));
    }

    #[test]
    fn resolves_tomorrow_and_today() {
        let z = resolve_day_clock(&now(), DaySpec::Tomorrow, "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 7, 14));
        assert_eq!(hm(&z), (8, 0));

        let z = resolve_day_clock(&now(), DaySpec::Today, "3pm", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 7, 13));
    }

    #[test]
    fn today_in_the_past_is_unresolvable_not_immediate() {
        // "resets today 8am" at 10:00 is self-contradictory. Returning a past
        // time would make the scheduler fire it at once; None lets the caller
        // refuse out loud instead.
        assert_eq!(
            resolve_day_clock(&now(), DaySpec::Today, "8am", DateOrder::Unknown),
            None
        );
    }

    #[test]
    fn a_day_with_an_unparseable_clock_is_unresolvable() {
        assert_eq!(
            resolve_day_clock(&now(), DaySpec::Tomorrow, "banana", DateOrder::Unknown),
            None
        );
    }

    /// A date with no year stated — the shape every captured banner uses.
    fn on(month: i8, day: i8) -> DaySpec {
        DaySpec::MonthDay {
            month,
            day,
            year: None,
        }
    }

    /// The same, as the word-scanner's answer.
    fn md(month: i8, day: i8) -> DayWords {
        DayWords::Named(on(month, day))
    }

    /// An all-numeric date, whose field order is not settled yet.
    fn num(first: i8, second: i8) -> DaySpec {
        DaySpec::Numeric {
            first,
            second,
            year: None,
        }
    }

    /// Month name + day number, in every spelling and order a banner might use.
    /// The connectives are what the banner keeps changing, so they must not
    /// change the reading.
    #[test]
    fn parses_month_day_words_in_any_arrangement() {
        for (words, want) in [
            (vec!["jul", "28"], md(7, 28)),
            (vec!["july", "28th"], md(7, 28)),
            (vec!["sept", "1st"], md(9, 1)),
            (vec!["december", "3rd"], md(12, 3)),
            (vec!["may", "2nd"], md(5, 2)),
            // Either order: a month name says which field it is.
            (vec!["28", "jul"], md(7, 28)),
            // Connectives, in the arrangements a banner drifts between.
            (vec!["at", "jul", "28"], md(7, 28)),
            (vec!["on", "the", "28th", "of", "july"], md(7, 28)),
            (vec!["by", "jul", "28"], md(7, 28)),
            // A weekday beside the date corroborates it; the date still wins.
            (vec!["tue", "jul", "28"], md(7, 28)),
            // A stated year is kept, not inferred.
            (
                vec!["jul", "28", "2026"],
                DayWords::Named(DaySpec::MonthDay {
                    month: 7,
                    day: 28,
                    year: Some(2026),
                }),
            ),
            // Nothing but connectives is the bare form, not a refusal.
            (vec![], DayWords::Unnamed),
            (vec!["at"], DayWords::Unnamed),
            (vec!["on", "the"], DayWords::Unnamed),
            // The single-word forms still read as they always did.
            (
                vec!["wed"],
                DayWords::Named(DaySpec::Weekday(jiff::civil::Weekday::Wednesday)),
            ),
            (vec!["tomorrow"], DayWords::Named(DaySpec::Tomorrow)),
        ] {
            assert_eq!(parse_day_words(&words), Some(want), "{words:?}");
        }
    }

    /// The shapes that must stay unreadable, so the caller refuses out loud
    /// rather than inventing a date.
    #[test]
    fn refuses_day_words_it_cannot_read_unambiguously() {
        for words in [
            // Two loose numbers are two day-of-month claims, not a date.
            vec!["7", "16"],
            // A two-digit year makes three fields that could each be anything.
            vec!["7/28/26"],
            // A date token beside spelled-out fields is a second date.
            vec!["7/16", "jul", "28"],
            vec!["7/16", "16/7"],
            // Not a month, not a weekday.
            vec!["banana", "28"],
            vec!["jul", "banana"],
            // Out of range as a day of the month.
            vec!["jul", "32"],
            vec!["jul", "0"],
            // A trailing word that is not an ordinal suffix.
            vec!["jul", "28x"],
            // Half a date is not a date.
            vec!["jul"],
            vec!["28"],
            vec!["2026"],
            // The same field twice is two dates, not one.
            vec!["jul", "28", "aug"],
            vec!["jul", "28", "29"],
        ] {
            assert_eq!(parse_day_words(&words), None, "{words:?}");
        }
    }

    /// The permissive reading, for text trailing the banner's time: a date is
    /// picked up, unrelated prose is skipped rather than refused.
    #[test]
    fn find_day_words_skips_prose_instead_of_refusing() {
        assert_eq!(find_day_words(&["on", "jul", "28"]), Some(on(7, 28)));
        // A trailing "/upgrade to increase your limits" names no day, and that
        // is an absence, not an error.
        assert_eq!(
            find_day_words(&["upgrade", "to", "increase", "your", "limits"]),
            None
        );
        // A zone abbreviation left over beside the time is likewise skipped.
        assert_eq!(find_day_words(&["pst", "on", "jul", "28"]), Some(on(7, 28)));
        // Half a date stays unread rather than becoming a guess.
        assert_eq!(find_day_words(&["jul"]), None);
    }

    /// A month/day names no year, so the resolver picks one — the current year
    /// when the date is still ahead.
    #[test]
    fn resolves_a_month_day_later_this_year() {
        // now() is 2026-07-13 10:00.
        let z = resolve_day_clock(&now(), on(7, 28), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 7, 28));
        assert_eq!(hm(&z), (8, 0));
    }

    /// A date already gone by rolls to next year — but only when that lands
    /// inside the horizon, which is what makes the New Year's rollover work
    /// without letting every past date become a nudge a year out.
    #[test]
    fn a_month_day_just_past_new_year_rolls_to_the_next_year() {
        let dec30 = date(2026, 12, 30)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap();
        let z = resolve_day_clock(&dec30, on(1, 2), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2027, 1, 2));
    }

    /// The horizon guard. "Jul 28 8am" read at 10:00 ON Jul 28 is a date whose
    /// hour has already passed; the only other year that fits is 365 days out,
    /// and a nudge a year late is the misfire this module exists to prevent.
    /// Refusing lets the caller say so instead.
    #[test]
    fn a_month_day_that_can_only_be_a_year_out_is_unresolvable() {
        let jul28 = date(2026, 7, 28)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap();
        assert_eq!(
            resolve_day_clock(&jul28, on(7, 28), "8am", DateOrder::Unknown),
            None
        );
        // Same for a date months away in either direction.
        assert_eq!(
            resolve_day_clock(&now(), on(1, 2), "8am", DateOrder::Unknown),
            None
        );
        assert_eq!(
            resolve_day_clock(&now(), on(12, 25), "8am", DateOrder::Unknown),
            None
        );
    }

    /// Feb 29 in a common year is not a date at all. It must skip to a year
    /// where it is rather than panicking or silently sliding to Mar 1 -- and
    /// when no such year is in reach, it is simply unresolvable.
    #[test]
    fn a_leap_day_skips_years_that_do_not_have_one() {
        let feb2028 = date(2028, 2, 20)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap();
        let z = resolve_day_clock(&feb2028, on(2, 29), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2028, 2, 29));

        // 2026 has no Feb 29 and 2027's is far outside the horizon.
        let feb2026 = date(2026, 2, 20)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap();
        assert_eq!(
            resolve_day_clock(&feb2026, on(2, 29), "8am", DateOrder::Unknown),
            None
        );
    }

    /// All-numeric dates parse into both readings; nothing is chosen yet.
    #[test]
    fn parses_all_numeric_date_tokens() {
        let both = |first, second, year| {
            Some(DayWords::Named(DaySpec::Numeric {
                first,
                second,
                year,
            }))
        };
        assert_eq!(parse_day_words(&["7/16"]), both(7, 16, None));
        assert_eq!(parse_day_words(&["16-7"]), both(16, 7, None));
        assert_eq!(parse_day_words(&["on", "7/16"]), both(7, 16, None));
        assert_eq!(parse_day_words(&["7/16/2026"]), both(7, 16, Some(2026)));
        // A year stated separately folds into the token's date.
        assert_eq!(parse_day_words(&["7/16", "2026"]), both(7, 16, Some(2026)));
        // ISO settles its own field order, so it is a plain date, not a pair.
        assert_eq!(
            parse_day_words(&["2026-07-28"]),
            Some(DayWords::Named(DaySpec::MonthDay {
                month: 7,
                day: 28,
                year: Some(2026)
            }))
        );
    }

    /// Stage one of the cascade: there are only twelve months, so a field above
    /// twelve can only be the day. No convention is consulted, and the answer is
    /// the same in every locale — which is why `DateOrder::Unknown` is passed.
    #[test]
    fn a_field_above_twelve_settles_the_order_by_itself() {
        // now() is 2026-07-13. "7/16" can only be Jul 16; month 16 does not exist.
        let z = resolve_day_clock(&now(), num(7, 16), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 7, 16));
        // And the mirror, which can only be day-first.
        let z = resolve_day_clock(&now(), num(16, 7), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 7, 16));
    }

    /// Stage two: both readings are real dates, but a weekly reset is days away,
    /// not months. The calendar position rules one out without a locale.
    #[test]
    fn the_weekly_window_settles_an_order_the_calendar_cannot() {
        // now() is 2026-07-13. "1/8" is either Jan 8 (half a year off) or Aug 1
        // (19 days). Only one of those is a date a weekly banner would name.
        let z = resolve_day_clock(&now(), num(1, 8), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 8, 1));
        // The mirror: "8/1" is Aug 1 read month-first, Jan 8 read day-first.
        let z = resolve_day_clock(&now(), num(8, 1), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 8, 1));
    }

    /// Where both fields could be a month, the weekly window still settles it
    /// without a locale -- the two readings are a month apart, and only one of
    /// them is a week away.
    #[test]
    fn a_month_apart_pair_is_settled_by_the_week_not_the_locale() {
        // On Jul 7, "7/8" is Jul 8 (one day out) month-first, or Aug 7 (31 days)
        // day-first. Both are real dates that clear the horizon; only Jul 8 is a
        // plausible weekly reset.
        let jul7 = date(2026, 7, 7)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap();
        let z = resolve_day_clock(&jul7, num(7, 8), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 7, 8));

        // The mirror, so the window test is not satisfied by favouring
        // month-first: "8/7" is Aug 7 month-first and Jul 8 day-first, and the
        // day-first reading is the one inside the week.
        let z = resolve_day_clock(&jul7, num(8, 7), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 7, 8));
    }

    /// Identical readings are not an ambiguity. "8/8" means Aug 8 in every
    /// locale, and must not be refused just because the machine's is unset.
    #[test]
    fn a_date_that_reads_the_same_both_ways_needs_no_locale() {
        let aug5 = date(2026, 8, 5)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap();
        let z = resolve_day_clock(&aug5, num(8, 8), "8am", DateOrder::Unknown).unwrap();
        assert_eq!(z.date(), date(2026, 8, 8));
    }

    /// The locale arm itself. It is a backstop the weekly window keeps
    /// unreachable in practice (see `pick_reading`), so it is driven directly
    /// rather than through a banner that cannot produce it.
    #[test]
    fn the_locale_arm_picks_by_convention_and_refuses_without_one() {
        let at = |d: jiff::civil::Date| {
            d.at(8, 0, 0, 0)
                .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
                .unwrap()
        };
        let month_first = at(date(2026, 7, 8));
        let day_first = at(date(2026, 8, 7));
        // A window wide enough to contain both, which is what makes it a tie.
        let window = at(date(2026, 12, 31));

        assert_eq!(
            pick_reading(
                month_first.clone(),
                day_first.clone(),
                &window,
                DateOrder::MonthFirst
            ),
            Some(month_first.clone())
        );
        assert_eq!(
            pick_reading(
                month_first.clone(),
                day_first.clone(),
                &window,
                DateOrder::DayFirst
            ),
            Some(day_first)
        );
        // No convention to read: refuse rather than pick one at random.
        assert_eq!(
            pick_reading(
                month_first.clone(),
                at(date(2026, 8, 7)),
                &window,
                DateOrder::Unknown
            ),
            None
        );
    }

    /// A date number the calendar rejects outright, both ways round, is not a
    /// date at all.
    #[test]
    fn a_numeric_date_with_no_valid_reading_is_unresolvable() {
        // Neither 13/13 reading is a month.
        assert_eq!(
            resolve_day_clock(&now(), num(13, 13), "8am", DateOrder::Unknown),
            None
        );
        // now() is 2026-07-13; "1/2" is Jan 2 or Feb 1, both far past the
        // horizon in either direction.
        assert_eq!(
            resolve_day_clock(&now(), num(1, 2), "8am", DateOrder::Unknown),
            None
        );
    }

    /// The locale reader, over the strings a real machine actually sets.
    #[test]
    fn the_locale_date_order_reads_a_posix_locale_name() {
        assert_eq!(locale_region("en_US.UTF-8"), Some("US"));
        assert_eq!(locale_region("en_GB.UTF-8"), Some("GB"));
        assert_eq!(locale_region("de_DE@euro"), Some("DE"));
        assert_eq!(locale_region("fr_CA"), Some("CA"));
        // Nothing to read: these must not be mistaken for a region, and above
        // all must not be mistaken for the US.
        assert_eq!(locale_region("C"), None);
        assert_eq!(locale_region("POSIX"), None);
        assert_eq!(locale_region("C.UTF-8"), None);
        assert_eq!(locale_region(""), None);
        assert_eq!(locale_region("en_"), None);
    }

    /// The order each locale implies. Driven directly rather than through
    /// `locale_date_order`, which would have to mutate the process environment
    /// out from under every test running beside it.
    #[test]
    fn a_locale_name_implies_a_date_order() {
        assert_eq!(order_for_locale("en_US.UTF-8"), Some(DateOrder::MonthFirst));
        assert_eq!(order_for_locale("en_us"), Some(DateOrder::MonthFirst));
        assert_eq!(order_for_locale("en_GB.UTF-8"), Some(DateOrder::DayFirst));
        assert_eq!(order_for_locale("de_DE@euro"), Some(DateOrder::DayFirst));
        assert_eq!(order_for_locale("ja_JP.UTF-8"), Some(DateOrder::DayFirst));
        // Silence, which is not a vote for either convention.
        assert_eq!(order_for_locale("C"), None);
        assert_eq!(order_for_locale("POSIX"), None);
        assert_eq!(order_for_locale(""), None);
    }

    /// A year stated twice, once inside the date token and once beside it, is a
    /// contradiction rather than a confirmation.
    #[test]
    fn a_year_stated_twice_is_refused() {
        assert_eq!(parse_day_words(&["2026-07-28", "2026"]), None);
        assert_eq!(parse_day_words(&["7/16/2026", "2026"]), None);
    }

    /// A day number the month does not have is unresolvable, not a slide into
    /// the next month.
    #[test]
    fn a_day_number_the_month_lacks_is_unresolvable() {
        assert_eq!(
            resolve_day_clock(&now(), on(9, 31), "8am", DateOrder::Unknown),
            None
        );
    }
}

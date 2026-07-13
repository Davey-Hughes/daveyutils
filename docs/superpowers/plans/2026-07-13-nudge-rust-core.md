# nudge Rust rewrite — Core Library Implementation Plan (Phase 1, increment 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the pure, fully-unit-testable core library of the Rust nudge — time parsing, the job/queue model, rate-limit banner detection, and option-precedence resolution — with no daemon, tmux, or IPC yet.

**Architecture:** A single `nudge-rs/` crate exposing a library (`lib.rs`) plus a stub binary. This increment implements only side-effect-free logic modules so every behavior is covered by `cargo test` without external processes. Later increments layer the tmux target, daemon, IPC, and TUI on top of these types.

**Tech Stack:** Rust (edition 2021), `jiff` (date/time), `serde`/`serde_json` (persistence), `regex` + `strip-ansi-escapes` (banner detection), `thiserror` (typed errors). Dev: `tempfile`.

## Global Constraints

- Crate is at `nudge-rs/` in the repo root; the bash `scripts/nudge` and `tests/` stay untouched (reference oracle).
- Rust edition 2021. Pin crate majors in `Cargo.toml`; do not add crates a task does not use (YAGNI).
- Every module is side-effect-free except `queue` (filesystem). No network, no subprocess, no clock reads inside pure functions — the caller passes `now` in explicitly (this is what makes them testable).
- Where this plan's implementation code and a crate's real API disagree, **the test is the contract** — adjust the implementation to make the test pass, keep the test's asserted behavior.
- `cargo fmt --check` and `cargo clippy -- -D warnings` must pass at every commit.
- Commit messages: `feat(nudge-rs): …` / `test(nudge-rs): …` / `chore(nudge-rs): …`. No attribution lines.

## File Structure

- `nudge-rs/Cargo.toml` — manifest and pinned dependencies.
- `nudge-rs/src/main.rs` — stub binary entry (prints a placeholder; real CLI arrives in a later increment).
- `nudge-rs/src/lib.rs` — library root; declares and re-exports the modules below.
- `nudge-rs/src/timespec.rs` — parse a user time-spec string into an absolute `jiff::Zoned`.
- `nudge-rs/src/job.rs` — the `Job` value type and `Target` enum (serde).
- `nudge-rs/src/queue.rs` — load/persist a list of jobs atomically to a JSON file.
- `nudge-rs/src/detect.rs` — scan tmux pane text for a rate-limit banner → absolute reset time.
- `nudge-rs/src/config.rs` — `env_bool` and env < flag < `--no-*` option precedence.
- `.github/workflows/nudge-rs.yml` — fmt/clippy/test on Linux + macOS, scoped to `nudge-rs/**`.

---

### Task 1: Project scaffold + CI

**Files:**
- Create: `nudge-rs/Cargo.toml`
- Create: `nudge-rs/src/main.rs`
- Create: `nudge-rs/src/lib.rs`
- Create: `.github/workflows/nudge-rs.yml`

**Interfaces:**
- Consumes: nothing.
- Produces: a compiling crate named `nudge` with an empty `lib.rs` module list; later tasks add `mod` declarations here.

- [ ] **Step 1: Write the manifest**

`nudge-rs/Cargo.toml`:

```toml
[package]
name = "nudge"
version = "0.1.0"
edition = "2021"
description = "Rate-limit auto-resumer for AI CLIs in tmux"
license = "GPL-3.0-or-later"

[[bin]]
name = "nudge"
path = "src/main.rs"

[lib]
name = "nudge"
path = "src/lib.rs"

[dependencies]
jiff = { version = "0.2", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
regex = "1"
strip-ansi-escapes = "0.2"
thiserror = "2"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the stub binary and empty lib**

`nudge-rs/src/main.rs`:

```rust
fn main() {
    eprintln!("nudge: CLI not implemented yet (core-library increment)");
    std::process::exit(1);
}
```

`nudge-rs/src/lib.rs`:

```rust
//! Core library for nudge. Side-effect-free logic used by the CLI and daemon.
```

- [ ] **Step 3: Verify it builds and is clean**

Run: `cd nudge-rs && cargo build && cargo fmt --check && cargo clippy -- -D warnings && cargo test`
Expected: builds; fmt/clippy clean; `test result: ok. 0 passed`.

- [ ] **Step 4: Write the CI workflow**

`.github/workflows/nudge-rs.yml`:

```yaml
name: nudge-rs

on:
  push:
    paths: ['nudge-rs/**', '.github/workflows/nudge-rs.yml']
  pull_request:
    paths: ['nudge-rs/**', '.github/workflows/nudge-rs.yml']

jobs:
  check:
    name: ${{ matrix.os }}
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    defaults:
      run:
        working-directory: nudge-rs
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - run: cargo fmt --check
      - run: cargo clippy -- -D warnings
      - run: cargo test
```

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/Cargo.toml nudge-rs/src/main.rs nudge-rs/src/lib.rs .github/workflows/nudge-rs.yml nudge-rs/Cargo.lock
git commit -m "chore(nudge-rs): scaffold crate and CI"
```

---

### Task 2: `timespec` — parse user time strings to absolute instants

Mirrors bash `normalize_clock` / `parse_clock_epoch` / relative-time handling, but unified via jiff (no GNU/BSD split, no BSD relative-time rejection).

**Files:**
- Create: `nudge-rs/src/timespec.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod timespec;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn parse_timespec(input: &str, now: &jiff::Zoned) -> Result<jiff::Zoned, TimespecError>`
  - `pub enum TimespecError { Empty, Unrecognized(String) }` (derives `Debug`, `thiserror::Error`, `PartialEq`)
  - Accepts: 24h clock (`"14:30"`), 12h clock (`"3pm"`, `"3:00pm"`, `"3:00 PM"`), named (`"noon"`, `"midnight"`), relative (`"now + 45 min"`, `"in 90m"`, `"45m"`, `"2h"`, `"1h30m"`). Clock times resolve to today; if the resolved time is already past `now`, roll to tomorrow.

- [ ] **Step 1: Write the failing tests**

Append to `nudge-rs/src/timespec.rs`:

```rust
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
        assert_eq!(hm(&parse_timespec("now + 45 min", &now()).unwrap()), (10, 45));
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
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd nudge-rs && cargo test timespec`
Expected: compile error / FAIL — `parse_timespec` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `nudge-rs/src/timespec.rs` (above the test module):

```rust
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

    let re = Regex::new(r"^(?:(\d+)\s*h(?:ours?|rs?)?)?\s*(?:(\d+)\s*m(?:in(?:ute)?s?)?)?$")
        .unwrap();
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
fn parse_clock(s: &str, now: &Zoned) -> Option<Zoned> {
    let up = s.to_uppercase();
    let meridiem = Regex::new(r"(AM|PM)").unwrap().find(&up).map(|m| m.as_str());

    let time_re = Regex::new(r"(\d{1,2})(?::(\d{2}))?").unwrap();
    let caps = time_re.captures(&up)?;
    let mut hour: i8 = caps.get(1)?.as_str().parse().ok()?;
    let minute: i8 = caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0));

    match meridiem {
        Some("PM") if hour < 12 => hour += 12,
        Some("AM") if hour == 12 => hour = 0,
        Some(_) => {}
        None => {
            // Bare number without a meridiem must look like a 24h clock (had a ':' or hour>12 is invalid).
            if caps.get(2).is_none() && !up.contains(':') {
                return None;
            }
        }
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    at_clock(now, hour, minute)
}

/// Build today's `hour:minute` in now's zone; if it's already past, roll to tomorrow.
fn at_clock(now: &Zoned, hour: i8, minute: i8) -> Option<Zoned> {
    let tz = now.time_zone().clone();
    let today = now.date().at(hour, minute, 0, 0).to_zoned(tz.clone()).ok()?;
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
```

Also add to `nudge-rs/src/lib.rs`:

```rust
pub mod timespec;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd nudge-rs && cargo test timespec && cargo clippy -- -D warnings`
Expected: all `timespec::tests::*` PASS; clippy clean. If a jiff API name differs (e.g. `tomorrow`, `at`, `to_zoned`), fix the impl to match jiff 0.2 — keep the test assertions unchanged.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/timespec.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): timespec parsing (clock, named, relative) via jiff"
```

---

### Task 3: `job` model + `queue` persistence

**Files:**
- Create: `nudge-rs/src/job.rs`
- Create: `nudge-rs/src/queue.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod job;` and `pub mod queue;`)

**Interfaces:**
- Consumes: nothing (jiff `Timestamp` for `fire_at`).
- Produces:
  - `job::Target` — `#[serde(tag = "kind")] enum Target { Tmux { pane: String } }` (derives `Serialize, Deserialize, Clone, Debug, PartialEq, Eq`).
  - `job::Job { id: u64, target: Target, messages: Vec<String>, send_delay_secs: f64, fire_at: jiff::Timestamp, notify: bool, verify: bool, auto_retry: bool, retries_left: i64, settle_secs: f64 }` (derives `Serialize, Deserialize, Clone, Debug, PartialEq`).
  - `job::JobSpec` — same fields as `Job` minus `id` (what a caller supplies to `Queue::add`).
  - `queue::Queue::load(path: PathBuf) -> std::io::Result<Queue>`
  - `queue::Queue::add(&mut self, spec: JobSpec) -> std::io::Result<u64>` (assigns a monotonic id, persists, returns the id)
  - `queue::Queue::remove(&mut self, id: u64) -> std::io::Result<bool>`
  - `queue::Queue::get(&self, id: u64) -> Option<&Job>`
  - `queue::Queue::all(&self) -> &[Job]`

- [ ] **Step 1: Write the failing tests**

`nudge-rs/src/job.rs`:

```rust
//! The persisted job value type and its target descriptor.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum Target {
    Tmux { pane: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Job {
    pub id: u64,
    pub target: Target,
    pub messages: Vec<String>,
    pub send_delay_secs: f64,
    pub fire_at: jiff::Timestamp,
    pub notify: bool,
    pub verify: bool,
    pub auto_retry: bool,
    pub retries_left: i64,
    pub settle_secs: f64,
}

/// What a caller supplies to `Queue::add` (everything but the id).
#[derive(Clone, Debug, PartialEq)]
pub struct JobSpec {
    pub target: Target,
    pub messages: Vec<String>,
    pub send_delay_secs: f64,
    pub fire_at: jiff::Timestamp,
    pub notify: bool,
    pub verify: bool,
    pub auto_retry: bool,
    pub retries_left: i64,
    pub settle_secs: f64,
}

impl JobSpec {
    pub fn into_job(self, id: u64) -> Job {
        Job {
            id,
            target: self.target,
            messages: self.messages,
            send_delay_secs: self.send_delay_secs,
            fire_at: self.fire_at,
            notify: self.notify,
            verify: self.verify,
            auto_retry: self.auto_retry,
            retries_left: self.retries_left,
            settle_secs: self.settle_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_roundtrips_through_json() {
        let job = Job {
            id: 7,
            target: Target::Tmux { pane: "bot:0.1".into() },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: true,
            verify: false,
            auto_retry: true,
            retries_left: -1,
            settle_secs: 5.0,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: Job = serde_json::from_str(&json).unwrap();
        assert_eq!(job, back);
        // Target is externally tagged by `kind` for readable state files.
        assert!(json.contains(r#""kind":"Tmux""#));
    }
}
```

`nudge-rs/src/queue.rs`:

```rust
//! Load and atomically persist the job list as JSON.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::job::{Job, JobSpec};

#[derive(Serialize, Deserialize, Default)]
struct State {
    next_id: u64,
    jobs: Vec<Job>,
}

pub struct Queue {
    path: PathBuf,
    state: State,
}

impl Queue {
    /// Load the queue from `path`, or start empty if the file is absent.
    pub fn load(path: PathBuf) -> std::io::Result<Queue> {
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e),
        };
        Ok(Queue { path, state })
    }

    pub fn all(&self) -> &[Job] {
        &self.state.jobs
    }

    pub fn get(&self, id: u64) -> Option<&Job> {
        self.state.jobs.iter().find(|j| j.id == id)
    }

    /// Assign a monotonic id, append, persist, and return the new id.
    pub fn add(&mut self, spec: JobSpec) -> std::io::Result<u64> {
        self.state.next_id += 1;
        let id = self.state.next_id;
        self.state.jobs.push(spec.into_job(id));
        self.save()?;
        Ok(id)
    }

    /// Remove the job with `id`. Returns whether one was removed.
    pub fn remove(&mut self, id: u64) -> std::io::Result<bool> {
        let before = self.state.jobs.len();
        self.state.jobs.retain(|j| j.id != id);
        let removed = self.state.jobs.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Write to a sibling temp file then rename, so a crash never leaves a
    /// half-written queue.
    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(&self.state)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Target;

    fn spec() -> JobSpec {
        JobSpec {
            target: Target::Tmux { pane: "bot:0.1".into() },
            messages: vec!["go".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 2,
            settle_secs: 5.0,
        }
    }

    #[test]
    fn add_assigns_monotonic_ids_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");

        let mut q = Queue::load(path.clone()).unwrap();
        let id1 = q.add(spec()).unwrap();
        let id2 = q.add(spec()).unwrap();
        assert_eq!((id1, id2), (1, 2));

        // Reloading sees both jobs and keeps counting from 2.
        let mut q2 = Queue::load(path.clone()).unwrap();
        assert_eq!(q2.all().len(), 2);
        assert_eq!(q2.add(spec()).unwrap(), 3);
    }

    #[test]
    fn remove_reports_hit_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        let id = q.add(spec()).unwrap();
        assert!(q.remove(id).unwrap());
        assert!(!q.remove(id).unwrap());
        assert!(q.get(id).is_none());
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::load(dir.path().join("nope.json")).unwrap();
        assert!(q.all().is_empty());
    }
}
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod job;
pub mod queue;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd nudge-rs && cargo test job queue`
Expected: compile error / FAIL — modules not declared yet (before lib.rs edit) or types missing.

- [ ] **Step 3: Confirm the implementation compiles**

The code in Step 1 is the implementation (model + persistence together with their tests). No separate implementation step is required beyond adding the `pub mod` lines.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd nudge-rs && cargo test job queue && cargo clippy -- -D warnings`
Expected: `job::tests::*` and `queue::tests::*` PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/job.rs nudge-rs/src/queue.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): job model and atomic JSON queue"
```

---

### Task 4: `detect` — rate-limit banner detection

Mirrors bash `clock_banner_re` / `duration_banner_re` / `detect_reset_epoch` / `normalize_clock`. A limit is one of two shapes: a **clock** reset time (Claude: "…resets 3:00pm") or a **duration** countdown (Antigravity: "…resets in 1h30m"). Adds 3-minute safety padding. Reuses `timespec::parse_timespec` for the clock case.

**Files:**
- Create: `nudge-rs/src/detect.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod detect;`)

**Interfaces:**
- Consumes: `timespec::parse_timespec`.
- Produces:
  - `pub fn detect_reset(pane_text: &str, now: &jiff::Zoned, clock_ext: Option<&str>, dur_ext: Option<&str>) -> Option<jiff::Zoned>` — returns the padded absolute reset time, or `None` if no banner matches.

- [ ] **Step 1: Write the failing tests**

Append to `nudge-rs/src/detect.rs`:

```rust
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
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd nudge-rs && cargo test detect`
Expected: FAIL — `detect_reset` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `nudge-rs/src/detect.rs`:

```rust
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

/// Returns the padded absolute reset time, or `None` if no banner is present.
pub fn detect_reset(
    pane_text: &str,
    now: &Zoned,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
) -> Option<Zoned> {
    let clean = String::from_utf8_lossy(&strip_ansi_escapes::strip(pane_text)).into_owned();

    // Duration shape: "... Resets in 1h30m / 45m".
    if duration_re(dur_ext).is_match(&clean) {
        if let Some(caps) =
            Regex::new(r"(?i)resets?\s+in\s+([0-9hm ]+)").unwrap().captures(&clean)
        {
            let spec = caps.get(1).unwrap().as_str().trim();
            if let Ok(z) = parse_timespec(spec, now) {
                return z.checked_add(PADDING_MINUTES.minutes()).ok();
            }
        }
    }

    // Clock shape: "... resets 3:00pm" / "... try again at 4pm".
    if clock_re(clock_ext).is_match(&clean) {
        if let Some(tok) = find_clock_token(&clean) {
            if let Ok(z) = parse_timespec(&tok, now) {
                return z.checked_add(PADDING_MINUTES.minutes()).ok();
            }
        }
    }

    None
}

/// Extract the first "3pm" / "3:00 PM" / "14:30" token from the text.
fn find_clock_token(text: &str) -> Option<String> {
    let re = Regex::new(r"(?i)\b(\d{1,2}(?::\d{2})?\s*(?:am|pm)|\d{1,2}:\d{2})\b").unwrap();
    re.find(text).map(|m| m.as_str().to_string())
}
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod detect;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd nudge-rs && cargo test detect && cargo clippy -- -D warnings`
Expected: all `detect::tests::*` PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/detect.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): rate-limit banner detection (clock + duration shapes)"
```

---

### Task 5: `config` — env_bool + option precedence

Mirrors bash `env_bool` and the env < CLI flag < `--no-*` precedence.

**Files:**
- Create: `nudge-rs/src/config.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod config;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn env_bool(v: Option<&str>) -> bool` — `1/true/yes/on` (any case) → true; anything else/None → false.
  - `pub struct Toggles { pub notify: bool, pub verify: bool, pub auto_retry: bool, pub retries: i64, pub settle_secs: f64 }`
  - `pub struct FlagOverrides { pub notify: Option<bool>, pub verify: Option<bool>, pub auto_retry: Option<bool>, pub retries: Option<i64>, pub settle_secs: Option<f64> }` — `Some(true)` from `-n`, `Some(false)` from `--no-notify`, `None` when the flag was absent.
  - `pub fn resolve(env: &Toggles, overrides: &FlagOverrides) -> Toggles` — an override, when present, wins over the env default. Setting `retries` to any value implies `auto_retry = true`.

- [ ] **Step 1: Write the failing tests**

Append to `nudge-rs/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_bool_truthy_and_falsy() {
        for t in ["1", "true", "TRUE", "Yes", "on"] {
            assert!(env_bool(Some(t)), "{t} should be truthy");
        }
        for f in ["0", "false", "no", "", "banana"] {
            assert!(!env_bool(Some(f)), "{f} should be falsy");
        }
        assert!(!env_bool(None));
    }

    fn env() -> Toggles {
        Toggles { notify: true, verify: false, auto_retry: false, retries: 2, settle_secs: 5.0 }
    }

    fn no_overrides() -> FlagOverrides {
        FlagOverrides { notify: None, verify: None, auto_retry: None, retries: None, settle_secs: None }
    }

    #[test]
    fn env_defaults_apply_when_no_flags() {
        let out = resolve(&env(), &no_overrides());
        assert!(out.notify);
        assert!(!out.verify);
    }

    #[test]
    fn flag_overrides_env() {
        let mut ov = no_overrides();
        ov.notify = Some(false); // --no-notify beats NUDGE_NOTIFY=1
        ov.verify = Some(true); // -v beats unset
        let out = resolve(&env(), &ov);
        assert!(!out.notify);
        assert!(out.verify);
    }

    #[test]
    fn setting_retries_implies_auto_retry() {
        let mut ov = no_overrides();
        ov.retries = Some(5);
        let out = resolve(&env(), &ov);
        assert!(out.auto_retry);
        assert_eq!(out.retries, 5);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd nudge-rs && cargo test config`
Expected: FAIL — `env_bool` / `resolve` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `nudge-rs/src/config.rs`:

```rust
//! Boolean env parsing and env < flag < `--no-*` option precedence.

#[derive(Clone, Debug, PartialEq)]
pub struct Toggles {
    pub notify: bool,
    pub verify: bool,
    pub auto_retry: bool,
    pub retries: i64,
    pub settle_secs: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlagOverrides {
    pub notify: Option<bool>,
    pub verify: Option<bool>,
    pub auto_retry: Option<bool>,
    pub retries: Option<i64>,
    pub settle_secs: Option<f64>,
}

/// `1/true/yes/on` (any case) -> true; everything else (incl. None) -> false.
pub fn env_bool(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Overlay `overrides` (present values only) onto the env `Toggles`.
pub fn resolve(env: &Toggles, overrides: &FlagOverrides) -> Toggles {
    let mut out = env.clone();
    if let Some(v) = overrides.notify {
        out.notify = v;
    }
    if let Some(v) = overrides.verify {
        out.verify = v;
    }
    if let Some(v) = overrides.auto_retry {
        out.auto_retry = v;
    }
    if let Some(v) = overrides.settle_secs {
        out.settle_secs = v;
    }
    if let Some(v) = overrides.retries {
        out.retries = v;
        out.auto_retry = true; // setting a retry count implies auto-retry
    }
    out
}
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod config;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd nudge-rs && cargo test && cargo fmt --check && cargo clippy -- -D warnings`
Expected: the whole suite PASSES (timespec + job + queue + detect + config); fmt/clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/config.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): env_bool and option-precedence resolution"
```

---

## Self-Review

**Spec coverage (core-library slice of the design):**
- Time parsing incl. removing the GNU/BSD split and BSD relative-time rejection → Task 2. ✅
- Job model + persistent queue owned by nudge (replaces `at -c` parsing) → Task 3. ✅
- Banner detection, clock vs duration shapes, `NUDGE_CLOCK_PATTERN`/`NUDGE_DURATION_PATTERN`, 3-min padding → Task 4. ✅
- `env_bool` + env < flag < `--no-*` precedence → Task 5. ✅
- Testing/CI (cargo test + fmt/clippy on Linux + macOS) → Task 1. ✅
- Out of this increment (later plans): tmux target, inject/verify/retry runtime, daemon/IPC/registration, CLI, TUI, notifications, packaging. Intentionally deferred.

**Placeholder scan:** No TBD/TODO; every code step contains complete code; tests are concrete. ✅

**Type consistency:** `Toggles`/`FlagOverrides` fields match between Task 5's interface block, tests, and impl. `JobSpec::into_job` field names match `Job`. `detect_reset` signature matches its Task-4 interface and its consumer note. `parse_timespec(&str, &Zoned)` signature is consistent across Tasks 2 and 4. ✅

## Notes for the next increment (plan 2)

- Add `Target::capture()`/`send_line()` behind a trait; implement `TmuxTarget` over `tmux capture-pane -p` / `send-keys`.
- Build the inject path (`-w` delay between messages, `--verify` re-check via `detect_reset`, auto-retry decrementing `retries_left`).
- Add `clap` and a real `main.rs` that can schedule directly (pre-daemon) to exercise the end-to-end path against a live tmux pane, with a self-skipping integration test.

# nudge Rust rewrite — tmux Target + inject path (Phase 1, increment 2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the injection Target abstraction, a real tmux backend, and the inject path (verify-gate + ordered sends) — the first slice that actually acts on a live pane.

**Architecture:** A `Target` trait (`capture` + `send_line`) decouples the inject logic from tmux. `run_injection` is pure-ish (its verify/send decisions are unit-tested against an in-memory fake); `TmuxTarget` is the real subprocess backend, validated by self-skipping integration tests against a private tmux server. No daemon, IPC, CLI, or retry-rescheduling yet (those are later increments).

**Tech Stack:** Rust 2021, `anyhow` (new — subprocess/app errors), reusing `jiff`, `detect`, `job` from increment 1.

## Context

This is increment 2, building on the increment-1 core library (branch `feat/nudge-rust`, PR #5). It consumes `crate::detect::detect_reset`, `crate::job::Job`, and jiff. The bash `scripts/nudge` `send-keys`/`capture-pane` behavior is the reference.

## Global Constraints

- Crate at `nudge-rs/`, edition 2021. Add `anyhow = "1"` to `Cargo.toml` (first use is this increment). No other new crates (YAGNI).
- The effectful layer is `target::tmux` (subprocess) and `inject` (delegates to a `Target`, sleeps between sends). `inject`'s **decision logic** must be testable with no external process — via an in-memory fake `Target`. `TmuxTarget`'s real behavior is covered by integration tests that **self-skip when tmux is absent** (mirrors the bash e2e).
- Integration tests MUST use a private tmux server (`tmux -L <unique-socket>`) and kill it on drop, so a developer's real tmux session is never touched.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass at every commit. Commit prefixes `feat/test/chore(nudge-rs): …`; NO attribution lines.
- Naming note (deliberate, do NOT "fix" here): `job::Target` (the serializable enum from increment 1) and the new `target::Target` (behavior trait) share a name in different modules. Tests disambiguate with `use crate::job::Target as TargetKind`. The rename `job::Target → job::TargetSpec` + a `spec → Box<dyn Target>` bridge is deferred to increment 3 (the daemon), where the bridge is actually needed. A reviewer flagging the collision should be told it is a tracked, deliberate deferral.

## File Structure

- `nudge-rs/Cargo.toml` — add `anyhow`.
- `nudge-rs/src/target/mod.rs` — the `Target` trait (and `pub mod tmux;`).
- `nudge-rs/src/target/tmux.rs` — `TmuxTarget` subprocess backend.
- `nudge-rs/src/inject.rs` — `run_injection` + `InjectOutcome`.
- `nudge-rs/src/lib.rs` — add `pub mod target;` and `pub mod inject;`.
- `nudge-rs/tests/tmux_e2e.rs` — self-skipping integration tests (round-trip + end-to-end injection).

---

### Task 1: `Target` trait + `inject` path (fake-target unit tests)

**Files:**
- Modify: `nudge-rs/Cargo.toml` (add `anyhow = "1"`)
- Create: `nudge-rs/src/target/mod.rs`
- Create: `nudge-rs/src/inject.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod target;` and `pub mod inject;`)

**Interfaces:**
- Consumes: `crate::detect::detect_reset`, `crate::job::Job` (fields incl. `verify: bool`, `messages: Vec<String>`, `send_delay_secs: f64`), jiff `Zoned`.
- Produces:
  - `target::Target` trait: `fn capture(&self) -> anyhow::Result<String>`; `fn send_line(&self, text: &str) -> anyhow::Result<()>`.
  - `inject::InjectOutcome` — `enum { Sent(usize), SkippedVerify }` (derives `Debug, PartialEq, Eq`).
  - `inject::run_injection(target: &dyn Target, job: &Job, now: &jiff::Zoned, clock_ext: Option<&str>, dur_ext: Option<&str>) -> anyhow::Result<InjectOutcome>`.

- [ ] **Step 1: Add the dependency**

In `nudge-rs/Cargo.toml`, under `[dependencies]`, add:

```toml
anyhow = "1"
```

- [ ] **Step 2: Create the trait**

`nudge-rs/src/target/mod.rs`:

```rust
//! The injection target abstraction: anything nudge can read a screen from
//! (for banner detection / `--verify`) and send a submitted line of text to.

use anyhow::Result;

/// A place nudge can read from and type into. `job::Target` is the serializable
/// *descriptor* of one of these; this trait is the runtime *behavior*.
pub trait Target {
    /// Capture the target's current visible screen text.
    fn capture(&self) -> Result<String>;

    /// Type `text` into the target and submit it (as if Enter were pressed).
    fn send_line(&self, text: &str) -> Result<()>;
}
```

- [ ] **Step 3: Write the failing inject tests**

`nudge-rs/src/inject.rs`:

```rust
//! Run a scheduled nudge against a `Target`: an optional `--verify` gate, then
//! type each message in order.

use anyhow::Result;

use crate::detect::detect_reset;
use crate::job::Job;
use crate::target::Target;

/// The result of an injection attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum InjectOutcome {
    /// Messages were sent; carries how many.
    Sent(usize),
    /// `--verify` was on and the pane no longer showed a rate-limit banner, so
    /// nothing was sent (the session was likely already resumed).
    SkippedVerify,
}

/// Execute `job`'s injection against `target`.
///
/// With `job.verify`, the pane is captured and checked for a rate-limit banner
/// first; if none is present the send is skipped. Otherwise each message is
/// typed and submitted, pausing `job.send_delay_secs` between messages.
pub fn run_injection(
    target: &dyn Target,
    job: &Job,
    now: &jiff::Zoned,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
) -> Result<InjectOutcome> {
    if job.verify {
        let screen = target.capture()?;
        if detect_reset(&screen, now, clock_ext, dur_ext).is_none() {
            return Ok(InjectOutcome::SkippedVerify);
        }
    }

    let delay = std::time::Duration::from_secs_f64(job.send_delay_secs.max(0.0));
    for (i, msg) in job.messages.iter().enumerate() {
        if i > 0 && !delay.is_zero() {
            std::thread::sleep(delay);
        }
        target.send_line(msg)?;
    }
    Ok(InjectOutcome::Sent(job.messages.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, Target as TargetKind};
    use jiff::{civil::date, tz::TimeZone};
    use std::cell::RefCell;

    fn now() -> jiff::Zoned {
        date(2026, 7, 13)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap()
    }

    /// In-memory Target: returns a fixed screen, records what was sent.
    struct FakeTarget {
        screen: String,
        sent: RefCell<Vec<String>>,
    }
    impl FakeTarget {
        fn new(screen: &str) -> Self {
            FakeTarget { screen: screen.to_string(), sent: RefCell::new(Vec::new()) }
        }
    }
    impl Target for FakeTarget {
        fn capture(&self) -> anyhow::Result<String> {
            Ok(self.screen.clone())
        }
        fn send_line(&self, text: &str) -> anyhow::Result<()> {
            self.sent.borrow_mut().push(text.to_string());
            Ok(())
        }
    }

    fn job(verify: bool, messages: &[&str]) -> Job {
        Job {
            id: 1,
            target: TargetKind::Tmux { pane: "x".into() },
            messages: messages.iter().map(|s| s.to_string()).collect(),
            send_delay_secs: 0.0,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
        }
    }

    #[test]
    fn sends_all_messages_in_order_when_verify_off() {
        let t = FakeTarget::new("");
        let out = run_injection(&t, &job(false, &["one", "two"]), &now(), None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(2));
        assert_eq!(*t.sent.borrow(), vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn verify_sends_when_banner_present() {
        let t = FakeTarget::new("quota reached. Resets in 45m");
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
        assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
    }

    #[test]
    fn verify_skips_when_banner_gone() {
        let t = FakeTarget::new("all done, no limits here");
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None).unwrap();
        assert_eq!(out, InjectOutcome::SkippedVerify);
        assert!(t.sent.borrow().is_empty());
    }
}
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod inject;
pub mod target;
```

- [ ] **Step 4: Run tests to verify they fail, then pass**

Run: `cd nudge-rs && cargo test inject`
Expected first: FAIL/compile-error (modules not declared / `run_injection` missing) if run before the lib.rs edit; after adding the code and `pub mod` lines, re-run — the three `inject::tests::*` PASS.

- [ ] **Step 5: Lint and commit**

Run: `cd nudge-rs && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: whole suite passes (26 tests: 23 prior + 3 new); clippy clean.

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/target/mod.rs nudge-rs/src/inject.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): Target trait and inject path (verify-gate + ordered sends)"
```

---

### Task 2: `TmuxTarget` backend + self-skipping tmux integration tests

**Files:**
- Create: `nudge-rs/src/target/tmux.rs`
- Modify: `nudge-rs/src/target/mod.rs` (add `pub mod tmux;`)
- Create: `nudge-rs/tests/tmux_e2e.rs`

**Interfaces:**
- Consumes: `target::Target`, `inject::run_injection`/`InjectOutcome`, `job::{Job, Target as TargetKind}`.
- Produces:
  - `target::tmux::TmuxTarget` with `pub fn new(pane: impl Into<String>) -> Self` and `pub fn with_socket(pane: impl Into<String>, socket: impl Into<String>) -> Self`.
  - `impl Target for TmuxTarget` using `tmux capture-pane -p -t <pane>` and `tmux send-keys -t <pane> -l <text>` + `send-keys -t <pane> Enter`, honoring an optional `-L <socket>`.

- [ ] **Step 1: Write the implementation**

`nudge-rs/src/target/tmux.rs`:

```rust
//! A tmux pane as an injection `Target`, via `tmux capture-pane` / `send-keys`.

use anyhow::{bail, Context, Result};
use std::process::{Command, Output};

use super::Target;

/// A specific tmux pane, addressed by tmux's target syntax (e.g. "bot:0.1").
/// An optional server socket (`tmux -L <socket>`) supports non-default servers
/// and test isolation.
pub struct TmuxTarget {
    pane: String,
    socket: Option<String>,
}

impl TmuxTarget {
    pub fn new(pane: impl Into<String>) -> Self {
        Self { pane: pane.into(), socket: None }
    }

    pub fn with_socket(pane: impl Into<String>, socket: impl Into<String>) -> Self {
        Self { pane: pane.into(), socket: Some(socket.into()) }
    }

    /// Run a tmux subcommand (with the configured socket, if any), erroring on
    /// non-zero exit.
    fn run(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = Command::new("tmux");
        if let Some(sock) = &self.socket {
            cmd.args(["-L", sock]);
        }
        cmd.args(args);
        let out = cmd.output().context("failed to run tmux")?;
        if !out.status.success() {
            bail!(
                "tmux {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }
}

impl Target for TmuxTarget {
    fn capture(&self) -> Result<String> {
        let out = self.run(&["capture-pane", "-p", "-t", &self.pane])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn send_line(&self, text: &str) -> Result<()> {
        // `-l` sends the text literally so tmux doesn't interpret key names;
        // a separate `Enter` submits it.
        self.run(&["send-keys", "-t", &self.pane, "-l", text])?;
        self.run(&["send-keys", "-t", &self.pane, "Enter"])?;
        Ok(())
    }
}
```

Add to `nudge-rs/src/target/mod.rs` (below the trait):

```rust
pub mod tmux;
```

- [ ] **Step 2: Write the integration tests**

`nudge-rs/tests/tmux_e2e.rs`:

```rust
//! Integration tests for the tmux backend and the end-to-end inject path.
//! Each runs against a PRIVATE tmux server (`tmux -L <socket>`) that is killed
//! on drop, so the developer's real tmux is never touched. All self-skip when
//! tmux is not installed.

use std::process::Command;
use std::{thread, time::Duration};

use jiff::{civil::date, tz::TimeZone};
use nudge::inject::{run_injection, InjectOutcome};
use nudge::job::{Job, Target as TargetKind};
use nudge::target::{tmux::TmuxTarget, Target};

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Owns a private tmux server; kills it on drop (even on panic).
struct Server {
    socket: String,
}
impl Server {
    /// Start a detached 80x24 session named `s` running a plain shell.
    fn start() -> Self {
        let socket = format!("nudge-it-{}-{}", std::process::id(), line!());
        let ok = Command::new("tmux")
            .args([
                "-L", &socket, "new-session", "-d", "-s", "s", "-x", "80", "-y", "24", "sh",
            ])
            .status()
            .expect("spawn tmux")
            .success();
        assert!(ok, "failed to start private tmux session");
        Server { socket }
    }
    fn target(&self) -> TmuxTarget {
        TmuxTarget::with_socket("s", &self.socket)
    }
    /// Type a raw line straight into the pane via tmux (bypassing TmuxTarget),
    /// used only to stage pane contents for a test.
    fn stage(&self, line: &str) {
        Command::new("tmux")
            .args(["-L", &self.socket, "send-keys", "-t", "s", "-l", line])
            .status()
            .unwrap();
        Command::new("tmux")
            .args(["-L", &self.socket, "send-keys", "-t", "s", "Enter"])
            .status()
            .unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .status();
    }
}

fn fixed_now() -> jiff::Zoned {
    date(2026, 7, 13)
        .at(10, 0, 0, 0)
        .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
        .unwrap()
}

#[test]
fn send_line_reaches_pane_and_capture_reads_it() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let target = server.target();

    target.send_line("echo nudge_marker_42").unwrap();
    thread::sleep(Duration::from_millis(500)); // let the shell run + render

    let screen = target.capture().unwrap();
    assert!(
        screen.contains("nudge_marker_42"),
        "captured pane missing the marker; got:\n{screen}"
    );
}

#[test]
fn end_to_end_injection_verifies_then_sends() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    // Stage a rate-limit banner into the pane so the verify-gate passes.
    server.stage("printf 'quota reached. Resets in 45m\\n'");
    thread::sleep(Duration::from_millis(500));

    let target = server.target();
    let job = Job {
        id: 1,
        target: TargetKind::Tmux { pane: "s".into() },
        messages: vec!["nudge_continue_marker".into()],
        send_delay_secs: 0.0,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: true,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
    };

    let out = run_injection(&target, &job, &fixed_now(), None, None).unwrap();
    assert_eq!(out, InjectOutcome::Sent(1));

    thread::sleep(Duration::from_millis(500));
    let screen = target.capture().unwrap();
    assert!(
        screen.contains("nudge_continue_marker"),
        "sent message not visible in pane; got:\n{screen}"
    );
}
```

- [ ] **Step 3: Run the tests**

Run: `cd nudge-rs && cargo test --test tmux_e2e -- --nocapture`
Expected (with tmux installed): both tests PASS. Without tmux: both print "skipping" and PASS. If a timing flake appears (the shell hadn't rendered), the 500ms sleeps are the knob — note it but do not reduce them.

- [ ] **Step 4: Lint and full suite**

Run: `cd nudge-rs && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: unit suite 26/26 plus the 2 integration tests; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/target/tmux.rs nudge-rs/src/target/mod.rs nudge-rs/tests/tmux_e2e.rs
git commit -m "feat(nudge-rs): tmux Target backend with self-skipping integration tests"
```

---

## Self-Review

**Spec coverage (increment-2 slice):**
- `Target` trait decoupling tmux → Task 1. ✅
- Inject path: `-w` delay between sends, `--verify` gate skipping when banner gone → Task 1 (`run_injection`), unit-tested via fake. ✅
- Real tmux backend (`send-keys`/`capture-pane`) → Task 2. ✅
- Self-skipping e2e mirroring bash `test_e2e_tmux.sh`; private socket isolation → Task 2. ✅
- Out of this increment (later): auto-retry rescheduling (needs daemon), notifications, CLI, daemon/IPC, TUI. Intentionally deferred.

**Placeholder scan:** No TBD/TODO; every code step is complete; tests are concrete. ✅

**Type consistency:** `Target` trait methods (`capture`/`send_line`) match between `target::mod`, `TmuxTarget`, the fake, and `run_injection`'s `&dyn Target`. `Job` construction in tests matches the increment-1 field set exactly (id, target, messages, send_delay_secs, fire_at, notify, verify, auto_retry, retries_left, settle_secs). `InjectOutcome` variants consistent across `inject.rs` and `tmux_e2e.rs`. `job::Target` imported `as TargetKind` everywhere to avoid the documented trait/enum name collision. ✅

## Notes for the next increment (daemon, increment 3)

- Rename `job::Target` → `job::TargetSpec` and add `fn connect(&self) -> Box<dyn target::Target>` (Tmux{pane} → TmuxTarget), resolving the name collision at the point the bridge is needed.
- The daemon owns retry/reschedule: after `run_injection` returns `Sent`, if `auto_retry` and `retries_left != 0`, wait `settle_secs`, re-scan via `detect_reset`, and reschedule with `retries_left - 1`.
- Fire notifications (`notify-rust`) on send.

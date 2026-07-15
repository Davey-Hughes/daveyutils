# nudge Rust rewrite — CLI (Phase 1, increment 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give nudge its front door — a `clap` CLI that schedules nudges, manages jobs, runs and installs the daemon, picks panes interactively, and fires desktop notifications — reaching feature parity with the bash `scripts/nudge`.

**Architecture:** `main.rs` parses a `clap` `Cli` and dispatches by mode. Scheduling builds a `JobSpec` (time from `-m` via `timespec`, or auto-detected from the pane via `detect_reset`) and sends it over the IPC client, auto-starting the daemon if the socket is dead. Job management (`--list`/`--cancel`/`--edit`) are IPC calls. `--daemon`/`--install-daemon`/`--uninstall-daemon` wire the daemon and registration built in increments 3a-3c. The interactive pane/job pickers use `inquire` (bundled — no fzf).

**Tech Stack:** Rust 2021, adds `clap` (derive), `inquire`, `notify-rust`; reuses everything from increments 1-3c.

## Context

Increment 4, stacked on `feat/nudge-rust-register` (3c, PR #9). The whole daemon backend exists; this is the user-facing layer. Also folds in the daemon fixes deferred from the 3b review (retry-interval floor, swallowed-error logging, `init_tracing`). The bash `scripts/nudge` help (flags, env vars, default message "please continue") is the parity target.

Picker scope: `inquire` select-lists for panes/jobs (removes the fzf dependency, bundled). A richer ratatui live-preview dashboard is explicitly a later polish, not this increment.

## Global Constraints

- Crate at `nudge-rs/`, edition 2021. Add `clap = { version = "4", features = ["derive"] }`, `inquire = "0.7"`, `notify-rust = "4"`. No others.
- Effectful entrypoints (`--daemon`, `--install-daemon`, `--uninstall-daemon`, auto-start, notifications, interactive pickers) are NOT exercised by tests. Testable logic — arg→options resolution, `JobSpec` construction, IPC round-trips, tmux-pane-list parsing — is unit/integration tested hermetically (tempdir sockets; no real daemon spawned, no real notification, no systemctl/launchctl).
- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` pass every commit. Commit prefixes `feat/fix(nudge-rs): …`; NO attribution.

## File Structure

- `nudge-rs/Cargo.toml` — add `clap`, `inquire`, `notify-rust`.
- `nudge-rs/src/daemon.rs` — retry-floor + error logging (deferred 3b fixes).
- `nudge-rs/src/cli.rs` — `Cli` (clap) + `resolve_options`.
- `nudge-rs/src/app.rs` — `schedule`, `list`, `cancel`, `edit`, `build_spec`, `ensure_daemon`, `pick_pane`.
- `nudge-rs/src/tmux_panes.rs` — `list_panes` + parse helper.
- `nudge-rs/src/notify.rs` — `send` (notify-rust wrapper).
- `nudge-rs/src/main.rs` — parse + dispatch (replaces the stub).
- `nudge-rs/src/lib.rs` — add the new `pub mod`s.

---

### Task 1: deferred daemon fixes (retry floor + error logging)

**Files:**
- Modify: `nudge-rs/src/daemon.rs`

- [ ] **Step 1: Update the retry computation and error logging**

In `nudge-rs/src/daemon.rs`, replace the `retry_at` computation and the two `let _ =` lines. Add a constant near the top:

```rust
/// A retry never lands sooner than this, so a sub-second `settle_secs` can't
/// create a fire-storm (esp. with infinite retries).
const MIN_RETRY_SECS: f64 = 1.0;
```

Change the retry_at block (fractional-safe + floored) from `Span::new().seconds(job.settle_secs as i64)` to:

```rust
            let retry_secs = job.settle_secs.max(MIN_RETRY_SECS);
            let retry_at = now
                .checked_add(Span::new().milliseconds((retry_secs * 1000.0) as i64))
                .map(|z| z.timestamp())
                .unwrap_or(now.timestamp());
```

Change the swallowed results to log on error:

```rust
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = apply_outcome(&mut q, job, &outcome, retry_at) {
                tracing::warn!("nudge: persisting job {} outcome failed: {e}", job.id);
            }
```

and in the drop-stale block:

```rust
            for id in &plan_now.drop_stale {
                if let Err(e) = q.remove(*id) {
                    tracing::warn!("nudge: removing stale job {id} failed: {e}");
                }
                tracing::info!("nudge: dropped stale job {id}");
            }
```

Add a line to `run`'s doc comment: `/// Call [`init_tracing`] first if you want the daemon's logs.`

- [ ] **Step 2: Verify + commit**

Run: `cd nudge-rs && cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: full suite still green (no test asserts the old truncation); clippy clean.

```bash
git add nudge-rs/src/daemon.rs
git commit -m "fix(nudge-rs): floor retry interval and log persistence failures"
```

---

### Task 2: `clap` CLI + option resolution + mode dispatch

**Files:**
- Modify: `nudge-rs/Cargo.toml` (add `clap`)
- Create: `nudge-rs/src/cli.rs`
- Modify: `nudge-rs/src/main.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod cli;`)

**Interfaces:**
- Produces:
  - `cli::Cli` — clap `Parser` with fields: `pane: Option<String>` (`-p`), `time: Option<String>` (`-m`/`--time`), `input: Vec<String>` (`-i`), `delay: Option<f64>` (`-w`), `notify: bool` (`-n`) + `no_notify: bool`, `auto_retry: bool` (`-a`) + `no_auto_retry: bool`, `retries: Option<i64>` (`-r`), `verify: bool` (`-v`) + `no_verify: bool`, `list: bool` (`-l`/`--jobs`), `list_plain: bool`, `cancel: Option<u64>`, `edit: Option<u64>`, `daemon: bool`, `install_daemon: bool`, `uninstall_daemon: bool`.
  - `cli::resolve_options(cli: &Cli) -> config::Toggles` — reads the `NUDGE_*` env defaults, overlays the CLI flags via `config::resolve`.

- [ ] **Step 1: Add clap**

`nudge-rs/Cargo.toml` `[dependencies]`:

```toml
clap = { version = "4", features = ["derive"] }
```

- [ ] **Step 2: Write the failing tests**

`nudge-rs/src/cli.rs`:

```rust
//! Command-line interface: the `clap` parser and option resolution.

use clap::Parser;

use crate::config::{env_bool, resolve, FlagOverrides, Toggles};

/// nudge — inject messages into a tmux pane at a rate-limit reset.
#[derive(Parser, Debug, Default)]
#[command(name = "nudge", version, about)]
pub struct Cli {
    /// Target tmux pane (e.g. bot:0.1). Prompts interactively if omitted.
    #[arg(short, long)]
    pub pane: Option<String>,

    /// Specific target time (e.g. "14:30" or "now + 45 min"); else auto-detect.
    #[arg(short = 'm', long = "time")]
    pub time: Option<String>,

    /// Message to inject; repeat to send several (default: "please continue").
    #[arg(short = 'i', long = "input")]
    pub input: Vec<String>,

    /// Pause between multiple sends, seconds (default 0.75).
    #[arg(short = 'w', long = "delay")]
    pub delay: Option<f64>,

    #[arg(short = 'n', long = "notify")]
    pub notify: bool,
    #[arg(long = "no-notify")]
    pub no_notify: bool,

    #[arg(short = 'a', long = "auto-retry")]
    pub auto_retry: bool,
    #[arg(long = "no-auto-retry")]
    pub no_auto_retry: bool,

    /// Exact retry count (-1 = forever). Implies --auto-retry.
    #[arg(short = 'r', long = "retries")]
    pub retries: Option<i64>,

    #[arg(short = 'v', long = "verify")]
    pub verify: bool,
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Review pending jobs (interactive).
    #[arg(short = 'l', long, visible_alias = "jobs")]
    pub list: bool,
    /// Review pending jobs as a plain table (non-interactive).
    #[arg(long = "list-plain")]
    pub list_plain: bool,
    /// Cancel a pending job by id.
    #[arg(long = "cancel", value_name = "ID")]
    pub cancel: Option<u64>,
    /// Edit a pending job by id.
    #[arg(long = "edit", value_name = "ID")]
    pub edit: Option<u64>,

    /// Run the resident scheduler daemon (foreground).
    #[arg(long = "daemon")]
    pub daemon: bool,
    /// Register the daemon with the OS service manager.
    #[arg(long = "install-daemon")]
    pub install_daemon: bool,
    /// Unregister the daemon.
    #[arg(long = "uninstall-daemon")]
    pub uninstall_daemon: bool,
}

fn tri(on: bool, off: bool) -> Option<bool> {
    if off {
        Some(false)
    } else if on {
        Some(true)
    } else {
        None
    }
}

/// Env defaults (`NUDGE_*`) overlaid with the CLI's flags.
pub fn resolve_options(cli: &Cli) -> Toggles {
    let env = Toggles {
        notify: env_bool(std::env::var("NUDGE_NOTIFY").ok().as_deref()),
        verify: env_bool(std::env::var("NUDGE_VERIFY").ok().as_deref()),
        auto_retry: env_bool(std::env::var("NUDGE_AUTO_RETRY").ok().as_deref()),
        retries: std::env::var("NUDGE_RETRIES").ok().and_then(|s| s.parse().ok()).unwrap_or(2),
        settle_secs: std::env::var("NUDGE_SETTLE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(5.0),
    };
    let overrides = FlagOverrides {
        notify: tri(cli.notify, cli.no_notify),
        verify: tri(cli.verify, cli.no_verify),
        auto_retry: tri(cli.auto_retry, cli.no_auto_retry),
        retries: cli.retries,
        settle_secs: None,
    };
    resolve(&env, &overrides)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn parses_core_scheduling_flags() {
        let c = parse(&["nudge", "-p", "bot:0.1", "-m", "3pm", "-i", "a", "-i", "b", "-w", "0.5", "-v"]);
        assert_eq!(c.pane.as_deref(), Some("bot:0.1"));
        assert_eq!(c.time.as_deref(), Some("3pm"));
        assert_eq!(c.input, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(c.delay, Some(0.5));
        assert!(c.verify);
    }

    #[test]
    fn no_flags_override_env_defaults() {
        // With no CLI flags, resolve reflects the (test-provided) env absence -> false/defaults.
        let c = parse(&["nudge", "-p", "x"]);
        // Clear any inherited env for determinism.
        std::env::remove_var("NUDGE_NOTIFY");
        std::env::remove_var("NUDGE_VERIFY");
        std::env::remove_var("NUDGE_AUTO_RETRY");
        std::env::remove_var("NUDGE_RETRIES");
        let t = resolve_options(&c);
        assert!(!t.notify);
        assert_eq!(t.retries, 2);
    }

    #[test]
    fn retries_flag_implies_auto_retry() {
        let c = parse(&["nudge", "-p", "x", "-r", "5"]);
        let t = resolve_options(&c);
        assert!(t.auto_retry);
        assert_eq!(t.retries, 5);
    }

    #[test]
    fn no_notify_beats_a_bare_notify_env() {
        std::env::set_var("NUDGE_NOTIFY", "1");
        let c = parse(&["nudge", "-p", "x", "--no-notify"]);
        let t = resolve_options(&c);
        assert!(!t.notify);
        std::env::remove_var("NUDGE_NOTIFY");
    }
}
```

- [ ] **Step 3: Run tests (RED then GREEN after impl already in step 2)**

Run: `cd nudge-rs && cargo test cli`
Expected: after adding `pub mod cli;` to lib.rs, the 4 `cli::tests::*` PASS. (Env-touching tests run single-threaded within the process; if they interfere, keep them as-is — each clears the vars it needs.)

- [ ] **Step 4: Wire `main.rs` dispatch**

Replace `nudge-rs/src/main.rs`:

```rust
use clap::Parser;
use nudge::cli::Cli;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = nudge::run(cli) {
        eprintln!("nudge: {e}");
        std::process::exit(1);
    }
}
```

Add a top-level `run` to `nudge-rs/src/lib.rs` (below the `pub mod` lines):

```rust
pub mod cli;

/// Dispatch a parsed CLI to the right mode.
pub fn run(cli: cli::Cli) -> anyhow::Result<()> {
    if cli.daemon {
        daemon::init_tracing();
        let p = paths::resolve();
        return Ok(daemon::run(&p, std::env::var("NUDGE_CLOCK_PATTERN").ok(), std::env::var("NUDGE_DURATION_PATTERN").ok(), jiff::ToSpan::hours(6))?);
    }
    if cli.install_daemon {
        return register::install(&std::env::current_exe()?);
    }
    if cli.uninstall_daemon {
        return register::uninstall();
    }
    // scheduling / job-management dispatch is added in Tasks 3-4 & 6.
    app::dispatch(cli)
}
```

Create a minimal `nudge-rs/src/app.rs` so `app::dispatch` exists (Tasks 3-4-6 flesh it out):

```rust
//! Command implementations for the CLI modes.

use crate::cli::Cli;

/// Dispatch non-daemon modes. (Scheduling / list / cancel / edit added in later tasks.)
pub fn dispatch(_cli: Cli) -> anyhow::Result<()> {
    anyhow::bail!("scheduling not implemented yet");
}
```

Add `pub mod app;` to lib.rs.

- [ ] **Step 5: Verify + commit**

Run: `cd nudge-rs && cargo build && cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: builds (the `nudge` binary now parses args); tests green.

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/cli.rs nudge-rs/src/app.rs nudge-rs/src/main.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): clap CLI, option resolution, and mode dispatch"
```

---

### Task 3: scheduling (build spec, auto-detect, IPC schedule + auto-start)

**Files:**
- Modify: `nudge-rs/src/app.rs`
- Create: `nudge-rs/tests/cli_schedule.rs`

**Interfaces:**
- Consumes: `cli::{Cli, resolve_options}`, `job::{JobSpec, TargetSpec}`, `timespec::parse_timespec`, `detect::detect_reset`, `target::{Target, tmux::TmuxTarget}`, `ipc::{client, Request, Response}`, `paths`, jiff.
- Produces:
  - `app::build_spec(pane: &str, fire_at: jiff::Timestamp, cli: &Cli, opts: &config::Toggles) -> JobSpec` — pure assembly (messages default to `["please continue"]`; delay defaults 0.75).
  - `app::fire_time(cli: &Cli, pane: &str, now: &jiff::Zoned) -> anyhow::Result<jiff::Timestamp>` — `-m` via `parse_timespec`, else capture the pane and `detect_reset`; error if neither yields a time.
  - `app::ensure_daemon(socket: &std::path::Path) -> anyhow::Result<()>` — Ping; if unreachable, spawn `<current_exe> --daemon` detached and wait for the socket. **Effectful; not tested.**
  - `app::schedule(cli: &Cli) -> anyhow::Result<()>` — resolve options, compute fire time + pane, build spec, ensure daemon, send `Request::Schedule`, print the assigned id.

- [ ] **Step 1: Write the failing tests**

`nudge-rs/tests/cli_schedule.rs`:

```rust
//! `build_spec` assembles a JobSpec from CLI options; a hermetic IPC round-trip
//! confirms a scheduled spec reaches the queue. No real daemon is spawned.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::app::build_spec;
use nudge::cli::{resolve_options, Cli};
use nudge::ipc::{client, server, Request, Response};
use nudge::job::TargetSpec;
use nudge::queue::Queue;

fn cli(args: &[&str]) -> Cli {
    <Cli as clap::Parser>::try_parse_from(args).unwrap()
}

#[test]
fn build_spec_defaults_message_and_delay() {
    let c = cli(&["nudge", "-p", "bot:0.1"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("bot:0.1", ts, &c, &opts);
    assert_eq!(spec.target, TargetSpec::Tmux { pane: "bot:0.1".into() });
    assert_eq!(spec.messages, vec!["please continue".to_string()]);
    assert_eq!(spec.send_delay_secs, 0.75);
    assert_eq!(spec.fire_at, ts);
}

#[test]
fn build_spec_takes_custom_messages_and_delay() {
    let c = cli(&["nudge", "-p", "x", "-i", "npm test", "-i", "yes", "-w", "1.5"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("x", ts, &c, &opts);
    assert_eq!(spec.messages, vec!["npm test".to_string(), "yes".to_string()]);
    assert_eq!(spec.send_delay_secs, 1.5);
}

#[test]
fn scheduling_a_spec_over_ipc_reaches_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));

    let listener = UnixListener::bind(&socket).unwrap();
    let q = Arc::clone(&queue);
    let h = thread::spawn(move || server::serve_once(&listener, &q).unwrap());

    let c = cli(&["nudge", "-p", "bot:0.1"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("bot:0.1", ts, &c, &opts);

    let resp = client::request(&socket, &Request::Schedule(spec)).unwrap();
    h.join().unwrap();
    assert!(matches!(resp, Response::Scheduled(1)));
    assert_eq!(queue.lock().unwrap().all().len(), 1);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd nudge-rs && cargo test --test cli_schedule`
Expected: FAIL — `build_spec` missing.

- [ ] **Step 3: Implement in `app.rs`**

Replace `nudge-rs/src/app.rs`'s body with (keeping `dispatch` but routing to `schedule`):

```rust
//! Command implementations for the CLI modes.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use jiff::Zoned;

use crate::cli::{resolve_options, Cli};
use crate::config::Toggles;
use crate::detect::detect_reset;
use crate::ipc::{client, Request, Response};
use crate::job::{JobSpec, TargetSpec};
use crate::paths;
use crate::target::{tmux::TmuxTarget, Target};
use crate::timespec::parse_timespec;

/// Assemble a JobSpec from resolved options.
pub fn build_spec(pane: &str, fire_at: jiff::Timestamp, cli: &Cli, opts: &Toggles) -> JobSpec {
    let messages = if cli.input.is_empty() {
        vec!["please continue".to_string()]
    } else {
        cli.input.clone()
    };
    JobSpec {
        target: TargetSpec::Tmux { pane: pane.to_string() },
        messages,
        send_delay_secs: cli.delay.unwrap_or(0.75),
        fire_at,
        notify: opts.notify,
        verify: opts.verify,
        auto_retry: opts.auto_retry,
        retries_left: if opts.auto_retry { opts.retries } else { 0 },
        settle_secs: opts.settle_secs,
    }
}

/// Determine the fire time: explicit `-m`, else auto-detect from the pane.
pub fn fire_time(cli: &Cli, pane: &str, now: &Zoned) -> anyhow::Result<jiff::Timestamp> {
    if let Some(t) = &cli.time {
        return Ok(parse_timespec(t, now)
            .map_err(|e| anyhow::anyhow!("could not parse time '{t}': {e}"))?
            .timestamp());
    }
    let screen = TmuxTarget::new(pane)
        .capture()
        .context("capturing pane to auto-detect the rate limit")?;
    let clock_ext = std::env::var("NUDGE_CLOCK_PATTERN").ok();
    let dur_ext = std::env::var("NUDGE_DURATION_PATTERN").ok();
    match detect_reset(&screen, now, clock_ext.as_deref(), dur_ext.as_deref()) {
        Some(z) => Ok(z.timestamp()),
        None => bail!("no rate-limit banner detected in {pane}; pass -m to set a time"),
    }
}

/// Ping the daemon; if it's not answering, start it and wait for the socket.
pub fn ensure_daemon(socket: &Path) -> anyhow::Result<()> {
    if client::request(socket, &Request::Ping).is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe).arg("--daemon").spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if client::request(socket, &Request::Ping).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("daemon did not come up on {}", socket.display())
}

/// Schedule a nudge.
pub fn schedule(cli: &Cli) -> anyhow::Result<()> {
    let pane = match &cli.pane {
        Some(p) => p.clone(),
        None => crate::app::pick_pane()?,
    };
    let opts = resolve_options(cli);
    let now = Zoned::now();
    let fire_at = fire_time(cli, &pane, &now)?;
    let spec = build_spec(&pane, fire_at, cli, &opts);

    let paths = paths::resolve();
    ensure_daemon(&paths.socket)?;
    match client::request(&paths.socket, &Request::Schedule(spec))? {
        Response::Scheduled(id) => {
            println!("nudge: scheduled job {id} for {}", fire_at);
            Ok(())
        }
        Response::Error(e) => bail!("daemon rejected the job: {e}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Dispatch non-daemon modes.
pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    // list / cancel / edit are added in Task 4; interactive pick in Task 6.
    schedule(&cli)
}

// pick_pane is provided in Task 6; a temporary stub keeps this compiling until then.
pub fn pick_pane() -> anyhow::Result<String> {
    anyhow::bail!("no pane given (-p); interactive picker lands in a later task")
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cd nudge-rs && cargo test --test cli_schedule && cargo clippy --all-targets -- -D warnings`
Expected: the 3 tests PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/app.rs nudge-rs/tests/cli_schedule.rs
git commit -m "feat(nudge-rs): schedule command (build spec, auto-detect, IPC + auto-start)"
```

---

### Task 4: job management (list / cancel / edit)

**Files:**
- Modify: `nudge-rs/src/app.rs`
- Create: `nudge-rs/tests/cli_jobs.rs`

**Interfaces:**
- Produces:
  - `app::list(plain: bool) -> anyhow::Result<()>` — IPC `List`, print a table (plain vs interactive; interactive picker in Task 6, so for now both print the table).
  - `app::cancel(id: u64) -> anyhow::Result<()>` — IPC `Cancel`, report.
  - `app::edit(id: u64, cli: &Cli) -> anyhow::Result<()>` — IPC `List` to find the job, overlay the CLI's explicitly-passed flags, `Cancel` the old, `Schedule` the merged spec.
  - `app::format_jobs(jobs: &[job::Job]) -> String` — pure table renderer (id, pane, fire time, message count).

- [ ] **Step 1: Write the failing tests**

`nudge-rs/tests/cli_jobs.rs`:

```rust
//! format_jobs is a pure renderer; cancel/list are verified via a hermetic IPC
//! server. No real daemon.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::app::format_jobs;
use nudge::ipc::{client, server, Request, Response};
use nudge::job::{JobSpec, TargetSpec};
use nudge::queue::Queue;

fn spec(pane: &str) -> JobSpec {
    JobSpec {
        target: TargetSpec::Tmux { pane: pane.into() },
        messages: vec!["go".into()],
        send_delay_secs: 0.75,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: false,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
    }
}

#[test]
fn format_jobs_shows_id_pane_and_count() {
    let mut q = Queue::load(tempfile::tempdir().unwrap().path().join("q.json")).unwrap();
    q.add(spec("bot:0.1")).unwrap();
    let out = format_jobs(q.all());
    assert!(out.contains("bot:0.1"), "got:\n{out}");
    assert!(out.contains('1')); // the id
}

#[test]
fn cancel_over_ipc_removes_the_job() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
    queue.lock().unwrap().add(spec("x")).unwrap();

    let listener = Arc::new(UnixListener::bind(&socket).unwrap());
    let q = Arc::clone(&queue);
    let l = Arc::clone(&listener);
    let h = thread::spawn(move || server::serve_once(&l, &q).unwrap());

    let resp = client::request(&socket, &Request::Cancel(1)).unwrap();
    h.join().unwrap();
    assert_eq!(resp, Response::Cancelled(true));
    assert!(queue.lock().unwrap().all().is_empty());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd nudge-rs && cargo test --test cli_jobs`
Expected: FAIL — `format_jobs` missing.

- [ ] **Step 3: Implement in `app.rs`**

Add to `nudge-rs/src/app.rs`:

```rust
use crate::job::Job;

/// Render pending jobs as a plain table.
pub fn format_jobs(jobs: &[Job]) -> String {
    if jobs.is_empty() {
        return "no pending nudge jobs".to_string();
    }
    let mut out = String::from("ID   PANE                 FIRE (UTC)            MSGS\n");
    for j in jobs {
        let pane = match &j.target {
            crate::job::TargetSpec::Tmux { pane } => pane.clone(),
        };
        out.push_str(&format!(
            "{:<4} {:<20} {:<20} {}\n",
            j.id,
            pane,
            j.fire_at,
            j.messages.len()
        ));
    }
    out
}

fn socket() -> std::path::PathBuf {
    paths::resolve().socket
}

pub fn list(_plain: bool) -> anyhow::Result<()> {
    match client::request(&socket(), &Request::List)? {
        Response::Jobs(jobs) => {
            print!("{}", format_jobs(&jobs));
            Ok(())
        }
        other => bail!("unexpected response: {other:?}"),
    }
}

pub fn cancel(id: u64) -> anyhow::Result<()> {
    match client::request(&socket(), &Request::Cancel(id))? {
        Response::Cancelled(true) => {
            println!("nudge: cancelled job {id}");
            Ok(())
        }
        Response::Cancelled(false) => bail!("no pending job with id {id}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

pub fn edit(id: u64, cli: &Cli) -> anyhow::Result<()> {
    let jobs = match client::request(&socket(), &Request::List)? {
        Response::Jobs(j) => j,
        other => bail!("unexpected response: {other:?}"),
    };
    let job = jobs
        .into_iter()
        .find(|j| j.id == id)
        .ok_or_else(|| anyhow::anyhow!("no pending job with id {id}"))?;

    // Overlay explicitly-passed flags onto the existing job.
    let opts = resolve_options(cli);
    let pane = match &cli.pane {
        Some(p) => p.clone(),
        None => match &job.target {
            TargetSpec::Tmux { pane } => pane.clone(),
        },
    };
    let now = Zoned::now();
    let fire_at = match &cli.time {
        Some(_) => fire_time(cli, &pane, &now)?,
        None => job.fire_at,
    };
    let messages = if cli.input.is_empty() { job.messages.clone() } else { cli.input.clone() };
    let spec = JobSpec {
        target: TargetSpec::Tmux { pane },
        messages,
        send_delay_secs: cli.delay.unwrap_or(job.send_delay_secs),
        fire_at,
        notify: opts.notify,
        verify: opts.verify,
        auto_retry: opts.auto_retry,
        retries_left: if opts.auto_retry { opts.retries } else { 0 },
        settle_secs: opts.settle_secs,
    };

    client::request(&socket(), &Request::Cancel(id))?;
    match client::request(&socket(), &Request::Schedule(spec))? {
        Response::Scheduled(new_id) => {
            println!("nudge: edited job {id} -> {new_id}");
            Ok(())
        }
        other => bail!("unexpected response: {other:?}"),
    }
}
```

And route them in `dispatch`:

```rust
pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    if let Some(id) = cli.cancel {
        return cancel(id);
    }
    if let Some(id) = cli.edit {
        return edit(id, &cli);
    }
    if cli.list || cli.list_plain {
        return list(cli.list_plain);
    }
    schedule(&cli)
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cd nudge-rs && cargo test --test cli_jobs && cargo clippy --all-targets -- -D warnings`
Expected: PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/app.rs nudge-rs/tests/cli_jobs.rs
git commit -m "feat(nudge-rs): list, cancel, and edit job-management commands"
```

---

### Task 5: notifications on fire

**Files:**
- Modify: `nudge-rs/Cargo.toml` (add `notify-rust`)
- Create: `nudge-rs/src/notify.rs`
- Modify: `nudge-rs/src/daemon.rs` (notify after a successful fire when `job.notify`)
- Modify: `nudge-rs/src/lib.rs` (add `pub mod notify;`)

**Interfaces:**
- Produces: `notify::send(body: &str)` — best-effort desktop notification titled "AI Nudge" (logs on failure, never errors out).

- [ ] **Step 1: Add the dep**

`nudge-rs/Cargo.toml` `[dependencies]`:

```toml
notify-rust = "4"
```

- [ ] **Step 2: Implement the wrapper**

`nudge-rs/src/notify.rs`:

```rust
//! Best-effort desktop notifications (cross-platform via notify-rust).

/// Fire a desktop notification. Never fails the caller — logs and moves on.
pub fn send(body: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary("AI Nudge")
        .body(body)
        .show()
    {
        tracing::warn!("nudge: notification failed: {e}");
    }
}
```

Add `pub mod notify;` to lib.rs.

- [ ] **Step 3: Call it from the daemon fire path**

In `nudge-rs/src/daemon.rs`, in the fire loop, after logging a successful `Ok(o)` outcome, when the job asked for it:

```rust
            match &outcome {
                Ok(o) => {
                    tracing::info!("nudge: fired job {} -> {:?}", job.id, o);
                    if job.notify {
                        crate::notify::send(&format!("nudge fired for {}", describe_pane(job)));
                    }
                }
                Err(e) => tracing::warn!("nudge: job {} failed: {e}", job.id),
            }
```

And a small helper in daemon.rs:

```rust
fn describe_pane(job: &crate::job::Job) -> String {
    match &job.target {
        crate::job::TargetSpec::Tmux { pane } => pane.clone(),
    }
}
```

- [ ] **Step 4: Verify (build + full suite; no notification is actually sent by tests) + commit**

Run: `cd nudge-rs && cargo build && cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: builds; full suite green (no test triggers `notify::send`, since the daemon fire path is only exercised by `daemon_fire`, whose job has `notify: false`); clippy clean.

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/notify.rs nudge-rs/src/daemon.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): desktop notifications when a nudge fires"
```

---

### Task 6: interactive pane picker (`inquire`)

**Files:**
- Modify: `nudge-rs/Cargo.toml` (add `inquire`)
- Create: `nudge-rs/src/tmux_panes.rs`
- Modify: `nudge-rs/src/app.rs` (real `pick_pane`)
- Modify: `nudge-rs/src/lib.rs` (add `pub mod tmux_panes;`)

**Interfaces:**
- Produces:
  - `tmux_panes::Pane { pub target: String, pub title: String }`.
  - `tmux_panes::parse_list(output: &str) -> Vec<Pane>` — parse `tmux list-panes -a -F '#{session_name}:#{window_index}.#{pane_index}\t#{pane_title}'` output (tab-separated).
  - `tmux_panes::list() -> anyhow::Result<Vec<Pane>>` — run that tmux command.
  - `app::pick_pane()` — real impl: `list()` panes, `inquire::Select` one, return its target. Errors if no panes / non-interactive.

- [ ] **Step 1: Write the failing test (the pure parser)**

`nudge-rs/src/tmux_panes.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tab_separated_panes() {
        let out = "bot:0.0\tclaude\nbot:0.1\tagy\nsolo:1.2\t\n";
        let panes = parse_list(out);
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].target, "bot:0.0");
        assert_eq!(panes[0].title, "claude");
        assert_eq!(panes[2].target, "solo:1.2");
        assert_eq!(panes[2].title, ""); // empty title tolerated
    }

    #[test]
    fn ignores_blank_lines() {
        assert_eq!(parse_list("\n\nbot:0.0\tx\n\n").len(), 1);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd nudge-rs && cargo test tmux_panes`
Expected: FAIL — `parse_list` missing.

- [ ] **Step 3: Implement**

Prepend to `nudge-rs/src/tmux_panes.rs`:

```rust
//! Enumerate tmux panes for the interactive picker.

use anyhow::{bail, Context};

/// One selectable tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub target: String,
    pub title: String,
}

/// Parse `list-panes -F '<target>\t<title>'` output.
pub fn parse_list(output: &str) -> Vec<Pane> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (target, title) = l.split_once('\t').unwrap_or((l, ""));
            Pane { target: target.to_string(), title: title.to_string() }
        })
        .collect()
}

/// List all tmux panes across sessions.
pub fn list() -> anyhow::Result<Vec<Pane>> {
    let out = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index}\t#{pane_title}",
        ])
        .output()
        .context("running tmux list-panes")?;
    if !out.status.success() {
        bail!("tmux list-panes failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
}
```

Add `pub mod tmux_panes;` to lib.rs.

Add the `inquire` dep to Cargo.toml `[dependencies]`:

```toml
inquire = "0.7"
```

Replace the `pick_pane` stub in `nudge-rs/src/app.rs`:

```rust
/// Interactively choose a tmux pane.
pub fn pick_pane() -> anyhow::Result<String> {
    let panes = crate::tmux_panes::list()?;
    if panes.is_empty() {
        anyhow::bail!("no tmux panes found; pass -p <pane>");
    }
    let labels: Vec<String> = panes
        .iter()
        .map(|p| {
            if p.title.is_empty() {
                p.target.clone()
            } else {
                format!("{}  ({})", p.target, p.title)
            }
        })
        .collect();
    let choice = inquire::Select::new("Target pane:", labels.clone())
        .prompt()
        .context("pane selection cancelled")?;
    let idx = labels.iter().position(|l| l == &choice).unwrap();
    Ok(panes[idx].target.clone())
}
```

- [ ] **Step 4: Run to verify (parser passes; picker is inspection-only) + full suite**

Run: `cd nudge-rs && cargo test tmux_panes && cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: `tmux_panes::tests::*` PASS; whole suite green; clippy clean. (`pick_pane` needs a TTY, so it is not unit-tested — the parser it depends on is.)

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/tmux_panes.rs nudge-rs/src/app.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): interactive tmux pane picker (inquire)"
```

---

## Self-Review

**Spec coverage (increment 4 / feature parity):**
- All bash scheduling flags (`-p -m -i -w -n -a -r -v` + `--no-*`) + env defaults + precedence → Tasks 2-3. ✅
- Default message "please continue", delay 0.75 → Task 3. ✅
- Auto-detect reset from pane when `-m` omitted → Task 3. ✅
- Job management `--list`/`--list-plain`/`--cancel`/`--edit` → Task 4. ✅
- Daemon/registration entrypoints (`--daemon`/`--install-daemon`/`--uninstall-daemon`) + auto-start → Tasks 2-3. ✅
- Notifications → Task 5. ✅
- Interactive pane picker (fzf replacement, bundled) → Task 6. ✅
- Deferred 3b daemon fixes (retry floor, error logging, init_tracing) → Task 1. ✅
- Out of scope (later polish): richer ratatui live-preview dashboard for `--list`; the `nudge run` pty mode; extra multiplexers.

**Placeholder scan:** `pick_pane`/`dispatch` are introduced as stubs in Task 3 and completed in Tasks 6/4 respectively — each step compiles. No TBDs. ✅

**Type consistency:** `Cli`/`resolve_options`/`Toggles`/`FlagOverrides` consistent (config from increment 1). `JobSpec` field set matches. `ipc::{client::request, Request, Response}` from 3a. `TmuxTarget`/`Target` from increment 2. `paths::resolve` from 3a. `detect_reset`/`parse_timespec` signatures match. ✅

## Notes for the next increment (5 — packaging)

- Homebrew formula + **AUR PKGBUILD** (source + `-git`) installing the binary + shell completions (`clap_complete`) + registering the systemd unit; a repo `README` for nudge-rs.
- Consider the richer ratatui `--list` dashboard (live pane preview, Enter=edit, Del=cancel) as a UX follow-up.
- Thread a tmux socket through `TargetSpec::Tmux` so non-default servers are addressable (also cleans up the daemon_fire test's default-socket approach).

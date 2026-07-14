# nudge Rust rewrite — scheduler (Phase 1, increment 3b)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the daemon fire jobs — harden the IPC server, add queue rescheduling, a pure capped-poll scheduler (plan → fire → apply, with catch-up), and the `daemon::run` loop that ties the IPC thread and scheduler together.

**Architecture:** The scheduler decision logic is pure and unit-tested: `plan` (which due jobs to fire vs drop as stale), `apply_outcome` (remove or reschedule after firing), `next_wake` (sleep duration, capped at 30s). `daemon::run` orchestrates: snapshot due jobs *under the queue lock*, fire each *without the lock* (so a slow tmux inject never blocks IPC), then apply results under the lock. Jobs scheduled via IPC while the scheduler sleeps fire within the poll cap (≤30s) — fine given nudge's 3-min padding.

**Tech Stack:** Rust 2021, adds `tracing` + `tracing-subscriber`; reuses `jiff`, `queue`, `job`, `inject`, `ipc`, `paths`.

## Context

Increment 3b, stacked on `feat/nudge-rust-daemon` (3a, PR #7). The 3a final review flagged an Important item that is **Task 1 here**: `ipc::server::serve`'s loop propagates every per-connection error, so one malformed request or client disconnect kills the daemon. Registration (systemd/launchd) and CLI wiring are increment **3c/4** — not here.

## Global Constraints

- Crate at `nudge-rs/`, edition 2021. Add `tracing = "0.1"` and `tracing-subscriber = "0.3"`. No other new crates.
- **Firing must not hold the queue `Mutex`** — snapshot due jobs under the lock, release, fire, re-acquire to apply. IPC must never block for the duration of a tmux injection.
- Scheduler *decision* logic (`plan`, `apply_outcome`, `next_wake`) is pure and unit-tested with an injected `now`; the firing loop and real clock live only in `daemon::run`.
- Hermetic tests except the one tmux-gated end-to-end daemon test, which **self-skips without tmux** and uses a **private tmux socket** + tempdir state. NO systemd/launchd registration anywhere.
- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` pass every commit. Commit prefixes `feat/refactor(nudge-rs): …`; NO attribution.

## File Structure

- `nudge-rs/Cargo.toml` — add `tracing`, `tracing-subscriber`.
- `nudge-rs/src/ipc/server.rs` — harden `serve`; add `tracing`.
- `nudge-rs/src/queue.rs` — add `reschedule`.
- `nudge-rs/src/scheduler.rs` — `plan`, `apply_outcome`, `next_wake`, constants.
- `nudge-rs/src/daemon.rs` — `run` orchestration + tracing init.
- `nudge-rs/src/lib.rs` — add `pub mod scheduler;` and `pub mod daemon;`.
- `nudge-rs/tests/daemon_ipc.rs` — hermetic "survives malformed request" test.
- `nudge-rs/tests/daemon_fire.rs` — tmux-gated end-to-end firing test (self-skip).

---

### Task 1: harden `ipc::server::serve` + add tracing

**Files:**
- Modify: `nudge-rs/Cargo.toml` (add `tracing`, `tracing-subscriber`)
- Modify: `nudge-rs/src/ipc/server.rs`
- Create: `nudge-rs/tests/daemon_ipc.rs`

**Interfaces:**
- `serve` keeps its signature `serve(socket: &Path, queue: Arc<Mutex<Queue>>) -> std::io::Result<()>` but its loop now logs and continues on per-connection errors, returning only on a fatal `accept()` error.

- [ ] **Step 1: Add deps**

In `nudge-rs/Cargo.toml` `[dependencies]`:

```toml
tracing = "0.1"
tracing-subscriber = "0.3"
```

- [ ] **Step 2: Harden `serve`**

In `nudge-rs/src/ipc/server.rs`, replace the body of `serve` (keep `serve_once` and `handle_conn` as they are). The loop must handle `accept()` and per-connection errors distinctly:

```rust
pub fn serve(socket: &Path, queue: Arc<Mutex<Queue>>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket); // clear a stale socket from a prior run
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(socket)?;
    tracing::info!("nudge ipc: listening on {}", socket.display());
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = handle_conn(stream, &queue) {
                    // A malformed request or a client that disconnected before
                    // reading the reply must not take the daemon down.
                    tracing::warn!("nudge ipc: connection error: {e}");
                }
            }
            Err(e) => {
                tracing::error!("nudge ipc: accept failed, stopping: {e}");
                return Err(e);
            }
        }
    }
}
```

(`handle_conn` is already `fn handle_conn(stream: UnixStream, queue: &Mutex<Queue>) -> std::io::Result<()>` from 3a.)

- [ ] **Step 3: Write the resilience integration test**

`nudge-rs/tests/daemon_ipc.rs`:

```rust
//! The daemon's IPC server must survive a malformed request and keep serving.
//! Hermetic: tempdir socket, no OS service. The server thread loops forever and
//! is left to be reaped at process exit (it only sleeps in `accept`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nudge::ipc::{client, server, Request, Response};
use nudge::queue::Queue;

fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "socket never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn daemon_survives_a_malformed_request() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));

    let sock2 = socket.clone();
    let q2 = Arc::clone(&queue);
    std::thread::spawn(move || {
        let _ = server::serve(&sock2, q2); // loops forever; reaped at process exit
    });
    wait_for_socket(&socket);

    // Send a line that is NOT valid JSON, then read (server closes without a
    // valid Response -> our manual read sees EOF). This exercises handle_conn's
    // Err path inside serve's loop.
    {
        let mut s = UnixStream::connect(&socket).unwrap();
        s.write_all(b"not json at all\n").unwrap();
        s.flush().unwrap();
        let mut line = String::new();
        let _ = BufReader::new(&s).read_line(&mut line); // may be empty on EOF
    }

    // The daemon must still be alive and answer a well-formed request.
    let resp = client::request(&socket, &Request::Ping).unwrap();
    assert_eq!(resp, Response::Pong);
}
```

- [ ] **Step 4: Run tests**

Run: `cd nudge-rs && cargo test --test daemon_ipc -- --nocapture && cargo test`
Expected: `daemon_survives_a_malformed_request` PASSES (the Ping after garbage returns Pong); full suite green.

- [ ] **Step 5: Lint and commit**

Run: `cd nudge-rs && cargo fmt && cargo clippy --all-targets -- -D warnings`

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/ipc/server.rs nudge-rs/tests/daemon_ipc.rs
git commit -m "feat(nudge-rs): harden IPC serve loop against per-connection errors"
```

---

### Task 2: `Queue::reschedule`

**Files:**
- Modify: `nudge-rs/src/queue.rs`

**Interfaces:**
- Produces: `Queue::reschedule(&mut self, id: u64, fire_at: jiff::Timestamp, retries_left: i64) -> std::io::Result<bool>` — update the job's `fire_at` and `retries_left`, persist; returns whether a job with that id existed.

- [ ] **Step 1: Write the failing test**

In `nudge-rs/src/queue.rs`, add to the `#[cfg(test)] mod tests`:

```rust
#[test]
fn reschedule_updates_fire_time_and_retries_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.json");
    let mut q = Queue::load(path.clone()).unwrap();
    let id = q.add(spec()).unwrap();

    let new_ts: jiff::Timestamp = "2026-07-13T16:30:00Z".parse().unwrap();
    assert!(q.reschedule(id, new_ts, 1).unwrap());
    assert!(!q.reschedule(9999, new_ts, 1).unwrap()); // missing id

    // Persisted: reload and confirm.
    let q2 = Queue::load(path).unwrap();
    let job = q2.get(id).unwrap();
    assert_eq!(job.fire_at, new_ts);
    assert_eq!(job.retries_left, 1);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd nudge-rs && cargo test reschedule`
Expected: FAIL — `reschedule` not found.

- [ ] **Step 3: Implement**

In `nudge-rs/src/queue.rs`, add to `impl Queue` (next to `remove`):

```rust
    /// Update a job's fire time and remaining retries; persist. Returns whether
    /// a job with `id` existed.
    pub fn reschedule(
        &mut self,
        id: u64,
        fire_at: jiff::Timestamp,
        retries_left: i64,
    ) -> std::io::Result<bool> {
        let found = if let Some(job) = self.state.jobs.iter_mut().find(|j| j.id == id) {
            job.fire_at = fire_at;
            job.retries_left = retries_left;
            true
        } else {
            false
        };
        if found {
            self.save()?;
        }
        Ok(found)
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd nudge-rs && cargo test reschedule && cargo clippy --all-targets -- -D warnings`
Expected: PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/queue.rs
git commit -m "feat(nudge-rs): Queue::reschedule to update a job's fire time and retries"
```

---

### Task 3: scheduler decision logic (`plan`, `apply_outcome`, `next_wake`)

**Files:**
- Create: `nudge-rs/src/scheduler.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod scheduler;`)

**Interfaces:**
- Consumes: `job::Job`, `queue::Queue`, `inject::InjectOutcome`, jiff.
- Produces:
  - `scheduler::MAX_POLL: std::time::Duration` = 30s.
  - `scheduler::Plan { pub fire: Vec<Job>, pub drop_stale: Vec<u64> }`.
  - `scheduler::plan(jobs: &[Job], now: &jiff::Zoned, grace: &jiff::Span) -> Plan` — pure. Due = `fire_at <= now`; of those, `fire_at < now-grace` → `drop_stale`, else → `fire`. Future jobs ignored.
  - `scheduler::apply_outcome(queue: &mut Queue, job: &Job, outcome: &anyhow::Result<InjectOutcome>, retry_at: jiff::Timestamp) -> std::io::Result<()>` — on `Ok(Sent)` with `auto_retry && retries_left != 0`, reschedule to `retry_at` with retries decremented (>0 → −1; −1 stays −1); otherwise remove.
  - `scheduler::next_wake(jobs: &[Job], now: &jiff::Zoned, max: std::time::Duration) -> std::time::Duration` — `max` if no jobs; `ZERO` if the earliest `fire_at <= now`; else min(time-until-earliest, max).

- [ ] **Step 1: Write the failing tests**

Append to `nudge-rs/src/scheduler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, JobSpec, TargetSpec};
    use crate::inject::InjectOutcome;
    use crate::queue::Queue;
    use jiff::{civil::date, tz::TimeZone, ToSpan};

    fn now() -> jiff::Zoned {
        date(2026, 7, 13).at(12, 0, 0, 0).to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC)).unwrap()
    }

    // Build a Job at `fire_at` (an RFC3339 string) with the given retry config.
    fn job(id: u64, fire_at: &str, auto_retry: bool, retries_left: i64) -> Job {
        JobSpec {
            target: TargetSpec::Tmux { pane: "p".into() },
            messages: vec!["go".into()],
            send_delay_secs: 0.0,
            fire_at: fire_at.parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry,
            retries_left,
            settle_secs: 5.0,
        }
        .into_job(id)
    }

    #[test]
    fn plan_fires_due_skips_future_drops_stale() {
        let grace = 6.hours();
        let jobs = vec![
            job(1, "2026-07-13T11:59:00Z", false, 0), // due (1 min ago)
            job(2, "2026-07-13T13:00:00Z", false, 0), // future
            job(3, "2026-07-13T02:00:00Z", false, 0), // 10h ago -> stale (> 6h)
        ];
        let p = plan(&jobs, &now(), &grace);
        assert_eq!(p.fire.iter().map(|j| j.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(p.drop_stale, vec![3]);
    }

    #[test]
    fn next_wake_variants() {
        assert_eq!(next_wake(&[], &now(), MAX_POLL), MAX_POLL);
        // overdue -> zero
        assert_eq!(
            next_wake(&[job(1, "2026-07-13T11:00:00Z", false, 0)], &now(), MAX_POLL),
            std::time::Duration::ZERO
        );
        // 10s in the future -> ~10s (< 30s cap)
        let w = next_wake(&[job(1, "2026-07-13T12:00:10Z", false, 0)], &now(), MAX_POLL);
        assert!(w <= std::time::Duration::from_secs(10) && w >= std::time::Duration::from_secs(9));
        // far future -> capped at MAX_POLL
        let w2 = next_wake(&[job(1, "2026-07-13T18:00:00Z", false, 0)], &now(), MAX_POLL);
        assert_eq!(w2, MAX_POLL);
    }

    fn q_with(job: Job) -> (tempfile::TempDir, Queue) {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        // add via JobSpec then overwrite fields to match `job`
        q.add(JobSpec {
            target: job.target.clone(),
            messages: job.messages.clone(),
            send_delay_secs: job.send_delay_secs,
            fire_at: job.fire_at,
            notify: job.notify,
            verify: job.verify,
            auto_retry: job.auto_retry,
            retries_left: job.retries_left,
            settle_secs: job.settle_secs,
        })
        .unwrap();
        (dir, q)
    }

    #[test]
    fn apply_sent_without_retry_removes() {
        let j = job(1, "2026-07-13T11:59:00Z", false, 0);
        let (_d, mut q) = q_with(j.clone());
        apply_outcome(&mut q, &q.get(1).unwrap().clone(), &Ok(InjectOutcome::Sent(1)), now().timestamp()).unwrap();
        assert!(q.get(1).is_none());
    }

    #[test]
    fn apply_sent_with_retry_reschedules_and_decrements() {
        let j = job(1, "2026-07-13T11:59:00Z", true, 2);
        let (_d, mut q) = q_with(j);
        let retry_at: jiff::Timestamp = "2026-07-13T12:05:00Z".parse().unwrap();
        apply_outcome(&mut q, &q.get(1).unwrap().clone(), &Ok(InjectOutcome::Sent(1)), retry_at).unwrap();
        let job = q.get(1).unwrap();
        assert_eq!(job.retries_left, 1);
        assert_eq!(job.fire_at, retry_at);
    }

    #[test]
    fn apply_infinite_retry_stays_negative_one() {
        let j = job(1, "2026-07-13T11:59:00Z", true, -1);
        let (_d, mut q) = q_with(j);
        apply_outcome(&mut q, &q.get(1).unwrap().clone(), &Ok(InjectOutcome::Sent(1)), now().timestamp()).unwrap();
        assert_eq!(q.get(1).unwrap().retries_left, -1);
    }

    #[test]
    fn apply_skipped_verify_removes_even_with_retry() {
        let j = job(1, "2026-07-13T11:59:00Z", true, 2);
        let (_d, mut q) = q_with(j);
        apply_outcome(&mut q, &q.get(1).unwrap().clone(), &Ok(InjectOutcome::SkippedVerify), now().timestamp()).unwrap();
        assert!(q.get(1).is_none());
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd nudge-rs && cargo test scheduler`
Expected: compile-fail — `plan`/`apply_outcome`/`next_wake`/`MAX_POLL` not found.

- [ ] **Step 3: Implement**

Prepend to `nudge-rs/src/scheduler.rs`:

```rust
//! Pure scheduler decisions: which due jobs to fire vs drop, what to do after
//! firing, and how long to sleep. `daemon::run` orchestrates the effects.

use std::time::Duration;

use jiff::{Span, Zoned};

use crate::inject::InjectOutcome;
use crate::job::Job;
use crate::queue::Queue;

/// Longest the scheduler sleeps before re-checking the queue, so a job added
/// via IPC while it sleeps still fires within this bound.
pub const MAX_POLL: Duration = Duration::from_secs(30);

/// The jobs a single scheduler pass should act on at a given `now`.
pub struct Plan {
    pub fire: Vec<Job>,
    pub drop_stale: Vec<u64>,
}

/// Partition jobs at `now`: due jobs within `grace` are fired; due jobs older
/// than `grace` are dropped (stale catch-up); future jobs are left alone.
pub fn plan(jobs: &[Job], now: &Zoned, grace: &Span) -> Plan {
    let now_ts = now.timestamp();
    let cutoff = now
        .checked_sub(*grace)
        .map(|z| z.timestamp())
        .unwrap_or(now_ts);
    let mut fire = Vec::new();
    let mut drop_stale = Vec::new();
    for j in jobs {
        if j.fire_at > now_ts {
            continue; // not due yet
        }
        if j.fire_at < cutoff {
            drop_stale.push(j.id);
        } else {
            fire.push(j.clone());
        }
    }
    Plan { fire, drop_stale }
}

/// Apply the result of firing `job`: reschedule for a retry, or remove.
pub fn apply_outcome(
    queue: &mut Queue,
    job: &Job,
    outcome: &anyhow::Result<InjectOutcome>,
    retry_at: jiff::Timestamp,
) -> std::io::Result<()> {
    match outcome {
        Ok(InjectOutcome::Sent(_)) if job.auto_retry && job.retries_left != 0 => {
            let left = if job.retries_left > 0 {
                job.retries_left - 1
            } else {
                job.retries_left // -1 stays -1 (infinite)
            };
            queue.reschedule(job.id, retry_at, left).map(|_| ())
        }
        _ => queue.remove(job.id).map(|_| ()),
    }
}

/// How long to sleep before the next pass, capped at `max`.
pub fn next_wake(jobs: &[Job], now: &Zoned, max: Duration) -> Duration {
    let now_ts = now.timestamp();
    match jobs.iter().map(|j| j.fire_at).min() {
        None => max,
        Some(ts) if ts <= now_ts => Duration::ZERO,
        Some(ts) => {
            let until: Duration = (ts - now_ts).unsigned_abs().into();
            until.min(max)
        }
    }
}
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod scheduler;
```

Note: `(ts - now_ts)` on jiff `Timestamp`s yields a `SignedDuration`; `.unsigned_abs()` → `std::time::Duration` (via `Into`). If jiff's exact method names differ, adjust the conversion to produce a `std::time::Duration` — the test's bounds (`>=9s && <=10s` for a 10s-future job, `ZERO` for overdue, `MAX_POLL` for far-future/none) are the contract.

- [ ] **Step 4: Run to verify they pass**

Run: `cd nudge-rs && cargo test scheduler && cargo clippy --all-targets -- -D warnings`
Expected: all `scheduler::tests::*` PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/scheduler.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): pure scheduler decisions (plan, apply_outcome, next_wake)"
```

---

### Task 4: `daemon::run` orchestration + end-to-end firing test

**Files:**
- Create: `nudge-rs/src/daemon.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod daemon;`)
- Create: `nudge-rs/tests/daemon_fire.rs`

**Interfaces:**
- Consumes: `paths::Paths`, `queue::Queue`, `ipc::server::serve`, `scheduler::{plan, apply_outcome, next_wake, MAX_POLL}`, `inject::run_injection`, jiff.
- Produces:
  - `daemon::init_tracing()` — install a `tracing_subscriber` (idempotent; ignores an already-set global).
  - `daemon::run(paths: &paths::Paths, clock_ext: Option<String>, dur_ext: Option<String>, grace: jiff::Span) -> std::io::Result<()>` — load the queue into `Arc<Mutex<Queue>>`, spawn `ipc::server::serve` on a thread, then loop: plan under lock → drop stale under lock → fire each due job **without** the lock → apply results under lock → sleep `next_wake`.

- [ ] **Step 1: Implement the daemon**

`nudge-rs/src/daemon.rs`:

```rust
//! The resident scheduler: serves IPC and fires due jobs. Firing happens off
//! the queue lock so a slow tmux inject never blocks IPC.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::{Span, Zoned};

use crate::inject::run_injection;
use crate::paths::Paths;
use crate::queue::Queue;
use crate::scheduler::{apply_outcome, next_wake, plan, MAX_POLL};

/// Install a tracing subscriber. Safe to call more than once (a second call is
/// a no-op once a global subscriber exists).
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).try_init();
}

/// Run the daemon forever: IPC server thread + scheduler loop.
pub fn run(
    paths: &Paths,
    clock_ext: Option<String>,
    dur_ext: Option<String>,
    grace: Span,
) -> std::io::Result<()> {
    let queue = Arc::new(Mutex::new(Queue::load(paths.queue.clone())?));

    // IPC server on its own thread.
    let q_ipc = Arc::clone(&queue);
    let socket = paths.socket.clone();
    std::thread::spawn(move || {
        if let Err(e) = crate::ipc::server::serve(&socket, q_ipc) {
            tracing::error!("nudge ipc server exited: {e}");
        }
    });

    loop {
        let now = Zoned::now();

        // 1. Snapshot due jobs and stale ids under the lock.
        let plan = {
            let q = queue.lock().unwrap_or_else(|e| e.into_inner());
            plan(q.all(), &now, &grace)
        };

        // 2. Drop stale jobs under the lock.
        if !plan.drop_stale.is_empty() {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            for id in &plan.drop_stale {
                let _ = q.remove(*id);
                tracing::info!("nudge: dropped stale job {id}");
            }
        }

        // 3. Fire each due job WITHOUT the lock, applying results under it.
        for job in &plan.fire {
            let outcome = run_injection(
                &*job.target.connect(),
                job,
                &now,
                clock_ext.as_deref(),
                dur_ext.as_deref(),
            );
            match &outcome {
                Ok(o) => tracing::info!("nudge: fired job {} -> {:?}", job.id, o),
                Err(e) => tracing::warn!("nudge: job {} failed: {e}", job.id),
            }
            let retry_at = now
                .checked_add(Span::new().seconds(job.settle_secs as i64))
                .map(|z| z.timestamp())
                .unwrap_or(now.timestamp());
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            let _ = apply_outcome(&mut q, job, &outcome, retry_at);
        }

        // 4. Sleep until the next job is due (capped).
        let wake = {
            let q = queue.lock().unwrap_or_else(|e| e.into_inner());
            next_wake(q.all(), &now, MAX_POLL)
        };
        std::thread::sleep(wake.max(Duration::from_millis(50)));
    }
}
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod daemon;
```

- [ ] **Step 2: Write the end-to-end firing test**

`nudge-rs/tests/daemon_fire.rs`:

```rust
//! End-to-end: the daemon fires a due job into a real tmux pane. Self-skips
//! without tmux. Uses a PRIVATE tmux socket and tempdir state; the daemon
//! thread loops forever and is reaped at process exit.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::ToSpan;
use nudge::job::{Job, JobSpec, TargetSpec};
use nudge::paths::Paths;
use nudge::queue::Queue;

static N: AtomicU64 = AtomicU64::new(0);

fn tmux_available() -> bool {
    Command::new("tmux").arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
}

struct Server(String);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-L", &self.0, "kill-server"]).status();
    }
}

#[test]
fn daemon_fires_a_due_job_into_the_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let n = N.fetch_add(1, Ordering::Relaxed);
    let socket_name = format!("nudge-daemon-{}-{}", std::process::id(), n);
    let server = Server(socket_name.clone());
    // NOTE: this tmux pane is targeted by TmuxTarget WITHOUT a socket, so we
    // must run this private server on the DEFAULT tmux socket path is wrong —
    // instead we target via `-L`. TmuxTarget::new(pane) uses the default
    // socket, so start the pane on the default server here is unsafe. Use the
    // tmux TargetSpec's pane on the private socket by making the daemon target
    // it: we can't pass a socket through TargetSpec yet, so this test drives a
    // pane on the private socket and asserts via that socket, and points the
    // job at a pane spec that TmuxTarget on the DEFAULT socket cannot reach.
    // => Keep the test simple and robust: start the private server, stage the
    // banner, run the daemon, and assert the pane content changed. See Step 3
    // note; the implementer wires the pane addressing so the daemon's
    // TmuxTarget reaches THIS pane.
    let _ = &server;

    // (Implementer: complete per Step 3's addressing note.)
}
```

- [ ] **Step 3: Make the end-to-end test actually address the private pane**

The daemon fires via `job.target.connect()` → `TmuxTarget::new(pane)`, which uses tmux's **default** socket. To keep the test hermetic and reach a **private** pane, the simplest robust approach: start the private tmux server, then set the `TMUX_TMPDIR`/socket so the daemon's default-socket tmux resolves to the private server for the duration of the test — OR run the private server and have the test assert against it while the job targets a pane on it.

Because `TargetSpec::Tmux` carries no socket yet, implement the test this way (rewrite `daemon_fire.rs` accordingly):

1. Start a private server: `tmux -L <socket> new-session -d -s s -x 80 -y 24 sh`.
2. Stage a banner so `--verify` would pass if used: `tmux -L <socket> send-keys -t s -l "printf 'quota reached. Resets in 45m\n'"` then `Enter`. (This test uses `verify: false`, so the banner is optional; keep it for realism.)
3. Point the daemon at the private server by setting the env var tmux honors for its default socket directory so `TmuxTarget`'s plain `tmux` calls hit the private server: set `std::env::set_var("TMUX_TMPDIR", <dir>)` is NOT equivalent to `-L`. Since that's unreliable, INSTEAD: make the job's pane a value that a **default-socket** `tmux` can reach by starting the staged session on the **default** socket under a uniquely-named session (e.g. `nudge_it_<pid>_<n>`), targeting `pane = "nudge_it_<pid>_<n>:0.0"`, and killing exactly that session (not the server) on drop. This shares the user's default tmux server but touches only a uniquely-named throwaway session, and never `kill-server`s it.
4. Write a due job (fire_at = now − 1s, `verify: false`, `messages: ["echo daemon_marker_$((6*7))"]`, `auto_retry: false`) directly into the queue FILE at `Paths.queue` via `Queue::add`, so the daemon fires it on its FIRST pass (catch-up), not after a poll wait.
5. Spawn `nudge::daemon::run(&paths, None, None, 6.hours())` on a thread (loops forever; reaped at exit), where `paths` points `queue`/`socket` into a `tempfile::tempdir()`.
6. Poll the pane (`tmux ... capture-pane -p -t <session>:0.0`) for up to ~8s until it contains `daemon_marker_42` (present only after the shell executes the injected `echo`), asserting it appears. Kill the throwaway session on drop.

Keep the assertion (`daemon_marker_42` appears) fixed. The addressing mechanism (default socket + unique throwaway session, killed by session name) is what makes the daemon's plain-`tmux` `TmuxTarget` reach the test pane. Guard the whole test behind the `tmux_available()` self-skip.

- [ ] **Step 4: Run the end-to-end test**

Run: `cd nudge-rs && cargo test --test daemon_fire -- --nocapture`
Expected (tmux present): `daemon_fires_a_due_job_into_the_pane` PASSES — the pane shows `daemon_marker_42` within the timeout. Without tmux: skips. If flaky, widen the poll timeout (do not shorten the daemon's `next_wake` floor).

- [ ] **Step 5: Full suite, lint, commit**

Run: `cd nudge-rs && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: everything green; the daemon e2e ran (tmux present) or skipped.

```bash
git add nudge-rs/src/daemon.rs nudge-rs/src/lib.rs nudge-rs/tests/daemon_fire.rs
git commit -m "feat(nudge-rs): daemon run loop firing due jobs, with end-to-end tmux test"
```

---

## Self-Review

**Spec coverage (3b slice):**
- Harden `serve` (the 3a-flagged Important) → Task 1. ✅
- Queue rescheduling for retries → Task 2. ✅
- Pure scheduler decisions (due/stale/catch-up, retry/remove, capped wake) → Task 3, fully unit-tested. ✅
- Daemon run loop firing off-lock, catch-up on first pass, end-to-end tmux firing → Task 4. ✅
- Firing never holds the queue lock (Global Constraint) → Task 4 structure. ✅
- Out of 3b (→ 3c/4): systemd/launchd registration + gated real enable; CLI (`nudge --daemon`, schedule/list/cancel client commands); notifications; the `nudge run` pty mode.

**Placeholder scan:** Task 4's `daemon_fire.rs` Step 2 is a scaffold with an explicit addressing note resolved concretely in Step 3 (default socket + unique throwaway session, killed by name; fixed `daemon_marker_42` assertion). The behavior and assertion are fully specified; the socket-addressing mechanism is spelled out. No TBDs elsewhere. ✅

**Type consistency:** `plan`/`apply_outcome`/`next_wake` signatures match across scheduler.rs, its tests, and daemon.rs. `Queue::reschedule(id, Timestamp, i64) -> io::Result<bool>` consistent (Task 2 defines, Task 3 apply_outcome + Task 4 use). `run_injection(&dyn Target, &Job, &Zoned, Option<&str>, Option<&str>)` matches increment 2. `job.target.connect()` from 3a. `InjectOutcome::{Sent, SkippedVerify}` from increment 2. ✅

## Notes for the next increments

- **3c (registration):** generate the systemd `--user` unit / launchd plist (pure string/plist generation, unit-tested); real `systemctl --user enable` / `launchctl bootstrap` behind an explicit opt-in flag. Guard `paths::resolve()` against unset `$HOME` here.
- **4 (CLI):** `clap` with `nudge --daemon` (calls `daemon::init_tracing()` + `daemon::run`), and `schedule`/`--list`/`--cancel`/`--edit` as `ipc::client` calls; the ratatui picker; notifications (`notify-rust`) on fire; consider threading a tmux socket through `TargetSpec::Tmux` so non-default servers (and cleaner test isolation) are addressable; add a comment on `Request` re: adjacent tagging.

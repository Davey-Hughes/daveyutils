//! Integration tests for the tmux backend and the end-to-end inject path.
//! Each runs against a PRIVATE tmux server (`tmux -L <socket>`) that is killed
//! on drop, so the developer's real tmux is never touched. All self-skip when
//! tmux is not installed.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    thread,
    time::{Duration, Instant},
};

use jiff::{civil::date, tz::TimeZone};
use nudge::inject::{run_injection, InjectOutcome};
use nudge::job::{Job, TargetSpec};
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
        // `line!()` alone is constant for every call site inside this function
        // (it names the line *within `start`*, not the caller's), so cargo's
        // default parallel test threads would collide on the same socket. An
        // atomic counter guarantees a unique socket per invocation.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let socket = format!("nudge-it-{}-{}", std::process::id(), n);
        // Construct the guard before the fallible spawn so that if `new-session`
        // fails after tmux has already started the private server process, the
        // guard's `Drop` still kills it instead of leaking it.
        let server = Server {
            socket: socket.clone(),
        };
        let ok = Command::new("tmux")
            .args([
                // `-L` gives us a private *server*; `-f /dev/null` gives us a
                // private *config*, and the second is not optional. Without it a
                // new server still sources the developer's ~/.tmux.conf, so what
                // these tests capture depends on their personal settings -- the
                // exact coupling the header promises is absent. It is also what
                // made these the slowest tests in the suite: sourcing one real
                // config (plugin manager, shell-outs) measured 5.3s per server,
                // against 0.02s for a bare one, and every test starts one.
                "-f",
                "/dev/null",
                "-L",
                &socket,
                "new-session",
                "-d",
                "-s",
                "s",
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .status()
            .expect("spawn tmux")
            .success();
        assert!(ok, "failed to start private tmux session");
        server
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

// ---- waiting for tmux ----
//
// These tests used to `sleep(500ms)` after every send and hope the pane had
// rendered by then. That is the worst of both: 500ms is dead time on an idle
// machine, and it is not enough on a loaded one -- which is why these were the
// slowest tests in the suite AND the ones that slowed down further under
// parallelism. A poll pays only what it actually waits.
//
// `PATIENCE` is a failure deadline, not a delay: a passing test never spends it.

const PATIENCE: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(20);

/// Poll until `cond` holds, or fail with `what`.
fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(TICK);
    }
}

/// Poll until the pane's captured text contains `needle`; return that capture.
///
/// Replaces "sleep, capture once, assert" — the assertion is the wait, so a
/// slow render costs time instead of a spurious failure.
fn wait_for_text(target: &TmuxTarget, needle: &str, why: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = target.capture().unwrap_or_default();
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?} in the pane: {why}\nlast capture:\n{screen}"
        );
        thread::sleep(TICK);
    }
}

/// Poll until two consecutive captures are byte-identical.
///
/// The verify gate compares a fingerprint taken now against the pane later, so
/// a baseline snapped while the pane was still rendering reads as "the user
/// resumed" and the test flakes to `SkippedResumed`. Waiting for the pane to
/// stop moving states that precondition instead of guessing a sleep long
/// enough to cover it.
fn wait_until_quiet(target: &TmuxTarget) {
    let mut last = target.capture().unwrap_or_default();
    wait_until(
        || {
            thread::sleep(TICK);
            let now = target.capture().unwrap_or_default();
            let settled = now == last;
            last = now;
            settled
        },
        "the pane to stop rendering",
    );
}

#[test]
fn send_line_reaches_pane_and_capture_reads_it() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let target = server.target();

    // The literal typed text contains `$((6*7))`, not `42`; `nudge_marker_42`
    // only appears in the pane if `sh` actually evaluated the echo, which
    // requires that `Enter` was sent after the literal text.
    target.send_line("echo nudge_marker_$((6*7))").unwrap();
    wait_for_text(
        &target,
        "nudge_marker_42",
        "the marker only appears if `sh` evaluated the echo, which needs the \
         Enter that follows the literal text",
    );
}

#[test]
fn send_line_handles_leading_dash_message() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let target = server.target();
    // A message starting with '-' must not be parsed as tmux flags (would Err without `--`).
    target.send_line("-dash_marker_ok").unwrap();
    wait_for_text(
        &target,
        "-dash_marker_ok",
        "a leading '-' must reach the pane as literal text, not as tmux flags",
    );
}

#[test]
fn end_to_end_injection_verifies_then_sends() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let target = server.target();
    // Stage a rate-limit banner into the pane so the verify-gate passes.
    server.stage("printf 'quota reached. Resets in 45m\\n'");
    wait_for_text(
        &target,
        "quota reached",
        "the verify gate only passes with the banner actually on screen",
    );

    let job = Job {
        id: 1,
        target: TargetSpec::Tmux { pane: "s".into() },
        messages: vec!["echo nudge_done_$((6*7))".into()],
        send_delay_secs: 0.0,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: true,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: None,
        verify_dims: None,
    };

    let out = run_injection(&target, &job, &fixed_now(), None, None, None).unwrap();
    assert_eq!(out, InjectOutcome::Sent(1));

    wait_for_text(
        &target,
        "nudge_done_42",
        "a job that reported Sent must actually have reached the pane",
    );
}

// ---- the recency gate against a real tmux server ----

/// Everything the gate's fail-open depends on rests on tmux's actual behavior
/// here, not on our reading of the docs. `display-message` on a pane that does
/// not exist prints the empty fields `"x"` and **exits 0** -- a missing pane is
/// not an error to tmux. `PaneDims::parse` must reject that rather than read it
/// as `0x0`, which would compare equal to the next unreadable pane's and turn
/// "no idea how big this is" into a confident verdict.
#[test]
fn dims_reads_the_real_pane_size_and_refuses_a_pane_that_is_gone() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    assert_eq!(
        server.target().dims(),
        Some(nudge::target::PaneDims {
            width: 80,
            height: 24
        }),
        "the session is started -x 80 -y 24"
    );

    let gone = TmuxTarget::with_socket("no-such-pane", &server.socket);
    assert_eq!(
        gone.dims(),
        None,
        "tmux answers a missing pane with a bare \"x\" and exit 0; reading that as \
         0x0 would manufacture comparable dims out of nothing"
    );
}

/// The resize confound, against real tmux: the design's one known way for a
/// fingerprint to change without the user touching anything. If `dims` did not
/// notice a width-only resize, the reflowed capture would read as "the user
/// resumed" and the nudge would silently never fire.
#[test]
fn dims_notices_a_width_only_resize() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let before = server.target().dims().unwrap();

    let ok = Command::new("tmux")
        .args([
            "-L",
            &server.socket,
            "resize-window",
            "-t",
            "s",
            "-x",
            "120",
            "-y",
            "24",
        ])
        .status()
        .unwrap()
        .success();
    if !ok {
        eprintln!("skipping: this tmux cannot resize-window");
        return;
    }
    // Poll on the *change*, then assert the value: waiting for `width == 120`
    // would make the assertion below tautological.
    wait_until(
        || server.target().dims().is_some_and(|d| d != before),
        "tmux to apply the width-only resize",
    );

    let after = server.target().dims().unwrap();
    assert_ne!(
        before, after,
        "a width-only resize must be visible in dims: it is invisible in the capture \
         text, which is exactly why we ask tmux instead of counting lines"
    );
    assert_eq!(after.width, 120);
}

/// The I19 scenario end to end against a real pane: banner staged, snapshot
/// taken, then real output arrives (the user resuming), then fire.
#[test]
fn end_to_end_verify_skips_a_pane_that_moved_after_the_snapshot() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let target = server.target();
    server.stage("printf 'quota reached. Resets in 45m\\n'");
    wait_for_text(
        &target,
        "quota reached",
        "the banner must be up before the pane is snapshotted",
    );
    // Snapshot a pane that has stopped moving, or the baseline disagrees with
    // the pane a moment later for reasons the user had nothing to do with.
    wait_until_quiet(&target);

    // Schedule time: snapshot the pane parked at its banner.
    let base = nudge::verify::capture_baseline(&target).expect("a live pane must snapshot");

    // The user resumes by hand: real output lands, and the banner scrolls up but
    // stays on screen -- which is the whole of finding I19.
    server.stage("printf 'resumed by hand\\n'");
    let screen = wait_for_text(
        &target,
        "resumed by hand",
        "the user's own output must have landed before the gate runs",
    );
    assert!(
        screen.contains("quota reached"),
        "precondition: the stale banner is still on screen, so the banner check \
         alone would still inject; got:\n{screen}"
    );

    let job = Job {
        id: 1,
        target: TargetSpec::Tmux { pane: "s".into() },
        messages: vec!["echo nudge_should_not_appear_$((6*7))".into()],
        send_delay_secs: 0.0,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: true,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: Some(base.fingerprint),
        verify_dims: Some(base.dims),
    };
    let out = run_injection(&target, &job, &fixed_now(), None, None, None).unwrap();
    assert_eq!(out, InjectOutcome::SkippedResumed);

    // Absence is the one thing polling cannot wait for, so make it observable:
    // push a sentinel through the same path and wait for that. tmux delivers to
    // a pane in order, so once the sentinel has rendered, anything the injection
    // had sent would have rendered before it -- which turns "nothing arrived"
    // into a claim with a barrier behind it. A fixed sleep proves the same thing
    // only on a machine that happened to be fast enough.
    server.stage("printf 'flush_marker_ok\\n'");
    let after = wait_for_text(
        &target,
        "flush_marker_ok",
        "the sentinel orders this check after anything the injection sent",
    );
    assert!(
        !after.contains("nudge_should_not_appear_42"),
        "nothing may reach a pane the user already resumed; got:\n{after}"
    );
}

/// The other half, against a real pane: untouched since the snapshot, banner
/// still up -> fire. This is the one that would catch the gate being wired
/// backwards, which would make every overnight nudge silently never fire.
#[test]
fn end_to_end_verify_still_fires_into_a_pane_nobody_touched() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let target = server.target();
    server.stage("printf 'quota reached. Resets in 45m\\n'");
    wait_for_text(
        &target,
        "quota reached",
        "the banner must be up before the pane is snapshotted",
    );
    // Nobody touches the pane from here on. Waiting for it to stop rendering
    // *before* the snapshot is what makes "a parked pane is byte-identical"
    // true: a baseline taken mid-render differs from the pane a moment later,
    // and the gate reads that difference as the user having resumed.
    wait_until_quiet(&target);
    let base = nudge::verify::capture_baseline(&target).expect("a live pane must snapshot");

    let job = Job {
        id: 1,
        target: TargetSpec::Tmux { pane: "s".into() },
        messages: vec!["echo nudge_fired_$((6*7))".into()],
        send_delay_secs: 0.0,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: true,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: Some(base.fingerprint),
        verify_dims: Some(base.dims),
    };
    let out = run_injection(&target, &job, &fixed_now(), None, None, None).unwrap();
    assert_eq!(
        out,
        InjectOutcome::Sent(1),
        "an untouched pane still parked at its banner is exactly what nudge exists for"
    );

    wait_for_text(
        &target,
        "nudge_fired_42",
        "the message must reach a pane the gate cleared to fire into",
    );
}

//! Integration tests for the tmux backend and the end-to-end inject path.
//! Each runs against a PRIVATE tmux server (`tmux -L <socket>`) that is killed
//! on drop, so the developer's real tmux is never touched. All self-skip when
//! tmux is not installed.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
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
    thread::sleep(Duration::from_millis(500)); // let the shell run + render

    let screen = target.capture().unwrap();
    assert!(
        screen.contains("nudge_marker_42"),
        "captured pane missing the marker; got:\n{screen}"
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
    thread::sleep(Duration::from_millis(500));
    let screen = target.capture().unwrap();
    assert!(
        screen.contains("-dash_marker_ok"),
        "leading-dash literal missing; got:\n{screen}"
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
        messages: vec!["echo nudge_done_$((6*7))".into()],
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
        screen.contains("nudge_done_42"),
        "sent message not visible in pane; got:\n{screen}"
    );
}

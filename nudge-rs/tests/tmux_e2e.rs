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
use nudge::inject::{InjectOutcome, run_injection};
use nudge::job::{Job, TargetSpec};
use nudge::target::{Target, tmux::TmuxTarget};

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Owns a private tmux server; kills it on drop (even on panic).
struct Server {
    socket: String,
}

/// Start a detached 80x24 session `s` running a plain shell on `socket`,
/// returning tmux's answer so that a failure can say what tmux said.
fn new_session(socket: &str) -> std::process::Output {
    Command::new("tmux")
        .args([
            "-f",
            "/dev/null",
            "-L",
            socket,
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
        .output()
        .expect("spawn tmux")
}

/// Fail with what tmux said if `out` is not a success.
fn assert_started(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "failed to {what} private tmux session: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

impl Server {
    /// Stop this server and wait until a client is refused outright.
    ///
    /// Refused, with tmux's `no server running on` or `error connecting to`,
    /// because that is what both callers need: a new server can start on the
    /// socket, and `server_restarted` sees no server at all. Weaker conditions
    /// were not enough. `list-sessions` merely failing let a restart's
    /// `new-session` fail under load. Even after the server's process had
    /// exited, a client asking straight away twice connected and was then
    /// told `server exited unexpectedly`.
    fn stop(&self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .status();
        wait_until(
            || {
                Command::new("tmux")
                    .args(["-L", &self.socket, "list-sessions"])
                    .output()
                    .is_ok_and(|o| {
                        let said = String::from_utf8_lossy(&o.stderr);
                        !o.status.success()
                            && (said.contains("no server running")
                                || said.contains("error connecting to"))
                    })
            },
            "a client to find no server",
        );
    }

    /// Stop this server and start a new one on the same socket, as a reboot
    /// would. Its first pane gets the id the old server's first pane had.
    fn restart(&self) {
        self.stop();
        assert_started(&new_session(&self.socket), "restart");
    }

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
        // `-L` gives us a private *server*; `-f /dev/null` gives us a
        // private *config*, and the second is not optional. Without it a
        // new server still sources the developer's ~/.tmux.conf, so what
        // these tests capture depends on their personal settings -- the
        // exact coupling the header promises is absent. It is also what
        // made these the slowest tests in the suite: sourcing one real
        // config (plugin manager, shell-outs) measured 5.3s per server,
        // against 0.02s for a bare one, and every test starts one.
        assert_started(&new_session(&socket), "start");
        server
    }
    fn target(&self) -> TmuxTarget {
        TmuxTarget::with_socket("s", &self.socket)
    }
    /// Run a tmux command on this server and return what it printed.
    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-L", &self.socket])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "tmux {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    /// Type a raw line straight into the pane via tmux (bypassing `TmuxTarget`),
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

/// Codex reads keystrokes that arrive faster than anyone types as a paste, and
/// for 120ms after the last of them (`PASTE_ENTER_SUPPRESS_WINDOW` in codex-rs
/// `tui/src/bottom_pane/paste_burst.rs`) it takes Enter as a newline inside that
/// paste rather than a submit. Seen live on codex 0.162.1: the nudge's message
/// sat in the composer, unsent, until a second Enter arrived on its own.
///
/// So the Enter has to reach the pane well after the text, not just after it.
#[test]
fn send_line_lets_the_text_settle_before_pressing_enter() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let log = KeyLog::attach(&server);

    server.target().send_line("please continue").unwrap();

    let keys = log.until(b'\r');
    assert_eq!(text_of(&keys), "please continue\r");
    let gap = keys[keys.len() - 1].0 - keys[keys.len() - 2].0;
    assert!(
        gap > PASTE_WINDOW,
        "Enter reached the pane {gap:?} after the text, inside Codex's 120ms \
         window, so Codex would add a newline and not submit"
    );
}

/// The same window from the other side. A key pressed just ahead of the text
/// is the first character of the paste, unless it is given time to be read as
/// a keypress first.
#[test]
fn send_key_is_read_before_the_text_that_follows_it() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let log = KeyLog::attach(&server);
    let target = server.target();

    target.send_key("i").unwrap();
    target.send_line("go").unwrap();

    let keys = log.until(b'\r');
    assert_eq!(text_of(&keys), "igo\r");
    let gap = keys[1].0 - keys[0].0;
    assert!(
        gap > PASTE_WINDOW,
        "the text reached the pane {gap:?} after the key, close enough to be \
         read as one paste with it"
    );
}

/// Codex's `PASTE_ENTER_SUPPRESS_WINDOW`, the longest of the intervals its
/// paste detection runs on.
const PASTE_WINDOW: Duration = Duration::from_millis(120);

/// A pane that reports every keystroke it is sent, and when.
///
/// It runs `cat` on a raw tty into a FIFO, which hands the test each write as
/// tmux delivers it. The gap between two keys is therefore measured on the
/// pane's side, not inferred from when this process asked for them.
struct KeyLog {
    rx: std::sync::mpsc::Receiver<Option<(Instant, Vec<u8>)>>,
    _dir: tempfile::TempDir,
}

impl KeyLog {
    fn attach(server: &Server) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("keys");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success(),
            "mkfifo failed"
        );

        // Every chunk the pane writes, stamped on arrival. The first message
        // only says the pane's `cat` has opened the FIFO -- which it does after
        // `stty` has put the tty in raw mode, so nothing sent from then on is
        // held back for a line discipline to release all at once.
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        thread::spawn(move || {
            let mut f = std::fs::File::open(&path).unwrap();
            let _ = tx.send(None);
            let mut buf = [0u8; 256];
            while let Ok(n @ 1..) = std::io::Read::read(&mut f, &mut buf) {
                if tx.send(Some((Instant::now(), buf[..n].to_vec()))).is_err() {
                    break;
                }
            }
        });
        server.stage(&format!("stty raw -echo; exec cat > '{}'", fifo.display()));
        assert_eq!(
            rx.recv_timeout(PATIENCE)
                .expect("the pane's cat never opened the FIFO"),
            None
        );
        KeyLog { rx, _dir: dir }
    }

    /// Every byte the pane receives up to and including `last`, each stamped
    /// with the arrival of the chunk that carried it. Two bytes with the same
    /// stamp reached the pane together.
    fn until(&self, last: u8) -> Vec<(Instant, u8)> {
        let mut keys = Vec::new();
        while keys.last().is_none_or(|&(_, b)| b != last) {
            let (at, bytes) = self
                .rx
                .recv_timeout(PATIENCE)
                .expect("the pane never received the last key")
                .expect("only the first message is the FIFO opening");
            keys.extend(bytes.into_iter().map(|b| (at, b)));
        }
        keys
    }
}

/// `send_key` takes tmux's key names, which is how it presses Backspace to
/// take back an `i` that a Claude box with no Vim keys typed as text.
#[test]
fn send_key_presses_a_named_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let log = KeyLog::attach(&server);
    let target = server.target();

    target.send_key("BSpace").unwrap();
    target.send_line("go").unwrap();

    assert_eq!(text_of(&log.until(b'\r')), "\x7fgo\r");
}

fn text_of(keys: &[(Instant, u8)]) -> String {
    String::from_utf8(keys.iter().map(|&(_, b)| b).collect()).unwrap()
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
        target: TargetSpec::Tmux {
            pane: "s".into(),
            server: None,
        },
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
        target: TargetSpec::Tmux {
            pane: "s".into(),
            server: None,
        },
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
        target: TargetSpec::Tmux {
            pane: "s".into(),
            server: None,
        },
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

// ---- naming a pane by its id ----

/// A job stores tmux's id for its pane (`%3`), not the pane's position
/// (`s:2.0`). The position is an index, and with `renumber-windows` on,
/// closing any window before it hands the index to the next window along, so
/// a nudge scheduled for one session landed in another. The id stays with the
/// pane until it closes.
#[test]
fn locate_names_a_pane_by_its_id_and_where_it_sits_now() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    server.tmux(&["split-window", "-t", "s:0", "sh"]);
    let id = server.tmux(&["display-message", "-p", "-t", "s:0.1", "#{pane_id}"]);

    let loc = TmuxTarget::with_socket("s:0.1", &server.socket)
        .locate()
        .unwrap();
    assert_eq!(loc.id, id);
    assert_eq!(loc.position, "s:0.1");
}

/// `locate` reads the session's name from the pane's title, and a title tmux
/// left at the host name gives none. The comma is there for tmux's format
/// conditional, which splits on commas.
#[test]
fn locate_reads_the_session_name_from_the_panes_title() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    server.tmux(&["split-window", "-t", "s:0", "sh"]);
    server.tmux(&["select-pane", "-t", "s:0.1", "-T", "✳ warmer, sessions"]);

    let named = TmuxTarget::with_socket("s:0.1", &server.socket)
        .locate()
        .unwrap();
    assert_eq!(named.name.as_deref(), Some("warmer, sessions"));
    let unnamed = TmuxTarget::with_socket("s:0.0", &server.socket)
        .locate()
        .unwrap();
    assert_eq!(unnamed.name, None);
}

/// The trap in asking tmux for a pane's id. `display-message -t` falls back
/// to the current pane when its window is gone, prints that pane's id, and
/// exits 0, so a typo'd or vanished window would have been stored as some
/// other pane.
#[test]
fn locate_refuses_a_window_that_does_not_exist() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let err = TmuxTarget::with_socket("s:9.0", &server.socket)
        .locate()
        .expect_err("there is no window 9, so there is no pane to name");
    assert!(err.to_string().contains("s:9.0"), "{err:#}");
}

#[test]
fn locate_refuses_a_pane_id_that_does_not_exist() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    assert!(
        TmuxTarget::with_socket("%999", &server.socket)
            .locate()
            .is_err()
    );
}

/// The stamp a job keeps is read along with the id, from the same server.
#[test]
fn locate_reads_which_server_the_pane_is_on() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let loc = server.target().locate().unwrap();
    let stamp = server.tmux(&["display-message", "-p", nudge::target::tmux::SERVER_FORMAT]);
    assert!(loc.server.is_some(), "tmux answered {stamp:?}");
    assert_eq!(loc.server.as_deref(), Some(stamp.as_str()));
}

/// The control for the guard: on the server it was read from, a stamped pane
/// has not restarted. Were tmux's answer misread, every stamped job would stop
/// firing.
#[test]
fn a_pane_on_the_server_it_was_read_from_has_not_restarted() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let loc = server.target().locate().unwrap();
    let target = TmuxTarget::with_socket(&loc.id, &server.socket).expecting_server(loc.server);
    assert!(!target.server_restarted().unwrap());
}

/// After a reboot and before tmux is started again there is no server at all,
/// which proves the one that issued the id is gone. It is not an unreachable
/// pane, so the job is refused as restarted rather than dropped with an error.
#[test]
fn a_pane_id_with_no_tmux_server_running_counts_as_restarted() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let loc = server.target().locate().unwrap();

    server.stop();

    let target = TmuxTarget::with_socket(&loc.id, &server.socket).expecting_server(loc.server);
    assert!(target.server_restarted().unwrap());
}

/// The bug: a new server numbers its panes from `%0` again, so a job kept
/// across a restart holds an id that now names some other pane.
#[test]
fn a_pane_id_from_a_restarted_server_is_refused() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    let loc = server.target().locate().unwrap();

    server.restart();
    let fresh = server.target().locate().unwrap();
    assert_eq!(fresh.id, loc.id, "the new server hands out the same id");
    assert_ne!(fresh.server, loc.server);

    let job = Job {
        id: 1,
        target: TargetSpec::Tmux {
            pane: loc.id.clone(),
            server: loc.server.clone(),
        },
        messages: vec!["echo stale_$((6*7))".into()],
        send_delay_secs: 0.0,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: false,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: None,
        verify_dims: None,
    };
    let target = TmuxTarget::with_socket(&loc.id, &server.socket).expecting_server(loc.server);
    let out = run_injection(&target, &job, &fixed_now(), None, None, None).unwrap();
    assert_eq!(out, InjectOutcome::ServerRestarted);
    // Nothing to wait for if nothing was sent, so a short look is the check.
    thread::sleep(Duration::from_millis(200));
    assert!(!target.capture().unwrap().contains("stale_"));
}

/// The bug itself: the windows renumber between scheduling and firing.
#[test]
fn a_pane_id_still_reaches_its_pane_after_the_windows_renumber() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let server = Server::start();
    server.tmux(&["set-option", "-g", "renumber-windows", "on"]);
    server.tmux(&["new-window", "-t", "s:1", "sh"]);
    server.tmux(&["new-window", "-t", "s:2", "sh"]);
    let id = TmuxTarget::with_socket("s:2.0", &server.socket)
        .locate()
        .unwrap()
        .id;

    server.tmux(&["kill-window", "-t", "s:1"]);

    let target = TmuxTarget::with_socket(&id, &server.socket);
    assert_eq!(target.locate().unwrap().position, "s:1.0");
    target.send_line("echo landed_$((6*7))").unwrap();
    wait_for_text(
        &target,
        "landed_42",
        "the id must still name the pane that was window 2",
    );
}

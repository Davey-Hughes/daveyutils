//! Integration tests for the herdr backend. Each runs against a PRIVATE herdr
//! server (`common::HerdrServer`), started in its own tempdir and killed on
//! drop, so the developer's own herdr is never touched. All self-skip when
//! herdr is not installed.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::HerdrServer;
use nudge::target::herdr::{HerdrTarget, list_panes};
use nudge::target::{Key, Target, WrongPane};

/// How long a pane gets to show what it was sent.
const PATIENCE: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(20);

/// The Claude session the tests report on a pane, in the form Claude's are.
const SESSION: &str = "11111111-2222-4333-8444-555555555555";

/// A pane that says `ready` once it is reading a line, then shows what it read
/// as `got:<line>`. `ready` is printed dim, for reading the styled capture.
/// The quotes split it so that the command line echoed above it, which also
/// shows on the screen, cannot pass for it.
const READ_ONE_LINE: &str =
    r#"sh -c 'printf "\033[2m%s\033[0m\n" rea""dy; read l; echo "got:$l"; sleep 30'"#;

/// Poll until `cond` holds of the pane's capture, returning that capture.
fn wait_for_screen(target: &HerdrTarget, cond: impl Fn(&str) -> bool, what: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = target.capture().unwrap_or_default();
        if cond(&screen) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}\nlast capture:\n{screen}"
        );
        thread::sleep(TICK);
    }
}

/// Poll until `cond` holds, failing with `what` if it never does.
fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(TICK);
    }
}

/// Wait until the pane prints its `ready` line, on a line of its own.
fn wait_ready(target: &HerdrTarget) {
    wait_for_screen(
        target,
        |s| s.lines().any(|l| l.trim_end() == "ready"),
        "the pane to be ready",
    );
}

/// Report `session` as the Claude session in `pane`, and wait until herdr
/// shows it.
///
/// Under `herdr:claude`, the source herdr's own Claude integration reports
/// with. A session reported under any other source, or for an agent herdr
/// does not know, is accepted with exit 0 and never shown (seen 2026-11-19).
fn report_session(server: &HerdrServer, pane: &str, session: &str) {
    server.run(&[
        "pane",
        "report-agent-session",
        "--source",
        "herdr:claude",
        "--agent",
        "claude",
        "--agent-session-id",
        session,
        pane,
    ]);
    let target = HerdrTarget::new(pane, server.socket());
    wait_until(
        || {
            target
                .get()
                .is_ok_and(|p| p.agent_session.as_deref() == Some(session))
        },
        "herdr to show the reported session",
    );
}

#[test]
fn herdr_target_reads_and_types() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let dir = common::short_tempdir();
    let server = HerdrServer::start(dir.path());
    let target = HerdrTarget::new(server.new_pane(READ_ONE_LINE), server.socket());
    wait_ready(&target);

    target.send_line("please continue").unwrap();
    wait_for_screen(
        &target,
        |s| s.contains("got:please continue"),
        "the line to be read",
    );

    // The plain capture has no escapes in it, and the styled one keeps the
    // dim `ready` was printed in. herdr ends each line of `--format ansi` with
    // CRLF (seen 2026-11-19), which the styled capture turns into the LF a
    // tmux capture has.
    let plain = target.capture().unwrap();
    assert!(
        !plain.contains('\x1b'),
        "plain capture has escapes: {plain:?}"
    );
    let styled = target.capture_styled().unwrap();
    assert!(!styled.contains('\r'), "styled capture has CRs: {styled:?}");
    let ready = styled
        .lines()
        .find(|l| l.ends_with("ready") || l.contains("ready\x1b"))
        .unwrap_or_else(|| panic!("no ready line in {styled:?}"));
    assert!(ready.contains("\x1b[2m"), "ready is not dim: {ready:?}");
}

#[test]
fn send_text_starting_with_a_dash_arrives_intact() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let dir = common::short_tempdir();
    let server = HerdrServer::start(dir.path());
    let target = HerdrTarget::new(server.new_pane(READ_ONE_LINE), server.socket());
    wait_ready(&target);

    target.send_line("-v please continue").unwrap();
    wait_for_screen(
        &target,
        |s| s.contains("got:-v please continue"),
        "the dash-led line to be read whole",
    );
}

#[test]
fn herdr_accepts_each_key_name() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let dir = common::short_tempdir();
    let server = HerdrServer::start(dir.path());
    let pane = server.new_pane("cat");
    let target = HerdrTarget::new(&pane, server.socket());

    // herdr checks every key name before it writes anything, so a name it
    // does not know fails here rather than typing something else.
    for key in [Key::Char('A'), Key::Char('i'), Key::Escape, Key::Backspace] {
        target
            .send_key(key)
            .unwrap_or_else(|e| panic!("herdr refused {key:?}: {e:#}"));
    }

    // Which is what makes the Ok above mean something: an unknown name is an
    // error, not a no-op. tmux's name for Backspace is one.
    let out = server
        .herdr()
        .args(["pane", "send-keys", &pane, "BSpace"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "herdr took tmux's BSpace");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid_key"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn herdr_dims_are_known() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let dir = common::short_tempdir();
    let server = HerdrServer::start(dir.path());
    let target = HerdrTarget::new(server.new_pane(""), server.socket());

    let dims = target.dims().expect("herdr knows the pane's size");
    assert!(dims.width > 0 && dims.height > 0, "{dims:?}");
    // A pane that is not there has no size, rather than an error.
    assert_eq!(HerdrTarget::new("w99:p99", server.socket()).dims(), None);
}

#[test]
fn a_zoomed_tab_has_no_known_dims() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let dir = common::short_tempdir();
    let server = HerdrServer::start(dir.path());
    let first = server.new_pane("");
    let split: serde_json::Value =
        serde_json::from_str(&server.run(&["pane", "split", &first, "--direction", "right"]))
            .expect("herdr JSON");
    let second = split["result"]["pane"]["pane_id"]
        .as_str()
        .expect("the split's new pane")
        .to_string();
    let zoomed = HerdrTarget::new(&first, server.socket());
    let beside = HerdrTarget::new(&second, server.socket());
    assert!(zoomed.dims().is_some(), "known before the zoom");

    // herdr's rects ignore a zoom, so while one is on no pane in the tab has
    // a size nudge can trust.
    server.run(&["pane", "zoom", &first, "--on"]);
    assert_eq!(zoomed.dims(), None, "the zoomed pane");
    assert_eq!(beside.dims(), None, "the pane under it");

    server.run(&["pane", "zoom", &first, "--off"]);
    assert!(zoomed.dims().is_some(), "known again after the zoom");
    assert!(beside.dims().is_some(), "known again after the zoom");
}

#[test]
fn a_pane_keeps_its_id_and_session_across_a_restart() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let dir = common::short_tempdir();
    let mut server = HerdrServer::start(dir.path());
    let pane = server.new_pane("cat");
    report_session(&server, &pane, SESSION);

    server.restart();

    let panes = list_panes(server.socket()).unwrap();
    let after = panes
        .iter()
        .find(|p| p.id == pane)
        .unwrap_or_else(|| panic!("{pane} is gone after a restart: {panes:?}"));
    assert_eq!(after.agent_session.as_deref(), Some(SESSION));
}

#[test]
fn herdr_wrong_pane() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let dir = common::short_tempdir();
    let server = HerdrServer::start(dir.path());

    // A pane holding another conversation than the one the job stamped.
    let stamped = server.new_pane("cat");
    report_session(&server, &stamped, SESSION);
    let other = HerdrTarget::new(&stamped, server.socket()).expecting_session(Some("other".into()));
    assert_eq!(
        other.wrong_pane().unwrap(),
        Some(WrongPane::OtherConversation)
    );
    let same = HerdrTarget::new(&stamped, server.socket()).expecting_session(Some(SESSION.into()));
    assert_eq!(same.wrong_pane().unwrap(), None);

    // A pane whose foreground is a tmux client. herdr names a process the way
    // the kernel does, after the file it ran, so a stand-in named tmux passes
    // for one.
    let fake_tmux = common::stand_in(dir.path(), "tmux");
    let tmux = HerdrTarget::new(
        server.new_pane(&format!(
            "{} -c '{}'",
            fake_tmux.display(),
            common::STAND_IN_WAIT
        )),
        server.socket(),
    );
    wait_until(
        || {
            tmux.foreground()
                .is_ok_and(|f| f.processes.iter().any(|p| p.name == "tmux"))
        },
        "tmux in the pane's foreground",
    );
    assert_eq!(tmux.wrong_pane().unwrap(), Some(WrongPane::RunsTmux));

    // The pid herdr gives is the process's own, which is what the system is
    // asked about it by: herdr gives no tty.
    let pid = tmux
        .foreground()
        .unwrap()
        .processes
        .into_iter()
        .find(|p| p.name == "tmux")
        .expect("tmux is still in the foreground")
        .pid;
    let comm = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&comm.stdout)
            .trim_end()
            .ends_with("tmux"),
        "pid {pid} is not the fake tmux: {}",
        String::from_utf8_lossy(&comm.stdout)
    );

    // A plain pane nobody stamped is the pane it was.
    let plain = HerdrTarget::new(server.new_pane(""), server.socket());
    assert_eq!(plain.wrong_pane().unwrap(), None);
}

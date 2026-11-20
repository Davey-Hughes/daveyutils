//! `common::Watchdog` cleans up after a test process that died without
//! dropping its fixture: a Ctrl-C at the terminal, a kill, an abort.
//!
//! The daemons these fixtures start are out of the terminal's reach on
//! purpose (see `common::DaemonGuard`), so a Ctrl-C used to leave each one
//! running beside its private tmux server and its tempdir. Measured on
//! 2026-11-20 by sending SIGINT to the process group of `daemon_batch`,
//! `daemon_fire`, `cli_daemon_log` and `cli_ask` in turn.
//!
//! No test process dies here. What a dead one causes is its end of the
//! watchdog's pipe closing, and `Watchdog::fire` closes that end and waits
//! for the watchdog to finish.

mod common;

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// `sh -c script` in a process group of its own, standing in for a daemon,
/// once it has printed `up`. The `sleep` its script starts stands in for a
/// `tmux` the daemon has in flight. Each holds the writing end of the pipe
/// returned, so that pipe reaches its end only once both are dead.
fn stand_in_daemon(script: &str) -> (Child, ChildStdout) {
    let mut child = Command::new("sh")
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn sh");
    let mut out = child.stdout.take().unwrap();
    let mut up = [0u8; 3];
    out.read_exact(&mut up)
        .expect("sh to say its child is running");
    assert_eq!(&up, b"up\n");
    (child, out)
}

/// A stand-in daemon that stays up, with its child.
const STAYS_UP: &str = "sleep 30 & echo up; wait";

/// A stand-in daemon that exits at once and leaves its child in its group.
const EXITS: &str = "sleep 30 & echo up";

fn pid_of(child: &Child) -> libc::pid_t {
    libc::pid_t::try_from(child.id()).expect("a process id is a pid_t")
}

/// Tells, once, when everything that held `out` open is dead.
fn all_dead(mut out: ChildStdout) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = out.read_to_end(&mut rest);
        let _ = tx.send(());
    });
    rx
}

/// Fail unless everything that held `out` open is dead.
fn assert_all_dead(out: ChildStdout) {
    all_dead(out)
        .recv_timeout(Duration::from_secs(5))
        .expect("the stand-in daemon or its child is alive: something holds its output open");
}

/// Whether no process has `pid` any more. One that has exited and that
/// nothing has collected counts as gone: under a container's init that
/// collects nothing, a dead tmux server stays that way.
fn gone(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 sends nothing; it only asks whether `pid` is there.
    if unsafe { libc::kill(pid, 0) } == -1 {
        return true;
    }
    // The state is the field after the name, which is in brackets and may
    // itself hold a bracket.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.starts_with(" Z"))
        })
        .unwrap_or(false)
}

/// Everything a fixture gets from its watchdog goes when the test process
/// does: the daemon and what it started, the tmux server, the tempdir.
///
/// The server and the stand-in daemon are started after the watchdog, as a
/// fixture starts them, and the daemon's guard is still there when the
/// watchdog goes off. If any of the three held the test's end of the pipe
/// open, `fire` would give up waiting.
#[test]
fn a_dead_test_process_leaves_no_daemon_no_tmux_server_and_no_tempdir() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let tmp = common::short_tempdir();
    let dir = tmp.path().to_path_buf();
    let watchdog = common::Watchdog::over(&dir);
    let tmux = watchdog.start_tmux(&dir.join("tmux"), "held");
    let server: libc::pid_t = tmux
        .run(&["display-message", "-p", "#{pid}"])
        .parse()
        .expect("the tmux server's pid");
    let (daemon, out) = stand_in_daemon(STAYS_UP);
    let guard = watchdog.guard(daemon);

    watchdog.fire();

    assert_all_dead(out);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gone(server) {
        assert!(Instant::now() < deadline, "the tmux server is running");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!dir.exists(), "the tempdir is still there");
    // The server's own cleanup has nothing left to do, and would warn that
    // its directory went first.
    std::mem::forget(tmux);
    drop(guard);
}

/// A daemon that has been collected has given up its pid, and the group of
/// that number, to whatever process is handed them next. The watchdog is
/// told to forget the group first, and then leaves that number alone.
///
/// Here the forgotten group is still the stand-in's, which is how the test
/// can see that it was left alone. It is told about first, so a watchdog
/// that killed both would have killed it before the one this waits for.
#[test]
fn a_group_the_watchdog_was_told_to_forget_is_left_alone() {
    let tmp = common::short_tempdir();
    let watchdog = common::Watchdog::over(tmp.path());
    let (forgotten, _forgotten_out) = stand_in_daemon(STAYS_UP);
    let (watched, watched_out) = stand_in_daemon(STAYS_UP);
    watchdog.watch_group(pid_of(&forgotten));
    watchdog.watch_group(pid_of(&watched));
    watchdog.forget_group(pid_of(&forgotten));

    watchdog.fire();

    assert_all_dead(watched_out);
    let forgotten = common::DaemonGuard::new(forgotten);
    assert!(
        !forgotten.has_exited(),
        "the watchdog killed a group it was told to forget"
    );
    drop(watched);
}

/// A number the watchdog forgot and is then told about again is a new
/// daemon's, handed the old one's pid, and is killed like any other.
#[test]
fn a_group_told_about_again_after_it_was_forgotten_is_killed() {
    let tmp = common::short_tempdir();
    let watchdog = common::Watchdog::over(tmp.path());
    let (daemon, out) = stand_in_daemon(STAYS_UP);
    watchdog.watch_group(pid_of(&daemon));
    watchdog.forget_group(pid_of(&daemon));
    watchdog.watch_group(pid_of(&daemon));

    watchdog.fire();

    assert_all_dead(out);
    drop(daemon);
}

/// Each forget takes back one telling, not every one so far. A guard whose
/// daemon was collected behind its back has the watchdog forget the number
/// only when the guard is stopped, and by then a newer daemon may have been
/// handed that pid and told about. That one is still killed.
#[test]
fn a_group_told_about_twice_and_forgotten_once_is_killed() {
    let tmp = common::short_tempdir();
    let watchdog = common::Watchdog::over(tmp.path());
    let (daemon, out) = stand_in_daemon(STAYS_UP);
    watchdog.watch_group(pid_of(&daemon));
    watchdog.watch_group(pid_of(&daemon));
    watchdog.forget_group(pid_of(&daemon));

    watchdog.fire();

    assert_all_dead(out);
    drop(daemon);
}

/// The guard is what tells the watchdog to forget, as it stops its daemon.
///
/// The daemon here was collected behind the guard's back, so the guard kills
/// nothing, and its group still has the `sleep` in it: that is how the test
/// can see the watchdog leave the group alone. A second stand-in, told about
/// later, is the one the watchdog does kill, and so killed second.
#[test]
fn a_daemon_its_guard_has_stopped_is_forgotten() {
    let tmp = common::short_tempdir();
    let watchdog = common::Watchdog::over(tmp.path());
    let (stopped, stopped_out) = stand_in_daemon(EXITS);
    let mut guard = watchdog.guard(stopped);
    let pid = guard.pid();
    // SAFETY: `waitpid` on this test's own child, with nowhere to put its
    // status.
    let collected = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
    assert_eq!(collected, pid, "collect sh behind the guard's back");
    assert_eq!(guard.stop(), None, "nobody knows how it ended");
    let (watched, watched_out) = stand_in_daemon(STAYS_UP);
    watchdog.watch_group(pid_of(&watched));

    watchdog.fire();

    assert_all_dead(watched_out);
    let stopped_dead = all_dead(stopped_out);
    assert!(
        stopped_dead
            .recv_timeout(Duration::from_millis(300))
            .is_err(),
        "the watchdog killed the group of a daemon its guard had stopped"
    );
    // The `sleep` is this test's to clear up. Its group still has it as a
    // member, so the number is still that group's.
    //
    // SAFETY: `kill` takes ids and no pointers.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    stopped_dead
        .recv_timeout(Duration::from_secs(5))
        .expect("the sleep to die");
    drop(watched);
}

/// A test that ends in the ordinary way stops its watchdog before anything
/// else, so the fixture's own cleanup is all that runs.
#[test]
fn a_dropped_watchdog_is_dead_and_has_touched_nothing() {
    let tmp = common::short_tempdir();
    let watchdog = common::Watchdog::over(tmp.path());
    let (daemon, _out) = stand_in_daemon(STAYS_UP);
    watchdog.watch_group(pid_of(&daemon));
    let pid = watchdog.pid();

    drop(watchdog);

    // Collected, not merely killed: `gone` would call a dead one that nobody
    // had collected gone as well, so ask the kernel alone.
    // SAFETY: signal 0 sends nothing; it only asks whether `pid` is there.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "the watchdog is still there once dropped"
    );
    assert!(tmp.path().is_dir(), "the tempdir went");
    let daemon = common::DaemonGuard::new(daemon);
    assert!(!daemon.has_exited(), "the daemon was killed");
}

/// The watchdog removes its directory with everything in it, so it takes
/// only a directory `common::short_tempdir` could have made. Asked of the
/// check alone: no watchdog is started over a directory that matters.
#[test]
fn only_a_fixtures_tempdir_is_one() {
    let tmp = common::short_tempdir();
    assert!(common::is_a_fixtures_tempdir(tmp.path()));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/someone".into());
    for not_one in [
        "/",
        "/tmp",
        "/tmp/",
        "/tmp/nudge-",
        "/tmp/other-3fK2",
        "/tmp/nudge-3fK2/state",
        "/var/tmp/nudge-3fK2",
        "nudge-3fK2",
        "tmp/nudge-3fK2",
        "",
        home.as_str(),
    ] {
        assert!(
            !common::is_a_fixtures_tempdir(Path::new(not_one)),
            "{not_one:?} passed for a fixture's tempdir"
        );
    }
}

/// And a watchdog asked to remove anything else refuses. The directory here
/// does not exist, should the refusal ever be lost.
#[test]
#[should_panic(expected = "not a fixture's tempdir")]
fn a_watchdog_refuses_a_directory_that_is_not_a_fixtures_tempdir() {
    let _ = common::Watchdog::over(Path::new("/nonexistent/nudge-3fK2"));
}

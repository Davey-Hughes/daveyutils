//! `common::DaemonGuard` must stop what a daemon started, not only the daemon.
//!
//! A daemon types into a pane by running `tmux`, and a `tmux` it had just
//! started outlives a kill aimed at the daemon alone. The test's tmux server
//! and tempdir go next, and a `tmux` that starts looking for its server after
//! that finds `TMUX_TMPDIR` missing and falls back to the developer's own
//! (see `common::TmuxServer::env`).

mod common;

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// `sh -c script` in a session of its own, as production `spawn_daemon` gives
/// a daemon, once it has printed `up`. `sh` stands in for the daemon, and the
/// `sleep` its script starts for a tmux call the daemon has in flight. Each
/// holds the writing end of the pipe returned, so that pipe reaches its end
/// only once both are dead.
fn stand_in_daemon(script: &str) -> (Child, ChildStdout) {
    let mut cmd = Command::new("sh");
    // SAFETY: the closure runs in the child between fork and exec, where only
    // async-signal-safe calls are allowed. `setsid` is one, and reading errno
    // allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    started(cmd, script)
}

/// Run `cmd` as `sh -c script` and wait for it to print `up`.
fn started(mut cmd: Command, script: &str) -> (Child, ChildStdout) {
    cmd.args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn sh");
    let mut out = child.stdout.take().unwrap();
    let mut up = [0u8; 3];
    out.read_exact(&mut up)
        .expect("sh to say its child is running");
    assert_eq!(&up, b"up\n");
    (child, out)
}

/// Fail unless everything that held `out` open is dead.
fn assert_all_dead(mut out: ChildStdout) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = out.read_to_end(&mut rest);
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the daemon's child outlived it: something still holds its output open");
}

#[test]
fn a_daemons_children_die_with_it() {
    let (child, out) = stand_in_daemon("sleep 30 & echo up; wait");

    drop(common::DaemonGuard::new(child));

    assert_all_dead(out);
}

/// A daemon that died on its own, which is when a test is most likely to be
/// failing, can leave a child behind just the same.
#[test]
fn the_children_of_a_daemon_that_already_exited_die_too() {
    let (child, out) = stand_in_daemon("sleep 30 & echo up");
    let daemon = common::DaemonGuard::new(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !daemon.has_exited() {
        assert!(Instant::now() < deadline, "sh never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(daemon.has_exited(), "asking must not collect the daemon");

    drop(daemon);

    assert_all_dead(out);
}

/// A wait that fails because the daemon died says how it died.
#[test]
fn stopping_a_daemon_that_exited_on_its_own_says_how_it_ended() {
    let (child, out) = stand_in_daemon("sleep 30 & echo up; exit 7");
    let mut daemon = common::DaemonGuard::new(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !daemon.has_exited() {
        assert!(Instant::now() < deadline, "sh never exited");
        std::thread::sleep(Duration::from_millis(10));
    }

    let ended = daemon.stop().expect("the daemon's exit status");

    assert_eq!(ended.code(), Some(7));
    assert!(daemon.has_exited(), "a stopped daemon has exited");
    assert_all_dead(out);
}

/// A daemon a test starts itself, with no session of its own, is reached
/// through `process_group(0)`.
#[test]
fn the_children_of_a_daemon_in_a_group_of_its_own_die_with_it() {
    let mut cmd = Command::new("sh");
    cmd.process_group(0);
    let (child, out) = started(cmd, "sleep 30 & echo up; wait");

    drop(common::DaemonGuard::new(child));

    assert_all_dead(out);
}

/// Once something else has collected the daemon, its pid, and the group of
/// that number, may belong to another process. The guard cannot tell, so it
/// kills nothing. Here the group is still the daemon's, with the `sleep` in
/// it, which is how the test can see that it was left alone.
#[test]
fn a_daemon_collected_behind_the_guards_back_is_left_alone() {
    let (child, out) = stand_in_daemon("sleep 30 & echo up");
    let mut daemon = common::DaemonGuard::new(child);
    let pid = daemon.pid();
    // SAFETY: `waitpid` on this test's own child, with nowhere to put its
    // status.
    let collected = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
    assert_eq!(collected, pid, "collect sh behind the guard's back");

    assert!(daemon.has_exited(), "a daemon that is gone has exited");
    assert_eq!(daemon.stop(), None, "nobody knows how it ended");

    let (tx, rx) = std::sync::mpsc::channel();
    let mut held = out;
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = held.read_to_end(&mut rest);
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the guard killed a group it could no longer call its own"
    );

    // The `sleep` is this test's to clear up. Its group still has it as a
    // member, so the number is still that group's.
    //
    // SAFETY: `kill` takes ids and no pointers.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the sleep to die");
}

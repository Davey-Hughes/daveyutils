//! `common::Descendants` must know when everything a fixture's commands
//! started has exited, including what outlived the command that started it.
//!
//! The CLI suites run a `nudge` that starts a daemon and does not wait for
//! it. The daemon is meant to lose the singleton lock and exit, but taking the
//! lock creates its directory first. One that got there after the fixture's
//! tempdir had been removed would put the directory back, win a lock nobody
//! holds any more, and run on.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

/// `sh` exits at once and leaves a `sleep` running, as the CLI leaves a
/// daemon.
fn leave_running(following: &common::Descendants, seconds: &str) {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", &format!("sleep {seconds} &")]);
    following.follow(&mut cmd);
    assert!(cmd.status().expect("run sh").success());
}

#[test]
fn it_waits_for_what_a_command_left_running() {
    let mut descendants = common::Descendants::new();
    leave_running(&descendants, "2");

    let started = Instant::now();
    assert!(descendants.wait_until_gone(Duration::from_secs(10)));

    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(1),
        "it stopped waiting after {waited:?}, with the sleep still running"
    );
}

#[test]
fn it_says_so_when_something_is_still_running() {
    let mut descendants = common::Descendants::new();
    leave_running(&descendants, "2");

    assert!(!descendants.wait_until_gone(Duration::from_millis(200)));
}

#[test]
fn with_nothing_started_there_is_nothing_to_wait_for() {
    let mut descendants = common::Descendants::new();

    // Not timed: where a pipe cannot be made close-on-exec in one step, a
    // process another test here starts at that moment holds this one open
    // until it exits (see `common::Descendants`).
    assert!(descendants.wait_until_gone(Duration::from_secs(10)));
}

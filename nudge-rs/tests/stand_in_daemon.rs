//! `common::StandInDaemon`, the in-process server the CLI suites put on the
//! socket in place of a daemon, must not outlive the fixture that started it.
//!
//! It binds after a delay, and a test can end sooner than that. The thread it
//! replaced then went on to `server::serve`, which creates the socket's
//! directory before it binds: a test that had already removed its tempdir got
//! it back, holding `run/nudge.sock`, and nothing was left to remove it (seen
//! 2026-11-20: one `/tmp/nudge-XXXXXX` per run of `cli_job_summary`, twenty
//! of them by then).

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nudge::ipc::{Request, Response, client};
use nudge::queue::Queue;

fn empty_queue(dir: &std::path::Path) -> Arc<Mutex<Queue>> {
    Arc::new(Mutex::new(Queue::load(dir.join("queue.json")).unwrap()))
}

#[test]
fn it_answers_once_its_delay_has_passed_and_not_before() {
    let tmp = common::short_tempdir();
    let socket = tmp.path().join("nudge.sock");
    let delay = Duration::from_millis(50);
    let started = Instant::now();
    let _daemon = common::StandInDaemon::start_after(delay, &socket, empty_queue(tmp.path()));

    let deadline = Instant::now() + Duration::from_secs(5);
    let reply = loop {
        match client::request(&socket, &Request::List) {
            Ok(reply) => break reply,
            Err(e) => assert!(
                Instant::now() < deadline,
                "the stand-in never answered on {}: {e}",
                socket.display()
            ),
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let waited = started.elapsed();
    assert_eq!(reply, Response::Jobs(vec![]));
    // The delay is what the CLI suites use it for: with a daemon listening
    // from the start, they would stop testing a CLI that finds none.
    assert!(
        waited >= delay,
        "the stand-in answered after {waited:?}, inside its {delay:?} delay"
    );
}

/// The leak itself: the fixture goes before the delay is up.
#[test]
fn dropped_before_its_delay_it_leaves_nothing_behind() {
    let tmp = common::short_tempdir();
    let dir = tmp.path().to_path_buf();
    let run = dir.join("run");
    std::fs::create_dir_all(&run).unwrap();
    // Long, so that a test thread held up between starting the stand-in and
    // dropping it still drops it first.
    let delay = Duration::from_millis(500);
    let daemon =
        common::StandInDaemon::start_after(delay, &run.join("nudge.sock"), empty_queue(&dir));

    drop(daemon);
    drop(tmp);
    assert!(!dir.exists(), "the tempdir was not removed");

    // Well past the delay: whatever the thread was going to do, it has done.
    std::thread::sleep(delay * 2);
    assert!(
        !dir.exists(),
        "the stand-in bound after its fixture was gone, and left {} behind",
        dir.display()
    );
}

/// The same with the directory still there, where nothing but the stand-in
/// itself stops the bind.
#[test]
fn dropped_before_its_delay_it_never_binds() {
    let tmp = common::short_tempdir();
    let socket = tmp.path().join("nudge.sock");
    // Long, so that a test thread held up between starting the stand-in and
    // dropping it still drops it first.
    let delay = Duration::from_millis(500);

    drop(common::StandInDaemon::start_after(
        delay,
        &socket,
        empty_queue(tmp.path()),
    ));

    std::thread::sleep(delay * 2);
    assert!(
        !socket.exists(),
        "the stand-in bound {} after it was dropped",
        socket.display()
    );
}

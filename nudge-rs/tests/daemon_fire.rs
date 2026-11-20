//! End-to-end: the daemon fires a due job into a real tmux pane. Self-skips
//! without tmux.
//!
//! Hermetic in the way `daemon_batch.rs` is, because the daemon types into a
//! tmux pane:
//! - The daemon is the real `nudge --daemon`, started by production
//!   `spawn_daemon` and killed on drop, before the tmux server it types into
//!   is stopped. It used to be `nudge::daemon::run` on a thread of the test,
//!   which nothing can stop: a test that failed midway took its tmux server
//!   and tempdir away from under a daemon still firing, and tmux then falls
//!   back to the developer's own server (see `common::TmuxServer::env`).
//!   The price is the one `daemon_batch.rs` pays: a run that is interrupted
//!   rather than failed drops nothing, and leaves the daemon running beside
//!   its private tmux server, where the thread died with the test.
//! - HOME/XDG_* point at a tempdir, so the queue and socket are ours.
//! - `TMUX_TMPDIR` points at a private tmux server, and `TMUX`/`TMUX_PANE`
//!   are removed, so neither the daemon nor this test can reach the
//!   developer's own server. The job names no socket, so the daemon finds
//!   its pane the way a real one does: on the server its environment leads
//!   to.
//!
//! Single test in this file on purpose: it sets process-wide env, which the
//! daemon inherits and which no sibling test may race.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use jiff::ToSpan;
use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{Os, Paths, resolve_from};
use nudge::queue::Queue;

/// The session the job is fired into, which is also how its pane is named:
/// a bare session name is that session's current pane.
const SESSION: &str = "fire";

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

#[test]
fn daemon_fires_a_due_job_into_the_pane() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }

    // common::short_tempdir, for the socket path's length on macOS (see
    // cli_daemon_log.rs).
    //
    // What follows is declared in the order tempdir, tmux server, daemon, and
    // so dropped in the reverse: the daemon is killed before the server it
    // types into is stopped, and the server is stopped before its directory
    // goes.
    let tmp = common::short_tempdir();
    let home = tmp.path().to_path_buf();
    let tmux_tmpdir = home.join("tmux");
    std::fs::create_dir_all(&tmux_tmpdir).unwrap();

    // Set before anything is spawned: the daemon inherits all of it, resolves
    // its own state dir from it, and runs its tmux calls under it.
    //
    // SAFETY: nothing else reads the environment while it changes. This is the
    // only test in its binary (see the module doc), and it has started no
    // thread of its own yet.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_STATE_HOME", home.join("state"));
        std::env::set_var("XDG_RUNTIME_DIR", home.join("run"));
        common::keep_this_process_off_the_developers_panes(&home);
    }

    let tmux = common::TmuxServer::start(&tmux_tmpdir, SESSION);
    // The server this test and its daemon reach is the one in the tempdir.
    // tmux resolves the directory before naming its socket, and macOS's /tmp
    // is a link, hence `canonicalize`.
    let socket = tmux.run(&["display-message", "-p", "#{socket_path}"]);
    let ours = std::fs::canonicalize(&home).unwrap();
    assert!(
        Path::new(&socket).starts_with(&ours),
        "the test's tmux server is at {socket}, outside {}",
        ours.display()
    );

    // Stage a rate-limit banner for realism, even though this job uses
    // `verify: false` so the banner isn't required for the send to happen.
    tmux.run(&[
        "send-keys",
        "-t",
        SESSION,
        "-l",
        "printf 'quota reached. Resets in 45m\\n'",
    ]);
    tmux.run(&["send-keys", "-t", SESSION, "Enter"]);

    let paths: Paths = resolve_from(
        &home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    );
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();

    // Write a DUE job straight into the queue file so the daemon fires it on
    // its first pass (catch-up), not after waiting out a poll interval.
    let fire_at = jiff::ZonedDateTime::now()
        .checked_sub(1.second())
        .unwrap()
        .timestamp();
    let mut queue = Queue::load(paths.queue.clone()).unwrap();
    queue
        .add(JobSpec {
            target: TargetSpec::Tmux {
                pane: SESSION.into(),
                server: None,
            },
            messages: vec!["echo daemon_marker_$((6*7))".into()],
            send_delay_secs: 0.0,
            fire_at,
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
            verify_no_banner: false,
        })
        .unwrap();
    drop(queue); // done writing; the daemon will `Queue::load` its own handle

    let mut daemon = common::DaemonGuard::new(
        nudge::app::spawn_daemon(Path::new(env!("CARGO_BIN_EXE_nudge")), &paths)
            .expect("spawn the daemon"),
    );
    let log = paths.state_dir.join("nudge.log");

    // Poll until `done` holds, or fail saying `what` never happened. A daemon
    // that has exited fails it at once, with its log: a wait that only times
    // out says nothing about why.
    let mut wait = |what: &str, done: &dyn Fn() -> Result<(), String>| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let Err(seen) = done() else { return };
            let exited = daemon.has_exited().then(|| daemon.stop()).flatten();
            if exited.is_some() || Instant::now() >= deadline {
                let log = std::fs::read_to_string(&log).unwrap_or_default();
                panic!("{what} (daemon exit: {exited:?}); {seen}\ndaemon log:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };

    // The injected echo has actually run.
    wait("the daemon never fired the due job into the pane", &|| {
        let screen = tmux.run(&["capture-pane", "-p", "-t", SESSION]);
        if screen.contains("daemon_marker_42") {
            Ok(())
        } else {
            Err(format!("last capture:\n{screen}"))
        }
    });

    // A fired job leaves the queue.
    wait("the fired job never left the queue", &|| {
        let left = Queue::load(paths.queue.clone()).unwrap().all().to_vec();
        if left.is_empty() {
            Ok(())
        } else {
            Err(format!("still queued: {left:?}"))
        }
    });
}

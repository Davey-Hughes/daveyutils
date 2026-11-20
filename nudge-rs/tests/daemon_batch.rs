//! A daemon fires the jobs due in one pass one at a time. A job waiting its
//! turn has not started firing, so cancelling or editing it must work as it
//! would for any job not yet due.
//!
//! Hermetic in the way `cli_daemon_log.rs` is, because this test runs a real
//! daemon that types into tmux panes:
//! - The daemon is the real `nudge --daemon`, started by production
//!   `spawn_daemon` and killed on drop, so nothing outlives the test.
//! - HOME/XDG_* point at a tempdir, so the queue and socket are ours.
//! - `TMUX_TMPDIR` points at a private tmux server, and `TMUX`/`TMUX_PANE`
//!   are removed, so neither the daemon nor this test can reach the
//!   developer's own server.
//!
//! Single test in this file on purpose: it sets process-wide env, which the
//! daemon inherits and which no sibling test may race.

mod common;

use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use jiff::ToSpan;
use nudge::ipc::{Request, Response, client};
use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{Os, Paths, resolve_from};
use nudge::queue::Queue;

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// Kills the daemon we spawned, on any exit path.
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A job due now on tmux session `session`'s pane, sending `messages`
/// `delay` seconds apart.
fn due_job(session: &str, messages: &[&str], delay: f64) -> JobSpec {
    JobSpec {
        target: TargetSpec::Tmux {
            pane: session.into(),
            server: None,
        },
        messages: messages.iter().map(|m| (*m).to_string()).collect(),
        send_delay_secs: delay,
        fire_at: jiff::ZonedDateTime::now()
            .checked_sub(1.second())
            .unwrap()
            .timestamp(),
        notify: false,
        verify: false,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: None,
        verify_dims: None,
        verify_no_banner: false,
    }
}

/// The tmux server and daemon one test runs against. Fields drop in order,
/// so the daemon is killed before the server it types into.
struct Rig {
    daemon: DaemonGuard,
    tmux: common::TmuxServer,
    /// The daemon's log, where it says why it stopped.
    log: std::path::PathBuf,
}

impl Rig {
    fn capture(&self, session: &str) -> String {
        self.tmux.run(&["capture-pane", "-p", "-t", session])
    }

    /// Poll `session`'s pane until it shows `marker`, or fail after `secs`.
    /// A daemon that has exited fails it at once, with its log: a marker
    /// that never appears says nothing about why.
    fn wait_for(&mut self, session: &str, marker: &str, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let screen = self.capture(session);
            if screen.contains(marker) {
                return;
            }
            let exited = self.daemon.0.try_wait().ok().flatten();
            if exited.is_some() || Instant::now() >= deadline {
                let log = std::fs::read_to_string(&self.log).unwrap_or_default();
                panic!(
                    "{marker} never appeared in {session} (daemon exit: \
                     {exited:?}); last capture:\n{screen}\ndaemon log:\n{log}"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Three jobs are due together. While the first is still being typed, the
/// second is cancelled and the third is edited. Neither of the later two had
/// started, so:
///
/// - the cancelled job is never typed into its pane, though the daemon had
///   already planned to fire it;
/// - the edit is accepted, not refused as "firing", and only the edited
///   message arrives, once.
#[test]
fn jobs_waiting_their_turn_can_be_cancelled_or_edited() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }

    // common::short_tempdir, for the socket path's length on macOS (see
    // cli_daemon_log.rs).
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
        std::env::set_var("TMUX_TMPDIR", &tmux_tmpdir);
        std::env::remove_var("TMUX");
        std::env::remove_var("TMUX_PANE");
    }

    let tmux = common::TmuxServer::start(&tmux_tmpdir, "slow");
    for session in ["cancelled", "edited"] {
        tmux.run(&[
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "80",
            "-y",
            "24",
            "sh",
        ]);
    }

    let paths: Paths = resolve_from(
        &home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    );
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();

    let mut queue = Queue::load(paths.queue.clone()).unwrap();
    // The window the other two wait through. On a plain shell each message is
    // read back for about two seconds after it is sent, looking for an input
    // box it will not find, so this job fires for about five.
    let slow_id = queue
        .add(due_job(
            "slow",
            &["echo slow_a_$((6*7))", "echo slow_b_$((6*7))"],
            0.5,
        ))
        .unwrap();
    let cancel_id = queue
        .add(due_job("cancelled", &["echo cancelled_$((6*7))"], 0.0))
        .unwrap();
    let edit_spec = due_job("edited", &["echo original_$((6*7))"], 0.0);
    let edit_id = queue.add(edit_spec.clone()).unwrap();
    let seen = queue.get(edit_id).unwrap().progress();
    assert_eq!((slow_id, cancel_id, edit_id), (1, 2, 3));
    drop(queue);

    let daemon = DaemonGuard(
        nudge::app::spawn_daemon(Path::new(env!("CARGO_BIN_EXE_nudge")), &paths)
            .expect("spawn the daemon"),
    );
    let mut rig = Rig {
        daemon,
        tmux,
        log: paths.state_dir.join("nudge.log"),
    };

    // The first job is being typed: the other two are waiting their turn.
    rig.wait_for("slow", "slow_a_42", 20);
    let request = |req: &Request| client::request(&paths.socket, req).unwrap();
    assert_eq!(
        request(&Request::Cancel(cancel_id)),
        Response::Cancelled(true)
    );
    let replaced = request(&Request::Replace {
        id: edit_id,
        seen,
        spec: JobSpec {
            messages: vec!["echo edited_$((6*7))".into()],
            ..edit_spec
        },
    });
    assert!(
        matches!(replaced, Response::Replaced(Some(_))),
        "a job that has not started firing must take an edit, got {replaced:?}"
    );

    // The edit's replacement fires on the daemon's next pass, after the
    // first job's batch: once it has, the batch is over.
    rig.wait_for("slow", "slow_b_42", 20);
    rig.wait_for("edited", "edited_42", 20);
    std::thread::sleep(Duration::from_millis(500));
    let screen = rig.capture("cancelled");
    assert!(
        !screen.contains("cancelled_42"),
        "a job cancelled before its turn was fired anyway:\n{screen}"
    );
    let screen = rig.capture("edited");
    assert!(
        !screen.contains("original_42"),
        "the edited job's old message was fired too:\n{screen}"
    );
    assert_eq!(
        screen.matches("edited_42").count(),
        1,
        "the edit must fire once:\n{screen}"
    );
}

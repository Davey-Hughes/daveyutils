//! The auto-started daemon's diagnostics must land somewhere a user can read.
//!
//! The recency design has an observability clause because "silence is
//! indistinguishable from never having run" — a `--verify` skip is the outcome
//! that most needs saying out loud, since it looks exactly like the failure the
//! whole design exists to prevent. The daemon does report it
//! (`tracing::info!("nudge: job {} -> {:?}")`), but the daemon the CLI starts
//! for you was spawned with `.stderr(Stdio::null())`, so that line went to
//! /dev/null. Only `--notify` made a skip visible. The *default* configuration
//! — auto-started daemon, no --notify — got precisely the silence the design
//! set out to eliminate, and the same hole swallowed every `tracing::error!`
//! and `warn!` the daemon has.
//!
//! Hermetic, and deliberately so, because this is the one test that runs a real
//! resident daemon:
//! - HOME/XDG_* point at a tempdir, so the queue, socket and log are ours.
//! - `TargetSpec::Tmux` carries no socket (increment 3b), so the daemon's tmux
//!   calls have no `-L` to isolate them. `TMUX_TMPDIR` is what keeps them off
//!   the developer's server, and `TMUX` — which names the socket of the session
//!   running this test, and which tmux prefers — is removed.
//! - The daemon is spawned by production `spawn_daemon` and killed on drop, so
//!   nothing outlives the test.
//!
//! Single test in this file on purpose: it sets process-wide env, which the
//! daemon inherits and which no sibling test may race.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{resolve_from, Os, Paths};
use nudge::queue::Queue;

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// A tmux server private to this test, living under its own `TMUX_TMPDIR`.
///
/// Every command carries the env explicitly rather than trusting the process
/// env: a `kill-server` that leaked to the default socket would take down the
/// developer's entire tmux, so it must not be possible for one to.
struct Server {
    tmpdir: PathBuf,
}

impl Server {
    fn start(tmpdir: &Path, session: &str) -> Self {
        let server = Server {
            tmpdir: tmpdir.to_path_buf(),
        };
        let ok = server
            .tmux()
            .args([
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .status()
            .expect("spawn tmux")
            .success();
        assert!(ok, "failed to start the private tmux session");
        server
    }

    fn tmux(&self) -> Command {
        let mut c = Command::new("tmux");
        c.env("TMUX_TMPDIR", &self.tmpdir).env_remove("TMUX");
        c
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Safe only because of the explicit TMUX_TMPDIR + TMUX removal above:
        // this can address nothing but the server we started in our own tempdir.
        let _ = self.tmux().arg("kill-server").status();
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

#[test]
fn a_verify_skip_lands_in_the_daemon_log() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let tmux_tmpdir = home.join("tmux");
    std::fs::create_dir_all(&tmux_tmpdir).unwrap();

    // Set before anything is spawned: the daemon inherits all of it, resolves
    // its own state dir from it, and runs its tmux calls under it.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_STATE_HOME", home.join("state"));
    std::env::set_var("XDG_RUNTIME_DIR", home.join("run"));
    std::env::set_var("TMUX_TMPDIR", &tmux_tmpdir);
    std::env::remove_var("TMUX");

    let session = format!("nudgelog_{}", std::process::id());
    let server = Server::start(&tmux_tmpdir, &session);

    let paths: Paths = resolve_from(
        &home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    );
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();

    // A --verify job, already due, against a pane showing a plain shell prompt
    // and no rate-limit banner: the daemon's first pass fires it, the banner
    // check finds nothing, and the outcome is a skip. `notify: false` is the
    // point -- this is the default configuration, the one that was silent.
    let fire_at = jiff::Zoned::now()
        .checked_sub(jiff::ToSpan::second(1))
        .unwrap()
        .timestamp();
    let mut queue = Queue::load(paths.queue.clone()).unwrap();
    queue
        .add(JobSpec {
            target: TargetSpec::Tmux {
                pane: session.clone(),
            },
            messages: vec!["echo this_must_not_be_sent".into()],
            send_delay_secs: 0.0,
            fire_at,
            notify: false,
            verify: true,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        })
        .unwrap();
    drop(queue);

    // Production spawn, production stderr wiring, the real nudge binary.
    let _daemon = DaemonGuard(
        nudge::app::spawn_daemon(Path::new(env!("CARGO_BIN_EXE_nudge")), &paths)
            .expect("spawn the daemon"),
    );

    let log = paths.state_dir.join("nudge.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    let contents = loop {
        let c = std::fs::read_to_string(&log).unwrap_or_default();
        if c.contains("SkippedNoBanner") {
            break c;
        }
        assert!(
            Instant::now() < deadline,
            "the skip never reached {}: a user whose nudge did not fire has \
             nothing to look at, which is the silence the design forbids.\n\
             log so far: {c:?}",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    assert!(
        contents.contains("job 1"),
        "the log must name the job that skipped; got:\n{contents}"
    );

    // The skip is real, not just logged: nothing was typed into the pane.
    let out = server
        .tmux()
        .args(["capture-pane", "-p", "-t", &session])
        .output()
        .unwrap();
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(
        !screen.contains("this_must_not_be_sent"),
        "the job skipped, so nothing may have reached the pane; got:\n{screen}"
    );
}

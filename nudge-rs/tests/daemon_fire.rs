//! End-to-end: the daemon fires a due job into a real tmux pane. Self-skips
//! without tmux.
//!
//! `TargetSpec::Tmux` carries no socket yet (increment 3b), so
//! `job.target.connect()` always builds a `TmuxTarget` on tmux's *default*
//! socket. To reach it hermetically without disturbing the developer's real
//! tmux usage, this test starts a uniquely-named throwaway *session* on the
//! default server (sharing the server, touching only that one session) and
//! kills exactly that session (never the server) on drop.
//!
//! State (queue/socket) lives in a tempdir; `nudge::daemon::run` is spawned on
//! a thread that loops forever and is reaped at process exit.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::ToSpan;
use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::Paths;
use nudge::queue::Queue;

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Owns a uniquely-named throwaway session on the *default* tmux server; kills
/// only that session (not the server) on drop, since other tests/tmux usage
/// share the default server.
struct Session {
    name: String,
}

impl Session {
    fn start(name: &str) -> Self {
        let ok = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .status()
            .expect("spawn tmux")
            .success();
        assert!(ok, "failed to start throwaway tmux session {name}");
        Session {
            name: name.to_string(),
        }
    }

    /// The pane spec addressing this session's active pane. Deliberately just
    /// the session name (not `"<name>:0.0"`): tmux's `base-index`/
    /// `pane-base-index` options are user-configurable (this host's default
    /// server has `base-index 1`, so window `0` doesn't exist there), and a
    /// bare session name always resolves to that session's current pane
    /// regardless of index configuration.
    fn pane(&self) -> String {
        self.name.clone()
    }

    fn capture(&self) -> String {
        let out = Command::new("tmux")
            .args(["capture-pane", "-p", "-t", &self.pane()])
            .output()
            .expect("capture-pane");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Kill the SESSION by name, not the server: the default tmux server
        // may be shared by other tests or by the developer's real tmux.
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .status();
    }
}

#[test]
fn daemon_fires_a_due_job_into_the_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }

    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let session_name = format!("nudge_daemon_{}_{}", std::process::id(), n);
    let session = Session::start(&session_name);

    // Stage a rate-limit banner for realism, even though this job uses
    // `verify: false` so the banner isn't required for the send to happen.
    Command::new("tmux")
        .args([
            "send-keys",
            "-t",
            &session_name,
            "-l",
            "printf 'quota reached. Resets in 45m\\n'",
        ])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["send-keys", "-t", &session_name, "Enter"])
        .status()
        .unwrap();

    // Hermetic daemon state: queue + socket in a tempdir.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        state_dir: dir.path().to_path_buf(),
        queue: dir.path().join("queue.json"),
        socket: dir.path().join("nudge.sock"),
    };

    // Write a DUE job straight into the queue file so the daemon fires it on
    // its first pass (catch-up), not after waiting out a poll interval.
    let fire_at = jiff::Zoned::now()
        .checked_sub(1.second())
        .unwrap()
        .timestamp();
    let mut queue = Queue::load(paths.queue.clone()).unwrap();
    queue
        .add(JobSpec {
            target: TargetSpec::Tmux {
                pane: session.pane(),
            },
            messages: vec!["echo daemon_marker_$((6*7))".into()],
            send_delay_secs: 0.0,
            fire_at,
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
        })
        .unwrap();
    drop(queue); // done writing; the daemon will `Queue::load` its own handle

    // Spawn the daemon: loops forever, reaped at process exit.
    let daemon_paths = paths.clone();
    std::thread::spawn(move || {
        let _ = nudge::daemon::run(&daemon_paths, None, None, 6.hours());
    });

    // Poll the pane until the injected echo has actually run.
    let deadline = Instant::now() + Duration::from_secs(8);
    let screen = loop {
        let screen = session.capture();
        if screen.contains("daemon_marker_42") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never fired the due job into the pane; last capture:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        screen.contains("daemon_marker_42"),
        "pane missing the marker after daemon fired; got:\n{screen}"
    );
}

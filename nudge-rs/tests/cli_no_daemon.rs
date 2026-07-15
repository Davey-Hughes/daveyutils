//! `--list`, `--cancel` and `--edit` must work when no daemon is running.
//!
//! Jobs live in queue.json and outlive the daemon, so after a reboot (or any
//! daemon exit — nothing restarts an ad-hoc one) the queue still holds jobs that
//! will fire the moment anything starts a daemon again. Only `schedule` called
//! `ensure_daemon`, so the job-management commands hit a socket that wasn't
//! there and died with a raw errno — and the user could not cancel a job that
//! was still going to fire.
//!
//! These drive the real `nudge` binary against an isolated HOME/XDG state dir.
//! No real daemon survives, by construction: each test holds the singleton lock
//! on its own state dir, so the daemon the CLI spawns loses the lock and exits
//! immediately (`lib::run` maps WouldBlock to a clean exit). Standing in for it
//! is an in-process IPC server that binds the socket *after a delay* — the delay
//! is the point, since it is what makes "no daemon is listening when the CLI
//! starts" true, and a server bound up front would let the pre-fix code pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nudge::ipc::server;
use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{resolve_from, Os, Paths};
use nudge::queue::Queue;

/// How long the stand-in daemon waits before binding. Long enough that the CLI
/// has certainly made its first (failing) Ping — process exec is tens of ms —
/// and far inside `ensure_daemon`'s 5s window, so it is not a race.
const DAEMON_START_DELAY: Duration = Duration::from_millis(750);

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// The paths the CLI child will resolve from the env we hand it.
fn child_paths(home: &Path) -> Paths {
    resolve_from(
        home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    )
}

fn spec() -> JobSpec {
    JobSpec {
        target: TargetSpec::Tmux {
            pane: "bot:0.1".into(),
        },
        messages: vec!["go".into()],
        send_delay_secs: 0.75,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: false,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: None,
        verify_dims: None,
    }
}

/// An isolated state dir holding one persisted job, with the singleton lock
/// held so no real daemon can start, plus a stand-in daemon that begins serving
/// `queue` on the socket after `DAEMON_START_DELAY`.
struct Fixture {
    home: PathBuf,
    queue: Arc<Mutex<Queue>>,
    _tmp: tempfile::TempDir,
    _lock: std::fs::File,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();

    // Hold the lock: the daemon the CLI spawns will refuse to run and exit, so
    // this suite can never leave one behind.
    let lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();

    let queue = Arc::new(Mutex::new(Queue::load(paths.queue.clone()).unwrap()));
    queue.lock().unwrap().add(spec()).unwrap();

    let q = Arc::clone(&queue);
    let socket = paths.socket.clone();
    std::thread::spawn(move || {
        std::thread::sleep(DAEMON_START_DELAY);
        let _ = server::serve(&socket, q);
    });

    Fixture {
        home,
        queue,
        _tmp: tmp,
        _lock: lock,
    }
}

fn run_nudge(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nudge"))
        .args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .output()
        .expect("failed to run the nudge binary")
}

fn report(what: &str, out: &Output) -> String {
    format!(
        "{what}: status {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn cancel_works_with_no_daemon_running() {
    // The finding's core: a persisted job the user cannot cancel is a job that
    // still fires. Pre-fix this died with "No such file or directory (os error
    // 2)" and job 1 stayed in the queue, ready to fire on the next daemon start.
    let f = fixture();
    let out = run_nudge(&f.home, &["--cancel", "1"]);

    assert!(out.status.success(), "{}", report("cancel", &out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cancelled job 1"),
        "{}",
        report("cancel", &out)
    );
    assert!(
        f.queue.lock().unwrap().all().is_empty(),
        "the job must be gone from the queue the daemon serves"
    );
}

#[test]
fn list_works_with_no_daemon_running() {
    let f = fixture();
    let out = run_nudge(&f.home, &["--list"]);

    assert!(out.status.success(), "{}", report("list", &out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("bot:0.1"),
        "{}",
        report("list", &out)
    );
}

#[test]
fn edit_works_with_no_daemon_running() {
    let f = fixture();
    let out = run_nudge(&f.home, &["--edit", "1", "-m", "now + 3 hours"]);

    assert!(out.status.success(), "{}", report("edit", &out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("edited job 1 -> 2"),
        "{}",
        report("edit", &out)
    );

    // Exactly one job survives an edit: the replacement, at the new time. Two
    // would mean the message fires twice.
    let q = f.queue.lock().unwrap();
    assert_eq!(q.all().len(), 1, "an edit must leave exactly one job");
    assert_eq!(q.all()[0].id, 2);
    assert_ne!(
        q.all()[0].fire_at,
        spec().fire_at,
        "the replacement must carry the new fire time"
    );
}

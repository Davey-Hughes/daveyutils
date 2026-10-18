//! What the CLI says once a job is queued or replaced.
//!
//! `nudge -a` used to confirm with `nudge: scheduled job 3 for
//! 2026-07-16T15:00:00Z` -- an id, a UTC timestamp, and nothing else. `--auto`
//! is the mode where that is least sufficient: it picks the pane for you, so
//! the one fact the confirmation most needs to carry is the one it omitted.
//! `--edit` was thinner still (`edited job 1 -> 2`), and it is the mode that
//! carries forward every field no flag overrode. Both now print the summary the
//! dashboard prints, so what the user reads does not depend on how they got
//! there.
//!
//! Fixture shape follows the other CLI suites: an isolated HOME with the
//! singleton lock held (so the daemon the CLI spawns loses it and exits) and an
//! in-process server standing in on the socket.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nudge::ipc::server;
use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{Os, Paths, resolve_from};
use nudge::queue::Queue;

const DAEMON_START_DELAY: Duration = Duration::from_millis(750);

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

fn child_paths(home: &Path) -> Paths {
    resolve_from(
        home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    )
}

/// The job `--edit` starts from: a pane and a message it must carry forward
/// when the only flag passed is a new time.
fn seed() -> JobSpec {
    JobSpec {
        target: TargetSpec::Tmux {
            pane: "bot:0.1".into(),
        },
        messages: vec!["keep going".into()],
        send_delay_secs: 0.75,
        fire_at: "2026-07-16T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: false,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: None,
        verify_dims: None,
    }
}

struct Fixture {
    home: PathBuf,
    _tmp: tempfile::TempDir,
    _lock: std::fs::File,
}

fn fixture(seeded: &[JobSpec]) -> Fixture {
    let tmp = common::short_tempdir();
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    let lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();
    let queue = Arc::new(Mutex::new(Queue::load(paths.queue.clone()).unwrap()));
    for spec in seeded {
        queue.lock().unwrap().add(spec.clone()).unwrap();
    }
    let socket = paths.socket.clone();
    std::thread::spawn(move || {
        std::thread::sleep(DAEMON_START_DELAY);
        let _ = server::serve(&socket, &queue);
    });
    Fixture {
        home,
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
        .env("TZ", "UTC")
        .output()
        .expect("failed to run the nudge binary")
}

fn stdout_of(what: &str, out: &Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{what}: status {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    stdout
}

fn assert_describes_the_job(what: &str, stdout: &str) {
    for expected in ["(in 2h", "bot:0.1", "\"keep going\""] {
        assert!(
            stdout.contains(expected),
            "{what} must carry {expected:?}, got:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("15:00:00Z"),
        "{what} still prints the raw UTC timestamp this replaced:\n{stdout}"
    );
}

/// `-p` stands in for `--auto` here: the two differ only in how the pane is
/// chosen (`--auto` needs a live tmux server to choose from), and they share
/// every line of the confirmation path after that.
#[test]
fn a_scheduled_job_is_confirmed_with_its_pane_message_and_flags() {
    let f = fixture(&[]);
    let out = run_nudge(
        &f.home,
        &[
            "-p",
            "bot:0.1",
            "-m",
            "now + 2 hours",
            "--notify",
            "-i",
            "keep going",
        ],
    );

    let stdout = stdout_of("schedule", &out);
    assert!(
        stdout.contains("nudge: scheduled job 1 for"),
        "the id must survive the richer line:\n{stdout}"
    );
    assert_describes_the_job("the schedule confirmation", &stdout);
    assert!(
        stdout.contains("notify"),
        "flags belong in the confirmation:\n{stdout}"
    );
}

/// An edit passing only `-m` carries the pane and message forward. The
/// confirmation is the only place the user sees what was carried.
#[test]
fn an_edited_job_is_confirmed_with_what_the_merge_carried_forward() {
    let f = fixture(&[seed()]);
    let out = run_nudge(&f.home, &["--edit", "1", "-m", "now + 2 hours"]);

    let stdout = stdout_of("edit", &out);
    assert!(
        stdout.contains("nudge: edited job 1 -> 2 for"),
        "both ids must survive the richer line:\n{stdout}"
    );
    assert_describes_the_job("the edit confirmation", &stdout);
}

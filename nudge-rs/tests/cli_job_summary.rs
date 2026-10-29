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
            server: None,
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
        // Off the developer's tmux, always: a test that wants a pane starts
        // one with `two_panes`, and every other tmux call finds no server.
        .env("TMUX_TMPDIR", home.join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("failed to run the nudge binary")
}

/// A private tmux server for `run_nudge`, holding panes `bot:0.0` and `bot:0.1`.
fn two_panes(home: &Path) -> common::TmuxServer {
    let server = common::TmuxServer::start(&home.join("tmux"), "bot");
    server.run(&["split-window", "-t", "bot:0", "sh"]);
    server
}

/// The jobs in the fixture's queue, as the in-process server persisted them.
fn queued(home: &Path) -> Vec<nudge::job::Job> {
    Queue::load(child_paths(home).queue).unwrap().all().to_vec()
}

fn pane_of(job: &nudge::job::Job) -> &str {
    let TargetSpec::Tmux { pane, .. } = &job.target;
    pane
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
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let f = fixture(&[]);
    let _tmux = two_panes(&f.home);
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

// ---- the pane is stored by its id ----

/// A job stores tmux's id for its pane (`%3`), not the position it was given
/// (`bot:0.1`). With `renumber-windows` on, closing any window before the
/// pane's hands its position to the next window along, and a nudge scheduled
/// for one session went to another.
#[test]
fn a_scheduled_job_stores_its_panes_id_not_its_position() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let f = fixture(&[]);
    let tmux = two_panes(&f.home);
    let id = tmux.run(&["display-message", "-p", "-t", "bot:0.1", "#{pane_id}"]);

    let out = run_nudge(&f.home, &["-p", "bot:0.1", "-m", "now + 2 hours"]);

    let stdout = stdout_of("schedule", &out);
    let jobs = queued(&f.home);
    assert_eq!(jobs.len(), 1);
    assert_eq!(pane_of(&jobs[0]), id);
    assert!(
        stdout.contains(&format!("bot:0.1 ({id})")),
        "the confirmation names the pane both ways:\n{stdout}"
    );
}

/// The bug end to end: the windows renumber after scheduling, and the job
/// still names the pane it was given, now at its new position.
#[test]
fn the_job_list_follows_the_pane_when_the_windows_renumber() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let f = fixture(&[]);
    let tmux = common::TmuxServer::start(&f.home.join("tmux"), "bot");
    tmux.run(&["set-option", "-g", "renumber-windows", "on"]);
    tmux.run(&["new-window", "-t", "bot:1", "sh"]);
    tmux.run(&["new-window", "-t", "bot:2", "sh"]);
    let id = tmux.run(&["display-message", "-p", "-t", "bot:2.0", "#{pane_id}"]);
    stdout_of(
        "schedule",
        &run_nudge(&f.home, &["-p", "bot:2.0", "-m", "now + 2 hours"]),
    );

    tmux.run(&["kill-window", "-t", "bot:1"]);

    assert_eq!(pane_of(&queued(&f.home)[0]), id);
    let list = stdout_of("list", &run_nudge(&f.home, &["--list"]));
    assert!(
        list.contains(&format!("bot:1.0 ({id})")),
        "the list shows where the pane is now:\n{list}"
    );
}

/// A pane tmux does not have is refused at schedule time. Stored by name, it
/// would only have failed hours later, at the reset the user is waiting on.
#[test]
fn scheduling_into_a_pane_that_does_not_exist_fails_and_queues_nothing() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let f = fixture(&[]);
    let _tmux = two_panes(&f.home);

    let out = run_nudge(&f.home, &["-p", "bot:7.0", "-m", "now + 2 hours"]);

    assert!(
        !out.status.success(),
        "a pane that is not there must be refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bot:7.0"), "name the pane: {stderr}");
    assert_eq!(queued(&f.home), vec![]);
}

/// `--edit -p` moves the job, and the pane it moves to is stored by id too.
#[test]
fn an_edit_that_moves_the_job_stores_the_new_panes_id() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let f = fixture(&[seed()]);
    let tmux = two_panes(&f.home);
    let id = tmux.run(&["display-message", "-p", "-t", "bot:0.0", "#{pane_id}"]);

    let out = run_nudge(&f.home, &["--edit", "1", "-p", "bot:0.0"]);

    let stdout = stdout_of("edit", &out);
    let jobs = queued(&f.home);
    assert_eq!(jobs.len(), 1);
    assert_eq!(pane_of(&jobs[0]), id);
    assert!(stdout.contains(&format!("bot:0.0 ({id})")), "{stdout}");
}

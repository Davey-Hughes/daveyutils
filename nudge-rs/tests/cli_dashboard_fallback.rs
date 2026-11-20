//! On a non-TTY (a pipe/CI), the dashboard degrades to the static table for
//! bare `nudge`, `--list`, and `--list-plain` alike — the CLI contract's
//! fallback, guaranteed without a pty.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn spec() -> JobSpec {
    JobSpec {
        target: TargetSpec::Tmux {
            pane: "bot:0.1".into(),
            server: None,
        },
        messages: vec!["go".into()],
        send_delay_secs: 0.75,
        fire_at: "2026-07-16T15:00:00Z".parse().unwrap(),
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

struct Fixture {
    home: PathBuf,
    // Above `_tmp` and `_lock`, so that it is dropped first: see
    // `common::Descendants`.
    cli: common::Descendants,
    // Above `_tmp`, so that it is dropped first: see `common::StandInDaemon`.
    _daemon: common::StandInDaemon,
    _tmp: tempfile::TempDir,
    _lock: std::fs::File,
}

fn fixture() -> Fixture {
    let tmp = common::short_tempdir();
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    let lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();
    let queue = Arc::new(Mutex::new(Queue::load(paths.queue.clone()).unwrap()));
    queue.lock().unwrap().add(spec()).unwrap();
    let daemon = common::StandInDaemon::start_after(DAEMON_START_DELAY, &paths.socket, queue);
    Fixture {
        home,
        cli: common::Descendants::new(),
        _daemon: daemon,
        _tmp: tmp,
        _lock: lock,
    }
}

fn run_nudge(f: &Fixture, args: &[&str]) -> Output {
    let home = &f.home;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nudge"));
    f.cli.follow(&mut cmd);
    // The job list labels its panes from every pane tmux and herdr can list.
    common::keep_off_the_developers_panes(&mut cmd, home);
    cmd.args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .output()
        .expect("failed to run the nudge binary")
}

fn assert_prints_table(args: &[&str]) {
    let f = fixture();
    let out = run_nudge(&f, args);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "args {args:?}: status {:?}\n{stdout}",
        out.status.code()
    );
    assert!(
        stdout.contains("bot:0.1"),
        "non-TTY {args:?} must print the static table, got:\n{stdout}"
    );
}

#[test]
fn bare_nudge_non_tty_prints_the_table() {
    assert_prints_table(&[]);
}

#[test]
fn list_non_tty_prints_the_table() {
    assert_prints_table(&["--list"]);
}

#[test]
fn list_plain_prints_the_table() {
    assert_prints_table(&["--list-plain"]);
}

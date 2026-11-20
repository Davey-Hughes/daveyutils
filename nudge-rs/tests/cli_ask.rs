//! `nudge -p <pane> --ask` over a pane with no rate-limit banner, through the
//! real binary in a real terminal: a tmux pane on a private tmux server.
//!
//! What only this can show: that the form opens on a terminal and not off
//! one, that it opens on the pane `-p` named and keeps it, that a time typed
//! into it reaches the daemon's queue, and that quitting it leaves nothing on
//! the terminal.
//!
//! Hermetic: the tmux server lives under its own `TMUX_TMPDIR` (see
//! [`common::TmuxServer`]), and `HOME`/`XDG_*` point into a tempdir, so the
//! queue, the socket and the daemon are the test's own. The daemon is this
//! test's child, killed by handle on drop. The nudge in the pane finds it
//! answering and never starts one of its own.

mod common;

use std::fmt::Write as _;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nudge::ipc::{Request, Response, client};
use nudge::job::{Job, TargetSpec};
use nudge::paths::{Os, Paths, resolve_from};

use common::TmuxServer;

const PATIENCE: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(100);

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// The paths a `nudge` given [`nudge_env`] resolves.
fn child_paths(home: &Path) -> Paths {
    resolve_from(
        home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    )
}

/// The environment every nudge here runs with, the daemon's included.
fn nudge_env(home: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join("config")),
        ("XDG_STATE_HOME", home.join("state")),
        ("XDG_RUNTIME_DIR", home.join("run")),
    ]
}

/// `nudge` with [`nudge_env`], aimed at `server`, and with nothing inherited
/// that could lead it to the developer's tmux or herdr.
fn nudge(home: &Path, server: &TmuxServer) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nudge"));
    common::keep_off_the_developers_panes(&mut cmd, home);
    server.env(&mut cmd);
    cmd.envs(nudge_env(home));
    cmd
}

/// A private tmux server whose first pane is a decoy, so that a fallback to
/// the first pane in the list would show; the pane the nudge is for, with no
/// banner on it; and the home and paths nudge runs with.
///
/// Fields drop in the order they are declared, and the order is load-bearing:
/// the server's `kill-server` must run while its `TMUX_TMPDIR`, inside the
/// tempdir, still exists. Once the directory is gone, tmux falls back to the
/// default socket directory, and `kill-server` there is the developer's own
/// tmux. So `server` comes first and `_tmp` last. The watchdog is still
/// there while the server is stopped, in case the run is interrupted then.
struct Fixture {
    server: TmuxServer,
    home: PathBuf,
    paths: Paths,
    agent: String,
    watchdog: common::Watchdog,
    _tmp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let tmp = common::short_tempdir();
    // First, so that a run that is interrupted leaves none of what follows.
    let watchdog = common::Watchdog::over(tmp.path());
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    std::fs::create_dir_all(home.join("config")).unwrap();
    let server = watchdog.start_tmux(&home.join("tmux"), "s");
    let agent = server.run(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "s:",
        "sh",
    ]);
    Fixture {
        server,
        home,
        paths,
        agent,
        watchdog,
        _tmp: tmp,
    }
}

impl Fixture {
    /// Start a daemon for this home, and wait until it answers.
    ///
    /// In a process group of its own, so that the guard can kill a `tmux`
    /// the daemon has in flight along with it (see `common::DaemonGuard`).
    /// That also puts it out of reach of a Ctrl-C at the terminal, which
    /// goes to the test's group, so the fixture's watchdog is told about it.
    fn daemon(&self) -> common::DaemonGuard {
        let log = std::fs::File::create(self.home.join("daemon.log")).unwrap();
        let child = nudge(&self.home, &self.server)
            .arg("--daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .process_group(0)
            .spawn()
            .expect("spawn the daemon");
        let guard = self.watchdog.guard(child);
        wait_until(
            || {
                matches!(
                    client::request(&self.paths.socket, &Request::Ping),
                    Ok(Response::Pong { .. })
                )
            },
            "the daemon to answer a ping",
        );
        guard
    }

    /// Run `nudge -p <agent> --ask` in a window of its own, so that the form
    /// has the whole 80x24. Once nudge exits, the pane prints its exit status
    /// and stays, so what nudge left on it can be read.
    fn ask_in_a_pane(&self) -> String {
        let mut line =
            String::from("env -u HERDR_SOCKET_PATH -u HERDR_PANE_ID -u HERDR_ACTIVE_PANE_ID");
        // Writing to a String cannot fail.
        for (key, value) in nudge_env(&self.home) {
            let _ = write!(line, " {key}='{}'", value.display());
        }
        let _ = write!(
            line,
            " '{}' -p '{}' --ask; echo nudge-exit=$?; exec sleep 600",
            env!("CARGO_BIN_EXE_nudge"),
            self.agent
        );
        self.server.run(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "s:",
            &line,
        ])
    }

    fn screen(&self, pane: &str) -> String {
        self.server.run(&["capture-pane", "-p", "-t", pane])
    }

    /// Poll `pane` until `cond` holds of what it shows.
    fn wait_for(&self, pane: &str, cond: impl Fn(&str) -> bool, what: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let screen = self.screen(pane);
            if cond(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\nlast capture:\n{screen}\ndaemon log:\n{}",
                std::fs::read_to_string(self.home.join("daemon.log")).unwrap_or_default()
            );
            std::thread::sleep(TICK);
        }
    }

    /// Wait for the form to open, on its reason and its manual time.
    fn wait_for_the_form(&self, pane: &str) {
        self.wait_for(
            pane,
            |s| s.contains("no rate-limit banner detected in") && s.contains("manual"),
            "the --ask form",
        );
    }

    fn jobs(&self) -> Vec<Job> {
        match client::request(&self.paths.socket, &Request::List) {
            Ok(Response::Jobs(jobs)) => jobs,
            other => panic!("listing jobs: {other:?}"),
        }
    }
}

fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(TICK);
    }
}

#[test]
fn ask_takes_a_typed_time_and_schedules_it() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let f = fixture();
    let _daemon = f.daemon();
    let pane = f.ask_in_a_pane();
    f.wait_for_the_form(&pane);

    let sent = jiff::Timestamp::now();
    f.server.run(&["send-keys", "-t", &pane, "5m", "Enter"]);
    let screen = f.wait_for(&pane, |s| s.contains("nudge-exit="), "nudge to exit");
    assert!(screen.contains("nudge-exit=0"), "{screen}");
    assert!(screen.contains("nudge: scheduled job"), "{screen}");

    let jobs = f.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = &jobs[0];
    assert!(
        matches!(&job.target, TargetSpec::Tmux { pane, .. } if *pane == f.agent),
        "the job is for the pane -p named, not the first in the list: {:?}",
        job.target
    );
    assert_eq!(job.messages, vec!["please continue".to_string()]);
    let due = sent.checked_add(jiff::ToSpan::minutes(5)).unwrap();
    let off = job.fire_at.duration_since(due).as_secs().abs();
    assert!(off <= 30, "fire_at {} is {off}s from {due}", job.fire_at);
}

#[test]
fn quitting_the_ask_form_schedules_nothing_and_prints_nothing() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let f = fixture();
    let _daemon = f.daemon();
    let pane = f.ask_in_a_pane();
    f.wait_for_the_form(&pane);

    f.server.run(&["send-keys", "-t", &pane, "C-c"]);
    let screen = f.wait_for(&pane, |s| s.contains("nudge-exit="), "nudge to exit");
    assert_eq!(screen.trim(), "nudge-exit=0");
    assert_eq!(f.jobs(), Vec::<Job>::new());
}

#[test]
fn ask_without_a_terminal_fails_as_before() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let f = fixture();
    let out = nudge(&f.home, &f.server)
        .args(["-p", &f.agent, "--ask"])
        .stdin(Stdio::null())
        .output()
        .expect("run nudge");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.starts_with("nudge: no rate-limit banner detected in "),
        "{stderr}"
    );
    assert!(stderr.contains("; pass -m to set a time"), "{stderr}");
}

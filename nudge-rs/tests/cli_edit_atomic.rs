//! `nudge --edit` must mutate the queue in ONE atomic request.
//!
//! The old edit did Schedule-then-Cancel. It scheduled first on purpose, so a
//! Schedule failure could never lose the job — but the reverse hazard was
//! unhandled: between the two round-trips both jobs are live, and if the Cancel
//! leg failed (daemon killed or restarted in the window, socket stolen, Ctrl-C)
//! the `?` surfaced a bare `nudge: Broken pipe (os error 32)`. The user reads
//! "the edit failed"; in fact jobs 5 AND 6 are both pending and the message
//! fires TWICE, at the old time and the new.
//!
//! No window, no duplicate: this pins that the CLI asks the daemon to swap the
//! jobs in a single request, which the daemon applies under the queue lock.
//!
//! Hermetic: a recording stand-in daemon on a tempdir socket answers the real
//! binary, which is pointed at an isolated HOME/XDG state dir. Nothing is
//! spawned — the stand-in answers Ping, so `ensure_daemon` never starts one —
//! and the singleton lock is held so that stays true even if it ever doesn't.
//! Relying on the stand-in alone would mean a real daemon, spawned into a
//! tempdir that is then deleted, running forever on the developer's machine.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use nudge::ipc::{Request, Response};
use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{Os, Paths, resolve_from};

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

fn job() -> nudge::job::Job {
    job_on(TargetSpec::Tmux {
        pane: "bot:0.1".into(),
        server: None,
    })
}

/// Job 1, on `target`.
fn job_on(target: TargetSpec) -> nudge::job::Job {
    JobSpec {
        target,
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
    .into_job(1)
}

/// A stand-in daemon that lists `listed` as its only job and records every
/// request it is sent, as the line it arrived as.
///
/// It reads the op out of the raw JSON and answers Replace with a raw line, so
/// this test compiles — and fails, recording Schedule and Cancel — against a
/// CLI that has never heard of an atomic replace.
fn recording_daemon(socket: &Path, listed: nudge::job::Job) -> Arc<Mutex<Vec<String>>> {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(socket).unwrap();
    let recorded = Arc::clone(&lines);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let op = v["op"].as_str().unwrap_or("?").to_string();
            recorded.lock().unwrap().push(line.trim_end().to_string());

            let reply = match op.as_str() {
                // A stand-in for THIS build, so it answers the versioned
                // Pong: an old daemon's bare "Pong" is now refused outright,
                // which tests/cli_daemon_version.rs is where we pin.
                "Ping" => serde_json::to_string(&Response::Pong {
                    version: nudge::VERSION.to_string(),
                })
                .unwrap(),
                "List" => serde_json::to_string(&Response::Jobs(vec![listed.clone()])).unwrap(),
                // The daemon's answer to an atomic replace: the new job's id.
                "Replace" => r#"{"Replaced":6}"#.to_string(),
                // Schedule/Cancel land here: an edit that reaches them has
                // already opened the double-fire window this test exists to
                // close.
                _ => r#"{"Error":"unexpected op"}"#.to_string(),
            };
            let _ = writeln!(&stream, "{reply}");
        }
    });
    lines
}

/// The `op` of each request line `recording_daemon` recorded.
fn ops(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["op"].as_str().unwrap_or("?").to_string()
        })
        .collect()
}

/// A home of its own for one `nudge --edit`, with a stand-in daemon on its
/// socket listing `listed`.
struct Fixture {
    home: std::path::PathBuf,
    lines: Arc<Mutex<Vec<String>>>,
    _tmp: tempfile::TempDir,
    _lock: std::fs::File,
}

fn fixture(listed: nudge::job::Job) -> Fixture {
    // common::short_tempdir, not tempfile::tempdir: this test binds a real
    // socket under a HOME rooted at the tempdir, and macOS's $TMPDIR is long
    // enough that resolve_from's "Library/Application Support/nudge" suffix
    // overflows SUN_LEN there.
    let tmp = common::short_tempdir();
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    std::fs::create_dir_all(home.join("config")).unwrap();

    // Leak-proof by construction, as cli_no_daemon.rs already is: hold the
    // singleton lock so that a daemon this test never means to start cannot
    // survive being started. The stand-in answering Ping is what stops one
    // being spawned; this is what stops that being the only thing.
    let lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();

    let lines = recording_daemon(&paths.socket, listed);
    Fixture {
        home,
        lines,
        _tmp: tmp,
        _lock: lock,
    }
}

/// Run `nudge args` against the fixture's daemon, with nothing inherited
/// that could lead it to the developer's tmux or herdr.
///
/// The confirmation an edit prints labels panes from every tmux and herdr
/// pane it can list. The session running these tests may sit in a herdr pane
/// inside tmux, so every `HERDR_*`, `TMUX` and `TMUX_PANE` goes, and
/// `TMUX_TMPDIR` and `XDG_CONFIG_HOME` point inside the fixture, where no tmux
/// server runs and herdr keeps no session.
fn run_nudge(home: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nudge"));
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("HERDR_")) {
            cmd.env_remove(key);
        }
    }
    cmd.args(args)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("TMUX_TMPDIR", home.join("tmux"))
        .output()
        .expect("failed to run the nudge binary")
}

fn report(out: &std::process::Output) -> String {
    format!(
        "status {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn edit_mutates_the_queue_in_one_atomic_request() {
    let f = fixture(job());

    let out = run_nudge(&f.home, &["--edit", "1", "-m", "now + 3 hours"]);
    let report = report(&out);

    // THE POINT: exactly one mutating request, and it is the atomic swap.
    // Schedule followed by Cancel is the two-step that leaves both jobs live
    // whenever the second step doesn't land.
    assert_eq!(
        ops(&f.lines.lock().unwrap()),
        vec![
            "Ping".to_string(),
            "List".to_string(),
            "Replace".to_string()
        ],
        "edit must ensure a daemon, read the job, then swap it in ONE request\n{report}"
    );
    assert!(out.status.success(), "{report}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("edited job 1 -> 6"),
        "{report}"
    );
}

/// An edit that names no pane leaves a herdr job in its pane, and keeps the
/// conversation it was stamped with. Stamped again now, the job would vouch
/// for whatever the pane holds at the edit, which may be another
/// conversation than the one the user scheduled it for.
#[test]
fn editing_a_herdr_job_keeps_its_pane_and_stamp() {
    let f = fixture(job_on(TargetSpec::Herdr {
        pane: "w7:pX".into(),
        socket: "/nonexistent/herdr/herdr.sock".into(),
        agent_session: Some("abc".into()),
    }));

    let out = run_nudge(&f.home, &["--edit", "1", "-m", "now + 3 hours"]);
    let report = report(&out);

    assert!(out.status.success(), "{report}");
    let lines = f.lines.lock().unwrap();
    let replace = lines
        .iter()
        .find_map(|l| match serde_json::from_str(l) {
            Ok(Request::Replace { spec, .. }) => Some(spec),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Replace request was made\n{report}"));
    assert_eq!(
        replace.target,
        TargetSpec::Herdr {
            pane: "w7:pX".into(),
            socket: "/nonexistent/herdr/herdr.sock".into(),
            agent_session: Some("abc".into()),
        },
        "{report}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("herdr:default:w7:pX"),
        "{report}"
    );
}

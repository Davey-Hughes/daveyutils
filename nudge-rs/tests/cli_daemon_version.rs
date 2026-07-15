//! A new CLI must refuse to drive an OLD resident daemon, and say so usefully.
//!
//! The daemon is long-lived and auto-started, and `ensure_daemon` only Pings.
//! An old daemon answers a Ping perfectly well, so rebuilding nudge never
//! replaced it: every daemon-side fix in this increment stayed inert until the
//! user happened to kill it by hand, and there is no `--stop-daemon` to do it
//! with. `--edit` at least broke loudly (`nudge: no response`, since an old
//! daemon has never heard of `Replace`); `--list`, `--cancel` and schedule kept
//! quietly running the OLD code and looked entirely successful doing it.
//!
//! So the handshake carries a version, and a daemon that isn't this build is an
//! error naming the remedy. The Pong shape itself is the giveaway: an old
//! daemon answers the unit-variant `"Pong"`, which this build cannot parse —
//! a reliable signal, and one that must stay distinguishable from "no daemon is
//! running at all" (ENOENT/ECONNREFUSED), which still auto-starts.
//!
//! Hermetic: a stand-in old daemon on a tempdir socket answers the real binary,
//! pointed at an isolated HOME/XDG state dir. The singleton lock is held so that
//! even if the CLI did try to spawn a daemon, it could not survive.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};

use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{resolve_from, Os, Paths};

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

fn job() -> nudge::job::Job {
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
    .into_job(1)
}

/// A stand-in for a nudge daemon built before this increment.
///
/// Faithful in the two ways that matter: it answers Ping with the bare
/// `"Pong"` unit variant, and it has never heard of `Replace`, so it hangs up
/// on one without replying — which is exactly the `nudge: no response` the
/// user hit.
fn old_daemon(socket: &Path) {
    let listener = UnixListener::bind(socket).unwrap();
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
            let reply = match v["op"].as_str().unwrap_or("?") {
                "Ping" => r#""Pong""#.to_string(),
                "List" => serde_json::json!({ "Jobs": [job()] }).to_string(),
                "Cancel" => r#"{"Cancelled":true}"#.to_string(),
                // An op from the future. The old daemon's `read_msg` fails to
                // deserialize it, `handle_conn` returns Err, and the connection
                // closes with nothing written.
                _ => continue,
            };
            let _ = writeln!(&stream, "{reply}");
        }
    });
}

struct Fixture {
    home: std::path::PathBuf,
    _tmp: tempfile::TempDir,
    _lock: std::fs::File,
}

/// An isolated state dir with an OLD daemon answering on its socket.
fn with_old_daemon() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();

    // Belt and braces: no real daemon can survive this test even if the CLI
    // tries to start one.
    let lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();
    old_daemon(&paths.socket);

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

/// Every command that reaches an old daemon must refuse it the same way, and
/// name the remedy. Nothing else in nudge tells the user how to do this.
fn assert_actionable(what: &str, out: &Output) {
    let r = report(what, out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must not report success\n{r}");
    assert!(
        stderr.contains("not this build"),
        "must say the daemon is the wrong build\n{r}"
    );
    assert!(
        stderr.contains("pkill -f 'nudge --daemon'"),
        "must name the remedy: there is no --stop-daemon\n{r}"
    );
    assert!(
        !stderr.contains("no response"),
        "a bare transport errno tells the user nothing about what to do\n{r}"
    );
}

#[test]
fn edit_against_an_old_daemon_says_what_to_do_about_it() {
    // Pre-fix this was `nudge: no response`: the old daemon has no Replace, so
    // it hung up, and the CLI reported the EOF and nothing more.
    let f = with_old_daemon();
    let out = run_nudge(&f.home, &["--edit", "1", "-m", "now + 3 hours"]);
    assert_actionable("edit", &out);
}

#[test]
fn list_against_an_old_daemon_is_refused_not_silently_served() {
    // The quiet half of the finding, and the worse one. Pre-fix this exited 0
    // and printed the table -- served by the OLD daemon's code, so every fix in
    // this increment was inert and nothing said so.
    let f = with_old_daemon();
    let out = run_nudge(&f.home, &["--list"]);
    assert_actionable("list", &out);
}

#[test]
fn cancel_against_an_old_daemon_is_refused_not_silently_served() {
    let f = with_old_daemon();
    let out = run_nudge(&f.home, &["--cancel", "1"]);
    assert_actionable("cancel", &out);
}

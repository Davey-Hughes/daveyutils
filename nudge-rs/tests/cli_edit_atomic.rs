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

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use nudge::ipc::Response;
use nudge::job::{JobSpec, TargetSpec};
use nudge::paths::{resolve_from, Os, Paths};

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
    }
    .into_job(1)
}

/// A stand-in daemon that records the `op` of every request it is sent.
///
/// It reads the op out of the raw JSON and answers Replace with a raw line, so
/// this test compiles — and fails, recording Schedule and Cancel — against a
/// CLI that has never heard of an atomic replace.
fn recording_daemon(socket: &Path) -> Arc<Mutex<Vec<String>>> {
    let ops: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(socket).unwrap();
    let recorded = Arc::clone(&ops);
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
            recorded.lock().unwrap().push(op.clone());

            let reply = match op.as_str() {
                // A stand-in for THIS build, so it answers the versioned
                // Pong: an old daemon's bare "Pong" is now refused outright,
                // which tests/cli_daemon_version.rs is where we pin.
                "Ping" => serde_json::to_string(&Response::Pong {
                    version: nudge::VERSION.to_string(),
                })
                .unwrap(),
                "List" => serde_json::to_string(&Response::Jobs(vec![job()])).unwrap(),
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
    ops
}

#[test]
fn edit_mutates_the_queue_in_one_atomic_request() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let paths = child_paths(home);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();

    // Leak-proof by construction, as cli_no_daemon.rs already is: hold the
    // singleton lock so that a daemon this test never means to start cannot
    // survive being started. The stand-in answering Ping is what stops one
    // being spawned; this is what stops that being the only thing.
    let _lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();

    let ops = recording_daemon(&paths.socket);

    let out = Command::new(env!("CARGO_BIN_EXE_nudge"))
        .args(["--edit", "1", "-m", "now + 3 hours"])
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .output()
        .expect("failed to run the nudge binary");

    let report = format!(
        "status {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // THE POINT: exactly one mutating request, and it is the atomic swap.
    // Schedule followed by Cancel is the two-step that leaves both jobs live
    // whenever the second step doesn't land.
    assert_eq!(
        *ops.lock().unwrap(),
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

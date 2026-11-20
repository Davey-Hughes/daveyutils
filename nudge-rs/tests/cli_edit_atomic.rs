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
        verify_no_banner: false,
    }
    .into_job(1)
}

/// A stand-in daemon that lists `listed` as its only job and records every
/// request it is sent, as the line it arrived as.
///
/// It reads the op out of the raw JSON and answers Replace with a raw line, so
/// this test compiles — and fails, recording Schedule and Cancel — against a
/// CLI that has never heard of an atomic replace.
///
/// Each Replace is answered with the next of `replace_replies`, a raw line.
/// Once they run out, every Replace is answered as swapped, as job 6.
fn recording_daemon(
    socket: &Path,
    listed: nudge::job::Job,
    replace_replies: Vec<String>,
) -> Arc<Mutex<Vec<String>>> {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(socket).unwrap();
    let recorded = Arc::clone(&lines);
    let mut replace_replies = std::collections::VecDeque::from(replace_replies);
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
                // The daemon's answer to an atomic replace: the new job's id,
                // unless the test scripted a refusal first.
                "Replace" => replace_replies
                    .pop_front()
                    .unwrap_or_else(|| r#"{"Replaced":6}"#.to_string()),
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
    fixture_answering(listed, Vec::new())
}

/// [`fixture`], with the stand-in answering each Replace with the next of
/// `replace_replies` until they run out.
fn fixture_answering(listed: nudge::job::Job, replace_replies: Vec<String>) -> Fixture {
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

    let lines = recording_daemon(&paths.socket, listed, replace_replies);
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
/// pane it can list (see `common::keep_off_the_developers_panes`).
fn run_nudge(home: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nudge"));
    common::keep_off_the_developers_panes(&mut cmd, home);
    cmd.args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
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

/// Job 1 with two retries left, as `-a -r 2` schedules it.
fn retrying_job() -> nudge::job::Job {
    let mut job = job();
    job.auto_retry = true;
    job.retries_left = 2;
    job
}

/// Every Replace request the stand-in was sent, in order, as `(seen, spec)`.
fn replaces(lines: &[String]) -> Vec<(nudge::job::Progress, JobSpec)> {
    lines
        .iter()
        .filter_map(|l| match serde_json::from_str(l) {
            Ok(Request::Replace { seen, spec, .. }) => Some((seen, spec)),
            _ => None,
        })
        .collect()
}

/// The job fired between the CLI's List and its Replace: the daemon moved it
/// to its retry and took a retry off, and refused the swap. The flags only
/// say what to change, so the CLI applies them to the job as it is now and
/// sends that, which keeps the new time and the retries now left.
#[test]
fn an_edit_of_a_job_that_fired_meanwhile_goes_to_the_job_as_it_is_now() {
    let listed = retrying_job();
    let mut fired = listed.clone();
    fired.fire_at = "2026-07-13T15:00:05Z".parse().unwrap();
    fired.retries_left = 1;
    let refusal = serde_json::to_string(&Response::Changed(fired.clone())).unwrap();
    let f = fixture_answering(listed.clone(), vec![refusal]);

    let out = run_nudge(&f.home, &["--edit", "1", "-i", "a new message"]);
    let report = report(&out);

    assert!(out.status.success(), "{report}");
    let sent = replaces(&f.lines.lock().unwrap());
    assert_eq!(sent.len(), 2, "one refused Replace, one more\n{report}");
    assert_eq!(sent[0].0, listed.progress(), "{report}");
    let (seen, spec) = &sent[1];
    assert_eq!(*seen, fired.progress(), "{report}");
    assert_eq!(
        spec.fire_at, fired.fire_at,
        "the time the job has now\n{report}"
    );
    assert_eq!(spec.retries_left, 1, "the retries it has now\n{report}");
    assert_eq!(spec.messages, vec!["a new message".to_string()], "{report}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("job 1 fired while it was being edited"),
        "{report}"
    );
    assert!(stdout.contains("edited job 1 -> 6"), "{report}");
}

/// Applied once more, the edit is refused again: the job fired a second
/// time. That is no edit to chase, so the CLI stops, and says the job is
/// unchanged.
#[test]
fn an_edit_refused_twice_leaves_the_job_unchanged() {
    let listed = retrying_job();
    let mut fired = listed.clone();
    fired.fire_at = "2026-07-13T15:00:05Z".parse().unwrap();
    fired.retries_left = 1;
    let refusal = serde_json::to_string(&Response::Changed(fired)).unwrap();
    let f = fixture_answering(listed, vec![refusal.clone(), refusal]);

    let out = run_nudge(&f.home, &["--edit", "1", "-i", "a new message"]);
    let report = report(&out);

    assert!(!out.status.success(), "{report}");
    assert_eq!(replaces(&f.lines.lock().unwrap()).len(), 2, "{report}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("job 1 keeps firing"), "{report}");
    assert!(stderr.contains("it is unchanged"), "{report}");
}

/// The daemon is firing the job right now. What the job will be after is not
/// known yet, so there is nothing to apply the edit to: the CLI says to try
/// again.
#[test]
fn an_edit_of_a_job_being_fired_says_to_try_again() {
    let f = fixture_answering(retrying_job(), vec![r#""Firing""#.to_string()]);

    let out = run_nudge(&f.home, &["--edit", "1", "-i", "a new message"]);
    let report = report(&out);

    assert!(!out.status.success(), "{report}");
    assert_eq!(replaces(&f.lines.lock().unwrap()).len(), 1, "{report}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("job 1 is firing right now; it is unchanged, try again in a few seconds"),
        "{report}"
    );
}

/// `nudge --edit 1 -i "a new message" <extra>` on a `--verify` job in pane
/// `bot:0` of a private tmux server, where the stand-in daemon refuses the
/// first Replace because the job has fired. Returns every Replace sent, as
/// `(seen, spec)`, and the run's report.
///
/// The server stands in the fixture's `TMUX_TMPDIR`, so the CLI can take real
/// snapshots. It has a second window, `bot:1`, for an edit to move the job to.
fn edit_after_a_fire(extra: &[&str]) -> (Vec<(nudge::job::Progress, JobSpec)>, String) {
    let tmp = common::short_tempdir();
    let tmux = common::TmuxServer::start(&tmp.path().join("tmux"), "bot");
    tmux.run(&["new-window", "-d", "-t", "bot:1", "sh"]);
    let pane = tmux.run(&["display-message", "-p", "-t", "bot:0", "#{pane_id}"]);
    let mut listed = retrying_job();
    listed.target = TargetSpec::Tmux { pane, server: None };
    listed.verify = true;
    let mut fired = listed.clone();
    fired.fire_at = "2026-07-13T15:00:05Z".parse().unwrap();
    fired.retries_left = 1;
    let refusal = serde_json::to_string(&Response::Changed(fired)).unwrap();
    let f = fixture_answering(listed, vec![refusal]);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nudge"));
    common::keep_off_the_developers_panes(&mut cmd, &f.home);
    cmd.args(["--edit", "1", "-i", "a new message"])
        .args(extra)
        .env("HOME", &f.home)
        .env("XDG_STATE_HOME", f.home.join("state"))
        .env("XDG_RUNTIME_DIR", f.home.join("run"));
    tmux.env(&mut cmd);
    let out = cmd.output().expect("failed to run the nudge binary");
    let report = report(&out);
    assert!(out.status.success(), "{report}");
    let sent = replaces(&f.lines.lock().unwrap());
    assert_eq!(sent.len(), 2, "{report}");
    (sent, report)
}

/// The job just typed into its pane, and the reply to it is likely still
/// arriving. A `--verify` fingerprint of that pane would read the reply as the
/// user resuming, and skip the retry; the fire dropped the job's own
/// fingerprint for that reason (`Queue::reschedule`). So the edit applied to
/// the job as the fire left it takes none of that pane either.
#[test]
fn an_edit_applied_after_a_fire_takes_no_snapshot_of_the_pane_it_fired_into() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let (sent, report) = edit_after_a_fire(&[]);

    assert!(
        sent[0].1.verify_fingerprint.is_some(),
        "precondition: the first edit, before the fire was known, snapshots \
         the pane\n{report}"
    );
    let again = &sent[1].1;
    assert!(again.verify, "{report}");
    assert_eq!(
        (again.verify_fingerprint.as_deref(), again.verify_dims),
        (None, None),
        "no snapshot of the pane the job just fired into\n{report}"
    );
    assert!(!again.verify_no_banner, "{report}");
}

/// The edit moves the job to a pane the fire never typed into, so that pane
/// is fingerprinted as any edit's would be.
#[test]
fn an_edit_applied_after_a_fire_snapshots_a_pane_it_moves_the_job_to() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let (sent, report) = edit_after_a_fire(&["-p", "bot:1"]);

    assert!(
        sent[1].1.verify_fingerprint.is_some(),
        "the new pane gets its snapshot\n{report}"
    );
}

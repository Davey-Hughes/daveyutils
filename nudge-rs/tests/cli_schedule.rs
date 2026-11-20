//! `build_spec` assembles a `JobSpec` from CLI options; a hermetic IPC round-trip
//! confirms a scheduled spec reaches the queue. No real daemon is spawned.
//!
//! The `nudge` runs at the end are refused before any request is made, in an
//! environment of their own: no tmux, herdr or daemon of the developer's can
//! be reached from one, and the singleton lock is held so that a daemon one
//! started by mistake could not survive.

mod common;

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::app::build_spec;
use nudge::cli::{Cli, resolve_options};
use nudge::ipc::{Request, Response, client, server};
use nudge::job::TargetSpec;
use nudge::paths::{Os, Paths, resolve_from};
use nudge::queue::Queue;
use nudge::target::PaneDims;
use nudge::verify::Baseline;

fn cli(args: &[&str]) -> Cli {
    <Cli as clap::Parser>::try_parse_from(args).unwrap()
}

/// A tmux pane by `pane`, read from `server`.
fn tmux(pane: &str, server: Option<&str>) -> TargetSpec {
    TargetSpec::Tmux {
        pane: pane.into(),
        server: server.map(Into::into),
    }
}

fn a_baseline() -> Baseline {
    Baseline {
        recency: Some((
            nudge::verify::fingerprint("⏸ session limit reached · resets 3:00am"),
            PaneDims {
                width: 80,
                height: 24,
            },
        )),
        banner: true,
    }
}

/// The schedule-time half of the recency gate: `-v` arms it with the pane as it
/// looks right now, which is what the fire-time check compares against.
#[test]
fn build_spec_stores_the_pane_snapshot_when_verify_is_on() {
    let c = cli(&["nudge", "-p", "bot:0.1", "-v"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("bot:0.1", None), ts, &c, &opts, Some(a_baseline()));
    assert!(spec.verify);
    let (fingerprint, dims) = a_baseline().recency.unwrap();
    assert_eq!(spec.verify_fingerprint, Some(fingerprint));
    assert_eq!(spec.verify_dims, Some(dims));
}

/// A pane that would not answer arms no gate -- and, above all, still schedules
/// the job. `--verify` failing to arm must never cost the user their nudge.
#[test]
fn build_spec_schedules_the_job_anyway_when_the_pane_cannot_be_snapshotted() {
    let c = cli(&["nudge", "-p", "bot:0.1", "-v"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("bot:0.1", None), ts, &c, &opts, None);
    assert!(spec.verify, "--verify stays on; it just fails open at fire");
    assert_eq!(spec.verify_fingerprint, None);
    assert_eq!(spec.verify_dims, None);
}

/// Nothing reads these without `--verify`, so storing one would only leave a
/// stale fingerprint in queue.json.
#[test]
fn build_spec_stores_no_snapshot_without_verify() {
    let c = cli(&["nudge", "-p", "bot:0.1"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("bot:0.1", None), ts, &c, &opts, Some(a_baseline()));
    assert!(!spec.verify);
    assert_eq!(spec.verify_fingerprint, None);
    assert_eq!(spec.verify_dims, None);
}

#[test]
// float_cmp: these assert a literal survived the CLI -> JobSpec round trip
// unchanged, not the result of any arithmetic. Exact equality is the
// property under test; an epsilon would weaken it to "close enough".
#[allow(clippy::float_cmp)]
fn build_spec_defaults_message_and_delay() {
    let c = cli(&["nudge", "-p", "bot:0.1"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("bot:0.1", None), ts, &c, &opts, None);
    assert_eq!(
        spec.target,
        TargetSpec::Tmux {
            pane: "bot:0.1".into(),
            server: None,
        }
    );
    assert_eq!(spec.messages, vec!["please continue".to_string()]);
    assert_eq!(spec.send_delay_secs, 0.75);
    assert_eq!(spec.fire_at, ts);
}

#[test]
// float_cmp: these assert a literal survived the CLI -> JobSpec round trip
// unchanged, not the result of any arithmetic. Exact equality is the
// property under test; an epsilon would weaken it to "close enough".
#[allow(clippy::float_cmp)]
fn build_spec_takes_custom_messages_and_delay() {
    let c = cli(&[
        "nudge", "-p", "x", "-i", "npm test", "-i", "yes", "-w", "1.5",
    ]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("x", None), ts, &c, &opts, None);
    assert_eq!(
        spec.messages,
        vec!["npm test".to_string(), "yes".to_string()]
    );
    assert_eq!(spec.send_delay_secs, 1.5);
}

#[test]
fn scheduling_a_spec_over_ipc_reaches_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));

    let listener = UnixListener::bind(&socket).unwrap();
    let q = Arc::clone(&queue);
    let h = thread::spawn(move || server::serve_once(&listener, &q).unwrap());

    let c = cli(&["nudge", "-p", "bot:0.1"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("bot:0.1", None), ts, &c, &opts, None);

    let resp = client::request(&socket, &Request::Schedule(spec)).unwrap();
    h.join().unwrap();
    assert!(matches!(resp, Response::Scheduled(1)));
    assert_eq!(queue.lock().unwrap().all().len(), 1);
}

/// The snapshot is taken in the CLI process and consulted hours later by the
/// daemon, so it has to survive both hops it makes: the `Schedule` request's
/// JSON, and queue.json on disk. If either drops it, the job reaches the daemon
/// with no baseline -- which fails open, so nothing breaks loudly; `--verify`
/// just quietly goes back to being the positionally-blind check that made
/// finding I19. Only an assertion catches that.
#[test]
fn the_pane_snapshot_survives_the_schedule_request_and_queue_json() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue_path = dir.path().join("q.json");
    let queue = Arc::new(Mutex::new(Queue::load(queue_path.clone()).unwrap()));

    let listener = UnixListener::bind(&socket).unwrap();
    let q = Arc::clone(&queue);
    let h = thread::spawn(move || server::serve_once(&listener, &q).unwrap());

    let c = cli(&["nudge", "-p", "bot:0.1", "-v"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("bot:0.1", None), ts, &c, &opts, Some(a_baseline()));

    client::request(&socket, &Request::Schedule(spec)).unwrap();
    h.join().unwrap();

    // Survived the IPC hop.
    let job = queue.lock().unwrap().all()[0].clone();
    assert_eq!(job.verify_baseline(), a_baseline().recency);

    // ...and the disk hop, as the daemon would reload it after a restart.
    let reloaded = Queue::load(queue_path).unwrap();
    assert_eq!(
        reloaded.all()[0].verify_baseline(),
        a_baseline().recency,
        "a daemon restarted between schedule and fire must still hold the baseline"
    );
}

/// The job keeps the server its pane's id was read from, so that firing
/// can tell that id from the same one on a server started since.
#[test]
fn build_spec_stores_the_server_the_pane_was_read_from() {
    let c = cli(&["nudge", "-p", "%19"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec(tmux("%19", Some("1793226261-4820")), ts, &c, &opts, None);
    assert_eq!(
        spec.target,
        TargetSpec::Tmux {
            pane: "%19".into(),
            server: Some("1793226261-4820".into())
        }
    );
}

// ---- `-p` and `-a` refusals, through the real binary ----

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// The paths the `nudge` child resolves from the environment it is given.
fn child_paths(home: &Path) -> Paths {
    resolve_from(
        home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    )
}

/// A home of its own for a `nudge` run, with the singleton lock held on its
/// state dir so that no daemon can start there.
struct Fixture {
    home: PathBuf,
    _tmp: tempfile::TempDir,
    _lock: std::fs::File,
}

fn fixture() -> Fixture {
    // common::short_tempdir: the daemon socket and herdr's are derived from
    // this directory, and macOS's $TMPDIR is long enough to overflow SUN_LEN.
    let tmp = common::short_tempdir();
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    std::fs::create_dir_all(home.join("config")).unwrap();
    let lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();
    Fixture {
        home,
        _tmp: tmp,
        _lock: lock,
    }
}

/// Run `nudge args` with `env` set and nothing inherited that could lead it
/// to the developer's tmux or herdr.
///
/// The session running these tests may sit in a herdr pane inside tmux, so
/// every `HERDR_*`, `TMUX` and `TMUX_PANE` goes. `TMUX_TMPDIR` and
/// `XDG_CONFIG_HOME` point inside the fixture, where no tmux server runs and
/// herdr keeps no session, so a tmux or herdr command that does run finds
/// nothing of the developer's.
fn run_nudge(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
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
        .env("TMUX_TMPDIR", home.join("tmux"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run the nudge binary")
}

fn report(out: &Output) -> String {
    format!(
        "status {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

/// A herdr pane id has a workspace and a pane. `herdr:w7` names only one of
/// them, so nudge says what it wants rather than ask tmux for a pane `w7`.
#[test]
fn a_herdr_pane_missing_its_workspace_is_refused_with_the_form_it_takes() {
    let f = fixture();
    let out = run_nudge(&f.home, &["-p", "herdr:w7"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("herdr:<workspace>:<pane>"),
        "{}",
        report(&out)
    );
}

/// A herdr session that is not running has no pane to type into, so the job
/// is refused when it is scheduled rather than failing at the reset.
#[test]
fn a_herdr_session_that_is_not_running_is_refused_by_name() {
    if !common::herdr_available() {
        eprintln!("skipping: herdr not installed");
        return;
    }
    let f = fixture();
    let out = run_nudge(&f.home, &["-p", "herdr:nope:w1:p1"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no running herdr session nope"),
        "{}",
        report(&out)
    );
}

/// From a herdr split, `-a` has no pane to go by: herdr does not say which
/// pane had focus before nudge's. It names the popup that does say, and
/// guesses nothing.
#[test]
fn auto_in_a_herdr_split_points_to_a_herdr_popup() {
    let f = fixture();
    let out = run_nudge(
        &f.home,
        &["-a"],
        &[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p1")],
    );
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("herdr popup"),
        "{}",
        report(&out)
    );
}

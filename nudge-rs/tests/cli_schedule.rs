//! `build_spec` assembles a JobSpec from CLI options; a hermetic IPC round-trip
//! confirms a scheduled spec reaches the queue. No real daemon is spawned.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::app::build_spec;
use nudge::cli::{resolve_options, Cli};
use nudge::ipc::{client, server, Request, Response};
use nudge::job::TargetSpec;
use nudge::queue::Queue;
use nudge::target::PaneDims;
use nudge::verify::Baseline;

fn cli(args: &[&str]) -> Cli {
    <Cli as clap::Parser>::try_parse_from(args).unwrap()
}

fn a_baseline() -> Baseline {
    Baseline {
        fingerprint: nudge::verify::fingerprint("⏸ session limit reached · resets 3:00am"),
        dims: PaneDims {
            width: 80,
            height: 24,
        },
    }
}

/// The schedule-time half of the recency gate: `-v` arms it with the pane as it
/// looks right now, which is what the fire-time check compares against.
#[test]
fn build_spec_stores_the_pane_snapshot_when_verify_is_on() {
    let c = cli(&["nudge", "-p", "bot:0.1", "-v"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("bot:0.1", ts, &c, &opts, Some(a_baseline()));
    assert!(spec.verify);
    assert_eq!(spec.verify_fingerprint, Some(a_baseline().fingerprint));
    assert_eq!(spec.verify_dims, Some(a_baseline().dims));
}

/// A pane that would not answer arms no gate -- and, above all, still schedules
/// the job. `--verify` failing to arm must never cost the user their nudge.
#[test]
fn build_spec_schedules_the_job_anyway_when_the_pane_cannot_be_snapshotted() {
    let c = cli(&["nudge", "-p", "bot:0.1", "-v"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("bot:0.1", ts, &c, &opts, None);
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
    let spec = build_spec("bot:0.1", ts, &c, &opts, Some(a_baseline()));
    assert!(!spec.verify);
    assert_eq!(spec.verify_fingerprint, None);
    assert_eq!(spec.verify_dims, None);
}

#[test]
fn build_spec_defaults_message_and_delay() {
    let c = cli(&["nudge", "-p", "bot:0.1"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("bot:0.1", ts, &c, &opts, None);
    assert_eq!(
        spec.target,
        TargetSpec::Tmux {
            pane: "bot:0.1".into()
        }
    );
    assert_eq!(spec.messages, vec!["please continue".to_string()]);
    assert_eq!(spec.send_delay_secs, 0.75);
    assert_eq!(spec.fire_at, ts);
}

#[test]
fn build_spec_takes_custom_messages_and_delay() {
    let c = cli(&[
        "nudge", "-p", "x", "-i", "npm test", "-i", "yes", "-w", "1.5",
    ]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("x", ts, &c, &opts, None);
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
    let spec = build_spec("bot:0.1", ts, &c, &opts, None);

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
    let spec = build_spec("bot:0.1", ts, &c, &opts, Some(a_baseline()));

    client::request(&socket, &Request::Schedule(spec)).unwrap();
    h.join().unwrap();

    // Survived the IPC hop.
    let job = queue.lock().unwrap().all()[0].clone();
    assert_eq!(job.verify_fingerprint, Some(a_baseline().fingerprint));
    assert_eq!(job.verify_dims, Some(a_baseline().dims));

    // ...and the disk hop, as the daemon would reload it after a restart.
    let reloaded = Queue::load(queue_path).unwrap();
    assert_eq!(
        reloaded.all()[0].verify_baseline(),
        Some((a_baseline().fingerprint, a_baseline().dims)),
        "a daemon restarted between schedule and fire must still hold the baseline"
    );
}

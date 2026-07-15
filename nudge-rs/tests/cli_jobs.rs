//! format_jobs is a pure renderer; cancel/list are verified via a hermetic IPC
//! server. No real daemon.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::app::format_jobs;
use nudge::config::Toggles;
use nudge::ipc::{client, server, Request, Response};
use nudge::job::{JobSpec, TargetSpec};
use nudge::queue::Queue;
use nudge::target::PaneDims;
use nudge::verify::Baseline;

/// A pane that yields no snapshot: the stand-in for `snapshot_pane` in tests
/// that are not about the recency gate.
fn no_snapshot(_pane: &str, _opts: &Toggles) -> Option<Baseline> {
    None
}

fn spec(pane: &str) -> JobSpec {
    JobSpec {
        target: TargetSpec::Tmux { pane: pane.into() },
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
}

#[test]
fn format_jobs_shows_id_pane_and_count() {
    let mut q = Queue::load(tempfile::tempdir().unwrap().path().join("q.json")).unwrap();
    q.add(spec("bot:0.1")).unwrap();
    let out = format_jobs(q.all());
    assert!(out.contains("bot:0.1"), "got:\n{out}");
    assert!(out.contains('1')); // the id
}

#[test]
fn cancel_over_ipc_removes_the_job() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
    queue.lock().unwrap().add(spec("x")).unwrap();

    let listener = Arc::new(UnixListener::bind(&socket).unwrap());
    let q = Arc::clone(&queue);
    let l = Arc::clone(&listener);
    let h = thread::spawn(move || server::serve_once(&l, &q).unwrap());

    let resp = client::request(&socket, &Request::Cancel(1)).unwrap();
    h.join().unwrap();
    assert_eq!(resp, Response::Cancelled(true));
    assert!(queue.lock().unwrap().all().is_empty());
}

fn noon() -> jiff::Zoned {
    use jiff::{civil::date, tz::TimeZone};
    date(2026, 7, 13)
        .at(12, 0, 0, 0)
        .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
        .unwrap()
}

fn cli(args: &[&str]) -> nudge::cli::Cli {
    <nudge::cli::Cli as clap::Parser>::try_parse_from(args).unwrap()
}

/// A job scheduled *without* auto-retry: `build_spec` stores retries_left == 0.
fn no_retry_job(id: u64) -> nudge::job::Job {
    JobSpec {
        auto_retry: false,
        retries_left: 0,
        ..spec("bot:0.1")
    }
    .into_job(id)
}

#[test]
fn edit_turning_on_auto_retry_gets_the_default_budget() {
    // `nudge --edit 5 --auto-retry` with no -r. merge_edit seeds its base from
    // the job, and a job scheduled without auto-retry holds retries_left == 0 --
    // so the replacement was stored with auto_retry=true and a budget of 0.
    // apply_outcome guards on `auto_retry && retries_left != 0`, so that job is
    // REMOVED on its first fire and never retries, while the CLI cheerfully
    // prints "edited job 5 -> 6". The flag must arm retries here exactly as it
    // does on a fresh schedule.
    let spec = nudge::app::merge_edit(
        &no_retry_job(5),
        &cli(&["nudge", "--edit", "5", "-a"]),
        &noon(),
        2,
        &no_snapshot,
    )
    .unwrap();
    assert!(spec.auto_retry, "the flag was passed");
    assert_eq!(
        spec.retries_left, 2,
        "--auto-retry on a job with no budget must fall back to the default \
         count, not 0 -- auto_retry=true with 0 retries never retries"
    );
}

#[test]
fn edit_auto_retry_with_an_explicit_count_uses_that_count() {
    // The explicit -r is the user's stated intent and must beat the fallback.
    let spec = nudge::app::merge_edit(
        &no_retry_job(5),
        &cli(&["nudge", "--edit", "5", "-a", "-r", "7"]),
        &noon(),
        2,
        &no_snapshot,
    )
    .unwrap();
    assert!(spec.auto_retry);
    assert_eq!(spec.retries_left, 7);
}

#[test]
fn edit_without_auto_retry_keeps_a_no_retry_job_at_zero() {
    // The fallback must not arm retries on a job the user never asked to retry.
    let spec = nudge::app::merge_edit(
        &no_retry_job(5),
        &cli(&["nudge", "--edit", "5", "-m", "6pm"]),
        &noon(),
        2,
        &no_snapshot,
    )
    .unwrap();
    assert!(!spec.auto_retry);
    assert_eq!(spec.retries_left, 0);
}

#[test]
fn merge_edit_preserves_options_not_passed() {
    let now = noon();

    // A job that had verify + notify + infinite retries.
    let job = JobSpec {
        target: TargetSpec::Tmux {
            pane: "bot:0.1".into(),
        },
        messages: vec!["go".into()],
        send_delay_secs: 0.5,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: true,
        verify: true,
        auto_retry: true,
        retries_left: -1,
        settle_secs: 9.0,
        verify_fingerprint: None,
        verify_dims: None,
    }
    .into_job(1);

    // Edit only the time; pass no toggle flags.
    let cli =
        <nudge::cli::Cli as clap::Parser>::try_parse_from(["nudge", "--edit", "1", "-m", "6pm"])
            .unwrap();

    let spec = nudge::app::merge_edit(&job, &cli, &now, 2, &no_snapshot).unwrap();

    assert!(spec.verify, "verify must be preserved");
    assert!(spec.notify, "notify must be preserved");
    assert!(spec.auto_retry);
    assert_eq!(spec.retries_left, -1, "infinite retries preserved");
    assert_eq!(spec.send_delay_secs, 0.5, "delay preserved");
    assert_eq!(spec.messages, vec!["go".to_string()], "messages preserved");
    // Time WAS changed (6pm today or tomorrow, not the original 15:00Z).
    assert_ne!(spec.fire_at, job.fire_at);
}

// ---- --edit re-snapshots for the recency gate ----

fn dims(width: u16) -> PaneDims {
    PaneDims { width, height: 24 }
}

/// A job carrying a snapshot from when it was first scheduled.
fn verify_job_snapshotted_at(pane: &str, screen: &str) -> nudge::job::Job {
    JobSpec {
        target: TargetSpec::Tmux { pane: pane.into() },
        messages: vec!["go".into()],
        send_delay_secs: 0.75,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: true,
        auto_retry: false,
        retries_left: 0,
        settle_secs: 5.0,
        verify_fingerprint: Some(nudge::verify::fingerprint(screen)),
        verify_dims: Some(dims(80)),
    }
    .into_job(1)
}

/// An edit is a re-schedule, so the pane as it looks *now* becomes the new
/// baseline. Inheriting the original snapshot would compare the pane against
/// however it looked hours ago: any edit of a job whose pane had since moved
/// would arm a gate that skips on sight, i.e. never fires.
#[test]
fn edit_resnapshots_the_pane_instead_of_inheriting_the_old_baseline() {
    let job = verify_job_snapshotted_at("bot:0.1", "the pane hours ago");
    let cli =
        <nudge::cli::Cli as clap::Parser>::try_parse_from(["nudge", "--edit", "1", "-m", "6pm"])
            .unwrap();
    let fresh = |_p: &str, _o: &Toggles| {
        Some(Baseline {
            fingerprint: nudge::verify::fingerprint("the pane at edit time"),
            dims: dims(80),
        })
    };
    let spec = nudge::app::merge_edit(&job, &cli, &noon(), 2, &fresh).unwrap();
    assert_eq!(
        spec.verify_fingerprint,
        Some(nudge::verify::fingerprint("the pane at edit time")),
        "the pane at edit time is the 'before' the user means"
    );
    assert_ne!(
        spec.verify_fingerprint, job.verify_fingerprint,
        "the stale baseline must not survive the edit"
    );
}

/// The snapshot has to describe the pane the job will actually watch, so
/// `-p` moving the job re-points it.
#[test]
fn edit_snapshots_the_new_pane_when_the_job_is_moved() {
    let job = verify_job_snapshotted_at("bot:0.1", "old pane");
    let cli = <nudge::cli::Cli as clap::Parser>::try_parse_from([
        "nudge",
        "--edit",
        "1",
        "-p",
        "other:0.2",
    ])
    .unwrap();
    let seen = std::cell::RefCell::new(Vec::new());
    let record = |p: &str, _o: &Toggles| {
        seen.borrow_mut().push(p.to_string());
        None
    };
    nudge::app::merge_edit(&job, &cli, &noon(), 2, &record).unwrap();
    assert_eq!(
        *seen.borrow(),
        vec!["other:0.2".to_string()],
        "the snapshot must describe the pane the job is being moved to"
    );
}

/// `--edit 1 --no-verify` turns the gate off, so no snapshot is taken or kept.
///
/// The closure hands back a snapshot *unconditionally*, which is the point:
/// this must hold because `merge_edit` drops it, not because the caller was
/// polite enough not to offer one. It used to be checked against a stub that
/// hard-coded `o.verify.then(...)` — so the assertion below passed on the
/// strength of the stub's own `if`, and `merge_edit` could have stored the
/// snapshot on a `verify: false` job forever without the test noticing. That
/// left the property resting entirely on a guard inside `snapshot_pane`, one
/// layer up and invisible from here.
#[test]
fn edit_with_no_verify_drops_the_snapshot() {
    let job = verify_job_snapshotted_at("bot:0.1", "parked");
    let cli =
        <nudge::cli::Cli as clap::Parser>::try_parse_from(["nudge", "--edit", "1", "--no-verify"])
            .unwrap();
    let always_snapshots = |_p: &str, _o: &Toggles| {
        Some(Baseline {
            fingerprint: "x".into(),
            dims: dims(80),
        })
    };
    let spec = nudge::app::merge_edit(&job, &cli, &noon(), 2, &always_snapshots).unwrap();
    assert!(!spec.verify);
    assert_eq!(
        spec.verify_fingerprint, None,
        "--no-verify means nothing consults a snapshot, so storing one only \
         leaves a stale fingerprint in queue.json"
    );
    assert_eq!(spec.verify_dims, None);
}

/// A pane that will not snapshot at edit time must still leave an edited job
/// that fires: no baseline means fail open, not skip.
#[test]
fn edit_keeps_the_job_when_the_pane_cannot_be_snapshotted() {
    let job = verify_job_snapshotted_at("bot:0.1", "parked");
    let cli =
        <nudge::cli::Cli as clap::Parser>::try_parse_from(["nudge", "--edit", "1", "-m", "6pm"])
            .unwrap();
    let spec = nudge::app::merge_edit(&job, &cli, &noon(), 2, &no_snapshot).unwrap();
    assert!(
        spec.verify,
        "--verify stays on; it just fails open at fire time"
    );
    assert_eq!(spec.verify_fingerprint, None);
}

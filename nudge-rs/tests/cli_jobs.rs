//! format_jobs is a pure renderer; cancel/list are verified via a hermetic IPC
//! server. No real daemon.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::app::format_jobs;
use nudge::ipc::{client, server, Request, Response};
use nudge::job::{JobSpec, TargetSpec};
use nudge::queue::Queue;

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
    }
    .into_job(1);

    // Edit only the time; pass no toggle flags.
    let cli =
        <nudge::cli::Cli as clap::Parser>::try_parse_from(["nudge", "--edit", "1", "-m", "6pm"])
            .unwrap();

    let spec = nudge::app::merge_edit(&job, &cli, &now, 2).unwrap();

    assert!(spec.verify, "verify must be preserved");
    assert!(spec.notify, "notify must be preserved");
    assert!(spec.auto_retry);
    assert_eq!(spec.retries_left, -1, "infinite retries preserved");
    assert_eq!(spec.send_delay_secs, 0.5, "delay preserved");
    assert_eq!(spec.messages, vec!["go".to_string()], "messages preserved");
    // Time WAS changed (6pm today or tomorrow, not the original 15:00Z).
    assert_ne!(spec.fire_at, job.fire_at);
}

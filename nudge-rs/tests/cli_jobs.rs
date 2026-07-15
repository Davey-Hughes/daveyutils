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

#[test]
fn merge_edit_preserves_options_not_passed() {
    use jiff::{civil::date, tz::TimeZone};

    let now = date(2026, 7, 13)
        .at(12, 0, 0, 0)
        .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
        .unwrap();

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

    let spec = nudge::app::merge_edit(&job, &cli, &now).unwrap();

    assert!(spec.verify, "verify must be preserved");
    assert!(spec.notify, "notify must be preserved");
    assert!(spec.auto_retry);
    assert_eq!(spec.retries_left, -1, "infinite retries preserved");
    assert_eq!(spec.send_delay_secs, 0.5, "delay preserved");
    assert_eq!(spec.messages, vec!["go".to_string()], "messages preserved");
    // Time WAS changed (6pm today or tomorrow, not the original 15:00Z).
    assert_ne!(spec.fire_at, job.fire_at);
}

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

fn cli(args: &[&str]) -> Cli {
    <Cli as clap::Parser>::try_parse_from(args).unwrap()
}

#[test]
fn build_spec_defaults_message_and_delay() {
    let c = cli(&["nudge", "-p", "bot:0.1"]);
    let opts = resolve_options(&c);
    let ts = "2026-07-13T15:00:00Z".parse().unwrap();
    let spec = build_spec("bot:0.1", ts, &c, &opts);
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
    let spec = build_spec("x", ts, &c, &opts);
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
    let spec = build_spec("bot:0.1", ts, &c, &opts);

    let resp = client::request(&socket, &Request::Schedule(spec)).unwrap();
    h.join().unwrap();
    assert!(matches!(resp, Response::Scheduled(1)));
    assert_eq!(queue.lock().unwrap().all().len(), 1);
}

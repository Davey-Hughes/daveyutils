//! Hermetic round-trip over a real Unix socket in a tempdir. The server thread
//! handles exactly one connection per client request, then joins — nothing
//! lingers, and no OS service is involved.

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::ipc::{client, server, Request, Response};
use nudge::job::{JobSpec, TargetSpec};
use nudge::queue::Queue;

fn spec() -> JobSpec {
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
        retries_left: 2,
        settle_secs: 5.0,
    }
}

/// Serve exactly one request on a background thread, then run the client.
///
/// The brief's original sketch derived the socket path from
/// `listener.local_addr()` inside the helper, but that requires a
/// doubly-referenced `Arc<UnixListener>` to yield a `&UnixListener`, which
/// doesn't compile as written. Instead we thread the socket `Path` through
/// explicitly alongside the listener — the behavior (one `serve_once` per
/// client request, joined before returning) is unchanged.
fn one_shot(
    listener: &Arc<UnixListener>,
    socket: &Path,
    queue: &Arc<Mutex<Queue>>,
    req: Request,
) -> Response {
    let l = Arc::clone(listener);
    let q = Arc::clone(queue);
    let handle = thread::spawn(move || server::serve_once(&l, &q).unwrap());
    let resp = client::request(socket, &req).unwrap();
    handle.join().unwrap();
    resp
}

#[test]
fn socket_roundtrip_schedule_list_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue_path = dir.path().join("queue.json");

    let listener = Arc::new(UnixListener::bind(&socket).unwrap());
    let queue = Arc::new(Mutex::new(Queue::load(queue_path).unwrap()));

    assert_eq!(
        one_shot(&listener, &socket, &queue, Request::Ping),
        Response::Pong {
            version: nudge::VERSION.to_string()
        }
    );

    match one_shot(&listener, &socket, &queue, Request::Schedule(spec())) {
        Response::Scheduled(id) => assert_eq!(id, 1),
        other => panic!("expected Scheduled, got {other:?}"),
    }

    match one_shot(&listener, &socket, &queue, Request::List) {
        Response::Jobs(jobs) => assert_eq!(jobs.len(), 1),
        other => panic!("expected Jobs, got {other:?}"),
    }

    assert_eq!(
        one_shot(&listener, &socket, &queue, Request::Cancel(1)),
        Response::Cancelled(true)
    );
    assert_eq!(
        one_shot(&listener, &socket, &queue, Request::Cancel(1)),
        Response::Cancelled(false)
    );
}

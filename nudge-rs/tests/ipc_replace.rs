//! The daemon must be able to swap a job for its replacement in ONE request.
//!
//! `edit` used to Schedule the replacement and then Cancel the original as two
//! separate round-trips. Between them both jobs are live, so anything that
//! stops the second leg — the daemon restarting, the socket being stolen, a
//! Ctrl-C — leaves the message scheduled twice, at the old time and the new.
//! The protocol had no way to express "swap these atomically"; `Replace` is it.
//!
//! Hermetic: tempdir socket and queue, no real daemon. The server thread loops
//! forever and is reaped at process exit (it only sleeps in `accept`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nudge::ipc::server;
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

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "socket never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Send one raw JSON line, return the raw response line (empty on EOF).
///
/// Raw JSON rather than `client::request(&Request::Replace { .. })` on purpose:
/// this test compiles — and fails — against a daemon that has never heard of
/// the op, which is what makes it evidence the daemon gained a real Replace
/// rather than a shape that moves whenever the code does.
fn round_trip(socket: &Path, line: &str) -> String {
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(&stream, "{line}").unwrap();
    let mut resp = String::new();
    let _ = BufReader::new(&stream).read_line(&mut resp); // empty on EOF
    resp.trim().to_string()
}

/// A queue holding one job (id 1) on `pane`, served over a tempdir socket.
fn serving_queue(pane: &str) -> (tempfile::TempDir, std::path::PathBuf, Arc<Mutex<Queue>>) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
    queue.lock().unwrap().add(spec(pane)).unwrap();

    let s = socket.clone();
    let q = Arc::clone(&queue);
    std::thread::spawn(move || {
        let _ = server::serve(&s, q);
    });
    wait_for_socket(&socket);
    (dir, socket, queue)
}

#[test]
fn replace_swaps_the_job_in_one_request() {
    let (_dir, socket, queue) = serving_queue("old:0.0");

    let new_spec = serde_json::to_string(&spec("new:0.0")).unwrap();
    let resp = round_trip(
        &socket,
        &format!(r#"{{"op":"Replace","data":{{"id":1,"spec":{new_spec}}}}}"#),
    );

    assert_eq!(
        resp, r#"{"Replaced":2}"#,
        "the daemon must answer a Replace with the new job's id"
    );
    let q = queue.lock().unwrap();
    assert_eq!(
        q.all().len(),
        1,
        "a replace must leave exactly one job -- two means the message fires twice"
    );
    assert_eq!(q.all()[0].id, 2, "the replacement's id");
    assert_eq!(
        q.all()[0].target,
        TargetSpec::Tmux {
            pane: "new:0.0".into()
        },
        "the replacement's spec must be the one we sent"
    );
}

#[test]
fn replace_of_an_unknown_id_changes_nothing() {
    // The original may have fired or been cancelled since `edit` listed it.
    // Scheduling the replacement anyway would resurrect a job the user no
    // longer has; the whole point of doing it in one request is that it is
    // all-or-nothing.
    let (_dir, socket, queue) = serving_queue("old:0.0");

    let new_spec = serde_json::to_string(&spec("new:0.0")).unwrap();
    let resp = round_trip(
        &socket,
        &format!(r#"{{"op":"Replace","data":{{"id":99,"spec":{new_spec}}}}}"#),
    );

    assert_eq!(
        resp, r#"{"Replaced":null}"#,
        "replacing an id that isn't there must report the miss"
    );
    let q = queue.lock().unwrap();
    assert_eq!(q.all().len(), 1, "nothing must have been added");
    assert_eq!(q.all()[0].id, 1, "the untouched original");
}

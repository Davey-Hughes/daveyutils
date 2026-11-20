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
        target: TargetSpec::Tmux {
            pane: pane.into(),
            server: None,
        },
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
}

/// The progress of `spec`'s job as an edit reads it before anything fires:
/// what every Replace here sends as `seen`, unless it means to send an older
/// copy.
const SEEN: &str = r#"{"fire_at":"2026-07-13T15:00:00Z","retries_left":0}"#;

/// Wait until the server takes a connection, not until the socket's file
/// appears: binding creates the file, and a client that connects before the
/// `listen` that follows is refused (see `daemon_ipc.rs`).
fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(path).is_err() {
        assert!(
            Instant::now() < deadline,
            "the server never took a connection"
        );
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
        let _ = server::serve(&s, &q);
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
        &format!(r#"{{"op":"Replace","data":{{"id":1,"seen":{SEEN},"spec":{new_spec}}}}}"#),
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
            pane: "new:0.0".into(),
            server: None,
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
        &format!(r#"{{"op":"Replace","data":{{"id":99,"seen":{SEEN},"spec":{new_spec}}}}}"#),
    );

    assert_eq!(
        resp, r#"{"Replaced":null}"#,
        "replacing an id that isn't there must report the miss"
    );
    let q = queue.lock().unwrap();
    assert_eq!(q.all().len(), 1, "nothing must have been added");
    assert_eq!(q.all()[0].id, 1, "the untouched original");
}

/// `--edit` re-snapshots the pane and ships the new baseline inside the
/// `Replace` spec. If the op drops it in transit the replacement lands with no
/// baseline and silently fails open -- `--verify` degrades back to the
/// positionally-blind check of finding I19, with nothing to show for it.
///
/// Raw JSON on purpose, like the tests above: this asserts the wire format
/// carries the fields, not merely that our own serializer round-trips.
#[test]
fn replace_carries_the_verify_snapshot_to_the_daemon() {
    let (_dir, socket, queue) = serving_queue("old:0.0");

    let fp = nudge::verify::fingerprint("⏸ session limit reached · resets 3:00am");
    let snapshotted = JobSpec {
        verify: true,
        verify_fingerprint: Some(fp.clone()),
        verify_dims: Some(nudge::target::PaneDims {
            width: 80,
            height: 24,
        }),
        ..spec("new:0.0")
    };
    let new_spec = serde_json::to_string(&snapshotted).unwrap();
    assert!(
        new_spec.contains(&fp),
        "precondition: the fingerprint is on the wire"
    );

    let resp = round_trip(
        &socket,
        &format!(r#"{{"op":"Replace","data":{{"id":1,"seen":{SEEN},"spec":{new_spec}}}}}"#),
    );
    assert_eq!(resp, r#"{"Replaced":2}"#);

    let q = queue.lock().unwrap();
    assert_eq!(
        q.all()[0].verify_baseline(),
        Some((
            fp,
            nudge::target::PaneDims {
                width: 80,
                height: 24
            }
        )),
        "an edited --verify job must reach the daemon still holding its new baseline"
    );
}

/// The job fired while the edit was open: the daemon moved it to its retry
/// and took a retry off. The edit, made against the job as it was, must not
/// land, and the answer carries the job as it is now for the edit to start
/// again from.
#[test]
fn replace_of_a_job_that_fired_since_is_refused_with_the_job_now() {
    let (_dir, socket, queue) = serving_queue("old:0.0");
    let retry_at: jiff::Timestamp = "2026-07-13T15:00:05Z".parse().unwrap();
    let fired = {
        let mut q = queue.lock().unwrap();
        assert!(q.reschedule(1, retry_at, 3).unwrap());
        q.get(1).unwrap().clone()
    };

    let new_spec = serde_json::to_string(&spec("new:0.0")).unwrap();
    let resp = round_trip(
        &socket,
        &format!(r#"{{"op":"Replace","data":{{"id":1,"seen":{SEEN},"spec":{new_spec}}}}}"#),
    );

    let resp: nudge::ipc::Response =
        serde_json::from_str(&resp).unwrap_or_else(|e| panic!("not a Response ({e}): {resp}"));
    assert_eq!(resp, nudge::ipc::Response::Changed(fired.clone()));
    assert_eq!(
        queue.lock().unwrap().all(),
        [fired],
        "the job must stay as the fire left it"
    );
}

/// Mid-fire the job still reads as the edit saw it, so only the daemon's
/// record that it is firing can stop the swap.
#[test]
fn replace_of_a_job_being_fired_is_refused() {
    let (_dir, socket, queue) = serving_queue("old:0.0");
    let before = {
        let mut q = queue.lock().unwrap();
        assert!(q.start_firing(1).is_some());
        q.all().to_vec()
    };

    let new_spec = serde_json::to_string(&spec("new:0.0")).unwrap();
    let resp = round_trip(
        &socket,
        &format!(r#"{{"op":"Replace","data":{{"id":1,"seen":{SEEN},"spec":{new_spec}}}}}"#),
    );

    assert_eq!(resp, r#""Firing""#);
    assert_eq!(
        queue.lock().unwrap().all(),
        before,
        "the job must be untouched"
    );
}

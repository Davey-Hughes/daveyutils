//! A peer that stops participating must not wedge the other end.
//!
//! Connections are handled serially by `serve`'s accept loop, and `read_line`
//! blocks until a newline or EOF, so one client that connects and writes a
//! partial line (`nc -U $XDG_RUNTIME_DIR/nudge.sock`, then idles) used to block
//! accept() forever -- every later `--list`, `--cancel` and schedule hung too,
//! with no way to cancel the jobs the scheduler kept firing.
//!
//! Both tests are bounded by `recv_timeout`, so a regression FAILS with a clear
//! message instead of hanging the suite forever.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nudge::ipc::{client, server, Request, Response, IO_TIMEOUT};
use nudge::queue::Queue;

/// Generous next to `IO_TIMEOUT` (the stuck peer must time out first, and the
/// server only then gets back to accept()), but still bounded.
fn patience() -> Duration {
    IO_TIMEOUT * 4 + Duration::from_secs(5)
}

/// Block until the server thread is actually listening.
fn wait_for_socket(socket: &Path) {
    for _ in 0..500 {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server never started listening on {}", socket.display());
}

/// Run `client::request` on its own thread so a hang is a test failure rather
/// than a hung suite.
fn request_within(
    socket: &Path,
    req: Request,
    within: Duration,
    ctx: &str,
) -> std::io::Result<Response> {
    let (tx, rx) = mpsc::channel();
    let socket = socket.to_path_buf();
    thread::spawn(move || {
        let _ = tx.send(client::request(&socket, &req));
    });
    rx.recv_timeout(within).unwrap_or_else(|_| panic!("{ctx}"))
}

#[test]
fn a_client_that_never_sends_a_newline_does_not_wedge_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));

    let serve_socket = socket.clone();
    thread::spawn(move || {
        let _ = server::serve(&serve_socket, queue);
    });
    wait_for_socket(&socket);

    // The wedger: connect, send a partial line with no newline, then idle
    // holding the connection open. (A client that *disconnects* is harmless --
    // the fd closes and read_line returns 0 -> EOF.)
    let mut wedger = UnixStream::connect(&socket).unwrap();
    wedger.write_all(b"abc").unwrap();
    wedger.flush().unwrap();

    let resp = request_within(
        &socket,
        Request::Ping,
        patience(),
        "a stuck client wedged the control plane: Ping never came back",
    )
    .expect("Ping failed");
    assert_eq!(
        resp,
        Response::Pong {
            version: nudge::VERSION.to_string()
        }
    );

    drop(wedger);
}

#[test]
fn the_client_gives_up_on_a_daemon_that_never_replies() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    // A daemon that accepts the connection and then never answers.
    thread::spawn(move || {
        let (_held_open, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_secs(600));
    });

    let result = request_within(
        &socket,
        Request::Ping,
        patience(),
        "client::request hung on an unresponsive daemon: the CLI must fail fast",
    );
    assert!(
        result.is_err(),
        "expected a timeout error from an unresponsive daemon, got {result:?}"
    );
}

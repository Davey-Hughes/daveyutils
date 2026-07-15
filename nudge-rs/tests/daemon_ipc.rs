//! The daemon's IPC server must survive a malformed request and keep serving.
//! Hermetic: tempdir socket, no OS service. The server thread loops forever and
//! is left to be reaped at process exit (it only sleeps in `accept`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nudge::ipc::{client, server, Request, Response};
use nudge::queue::Queue;

fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "socket never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn daemon_survives_a_malformed_request() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));

    let sock2 = socket.clone();
    let q2 = Arc::clone(&queue);
    std::thread::spawn(move || {
        let _ = server::serve(&sock2, q2); // loops forever; reaped at process exit
    });
    wait_for_socket(&socket);

    // Send a line that is NOT valid JSON, then read (server closes without a
    // valid Response -> our manual read sees EOF). This exercises handle_conn's
    // Err path inside serve's loop.
    {
        let mut s = UnixStream::connect(&socket).unwrap();
        s.write_all(b"not json at all\n").unwrap();
        s.flush().unwrap();
        let mut line = String::new();
        let _ = BufReader::new(&s).read_line(&mut line); // may be empty on EOF
    }

    // The daemon must still be alive and answer a well-formed request.
    let resp = client::request(&socket, &Request::Ping).unwrap();
    assert_eq!(resp, Response::Pong);
}

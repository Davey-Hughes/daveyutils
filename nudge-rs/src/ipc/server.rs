//! Unix-socket server: applies IPC requests to the shared queue.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{handle_request, read_msg, write_msg, Request, Response};
use crate::queue::Queue;

/// Handle exactly one incoming connection (one request → one response).
pub fn serve_once(listener: &UnixListener, queue: &Mutex<Queue>) -> std::io::Result<()> {
    let (stream, _) = listener.accept()?;
    handle_conn(stream, queue)
}

/// Bind `socket` and serve connections forever. Removes a stale socket file
/// first. Used by the daemon (increment 3b).
pub fn serve(socket: &Path, queue: Arc<Mutex<Queue>>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket); // clear a stale socket from a prior run
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(socket)?;
    loop {
        serve_once(&listener, &queue)?;
    }
}

fn handle_conn(stream: UnixStream, queue: &Mutex<Queue>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let req: Request = match read_msg(&mut reader)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let resp: Response = {
        let mut q = queue.lock().expect("queue mutex poisoned");
        handle_request(req, &mut q)
    };
    write_msg(&mut &stream, &resp)
}

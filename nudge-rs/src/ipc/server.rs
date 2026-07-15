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

/// Bind `socket` and serve connections forever. Reclaims a stale socket file
/// first, but never steals one another daemon is actively listening on. Used
/// by the daemon (increment 3b).
///
/// A per-connection error (a malformed request, or a client that disconnects
/// before reading the reply) is logged and the loop continues; only a fatal
/// `accept()` error ends the loop.
pub fn serve(socket: &Path, queue: Arc<Mutex<Queue>>) -> std::io::Result<()> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Never steal a live socket. If something answers, another daemon owns it.
    // Only a socket nobody is listening on is stale and safe to reclaim.
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "another nudge daemon is already listening on {}",
                        socket.display()
                    ),
                ));
            }
            Err(_) => {
                tracing::warn!("nudge ipc: reclaiming stale socket {}", socket.display());
                std::fs::remove_file(socket)?;
            }
        }
    }
    let listener = UnixListener::bind(socket)?;
    tracing::info!("nudge ipc: listening on {}", socket.display());
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = handle_conn(stream, &queue) {
                    // A malformed request or a client that disconnected before
                    // reading the reply must not take the daemon down.
                    tracing::warn!("nudge ipc: connection error: {e}");
                }
            }
            Err(e) => {
                tracing::error!("nudge ipc: accept failed, stopping: {e}");
                return Err(e);
            }
        }
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

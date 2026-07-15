//! Unix-socket server: applies IPC requests to the shared queue.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{handle_request, read_msg, write_msg, Request, Response, IO_TIMEOUT};
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
/// Each connection is served on a short-lived worker thread, so a peer that
/// stalls cannot delay the next client. A per-connection error (a malformed
/// request, a stalled peer, or a client that disconnects before reading the
/// reply) is logged and the loop continues. A transient `accept()` error (a
/// peer vanishing mid-handshake, a signal, or a momentary fd shortage) is also
/// retried; only a genuinely fatal listener error ends the loop.
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
                // Hand the connection to a short-lived worker so a peer that
                // stalls cannot delay the next client. `IO_TIMEOUT` (set inside
                // handle_conn) bounds how long such a worker can linger, so the
                // thread count stays proportional to live clients, not to
                // however long a stuck peer feels like holding on.
                let queue = Arc::clone(&queue);
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, &queue) {
                        // A malformed request, a stalled peer, or a client that
                        // disconnected before reading the reply must not take
                        // the daemon down.
                        tracing::warn!("nudge ipc: connection error: {e}");
                    }
                });
            }
            Err(e) => match e.kind() {
                // A peer vanished mid-handshake, or a signal interrupted us.
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted => {
                    tracing::debug!("nudge ipc: transient accept error: {e}");
                    continue;
                }
                _ => {
                    // EMFILE/ENFILE (out of descriptors) is also transient, but
                    // hammering accept() would spin: back off briefly and retry.
                    if matches!(e.raw_os_error(), Some(24) | Some(23)) {
                        tracing::warn!("nudge ipc: accept: {e}; backing off");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                    tracing::error!("nudge ipc: accept failed, stopping: {e}");
                    return Err(e);
                }
            },
        }
    }
}

fn handle_conn(stream: UnixStream, queue: &Mutex<Queue>) -> std::io::Result<()> {
    // Before reading a byte: `read_line` blocks until a newline or EOF, so a
    // peer that connects and then stalls would hold this worker forever. The
    // timeout reaps it, which is what keeps `serve`'s per-connection threads
    // bounded -- and it is the whole protection for `serve_once`, which has no
    // worker to spare.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

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

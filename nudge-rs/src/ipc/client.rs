//! Unix-socket client: send one request, read one response.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use super::{read_msg, write_msg, Request, Response, IO_TIMEOUT};

/// Connect to the daemon socket, send `req`, and return its `Response`.
pub fn request(socket: &Path, req: &Request) -> std::io::Result<Response> {
    request_with_timeout(socket, req, IO_TIMEOUT)
}

/// [`request`], with the I/O timeout spelled out.
///
/// Every caller in the binary wants [`IO_TIMEOUT`] and should use [`request`].
/// This exists for the test that proves the give-up path *works*: waiting out
/// the real five seconds to watch a timeout fire costs the suite five seconds
/// per run to observe a `Duration` it already knows. The mechanism is what the
/// test is about, so it passes a short one; `io_timeout_is_five_seconds` in
/// `super` pins the production value separately.
pub fn request_with_timeout(
    socket: &Path,
    req: &Request,
    timeout: Duration,
) -> std::io::Result<Response> {
    let stream = UnixStream::connect(socket)?;
    // A daemon that accepts but never answers must not hang the CLI: report an
    // error the caller can print instead of blocking the user's terminal.
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write_msg(&mut &stream, req)?;
    let mut reader = BufReader::new(&stream);
    read_msg(&mut reader)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no response"))
}

//! Unix-socket client: send one request, read one response.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{read_msg, write_msg, Request, Response, IO_TIMEOUT};

/// Connect to the daemon socket, send `req`, and return its `Response`.
pub fn request(socket: &Path, req: &Request) -> std::io::Result<Response> {
    let stream = UnixStream::connect(socket)?;
    // A daemon that accepts but never answers must not hang the CLI: report an
    // error the caller can print instead of blocking the user's terminal.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    write_msg(&mut &stream, req)?;
    let mut reader = BufReader::new(&stream);
    read_msg(&mut reader)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no response"))
}

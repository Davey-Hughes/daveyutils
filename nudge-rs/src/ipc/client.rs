//! Unix-socket client: send one request, read one response.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{read_msg, write_msg, Request, Response};

/// Connect to the daemon socket, send `req`, and return its `Response`.
pub fn request(socket: &Path, req: &Request) -> std::io::Result<Response> {
    let stream = UnixStream::connect(socket)?;
    write_msg(&mut &stream, req)?;
    let mut reader = BufReader::new(&stream);
    read_msg(&mut reader)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no response"))
}

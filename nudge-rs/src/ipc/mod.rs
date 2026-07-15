//! IPC protocol between the nudge CLI and the daemon: newline-delimited JSON
//! `Request`/`Response` messages over a Unix socket.

pub mod client;
pub mod server;

use std::io::{BufRead, Write};
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::job::{Job, JobSpec};
use crate::queue::Queue;

/// Read/write timeout for one IPC exchange, applied on both ends.
///
/// The protocol is one line in, one line out, between two processes on the same
/// machine, so a peer this slow has stopped participating. Without it:
///
/// - the server blocks in `read_line` forever, and because `serve` handles
///   connections serially that wedges accept() and with it every later
///   `--list`/`--cancel`/schedule;
/// - the client hangs the CLI instead of reporting an error.
///
/// This bounds a *stalled* peer, not a slow-drip one (the timeout is per read,
/// so a byte every 4s would still hold a connection). Serving each connection
/// on its own thread is the fuller fix; this closes the wedge at far less risk.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "op", content = "data")]
pub enum Request {
    Schedule(JobSpec),
    List,
    Cancel(u64),
    /// Swap job `id` for `spec` atomically: `edit`'s whole mutation in one
    /// request, applied under the daemon's queue lock.
    ///
    /// Doing it as Schedule-then-Cancel left both jobs live in between, so a
    /// failure of the second leg fired the message twice. A round-trip cannot
    /// half-apply, so there is no window to lose.
    Replace {
        id: u64,
        spec: JobSpec,
    },
    Ping,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Response {
    Scheduled(u64),
    Jobs(Vec<Job>),
    Cancelled(bool),
    /// The replacement's id, or `None` if the original was already gone.
    Replaced(Option<u64>),
    /// The answering daemon's version, which is the point of the exchange.
    ///
    /// A Ping used to ask only "is anything there?", and an old daemon answers
    /// that as well as a new one — so `ensure_daemon` was satisfied by a daemon
    /// running code from before every fix the CLI assumes. Carrying the version
    /// makes the handshake ask the question that actually matters.
    ///
    /// This is also why it is a struct variant: an old daemon answers the bare
    /// unit `"Pong"`, which no longer deserializes, so a stale daemon is
    /// detectable even though it predates the version field entirely.
    Pong {
        version: String,
    },
    Error(String),
}

/// Write one message as a single JSON line and flush.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

/// Read one newline-delimited JSON message. `Ok(None)` on clean EOF.
pub fn read_msg<R: BufRead, T: DeserializeOwned>(r: &mut R) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let msg = serde_json::from_str(line.trim_end())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

/// Apply a request to the queue and produce the response.
pub fn handle_request(req: Request, queue: &mut Queue) -> Response {
    match req {
        Request::Ping => Response::Pong {
            version: crate::VERSION.to_string(),
        },
        Request::Schedule(spec) => match queue.add(spec) {
            Ok(id) => Response::Scheduled(id),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::List => Response::Jobs(queue.all().to_vec()),
        Request::Cancel(id) => match queue.remove(id) {
            Ok(removed) => Response::Cancelled(removed),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::Replace { id, spec } => match queue.replace(id, spec) {
            Ok(new_id) => Response::Replaced(new_id),
            Err(e) => Response::Error(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::TargetSpec;
    use std::io::BufReader;

    fn spec() -> JobSpec {
        JobSpec {
            target: TargetSpec::Tmux {
                pane: "bot:0.1".into(),
            },
            messages: vec!["go".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 2,
            settle_secs: 5.0,
        }
    }

    #[test]
    fn framing_roundtrips_one_message_per_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_msg(&mut buf, &Request::Ping).unwrap();
        write_msg(&mut buf, &Request::Cancel(7)).unwrap();
        // Two distinct lines.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 2);

        let mut r = BufReader::new(&buf[..]);
        let a: Request = read_msg(&mut r).unwrap().unwrap();
        let b: Request = read_msg(&mut r).unwrap().unwrap();
        assert_eq!(a, Request::Ping);
        assert_eq!(b, Request::Cancel(7));
        // EOF -> None.
        assert!(read_msg::<_, Request>(&mut r).unwrap().is_none());
    }

    #[test]
    fn handle_schedule_list_cancel_against_a_real_queue() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();

        let resp = handle_request(Request::Schedule(spec()), &mut q);
        assert_eq!(resp, Response::Scheduled(1));

        match handle_request(Request::List, &mut q) {
            Response::Jobs(jobs) => assert_eq!(jobs.len(), 1),
            other => panic!("expected Jobs, got {other:?}"),
        }

        assert_eq!(
            handle_request(Request::Cancel(1), &mut q),
            Response::Cancelled(true)
        );
        assert_eq!(
            handle_request(Request::Cancel(1), &mut q),
            Response::Cancelled(false)
        );
    }

    #[test]
    fn ping_pongs() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        assert_eq!(
            handle_request(Request::Ping, &mut q),
            Response::Pong {
                version: crate::VERSION.to_string()
            },
            "a Pong must name the build that sent it: the CLI has no other way \
             to tell a resident daemon from another version apart from this one"
        );
    }
}

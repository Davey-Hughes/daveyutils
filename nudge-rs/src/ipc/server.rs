//! Unix-socket server: applies IPC requests to the shared queue.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::{handle_request, read_msg, write_msg, Request, Response, IO_TIMEOUT};
use crate::queue::Queue;

/// Ceiling on IPC worker threads in flight at once.
///
/// The daemon's thread budget is not its own: `RLIMIT_NPROC` counts every
/// thread the user has, and a systemd user unit's `TasksMax` is a far lower
/// ceiling still. Exhausting it costs more than workers — `run_injection` forks
/// tmux, so a daemon that has burned the budget quietly stops firing jobs. One
/// stalled peer holds a worker for the whole `IO_TIMEOUT`, so an unbounded
/// spawn is all it takes to turn a flood of them into exactly that. Past the
/// cap a connection is served inline instead: the loop pauses for at worst
/// IO_TIMEOUT, which is a far better failure than an unbounded one.
const MAX_WORKERS: usize = 64;

/// Releases a worker slot however the worker leaves — including a panic, which
/// would otherwise retire a slot from the cap permanently.
struct WorkerSlot(Arc<AtomicUsize>);

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How a worker thread is created. Injected so a test can hand `serve` a
/// builder the OS refuses, and prove the accept loop survives it.
fn worker_builder() -> std::thread::Builder {
    std::thread::Builder::new().name("nudge-ipc".to_string())
}

/// Handle exactly one incoming connection (one request → one response).
pub fn serve_once(listener: &UnixListener, queue: &Mutex<Queue>) -> std::io::Result<()> {
    let (stream, _) = listener.accept()?;
    handle_conn(&stream, queue)
}

/// Serve one connection, logging rather than propagating its error: a malformed
/// request, a stalled peer, or a client that disconnected before reading the
/// reply must not take the daemon down.
fn serve_conn(stream: &UnixStream, queue: &Mutex<Queue>) {
    if let Err(e) = handle_conn(stream, queue) {
        tracing::warn!("nudge ipc: connection error: {e}");
    }
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
///
/// The accept loop never panics, and that is load-bearing: `daemon::run` treats
/// `serve` *returning* as fatal and exits the process, so a panic — which
/// escapes instead of returning — would leave the daemon headless but still
/// firing. Worker threads are capped ([`MAX_WORKERS`]) and a refused one is
/// served inline, so neither a flood nor a stingy OS can panic us.
pub fn serve(socket: &Path, queue: Arc<Mutex<Queue>>) -> std::io::Result<()> {
    serve_with(socket, queue, worker_builder, MAX_WORKERS)
}

fn serve_with<B>(
    socket: &Path,
    queue: Arc<Mutex<Queue>>,
    builder: B,
    max_workers: usize,
) -> std::io::Result<()>
where
    B: Fn() -> std::thread::Builder,
{
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
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Hand the connection to a short-lived worker so a peer that
                // stalls cannot delay the next client. `IO_TIMEOUT` (set inside
                // handle_conn) bounds how long such a worker can linger, so the
                // thread count stays proportional to live clients, not to
                // however long a stuck peer feels like holding on.
                let stream = Arc::new(stream);
                if live.load(Ordering::SeqCst) >= max_workers {
                    tracing::warn!(
                        "nudge ipc: {max_workers} workers already in flight; serving inline"
                    );
                    serve_conn(&stream, &queue);
                    continue;
                }
                live.fetch_add(1, Ordering::SeqCst);
                let slot = WorkerSlot(Arc::clone(&live));
                let q = Arc::clone(&queue);
                let s = Arc::clone(&stream);
                let worker = move || {
                    let _slot = slot;
                    serve_conn(&s, &q);
                };
                // NOT `std::thread::spawn`: that is `.expect("failed to spawn
                // thread")`, and the OS refusing a thread is a condition we
                // reach in production, not a bug. The panic would escape
                // `serve` rather than return from it, so daemon.rs's fatal
                // exit — the whole reason `serve` returning is fatal — never
                // runs, and a headless daemon keeps firing jobs into panes
                // while answering no one. Serve it here instead.
                if let Err(e) = builder().spawn(worker) {
                    // Dropping the closure released the slot.
                    tracing::warn!("nudge ipc: could not spawn a worker ({e}); serving inline");
                    serve_conn(&stream, &queue);
                }
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

fn handle_conn(stream: &UnixStream, queue: &Mutex<Queue>) -> std::io::Result<()> {
    // Before reading a byte: `read_line` blocks until a newline or EOF, so a
    // peer that connects and then stalls would hold this worker forever. The
    // timeout reaps it, which is what keeps `serve`'s per-connection threads
    // bounded -- and it is the whole protection for `serve_once`, which has no
    // worker to spare.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut reader = BufReader::new(stream);
    let req: Request = match read_msg(&mut reader)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let resp: Response = {
        // Tolerate poison, as daemon.rs does at all four of its lock sites: the
        // state behind this lock is a queue of real jobs that will fire, and a
        // thread that panicked while holding it does not make them less real.
        // Refusing here is worse than useless -- the worker panics, so `serve`
        // accepts every connection and answers none, and never returns, so the
        // fatal exit that would replace this daemon never runs. The user gets a
        // daemon that fires jobs, cannot be listed or cancelled, and cannot be
        // superseded (the singleton lock is doing its job).
        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
        handle_request(req, &mut q)
    };
    write_msg(&mut &*stream, &resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};

    /// A builder the OS refuses: no thread stack that large can be mapped, so
    /// `spawn` returns Err exactly as it does when the thread/task limit is
    /// reached. `std::thread::spawn` PANICS on this same Err.
    fn refused_builder() -> std::thread::Builder {
        std::thread::Builder::new().stack_size(usize::MAX)
    }

    fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// What this build answers a Ping with.
    fn pong() -> String {
        serde_json::to_string(&Response::Pong {
            version: crate::VERSION.to_string(),
        })
        .unwrap()
    }

    /// Send one raw JSON line; return the raw reply (empty on EOF).
    fn round_trip(socket: &Path, line: &str) -> String {
        let stream = UnixStream::connect(socket).expect("connect");
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        writeln!(&stream, "{line}").unwrap();
        let mut resp = String::new();
        let _ = BufReader::new(&stream).read_line(&mut resp);
        resp.trim().to_string()
    }

    /// Serve `queue` on a socket under `dir`, and wait until it is bound.
    fn serve_on(
        dir: &Path,
        queue: Arc<Mutex<Queue>>,
        builder: impl Fn() -> std::thread::Builder + Send + 'static,
        max_workers: usize,
    ) -> std::path::PathBuf {
        let socket = dir.join("nudge.sock");
        let s = socket.clone();
        std::thread::spawn(move || {
            let _ = serve_with(&s, queue, builder, max_workers);
        });
        wait_until(|| socket.exists(), "the socket to be bound");
        socket
    }

    fn serving(
        builder: impl Fn() -> std::thread::Builder + Send + 'static,
        max_workers: usize,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
        let socket = serve_on(dir.path(), queue, builder, max_workers);
        (dir, socket)
    }

    #[test]
    fn an_os_refused_worker_does_not_panic_the_accept_loop() {
        // `serve` returning is fatal to the daemon by design (daemon.rs exits
        // the process), but a PANIC escaping `serve` is not a return: the exit
        // never runs, and what is left is a headless daemon that still fires
        // jobs while answering nothing -- with the singleton lock (correctly)
        // blocking any replacement. The accept loop must never panic.
        let (_dir, socket) = serving(refused_builder, MAX_WORKERS);

        // Twice: the first proves the connection is still served when the OS
        // refuses its worker, the second that the accept loop outlived it.
        for i in 1..=2 {
            assert_eq!(
                round_trip(&socket, r#"{"op":"Ping"}"#),
                pong(),
                "ping {i}: a refused worker thread must not cost us the daemon"
            );
        }
    }

    #[test]
    fn the_worker_cap_serves_the_overflow_inline_rather_than_spawning() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawned);
        let builder = move || {
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::Builder::new()
        };
        let (_dir, socket) = serving(builder, 1);

        // One stalled peer: connects, sends nothing, holds its worker for the
        // whole IO_TIMEOUT. That is the connection that fills the cap -- and it
        // is what a flood is made of.
        let _stalled = UnixStream::connect(&socket).unwrap();
        wait_until(
            || spawned.load(Ordering::SeqCst) == 1,
            "the stalled peer's worker",
        );

        // At the cap the next client is still answered correctly...
        assert_eq!(round_trip(&socket, r#"{"op":"Ping"}"#), pong());
        // ...but not by a thread we cannot afford. Unbounded, a flood of
        // stalled peers walks the daemon into TasksMax/RLIMIT_NPROC, and past
        // that the scheduler cannot fork tmux either: the daemon stops firing
        // jobs while still looking healthy.
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            1,
            "the worker cap must bound threads in flight"
        );
    }

    #[test]
    fn a_poisoned_queue_mutex_still_gets_answered() {
        // Poisoning is not hypothetical: any panic under the queue lock does
        // it, and it is permanent. `daemon::run` tolerates it at all four of
        // its own lock sites for exactly that reason -- the state behind the
        // lock is a queue of jobs, and a panicking thread does not make those
        // jobs any less real.
        //
        // Refusing it HERE was the worst place to do it: the worker panics, so
        // `serve` accepts every connection and answers none -- and never
        // returns, so daemon.rs's fatal exit never fires. Ping times out, the
        // CLI reports no daemon, `ensure_daemon` spawns a second one, that one
        // loses the singleton lock, and the user gets `daemon did not come up`
        // forever with a daemon still firing jobs at them.
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));

        let q = Arc::clone(&queue);
        let _ = std::thread::spawn(move || {
            let _guard = q.lock().unwrap();
            panic!("poisoning the queue mutex (this panic is the test's fixture)");
        })
        .join();
        assert!(
            queue.is_poisoned(),
            "the fixture must actually poison the mutex"
        );

        let socket = serve_on(dir.path(), queue, worker_builder, MAX_WORKERS);
        assert_eq!(
            round_trip(&socket, r#"{"op":"Ping"}"#),
            pong(),
            "a poisoned queue mutex must not cost the daemon its control plane"
        );
    }
}

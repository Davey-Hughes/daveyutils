//! The daemon must be a singleton: a second one must neither take the lock nor
//! steal a live socket. Hermetic — tempdir lock/socket, no real daemon spawned.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use nudge::daemon::acquire_singleton_lock;
use nudge::paths::Paths;
use nudge::queue::Queue;

#[test]
fn second_lock_attempt_fails_while_first_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let first = acquire_singleton_lock(dir.path()).expect("first lock should succeed");
    assert!(
        acquire_singleton_lock(dir.path()).is_err(),
        "a second daemon must NOT be able to take the lock while the first holds it"
    );
    drop(first);
    // Once released, a new daemon can take it.
    assert!(
        acquire_singleton_lock(dir.path()).is_ok(),
        "lock must be reusable after the holder exits"
    );
}

#[test]
fn serve_refuses_to_steal_a_live_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");

    // A "live daemon" already listening on the socket.
    let live = UnixListener::bind(&socket).unwrap();

    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
    let err = nudge::ipc::server::serve(&socket, queue)
        .expect_err("serve must refuse to bind over a live socket instead of stealing it");
    let _ = err;

    // The original listener must still own a working socket.
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "the live daemon's socket must still be connectable — it was stolen/unlinked"
    );
    drop(live);
}

#[test]
fn serve_reclaims_a_stale_socket_file() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    // A stale socket file with nobody listening (previous daemon crashed).
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    // serve() should reclaim it. Run it briefly on a thread; it loops forever,
    // so just prove the socket becomes connectable (i.e. it bound successfully).
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
    let s = socket.clone();
    std::thread::spawn(move || {
        let _ = nudge::ipc::server::serve(&s, queue);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&socket).is_ok() {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ok, "serve must reclaim a stale socket file and bind");
}

/// The three tests above exercise `acquire_singleton_lock` directly. Nothing
/// pins the wiring in `daemon::run` itself: if the acquire call were removed,
/// or its `Result` silently discarded instead of propagated with `?`, every
/// other test here would still pass. This test proves `run` refuses to start
/// while another daemon holds the lock, by holding the lock externally first
/// and asserting `run` returns `WouldBlock` instead of proceeding into its loop.
#[test]
fn run_refuses_to_start_while_another_daemon_holds_the_lock() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    // Simulate a daemon already running: hold the singleton lock.
    let _held = acquire_singleton_lock(dir.path()).expect("first lock");

    let paths = Paths {
        state_dir: dir.path().to_path_buf(),
        queue: dir.path().join("queue.json"),
        socket: dir.path().join("nudge.sock"),
    };

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = nudge::daemon::run(&paths, None, None, jiff::ToSpan::hours(6));
        let _ = tx.send(r.map(|_| ()).map_err(|e| e.kind()));
    });

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(kind)) => assert_eq!(
            kind,
            std::io::ErrorKind::WouldBlock,
            "run must refuse with WouldBlock while another daemon holds the lock"
        ),
        Ok(Ok(())) => panic!("run returned Ok — it must refuse while the lock is held"),
        Err(_) => panic!(
            "run did NOT refuse: it started a second daemon while the lock was held \
             (is `run` still taking the singleton lock, and binding it to a named \
             variable so it isn't dropped immediately?)"
        ),
    }
}

/// The test above exercises the CONTENDED path (lock already held before `run`
/// starts), which errors out before the binding's drop timing matters — it
/// can't tell `let _lock = acquire_singleton_lock(..)?;` apart from
/// `let _ = acquire_singleton_lock(..)?;`, because in Rust `let _ = expr;`
/// drops the temporary immediately while `let _lock = expr;` keeps it alive
/// for the rest of the scope. If `run` ever regresses to the latter, the lock
/// is released the instant it's taken and a running daemon holds no lock at
/// all: the double-daemon data-loss defect reopens silently. This test
/// exercises the UNCONTENDED path: a daemon that starts clean and runs must
/// still hold the lock for as long as it's alive.
#[test]
fn a_running_daemon_holds_the_lock_for_its_whole_life() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let paths = Paths {
        state_dir: dir.path().to_path_buf(),
        queue: dir.path().join("queue.json"),
        socket: socket.clone(),
    };

    std::thread::spawn(move || {
        let _ = nudge::daemon::run(&paths, None, None, jiff::ToSpan::hours(6));
    });

    // Wait until it's actually up (its socket appears / accepts).
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && UnixStream::connect(&socket).is_err() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "daemon under test never came up"
    );

    // THE POINT: while that daemon runs, its singleton lock must still be
    // held. If `run` bound the lock to `let _` it would already be released
    // here and this would succeed -- silently reopening the double-daemon
    // defect.
    assert!(
        acquire_singleton_lock(dir.path()).is_err(),
        "a running daemon must hold the singleton lock for its whole lifetime \
         (is it bound to a NAMED variable, not `let _`?)"
    );
}

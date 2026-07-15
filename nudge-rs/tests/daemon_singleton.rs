//! The daemon must be a singleton: a second one must neither take the lock nor
//! steal a live socket. Hermetic — tempdir lock/socket, no real daemon spawned.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use nudge::daemon::acquire_singleton_lock;
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

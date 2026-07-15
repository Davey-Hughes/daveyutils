//! Shared fixtures for nudge's CLI integration tests.
//!
//! `#![allow(dead_code)]` because this module is compiled once per consuming
//! test binary (cargo treats every file under `tests/` as its own crate) and
//! most consumers only need one or two of these items -- the rest would
//! otherwise warn as unused in every binary that doesn't happen to call them.
#![allow(dead_code)]

use std::path::Path;

use tempfile::TempDir;

/// The byte budget every socket path built by these tests must fit inside.
///
/// A Unix domain socket path lives in `sockaddr_un.sun_path`, which is capped
/// at 104 bytes on macOS and 108 on Linux. CI only runs Linux, so 108 is the
/// only bound a Linux-only run would ever enforce on its own -- and that is
/// exactly how a macOS-only failure got through: the 104-byte bound is
/// invisible unless something on Linux checks it deliberately. Every fixture
/// here targets the tighter, macOS number instead, so a green Linux CI run
/// can't hide a `UnixListener::bind` that only fails on macOS.
pub const SOCKET_PATH_BUDGET: usize = 104;

/// A tempdir rooted at `/tmp`, for any test that points `HOME` (or otherwise
/// derives a socket path) at it.
///
/// `tempfile::tempdir()` roots at `$TMPDIR`, which on macOS looks like
/// `/var/folders/8j/sfr9qqcj73j4p6nhwcfpr0th0000gn/T/.tmpXXXXXX` -- ~59 bytes
/// on its own, before anything is joined onto it. macOS has no
/// `XDG_RUNTIME_DIR`, so `paths::resolve_from` puts the socket at
/// `<home>/Library/Application Support/nudge/nudge.sock`, another ~45 bytes.
/// Put together that's 104: AT the SUN_LEN budget, so `UnixListener::bind`
/// fails with "path must be shorter than SUN_LEN" before the test gets to
/// exercise anything.
///
/// Rooting at `/tmp` instead gives a path like `/tmp/nudge-XXXXXX` -- ~21
/// bytes -- for a total of ~66 with the same suffix: comfortably inside the
/// budget. A real macOS user is never at risk either way --
/// `/Users/name/Library/Application Support/nudge/nudge.sock` is ~56 bytes --
/// this is purely a hazard of using a tempdir as `HOME` in a test.
///
/// DO NOT "helpfully" change this back to `tempfile::tempdir()`. On Linux
/// (SUN_LEN 108, and often a short `$TMPDIR` already) that revert keeps
/// passing locally and in CI, which is exactly why this got missed the first
/// time -- it only breaks for a macOS user, who is not the one reverting it.
pub fn short_tempdir() -> TempDir {
    tempfile::Builder::new()
        .prefix("nudge-")
        .tempdir_in("/tmp")
        .expect("create a tempdir under /tmp")
}

/// Fails loudly, at fixture-build time, if `socket` would not fit in
/// `sockaddr_un.sun_path` on macOS -- rather than surfacing however many
/// commands later as an opaque `InvalidInput`/EINVAL out of
/// `UnixListener::bind`, deep inside whatever the test was actually trying to
/// check.
///
/// Call this right after building a `Paths` (or any ad hoc socket path) in a
/// fixture that derives it from a tempdir, so a future change to
/// `resolve_from`, a longer prefix, or a longer platform `$TMPDIR` fails here,
/// with a byte count and the path attached, instead of mysteriously.
pub fn assert_socket_path_fits(socket: &Path) {
    let len = socket.as_os_str().len();
    assert!(
        len < SOCKET_PATH_BUDGET,
        "socket path is {len} bytes, budget is {SOCKET_PATH_BUDGET} (macOS's \
         SUN_LEN; Linux's is a looser 108, but this suite always targets the \
         tighter number): {socket:?}\n\
         UnixListener::bind will fail with 'path must be shorter than SUN_LEN' \
         at this length. If this fixture builds HOME from a tempdir, use \
         common::short_tempdir() instead of tempfile::tempdir()."
    );
}

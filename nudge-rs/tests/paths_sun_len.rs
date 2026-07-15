//! Pins the arithmetic that `common::short_tempdir` and
//! `common::assert_socket_path_fits` rest on.
//!
//! CI is Linux-only, and Linux's SUN_LEN (108) is looser than macOS's (104),
//! so a passing Linux suite proves nothing about the macOS budget on its own
//! -- that's exactly how `tests/cli_daemon_version.rs` broke on macOS while
//! staying green everywhere this project's CI actually runs. This test cannot
//! bind a real macOS socket from Linux, so it verifies by computing the
//! macOS-shaped path and asserting its length instead of by hoping.

mod common;

use nudge::paths::{resolve_from, Os};

#[test]
fn short_tempdir_leaves_room_for_the_macos_socket_path() {
    let tmp = common::short_tempdir();
    let paths = resolve_from(tmp.path(), None, None, Os::Macos);
    common::assert_socket_path_fits(&paths.socket);
}

/// The negative case: this is *why* `short_tempdir` exists, not just that it
/// works. `tempfile::tempdir()`'s macOS shape --
/// `/var/folders/8j/sfr9qqcj73j4p6nhwcfpr0th0000gn/T/.tmpXXXXXX`, ~59 bytes --
/// plus `resolve_from`'s `/Library/Application Support/nudge/nudge.sock`
/// suffix, ~45 bytes, lands at exactly 104: AT the budget, which is already
/// over it (a bind of a path this long is what actually failed in CI).
#[test]
fn a_tempfile_tempdir_shaped_macos_home_overflows_the_budget() {
    let macos_tmpdir_shape =
        std::path::Path::new("/var/folders/8j/sfr9qqcj73j4p6nhwcfpr0th0000gn/T/.tmp1a2b3c");
    let paths = resolve_from(macos_tmpdir_shape, None, None, Os::Macos);
    let len = paths.socket.as_os_str().len();
    assert!(
        len >= common::SOCKET_PATH_BUDGET,
        "this fixture is meant to demonstrate the overflow this suite guards \
         against; got {len} bytes, budget {}: {:?} -- if this now fits, the \
         shape below is no longer representative of macOS's real $TMPDIR and \
         should be updated, not deleted",
        common::SOCKET_PATH_BUDGET,
        paths.socket
    );
}

/// A real macOS user's home directory, by contrast, is nowhere near the
/// budget -- this is purely a hazard of tests using a tempdir as `HOME`.
#[test]
fn a_real_macos_home_directory_fits_comfortably() {
    let paths = resolve_from(
        std::path::Path::new("/Users/reasonablename"),
        None,
        None,
        Os::Macos,
    );
    common::assert_socket_path_fits(&paths.socket);
}

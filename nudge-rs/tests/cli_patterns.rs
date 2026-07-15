//! A typo'd `NUDGE_*_PATTERN` must be reported to the user running the CLI.
//!
//! `detect.rs` deliberately degrades to the built-in banner rather than panic on
//! a bad pattern — it runs on the daemon's scheduler thread, where a panic kills
//! every pending job — and warns instead. But that warning reached nobody:
//! `init_tracing()` is only called in daemon mode, so on the CLI path there is
//! no subscriber and the warning is discarded, and in the auto-started daemon
//! stderr is /dev/null. So a typo'd pattern was silently ignored and all the
//! user saw was `no rate-limit banner detected in <pane>` — which points at the
//! pane, not at the variable that is actually wrong.
//!
//! Hermetic: no daemon is spawned. Validation happens before any of these
//! commands reach a socket, and the singleton lock is held regardless so that
//! nothing can leave one behind.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use nudge::paths::{resolve_from, Os, Paths};

fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

fn child_paths(home: &Path) -> Paths {
    resolve_from(
        home,
        Some(&home.join("state")),
        Some(&home.join("run")),
        os(),
    )
}

struct Fixture {
    home: std::path::PathBuf,
    _tmp: tempfile::TempDir,
    _lock: std::fs::File,
}

fn fixture() -> Fixture {
    // common::short_tempdir, not tempfile::tempdir: these commands never
    // reach the socket today (validation rejects the bad pattern first), but
    // the fixture still resolves and creates the socket's parent dir from a
    // HOME rooted at the tempdir, so it's exposed the moment that changes.
    // macOS's $TMPDIR is long enough that resolve_from's suffix overflows
    // SUN_LEN there.
    let tmp = common::short_tempdir();
    let home = tmp.path().to_path_buf();
    let paths = child_paths(&home);
    common::assert_socket_path_fits(&paths.socket);
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    let lock = nudge::daemon::acquire_singleton_lock(&paths.state_dir).unwrap();
    Fixture {
        home,
        _tmp: tmp,
        _lock: lock,
    }
}

fn run_nudge(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nudge"));
    cmd.args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run the nudge binary")
}

fn report(out: &Output) -> String {
    format!(
        "status {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn an_invalid_clock_pattern_is_reported_not_swallowed() {
    // "codex (" is not a perverse input: an unbalanced paren is what you get
    // from typing a banner phrase, which is exactly what this variable is for.
    let f = fixture();
    let out = run_nudge(
        &f.home,
        &["-p", "nosuch:0.0"],
        &[("NUDGE_CLOCK_PATTERN", "codex (")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        stderr.contains("invalid NUDGE_CLOCK_PATTERN"),
        "the user must be told which variable is wrong\n{}",
        report(&out)
    );
}

#[test]
fn an_invalid_duration_pattern_is_reported_not_swallowed() {
    let f = fixture();
    let out = run_nudge(
        &f.home,
        &["-p", "nosuch:0.0"],
        &[("NUDGE_DURATION_PATTERN", "a[b")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        stderr.contains("invalid NUDGE_DURATION_PATTERN"),
        "the user must be told which variable is wrong\n{}",
        report(&out)
    );
}

#[test]
fn a_valid_pattern_does_not_trip_the_check() {
    // The check must not become a new way for nudge to refuse to run. This gets
    // past validation and fails later, on the pane -- which is the pre-existing
    // behaviour and none of this test's business beyond not being the pattern.
    let f = fixture();
    let out = run_nudge(
        &f.home,
        &["-p", "nosuch:0.0"],
        &[("NUDGE_CLOCK_PATTERN", "rate limited")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stderr.contains("invalid NUDGE_CLOCK_PATTERN"),
        "a perfectly good pattern must not be rejected\n{}",
        report(&out)
    );
}

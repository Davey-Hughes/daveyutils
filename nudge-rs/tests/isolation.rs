//! `common::keep_off_the_developers_panes` must leave a command no way to
//! the developer's own tmux or herdr.
//!
//! nudge labels the panes in its job list and its confirmations from every
//! pane it can list, so most of what the CLI suites run asks tmux for them.
//! Two of those suites set nothing to stop it. Two more pointed `TMUX_TMPDIR`
//! at a directory they never made, and tmux skips one that does not exist in
//! favour of its default socket directory. Either way the `list-panes` went
//! to the developer's server (seen 2026-11-20, with a `tmux` on `PATH` that
//! logged where each call would land).

mod common;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

#[test]
fn a_kept_command_has_no_way_to_the_developers_tmux_or_herdr() {
    let tmp = common::short_tempdir();
    let mut cmd = Command::new("true");

    common::keep_off_the_developers_panes(&mut cmd, tmp.path());

    let env: HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
    let tmux_tmpdir = env
        .get(OsStr::new("TMUX_TMPDIR"))
        .copied()
        .flatten()
        .expect("TMUX_TMPDIR is set");
    assert!(
        Path::new(tmux_tmpdir).starts_with(tmp.path()),
        "TMUX_TMPDIR must be the fixture's own"
    );
    assert!(
        Path::new(tmux_tmpdir).is_dir(),
        "TMUX_TMPDIR must exist, or tmux falls back to its default"
    );
    // `Some(None)` is a variable the command will not inherit.
    for gone in ["TMUX", "TMUX_PANE"] {
        assert_eq!(env.get(OsStr::new(gone)), Some(&None), "{gone} must go");
    }
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("HERDR_")) {
            assert_eq!(env.get(key.as_os_str()), Some(&None), "{key:?} must go");
        }
    }
    let config = env
        .get(OsStr::new("XDG_CONFIG_HOME"))
        .copied()
        .flatten()
        .expect("XDG_CONFIG_HOME is set");
    assert!(
        Path::new(config).starts_with(tmp.path()),
        "herdr keeps its sessions under XDG_CONFIG_HOME, which must be the fixture's own"
    );
}

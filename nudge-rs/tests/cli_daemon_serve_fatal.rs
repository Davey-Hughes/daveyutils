//! A daemon that loses its IPC server must not stay alive.
//!
//! `daemon::run` treats `serve` returning as fatal and exits the process: a
//! daemon with no control plane still fires jobs into panes but can't be
//! listed, cancelled or edited, and the singleton lock (correctly) stops a
//! replacement from taking over. So the policy is "take the whole process
//! down", and until now nothing tested it -- `process::exit` can't be observed
//! from inside the process that calls it, so the policy shipped on inspection
//! alone.
//!
//! Spawning the real binary is what makes it observable: the exit code is the
//! assertion. Hermetic -- every path is a child-process env var, set with
//! `Command::env` and never on this process, so nothing here can race a
//! sibling test. No tmux: the daemon dies before it reaches a job.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the daemon on any exit path, including the assertion failures below.
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait up to `secs` for `child` to exit, returning its status.
fn wait_for_exit(child: &mut Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    None
}

#[test]
fn a_daemon_whose_ipc_server_cannot_start_takes_the_process_down() {
    let tmp = common::short_tempdir();
    let home = tmp.path();
    let state = home.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // Linux only, and not for want of trying: the fixture needs a failure that
    // `serve` reaches but the singleton lock does not, and on Linux
    // XDG_RUNTIME_DIR is exactly that -- the socket's directory, which nothing
    // before `serve` touches. On macOS the socket sits beside the state file,
    // so anything that blocks the socket dir blocks `acquire_singleton_lock`
    // first and the daemon dies for a different reason, testing nothing. The
    // policy under test is platform-independent, so pinning it on Linux pins it.
    if !cfg!(target_os = "linux") {
        eprintln!("skipping: fixture needs XDG_RUNTIME_DIR to be the socket dir (Linux)");
        return;
    }

    // A REGULAR FILE where the socket's directory belongs. `serve` opens with
    // `create_dir_all(socket.parent())`, which fails on it, so serve returns
    // Err -- the fatal path -- without ever binding. A merely *missing*
    // directory would not do it: serve creates that.
    let runtime = home.join("run-is-a-file");
    std::fs::write(&runtime, b"not a directory").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nudge"));
    cmd.arg("--daemon")
        .env("HOME", home)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut daemon = DaemonGuard(cmd.spawn().expect("spawn the daemon"));

    let status = wait_for_exit(&mut daemon.0, 10).unwrap_or_else(|| {
        panic!(
            "the daemon is STILL RUNNING after its IPC server failed to start. \
             It has no control plane -- it cannot be listed, cancelled or edited, \
             and the singleton lock stops a working daemon from replacing it -- \
             but it will still fire every pending job into the user's panes."
        )
    });

    assert!(
        !status.success(),
        "a daemon that lost its control plane must not exit 0: {status}"
    );
    assert_eq!(
        status.code(),
        Some(1),
        "the fatal serve path exits 1; got {status}"
    );

    let mut stderr = String::new();
    if let Some(mut e) = daemon.0.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut stderr);
    }
    assert!(
        stderr.contains("ipc server exited"),
        "the daemon must say why it died, or its death is as silent as the \
         headless daemon this policy replaces; got:\n{stderr}"
    );
}

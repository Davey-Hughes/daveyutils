//! Shared fixtures for nudge's CLI integration tests.
//!
//! `#![allow(dead_code)]` because this module is compiled once per consuming
//! test binary (cargo treats every file under `tests/` as its own crate) and
//! most consumers only need one or two of these items -- the rest would
//! otherwise warn as unused in every binary that doesn't happen to call them.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// What to pass a [`stand_in`] so that it waits for half a minute: `-c` and
/// this. The `; :` after `sleep` stops bash from running sleep in its own
/// place, which would rename the process `sleep`.
pub const STAND_IN_WAIT: &str = "sleep 30; :";

/// A copy of bash at `dir/name`, to stand in for a program, such as herdr's
/// client or tmux's, that tmux or herdr is meant to find in a pane. The kernel
/// names a process after the file it ran, and tmux and herdr both go by that
/// name, so the copy passes for the program without it being installed. Run it
/// with `-c` and [`STAND_IN_WAIT`].
///
/// bash, not sleep: Ubuntu 26.04's sleep is uutils' coreutils, one program that
/// picks its tool by the name it runs under, and a copy of it named `herdr`
/// exits at once with `coreutils: unknown program 'herdr'` (seen 2026-11-19).
/// bash runs under any name.
pub fn stand_in(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::copy("/bin/bash", &path).expect("copy /bin/bash");
    path
}

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
/// Put together that's 104: AT the `SUN_LEN` budget, so `UnixListener::bind`
/// fails with "path must be shorter than `SUN_LEN`" before the test gets to
/// exercise anything.
///
/// Rooting at `/tmp` instead gives a path like `/tmp/nudge-XXXXXX` -- ~21
/// bytes -- for a total of ~66 with the same suffix: comfortably inside the
/// budget. A real macOS user is never at risk either way --
/// `/Users/name/Library/Application Support/nudge/nudge.sock` is ~56 bytes --
/// this is purely a hazard of using a tempdir as `HOME` in a test.
///
/// DO NOT "helpfully" change this back to `tempfile::tempdir()`. On Linux
/// (`SUN_LEN` 108, and often a short `$TMPDIR` already) that revert keeps
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
    // `display()`, not `{socket:?}`: the Debug form escapes and quotes, which is
    // noise in a message whose whole job is to show you a path you can measure.
    let shown = socket.display();
    assert!(
        len < SOCKET_PATH_BUDGET,
        "socket path is {len} bytes, budget is {SOCKET_PATH_BUDGET} (macOS's \
         SUN_LEN; Linux's is a looser 108, but this suite always targets the \
         tighter number): {shown}\n\
         UnixListener::bind will fail with 'path must be shorter than SUN_LEN' \
         at this length. If this fixture builds HOME from a tempdir, use \
         common::short_tempdir() instead of tempfile::tempdir()."
    );
}

/// A tmux server private to one test, living under its own `TMUX_TMPDIR`.
///
/// `TMUX_TMPDIR` rather than `-L`, because the point is usually a child `nudge`
/// that has to reach this server with no socket flag of its own: give it
/// [`TmuxServer::env`]. `-f /dev/null` keeps the developer's ~/.tmux.conf out,
/// so `base-index` and the like are tmux's defaults.
///
/// Every command carries the env explicitly rather than trusting the process
/// env: a `kill-server` that leaked to the default socket would take down the
/// developer's entire tmux, so it must not be possible for one to.
pub struct TmuxServer {
    tmpdir: std::path::PathBuf,
}

impl TmuxServer {
    /// Start a detached 80x24 session named `session` running `sh`.
    pub fn start(tmpdir: &Path, session: &str) -> Self {
        std::fs::create_dir_all(tmpdir).unwrap();
        let server = TmuxServer {
            tmpdir: tmpdir.to_path_buf(),
        };
        let ok = server
            .tmux()
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .status()
            .expect("spawn tmux")
            .success();
        assert!(ok, "failed to start the private tmux session");
        server
    }

    /// A `tmux` command aimed at this server and nothing else.
    pub fn tmux(&self) -> std::process::Command {
        let mut c = std::process::Command::new("tmux");
        self.env(&mut c);
        c
    }

    /// Run a tmux command on this server and return what it printed.
    pub fn run(&self, args: &[&str]) -> String {
        let out = self.tmux().args(args).output().expect("spawn tmux");
        assert!(out.status.success(), "tmux {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Point `cmd`'s tmux calls at this server. `TMUX` names the socket of the
    /// session running the test, and tmux prefers it, so it goes.
    ///
    /// Only while `TMUX_TMPDIR` exists. tmux skips a `TMUX_TMPDIR` that does
    /// not, and falls back to the default socket directory, where the
    /// developer's own server lives (seen 2026-11-19: a fixture that deleted
    /// its tempdir before this server's `kill-server` reached
    /// `/tmp/tmux-1000/default`). A command aimed there must fail here.
    pub fn env(&self, cmd: &mut std::process::Command) {
        assert!(
            self.tmpdir.is_dir(),
            "TMUX_TMPDIR {} is gone, so tmux would fall back to the developer's \
             own server; drop the TmuxServer before its tempdir",
            self.tmpdir.display()
        );
        // HISTFILE reaches the panes through the command that starts the
        // server. A pane's `sh` writes what was typed into it to `$HOME` as it
        // exits: the developer's own home, unless the test moved HOME into
        // its tempdir. There the write can land while the tempdir is being
        // removed, which then fails and leaves it behind. An empty HISTFILE
        // keeps none, as `herdr_env` does for herdr.
        cmd.env("TMUX_TMPDIR", &self.tmpdir)
            .env("HISTFILE", "")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        // With the directory gone, `kill-server` would reach the developer's
        // own server (see `env`). Leave this one running, and say so, rather
        // than risk that. A panic here would abort a test already unwinding.
        if !self.tmpdir.is_dir() {
            eprintln!(
                "warning: TMUX_TMPDIR {} went before its tmux server was stopped; \
                 leaving the server running rather than risk the developer's own",
                self.tmpdir.display()
            );
            return;
        }
        // Safe only because of the explicit TMUX_TMPDIR + TMUX removal in
        // `env`: this can address nothing but the server we started in our own
        // tempdir.
        let _ = self.tmux().arg("kill-server").status();
    }
}

/// Whether tmux is installed. Tests that need a real server skip without it.
pub fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A herdr server private to one test, with its own config, state and home
/// directories under `tmpdir`.
///
/// herdr keeps its socket, session file and logs in `$XDG_CONFIG_HOME/herdr`,
/// and fetched agent manifests in `$XDG_STATE_HOME/herdr` (seen 2026-11-19),
/// so pointing those and `HOME` into `tmpdir` leaves nothing of the
/// developer's for it to read or write. The session running these tests may
/// itself sit in a herdr pane, whose `HERDR_SOCKET_PATH` names the developer's
/// live server: every inherited `HERDR_*` is removed from the server and from
/// every command handed out here, and each of those names this server's
/// socket, so none of them can reach that one.
///
/// It is never stopped with `herdr server stop`, which goes through a socket:
/// it is a child of the test, and is signalled by its PID.
pub struct HerdrServer {
    dir: std::path::PathBuf,
    socket: std::path::PathBuf,
    child: std::process::Child,
}

impl HerdrServer {
    /// Start a headless herdr server whose files all live under `tmpdir`, and
    /// wait until it answers.
    pub fn start(tmpdir: &Path) -> Self {
        let dir = tmpdir.to_path_buf();
        // Where herdr puts its API socket when `XDG_CONFIG_HOME` is `dir`, as
        // the server's own banner reported it on 2026-11-19.
        let socket = dir.join("herdr").join("herdr.sock");
        assert_socket_path_fits(&socket);
        // herdr resumes the agent a pane last reported once it restarts, by
        // typing `claude --resume <id>` into the pane's shell (seen
        // 2026-11-19), so a test that reports a Claude session and restarts
        // the server would start a real Claude Code. This stub is first on
        // the server's PATH and stands in for it, holding the pane as an agent
        // would.
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("claude");
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 300\n").unwrap();
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        // Constructed before the wait, so that a server that never answers is
        // still killed by `Drop`.
        let server = HerdrServer {
            child: spawn_herdr_server(&dir),
            dir,
            socket,
        };
        server.wait_until_up();
        server
    }

    /// The server's API socket, as a job would store it.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// A `herdr` command aimed at this server and nothing else.
    pub fn herdr(&self) -> std::process::Command {
        let mut c = std::process::Command::new("herdr");
        herdr_env(&mut c, &self.dir);
        c.env("HERDR_SOCKET_PATH", &self.socket);
        c
    }

    /// Run a herdr command on this server and return what it printed.
    pub fn run(&self, args: &[&str]) -> String {
        let out = self.herdr().args(args).output().expect("spawn herdr");
        assert!(
            out.status.success(),
            "herdr {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Open a pane in a workspace of its own and type `cmd` into its shell, or
    /// leave the shell at its prompt if `cmd` is empty. Returns the pane's id.
    ///
    /// `pane run` only queues the command: it may still be starting when this
    /// returns.
    pub fn new_pane(&self, cmd: &str) -> String {
        let created: serde_json::Value =
            serde_json::from_str(&self.run(&["workspace", "create"])).expect("herdr JSON");
        let pane = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .expect("a new workspace's root pane")
            .to_string();
        if !cmd.is_empty() {
            self.run(&["pane", "run", &pane, cmd]);
        }
        pane
    }

    /// Stop the server and start a new one with the same directories, as a
    /// reboot would.
    ///
    /// SIGTERM and not `Child::kill`'s SIGKILL: herdr saves the session it
    /// restores from as the session changes, but not at once, and always on
    /// the way down. A test server killed outright a second after its pane
    /// was made came back with no workspaces at all (seen 2026-11-19). A
    /// reboot ends it with SIGTERM too.
    pub fn restart(&mut self) {
        let pid = self.child.id().to_string();
        let ok = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("spawn kill")
            .success();
        assert!(ok, "could not signal the herdr server {pid}");
        let deadline = std::time::Instant::now() + HERDR_PATIENCE;
        while self.child.try_wait().expect("wait for herdr").is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the herdr server {pid} did not exit on SIGTERM"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.child = spawn_herdr_server(&self.dir);
        self.wait_until_up();
    }

    /// Wait until the server answers a request, which is later than its socket
    /// appearing. After a restart the old socket file may still be there.
    fn wait_until_up(&self) {
        let deadline = std::time::Instant::now() + HERDR_PATIENCE;
        loop {
            let up = self
                .herdr()
                .args(["pane", "list"])
                .output()
                .is_ok_and(|o| o.status.success());
            if up {
                return;
            }
            let log = std::fs::read_to_string(self.dir.join("server.out")).unwrap_or_default();
            assert!(
                std::time::Instant::now() < deadline,
                "the herdr server never answered on {}:\n{log}",
                self.socket.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for HerdrServer {
    fn drop(&mut self) {
        // By the handle, never by name or through a socket: this can only be
        // the server this test started.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How long a private herdr server gets to start or stop. A failure deadline,
/// not a delay: one that works never spends it.
const HERDR_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Start `herdr server` with its files in `dir`, its output in
/// `dir/server.out`.
fn spawn_herdr_server(dir: &Path) -> std::process::Child {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("server.out"))
        .unwrap();
    let mut cmd = std::process::Command::new("herdr");
    herdr_env(&mut cmd, dir);
    cmd.arg("server")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect("spawn herdr server")
}

/// The environment a private herdr server and every command aimed at it run
/// in: everything that could lead to the developer's herdr, tmux or Claude
/// Code removed, and every directory herdr writes to inside `dir`.
fn herdr_env(cmd: &mut std::process::Command, dir: &Path) {
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("HERDR_")) {
            cmd.env_remove(key);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(dir.join("bin")).chain(std::env::split_paths(&path)))
            .unwrap();
    cmd.env_remove("TMUX")
        .env_remove("TMUX_PANE")
        // A Claude Code that started anyway, the stub notwithstanding, would
        // otherwise use the developer's own settings and sessions.
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        // herdr starts a pane in `$SHELL`. A plain sh keeps the developer's
        // shell config out of what the tests read.
        .env("SHELL", "/bin/sh")
        // A pane's shell writes its history to `$HOME` as it exits, which is
        // after the server is killed and can be after the test has removed
        // `dir`, leaving a `.bash_history` behind in a directory of its own
        // (one in five runs, 2026-11-19). An empty HISTFILE keeps none.
        .env("HISTFILE", "")
        .env("PATH", path);
}

/// Whether herdr is installed. Tests that need a real server skip without it.
pub fn herdr_available() -> bool {
    std::process::Command::new("herdr")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

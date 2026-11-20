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

/// An in-process server standing in for a nudge daemon that is still
/// starting: it binds the socket only after a delay, so the CLI under test
/// finds nothing listening at first, and then answers from a queue the test
/// holds.
///
/// Dropping it holds it back for good, so declare it above the fixture's
/// `TempDir`: fields drop in the order they are declared, and it must go
/// first. A test can end before the delay is up. The thread this replaced
/// then called `server::serve`, which creates the socket's directory before
/// binding, and so put back a tempdir the test had already removed, with
/// `run/nudge.sock` in it and nothing left to remove it (seen 2026-11-20: one
/// `/tmp/nudge-XXXXXX` per run of `cli_job_summary`).
pub struct StandInDaemon {
    /// Set once the fixture is going. The thread binds while holding this
    /// lock and `Drop` sets it under the same lock, so a bind either finished
    /// before the tempdir began to go, and its socket goes with the rest, or
    /// never happens. A check without the lock would leave room for the
    /// socket to appear in a directory already being emptied, which then
    /// cannot be removed.
    gone: std::sync::Arc<std::sync::Mutex<bool>>,
}

impl StandInDaemon {
    /// After `delay`, bind `socket` and answer requests from `queue`, one
    /// connection at a time. `socket`'s directory must already exist: this
    /// never creates it.
    pub fn start_after(
        delay: std::time::Duration,
        socket: &Path,
        queue: std::sync::Arc<std::sync::Mutex<nudge::queue::Queue>>,
    ) -> Self {
        let gone = std::sync::Arc::new(std::sync::Mutex::new(false));
        let fixture_gone = std::sync::Arc::clone(&gone);
        let socket = socket.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let listener = {
                let gone = fixture_gone.lock().unwrap();
                if *gone {
                    return;
                }
                match std::os::unix::net::UnixListener::bind(&socket) {
                    Ok(listener) => listener,
                    Err(e) => {
                        eprintln!("stand-in daemon: bind {}: {e}", socket.display());
                        return;
                    }
                }
            };
            // Loops forever, as a daemon does, and is reaped when the test
            // binary exits. One connection's error is that client's affair,
            // but an `accept` that keeps failing, as it does once the test
            // binary is out of file descriptors, must not spin.
            loop {
                if nudge::ipc::server::serve_once(&listener, &queue).is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        });
        StandInDaemon { gone }
    }
}

impl Drop for StandInDaemon {
    fn drop(&mut self) {
        // A poisoned lock still holds the flag. Panicking here would abort a
        // test that is already unwinding.
        *self
            .gone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }
}

/// Kills a daemon a test started, on any exit path, along with whatever the
/// daemon had started itself.
///
/// A daemon types into a pane by running `tmux`. Killing the daemon alone
/// leaves a `tmux` it had just started to run on, and the test's tmux server
/// and tempdir are the next things to go: a `tmux` that looks for its server
/// after that finds `TMUX_TMPDIR` missing and falls back to the developer's
/// own (see [`TmuxServer::env`]). So the daemon's whole process group is
/// killed. A daemon from production `spawn_daemon` leads a session, and with
/// it a group that its children stay in. One a test starts itself needs
/// `process_group(0)` for this to reach its children.
///
/// The daemon is kept in here, out of reach, so that nothing waits for it
/// before [`DaemonGuard::stop`] does. Until it has been waited for, its pid
/// is its own, even once it has exited, and so is the group of that number.
/// Afterwards the number can pass to some other process, whose group a kill
/// would then take.
pub struct DaemonGuard {
    daemon: std::process::Child,
    /// Set by the first [`DaemonGuard::stop`], which is the only one to act.
    stopped: bool,
    /// How the daemon ended, once stopped.
    ended: Option<std::process::ExitStatus>,
    /// The pipe of the [`Watchdog`] that kills this daemon's group if the
    /// test process ends first, for a guard from [`Watchdog::guard`]. Not a
    /// hold on it: the watchdog goes off when the test's one end closes.
    watchdog: std::sync::Weak<std::io::PipeWriter>,
}

impl DaemonGuard {
    pub fn new(daemon: std::process::Child) -> Self {
        DaemonGuard {
            daemon,
            stopped: false,
            ended: None,
            watchdog: std::sync::Weak::new(),
        }
    }

    /// The daemon's process id.
    pub fn pid(&self) -> libc::pid_t {
        libc::pid_t::try_from(self.daemon.id()).expect("a process id is a pid_t")
    }

    /// Whether the daemon has exited. This only looks. `Child::try_wait`
    /// would also collect the daemon, and its group could then no longer be
    /// killed (see the type's doc), though a daemon that died on its own is
    /// the one most likely to have left a `tmux` behind.
    pub fn has_exited(&self) -> bool {
        self.stopped || matches!(self.look(), Seen::Exited | Seen::NotOurs)
    }

    /// What `waitid` says of the daemon, without collecting it.
    fn look(&self) -> Seen {
        // SAFETY: `info` is a plain struct of integers that `waitid` fills
        // in, and zeroes are a valid one.
        let (answered, info) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
            let answered = libc::waitid(libc::P_PID, self.daemon.id(), &raw mut info, options);
            (answered, info)
        };
        if answered == 0 {
            if info.si_signo == libc::SIGCHLD {
                Seen::Exited
            } else {
                Seen::Running
            }
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            Seen::NotOurs
        } else {
            // `waitid` could not say, and the daemon may well be running:
            // calling it exited would have a test kill it and then blame it
            // for dying.
            Seen::Running
        }
    }

    /// Kill the daemon and everything in its group, and say how the daemon
    /// ended: with its own exit status if it had already exited, which is
    /// what a test that found it dead wants to show.
    pub fn stop(&mut self) -> Option<std::process::ExitStatus> {
        if self.stopped {
            return self.ended;
        }
        self.stopped = true;
        // Collected by something other than this guard: its pid may already
        // be another process's (see the type's doc), so nothing is killed
        // and nobody knows how it ended.
        let ours = self.look() != Seen::NotOurs;
        let pid = self.pid();
        if ours {
            // 0 and -1 name the caller's own group and every process there
            // is. No child has pid 0 or 1, but nothing here should rest on
            // that.
            if pid > 1 {
                // SAFETY: `kill` takes ids and no pointers. A group numbered
                // after the daemon's pid can only be one the daemon leads
                // (see the type's doc). A daemon that leads none, having
                // been started in the test's own group, has no such group,
                // and this fails with ESRCH and touches nothing.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = self.daemon.kill();
        }
        // Between the kill and the collecting. Any sooner, and a test that
        // ended right here would leave a daemon its watchdog had been told
        // to spare, with its tmux server and tempdir gone from under it. Any
        // later, and the watchdog could kill a group that is no longer the
        // daemon's.
        if let Some(pipe) = self.watchdog.upgrade() {
            tell(&pipe, &format!("reaped {pid}"));
        }
        if !ours {
            return None;
        }
        self.ended = self.daemon.wait().ok();
        self.ended
    }
}

/// What [`DaemonGuard`] can see of its daemon.
#[derive(PartialEq)]
enum Seen {
    Running,
    /// Exited, and still this process's to collect.
    Exited,
    /// No child of this process any more: something else collected it.
    NotOurs,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a [`Watchdog`] runs. Its input is the pipe the test holds the other
/// end of, and `$1` is the fixture's tempdir. It reads what the fixture tells
/// it until the pipe closes, which is the test process ending, and then
/// stops what it was told about: the daemons first, then the tmux servers
/// they type into, then the directory all of it lives in.
///
/// - `group N`: kill process group N.
/// - `reaped N`: do not, after all, for one `group N` told before. N's daemon
///   is about to be collected, and the number may then pass to another
///   process. A `group N` that comes later is that other process, and counts
///   again. So group N is killed while some `group N` is left that no
///   `reaped N` after it has taken back; a `reaped N` that finds none is
///   ignored. Which of the same number a `reaped` takes cannot matter. Not all
///   of them, because a guard whose daemon was collected behind its back
///   tells `reaped` late: by then a newer daemon may hold N too, and its own
///   `group N` must stay.
/// - `tmux PATH`: stop the tmux server on the socket at PATH. `-S` names the
///   socket outright, so with it gone tmux fails rather than fall back to the
///   developer's own server. Asked again for two seconds while no server
///   answers: one that was still starting when the test died is already out
///   of reach, and has not made its socket yet (seen 2026-11-20, in three of
///   twelve runs killed within 80ms of starting). A server slower than that
///   is left running. A server the test had already stopped costs the two
///   seconds too.
///
/// The directory is removed again for as long as it is still there, since a
/// process on its way out can still be writing into it.
///
/// Plain `sh`, because that is what a CI container has: `kill -s` and `--`
/// before a negative number are the one spelling both bash and dash take.
const WATCHDOG_SCRIPT: &str = r#"
dir=$1
shift
groups=
while IFS=' ' read -r kind what; do
  case $kind in
    group) groups="$groups $what" ;;
    reaped)
      kept=
      found=
      for group in $groups; do
        if [ -z "$found" ] && [ "$group" = "$what" ]; then
          found=1
        else
          kept="$kept $group"
        fi
      done
      groups=$kept
      ;;
    tmux) set -- "$@" "$what" ;;
  esac
done
for group in $groups; do
  [ "$group" -gt 1 ] || continue
  kill -s KILL -- "-$group"
done
for socket in "$@"; do
  for try in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    tmux -S "$socket" kill-server && break
    sleep 0.1
  done
done
for try in 1 2 3 4 5 6 7 8 9 10; do
  rm -rf -- "${dir:?}"
  [ -e "$dir" ] || break
  sleep 0.2
done
"#;

/// Cleans up after a test process that ended without dropping its fixture.
///
/// A daemon these tests start is out of the terminal's reach, in a session
/// or a process group of its own: that is what lets [`DaemonGuard`] kill
/// everything the daemon started. So a Ctrl-C, which goes to the test's
/// group, kills the test and not the daemon, and no `Drop` runs in a killed
/// process. Each interrupted run used to leave a `nudge --daemon`, a private
/// tmux server with its panes, and a tempdir, until the next reboot.
///
/// The watchdog is a `sh` in a session of its own, so a Ctrl-C does not
/// reach it either. It waits on a pipe that only the test process holds the
/// writing end of. However the test ends, the kernel closes that end, and
/// the watchdog then stops what the fixture told it about (see
/// [`WATCHDOG_SCRIPT`]). A test that ends in the ordinary way kills its
/// watchdog as it is dropped, before the pipe closes, and the fixture's own
/// cleanup is all that runs.
///
/// Start it first, straight after the tempdir, and tell it about each thing
/// as it is started. What the test starts afterwards does not get the pipe:
/// Rust opens it close-on-exec. (macOS sets that in a second step, so there
/// a command started from another thread in between would be handed the
/// pipe, and this watchdog would wait for that command to end as well.)
///
/// A pid is only safely a daemon's until the daemon is collected (see
/// [`DaemonGuard`]). A guard made by [`Watchdog::guard`] therefore tells the
/// watchdog to forget its daemon before it collects it.
///
/// Two cases are left, and accepted. A daemon that has exited, on its own or
/// by its guard's kill just before the guard could tell `reaped`, and that
/// the test had not yet collected, is collected by init when the test
/// process dies, a moment before the watchdog kills the group of its number.
/// If that group was empty, a process started in that moment could have been
/// handed the number. That takes the system to run through every pid it has
/// within the milliseconds of one interrupted test.
///
/// And a daemon collected by something other than its guard leaves its
/// number with the watchdog until the guard is stopped. The number can be
/// another process's by then, and if the test dies first, the watchdog kills
/// whatever group has it. Nothing in these suites does that: the guard
/// alone holds the daemon's `Child`, and of the daemons a watchdog knows,
/// only one in a test of the watchdog itself is collected behind its guard's
/// back, on purpose.
pub struct Watchdog {
    sh: std::process::Child,
    /// The test's end of the pipe, and the only thing that keeps it open:
    /// a guard holds a `Weak` of it. Only ever `None` once
    /// [`Watchdog::fire`] has closed it.
    pipe: Option<std::sync::Arc<std::io::PipeWriter>>,
}

impl Watchdog {
    /// Start a watchdog that will remove `dir`, a tempdir from
    /// [`short_tempdir`], with everything in it.
    pub fn over(dir: &Path) -> Self {
        use std::os::unix::process::CommandExt as _;

        assert!(
            is_a_fixtures_tempdir(dir),
            "{} is not a fixture's tempdir, and the watchdog would remove it",
            dir.display()
        );
        let (ours, pipe) = std::io::pipe().expect("open a pipe");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(WATCHDOG_SCRIPT)
            .arg("nudge-test-watchdog")
            .arg(dir)
            .stdin(ours)
            // Not the test's own: whatever reads the test's output would
            // wait for the watchdog too.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        // SAFETY: the closure runs in the child between fork and exec, where
        // only async-signal-safe calls are allowed. `setsid` is one, and
        // reading errno allocates nothing.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let sh = cmd.spawn().expect("start the watchdog");
        Watchdog {
            sh,
            pipe: Some(std::sync::Arc::new(pipe)),
        }
    }

    /// Write `line` to the watchdog.
    fn tell(&self, line: &str) {
        if let Some(pipe) = &self.pipe {
            tell(pipe, line);
        }
    }

    /// The watchdog's process id.
    pub fn pid(&self) -> libc::pid_t {
        libc::pid_t::try_from(self.sh.id()).expect("a process id is a pid_t")
    }

    /// [`TmuxServer::start`], for a server this watchdog stops before the
    /// directory goes. It is told where the server will listen before the
    /// server is started: tmux puts its server out of the terminal's reach
    /// at once, and a run interrupted while it starts would otherwise leave
    /// a server that nothing knows of.
    pub fn start_tmux(&self, tmpdir: &Path, session: &str) -> TmuxServer {
        let socket = tmux_socket(tmpdir);
        let socket = socket.to_str().expect("a socket path that is text");
        assert!(!socket.contains('\n'), "a socket path on one line");
        self.tell(&format!("tmux {socket}"));
        TmuxServer::start(tmpdir, session)
    }

    /// Kill process group `pid` too. For a daemon, use [`Watchdog::guard`],
    /// which also has the group forgotten in time.
    pub fn watch_group(&self, pid: libc::pid_t) {
        // 0 and -1 name the caller's own group and every process there is.
        assert!(pid > 1, "{pid} is no process group to kill");
        self.tell(&format!("group {pid}"));
    }

    /// Leave process group `pid` alone after all: its number is about to
    /// stop being the daemon's.
    pub fn forget_group(&self, pid: libc::pid_t) {
        self.tell(&format!("reaped {pid}"));
    }

    /// A [`DaemonGuard`] for `daemon` whose group this watchdog kills if the
    /// test process ends before the guard has stopped it.
    pub fn guard(&self, daemon: std::process::Child) -> DaemonGuard {
        let mut guard = DaemonGuard::new(daemon);
        self.watch_group(guard.pid());
        if let Some(pipe) = &self.pipe {
            guard.watchdog = std::sync::Arc::downgrade(pipe);
        }
        guard
    }

    /// Do what the test process ending does: close its end of the pipe, and
    /// wait for the watchdog to finish. For the watchdog's own tests.
    ///
    /// Panics if the watchdog does not finish, which is what it does while
    /// anything else holds the pipe open: a command that was handed it.
    pub fn fire(mut self) {
        drop(self.pipe.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if self
                .sh
                .try_wait()
                .expect("ask after the watchdog")
                .is_some()
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the watchdog never finished: something else holds its pipe open, \
                 or what it runs is stuck"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// Write `line` to a watchdog's pipe, whole. A watchdog that has stopped
/// reading is not an error.
fn tell(mut pipe: &std::io::PipeWriter, line: &str) {
    use std::io::Write as _;

    let _ = pipe.write_all(format!("{line}\n").as_bytes());
}

/// Whether `dir` is one [`short_tempdir`] could have made: `/tmp/nudge-`
/// and something more, and nothing below it.
pub fn is_a_fixtures_tempdir(dir: &Path) -> bool {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    dir.parent() == Some(Path::new("/tmp"))
        && name.len() > "nudge-".len()
        && name.starts_with("nudge-")
}

/// Where a tmux server started under `tmpdir` as its `TMUX_TMPDIR` listens:
/// tmux's default socket there.
fn tmux_socket(tmpdir: &Path) -> PathBuf {
    // SAFETY: `getuid` takes nothing and cannot fail.
    let uid = unsafe { libc::getuid() };
    tmpdir.join(format!("tmux-{uid}")).join("default")
}

impl Drop for Watchdog {
    /// Kill the watchdog before the pipe closes, which is what would set it
    /// off. The pipe is a field, and fields go after this returns.
    ///
    /// Through `Child`, which knows whether the watchdog has been collected
    /// and then sends nothing: after [`Watchdog::fire`] it has been, and its
    /// pid may be another process's. A `libc::kill` here would not know.
    fn drop(&mut self) {
        let _ = self.sh.kill();
        let _ = self.sh.wait();
    }
}

/// How long a fixture waits, as it is dropped, for what its commands started.
/// A failure deadline, not a delay: a daemon that loses the singleton lock
/// exits in milliseconds, long before the test that started it ends.
const DESCENDANT_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Knows when every process a fixture's commands started has exited, down to
/// those that outlived the command that started them.
///
/// The CLI suites run a `nudge` that starts a daemon and does not wait for
/// it. The daemon is meant to lose the singleton lock the fixture holds and
/// exit, but taking the lock creates its directory first. One that got there
/// after the fixture's tempdir had been removed would put the directory back,
/// win a lock nobody holds any more, and run on. Dropping this waits for all
/// of them, so declare it above the fixture's `TempDir` and above the lock
/// it holds: fields drop in the order they are declared, and both must
/// still be there while a late daemon finds out that it lost.
///
/// Each command followed is handed the writing end of a pipe, which it passes
/// on to whatever it starts. The pipe reaches its end once the last of them
/// has exited. Nothing else gets a copy: the descriptor is close-on-exec here,
/// and only a followed command clears that, in its own process, before it
/// runs.
///
/// One exception, on a system that cannot make a pipe close-on-exec in one
/// step (macOS): a process some other test starts in the moment between the
/// two steps keeps a copy. The wait then lasts as long as that process does,
/// or until the patience above runs out, and the warning it prints is false.
pub struct Descendants {
    writer: Option<std::io::PipeWriter>,
    reader: Option<std::io::PipeReader>,
    /// Hears from the thread reading the pipe once it has reached the end.
    ended: Option<std::sync::mpsc::Receiver<()>>,
    gone: bool,
}

impl Descendants {
    pub fn new() -> Self {
        let (reader, writer) = std::io::pipe().expect("create a pipe");
        Descendants {
            writer: Some(writer),
            reader: Some(reader),
            ended: None,
            gone: false,
        }
    }

    /// Count `cmd`, once it is started, and everything it starts in turn.
    /// Start it before this is dropped or waited on: afterwards the
    /// descriptor it would pass on is closed, and its number may be some
    /// other file's.
    pub fn follow(&self, cmd: &mut std::process::Command) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = self
            .writer
            .as_ref()
            .expect("follow a command before waiting for the rest")
            .as_raw_fd();
        // SAFETY: the closure runs in the child between fork and exec, where
        // only async-signal-safe calls are allowed. `fcntl` is one, and
        // reading errno allocates nothing. It clears close-on-exec in the
        // child alone: descriptor flags are not shared across a fork.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Wait up to `patience` for every one of them to exit. `false` if some
    /// were still running by then. No command can be followed after this.
    pub fn wait_until_gone(&mut self, patience: std::time::Duration) -> bool {
        if self.gone {
            return true;
        }
        // This end first, or the pipe would never reach its own.
        drop(self.writer.take());
        if let Some(mut reader) = self.reader.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut nothing = Vec::new();
                let _ = std::io::Read::read_to_end(&mut reader, &mut nothing);
                let _ = tx.send(());
            });
            self.ended = Some(rx);
        }
        self.gone = self
            .ended
            .as_ref()
            .is_some_and(|ended| ended.recv_timeout(patience).is_ok());
        self.gone
    }
}

impl Drop for Descendants {
    fn drop(&mut self) {
        // Said, not panicked: a panic here would abort a test that is
        // already unwinding.
        if !self.wait_until_gone(DESCENDANT_PATIENCE) {
            eprintln!(
                "warning: something a test's command started was still running \
                 after {DESCENDANT_PATIENCE:?}; it may put the test's tempdir back"
            );
        }
    }
}

/// Leave `cmd` no way to the developer's own tmux or herdr, for a `nudge`
/// run under a fixture whose home is `home`.
///
/// nudge labels the panes in its job list and its confirmations from every
/// tmux and herdr pane it can list, and the session running the tests may
/// itself sit in a tmux or herdr pane. So every `HERDR_*`, `TMUX` and
/// `TMUX_PANE` goes, and `TMUX_TMPDIR` and `XDG_CONFIG_HOME` point inside
/// the fixture, where no tmux server runs unless the test starts one there
/// (`home/tmux`) and herdr keeps no session.
///
/// `TMUX_TMPDIR` is created here, and that is the point of doing this in one
/// place: tmux skips a `TMUX_TMPDIR` that does not exist and falls back to
/// its default socket directory, where the developer's server lives. Two
/// suites set the variable and never made the directory, and two set
/// nothing, so their `list-panes` went to that server (seen 2026-11-20).
pub fn keep_off_the_developers_panes(cmd: &mut std::process::Command, home: &Path) {
    let tmux_tmpdir = home.join("tmux");
    std::fs::create_dir_all(&tmux_tmpdir).expect("create the fixture's TMUX_TMPDIR");
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("HERDR_")) {
            cmd.env_remove(key);
        }
    }
    cmd.env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("TMUX_TMPDIR", tmux_tmpdir)
        .env("XDG_CONFIG_HOME", home.join("config"));
}

/// [`keep_off_the_developers_panes`] for this process, and so for a daemon it
/// starts with production `spawn_daemon`, which inherits its environment.
///
/// # Safety
///
/// Nothing else may read or write the environment while this runs: call it
/// from the only test in its binary, before that test starts a thread.
pub unsafe fn keep_this_process_off_the_developers_panes(home: &Path) {
    let tmux_tmpdir = home.join("tmux");
    std::fs::create_dir_all(&tmux_tmpdir).expect("create the fixture's TMUX_TMPDIR");
    // SAFETY: the caller's, as above.
    unsafe {
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|k| k.starts_with("HERDR_")) {
                std::env::remove_var(key);
            }
        }
        std::env::remove_var("TMUX");
        std::env::remove_var("TMUX_PANE");
        std::env::set_var("TMUX_TMPDIR", tmux_tmpdir);
        std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    }
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

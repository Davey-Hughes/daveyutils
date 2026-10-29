//! The process groups subcheck started that are still running (LLM commands, review's mpv,
//! apply's mkvmerge, audio's ffmpeg), and the files to remove, so that a signal that ends
//! subcheck ends those groups too and leaves no half-written file behind.
//!
//! The signals are taken on a thread of their own, so ending the groups is ordinary code that
//! waits for the groups being started to be registered, and stops new ones: a signal ends subcheck
//! before a group starts or after it is registered, never between. Files have a lock of their own,
//! so that a file stuck on a stalled mount does not keep a signal from the groups.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, Once, PoisonError, RwLock, RwLockWriteGuard, mpsc};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use signal_hook::iterator::Signals;
use signal_hook::low_level::emulate_default_handler;

/// The signals that end subcheck.
const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
/// How long a signal's cleanup has before a second SIGINT or SIGTERM ends subcheck without it.
const PATIENCE: Duration = Duration::from_secs(1);

/// What a signal ends, each entry under the id its guard forgets it by.
struct List<T> {
    next: u64,
    entries: Vec<(u64, T)>,
}

impl<T> List<T> {
    const fn new() -> List<T> {
        List {
            next: 0,
            entries: Vec::new(),
        }
    }

    fn add(&mut self, entry: T) -> u64 {
        self.next += 1;
        self.entries.push((self.next, entry));
        self.next
    }

    fn forget(&mut self, id: u64) {
        self.entries.retain(|(i, _)| *i != id);
    }
}

static GROUPS: Mutex<List<Pid>> = Mutex::new(List::new());
static FILES: Mutex<List<PathBuf>> = Mutex::new(List::new());
/// Read by each [`spawn`] from before its child starts until the child is registered, side by
/// side; written by the signal thread, which so waits for the spawns under way and stops new ones.
static STARTING: RwLock<()> = RwLock::new(());
static INSTALL: Once = Once::new();
/// Set by the signal thread before it kills anything: from then on, a thread that comes back
/// through this module (its child killed under it, say) waits for the end instead of acting on
/// it.
static ENDING: AtomicBool = AtomicBool::new(false);
/// Set by [`end`]: the ending is a quit, not a signal, so [`finish`] lets subcheck end its own
/// way.
static QUIT: AtomicBool = AtomicBool::new(false);

/// Wait for the end, should a signal be ending subcheck.
fn wait_if_ending() {
    if ENDING.load(Ordering::SeqCst) {
        loop {
            std::thread::park();
        }
    }
}

/// A list, locked. A panic while it was held left it whole: each change is one push or retain.
fn lock<T>(list: &'static Mutex<List<T>>) -> MutexGuard<'static, List<T>> {
    list.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A registered group; dropping it forgets the group (and kills nothing).
pub struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        wait_if_ending();
        lock(&GROUPS).forget(self.0);
    }
}

/// Start `cmd` in a process group of its own, registered so that a signal that ends subcheck ends
/// it too.
pub fn spawn(cmd: &mut Command) -> io::Result<(Child, Registered)> {
    INSTALL.call_once(install);
    wait_if_ending();
    let _starting = STARTING.read().unwrap_or_else(PoisonError::into_inner);
    let child = cmd.process_group(0).spawn()?;
    let id = lock(&GROUPS).add(Pid::from_child(&child));
    Ok((child, Registered(id)))
}

/// A file a signal removes; dropping it forgets the file (and removes nothing).
pub struct Removing(u64);

impl Drop for Removing {
    fn drop(&mut self) {
        wait_if_ending();
        lock(&FILES).forget(self.0);
    }
}

impl Removing {
    /// Run `f`, then forget the file, with no signal between: one that ends subcheck does so
    /// before `f` (removing the file) or after the file is forgotten. `f` runs with the files
    /// locked, which the signal thread takes last: it must not call into this module or drop a
    /// guard of it.
    pub fn forget_after<T>(self, f: impl FnOnce() -> T) -> T {
        wait_if_ending();
        let mut files = lock(&FILES);
        let out = f();
        files.forget(self.0);
        drop(files);
        std::mem::forget(self);
        out
    }
}

/// Make `path` with `create`, removed if a signal ends subcheck before the returned guard is
/// dropped, with no signal between the two. A `create` that fails registers nothing: a link that
/// finds the name taken leaves what has it alone. `create` runs with the files locked, which the
/// signal thread takes last: it must not call into this module or drop a guard of it.
pub fn create_removing(
    path: &Path,
    create: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<Removing> {
    INSTALL.call_once(install);
    wait_if_ending();
    let mut files = lock(&FILES);
    create(path)?;
    Ok(Removing(files.add(path.to_path_buf())))
}

/// To call as subcheck's main returns. While a signal is ending subcheck, a run returning on the
/// way would end it with a status of its own, and maybe before the cleanup is done: it waits for
/// the signal to end it instead. After [`end`], a quit, it returns at once.
pub fn finish() {
    if !QUIT.load(Ordering::SeqCst) {
        wait_if_ending();
    }
}

/// Why [`output`] has no output: the command did not start, or could not be waited for.
#[derive(Debug)]
pub enum Failed {
    Start(io::Error),
    Wait(io::Error),
}

/// Run `cmd` to its end with an empty stdin and its stdout and stderr captured, in a process
/// group of its own, registered so that a signal that ends subcheck ends it too.
pub fn output(cmd: &mut Command) -> Result<Output, Failed> {
    let (child, _running) = spawn(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .map_err(Failed::Start)?;
    child.wait_with_output().map_err(Failed::Wait)
}

/// Kill the process group `child` leads, as [`spawn`] started it.
pub fn kill(child: &Child) {
    let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
}

/// Take [`SIGNALS`] on a thread of their own. One ignored when subcheck started stays ignored:
/// under `nohup` a hang-up must not end it.
fn install() {
    let taken: Vec<libc::c_int> = SIGNALS.into_iter().filter(|&sig| !ignored(sig)).collect();
    if taken.is_empty() {
        return;
    }
    // Taken on the thread, which says when: a signal taken with no thread to read it would be
    // lost, so a thread that does not start leaves every signal its default action.
    let (taking, taken_now) = mpsc::channel();
    let started = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let signals = Signals::new(taken);
            let _ = taking.send(());
            if let Ok(mut signals) = signals
                && let Some(sig) = signals.forever().next()
            {
                let first = Instant::now();
                // The cleanup on a thread of its own, or on this one should none start.
                let ending = std::thread::Builder::new().name("ending".into());
                if ending.spawn(move || end_by_signal(sig)).is_err() {
                    end_by_signal(sig);
                }
                // Should the cleanup wait on a stalled mount, a second SIGINT or SIGTERM (Ctrl-C
                // again) ends subcheck as if there were no handler, once the first has had
                // [`PATIENCE`], leaving what it had yet to do. Not a second SIGHUP: a terminal that
                // closes sends two, the kernel's and the shell's, and nobody is there to insist.
                if let Some(again) = signals.forever().find(|&s| s != libc::SIGHUP) {
                    std::thread::sleep(PATIENCE.saturating_sub(first.elapsed()));
                    let _ = emulate_default_handler(again);
                }
            }
        });
    if started.is_ok() {
        let _ = taken_now.recv();
    }
}

/// Whether `sig` is ignored, as `nohup` leaves SIGHUP.
fn ignored(sig: libc::c_int) -> bool {
    // SAFETY: a null new action only reads the current one into `old`.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(sig, std::ptr::null(), &raw mut old) == 0
            && old.sa_sigaction == libc::SIG_IGN
    }
}

/// Kill every registered group, then remove every registered file, then die of `sig` as if there
/// were no handler.
fn end_by_signal(sig: libc::c_int) {
    let _held = end_all();
    let _ = emulate_default_handler(sig);
}

/// The registry as [`end_all`] leaves it: held, so no group starts and no file is made.
type Held = (
    RwLockWriteGuard<'static, ()>,
    MutexGuard<'static, List<Pid>>,
    MutexGuard<'static, List<PathBuf>>,
);

/// Kill every registered group, then remove every registered file. The groups running now are
/// killed first, so a start stuck in exec does not keep them alive; then, once no start is under
/// way and none can begin, any registered since. The files come last, so one stuck on a stalled
/// mount does not keep the groups alive either. Returns the registry, held.
fn end_all() -> Held {
    ENDING.store(true, Ordering::SeqCst);
    let kill_all = |groups: &List<Pid>| {
        for &(_, pgid) in &groups.entries {
            let _ = kill_process_group(pgid, Signal::KILL);
        }
    };
    kill_all(&lock(&GROUPS));
    let starting = STARTING.write().unwrap_or_else(PoisonError::into_inner);
    let groups = lock(&GROUPS);
    kill_all(&groups);
    let files = lock(&FILES);
    for (_, path) in &files.entries {
        let _ = std::fs::remove_file(path);
    }
    (starting, groups, files)
}

/// A signal's cleanup, for a quit that leaves work under way (review's screen quit mid-scan):
/// every registered group killed and every registered file removed, and the registry held for
/// good, so no group starts after. A thread that comes back through this module waits for the
/// end, as under a signal: an LLM call killed under a worker is not recorded as a failure.
/// Unlike a signal, it does not end subcheck: the caller says what it did and returns from main,
/// where [`finish`] lets it.
///
/// Call it last: a guard of this module dropped after it (review's player) would wait forever.
pub fn end() {
    QUIT.store(true, Ordering::SeqCst);
    std::mem::forget(end_all());
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::process::{Resource, Rlimit, kill_process, setrlimit};
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;

    const CHILD: &str = "group::tests::child_that_registers_a_file_and_waits";
    const STARTER: &str = "group::tests::child_that_starts_a_group_slowly";

    /// Start `true` from another thread, its start stuck in exec for `secs` (its program on a
    /// stalled mount, say).
    fn start_stuck(secs: libc::time_t) {
        std::thread::spawn(move || {
            let mut stuck = Command::new("true");
            stuck.stdout(Stdio::null()).stderr(Stdio::null());
            // SAFETY: nanosleep is async-signal-safe.
            unsafe {
                stuck.pre_exec(move || {
                    let held = libc::timespec {
                        tv_sec: secs,
                        tv_nsec: 0,
                    };
                    libc::nanosleep(&raw const held, std::ptr::null_mut());
                    Ok(())
                });
            }
            let _started = spawn(&mut stuck);
        });
    }

    /// Remove the existing `path` on a signal, as [`create_removing`] does a file it makes.
    fn remove_on_signal(path: &Path) -> Removing {
        create_removing(path, |_| Ok(())).unwrap()
    }

    /// Time enough for a child to die of a signal, on a loaded machine too.
    const DIES: Duration = Duration::from_secs(5);

    /// How long the child holds the registry in `forget` and `create`; and in `hold`, much longer.
    const HELD: Duration = Duration::from_millis(300);
    const HELD_LONG: Duration = Duration::from_secs(10);
    const HELD_PAST: Duration = Duration::from_millis(1500);

    /// Run by `signal`, as the child it signals: registers $SUBCHECK_TEST_REMOVE the way
    /// $SUBCHECK_TEST_HOW says, writes `<path>.ready`, and waits.
    #[test]
    #[ignore = "the child of the signal tests"]
    #[allow(clippy::too_many_lines)] // one arm per way the child registers
    fn child_that_registers_a_file_and_waits() {
        let Some(path) = std::env::var_os("SUBCHECK_TEST_REMOVE") else {
            return;
        };
        let path = PathBuf::from(path);
        let ready = || {
            let mut ready = path.clone().into_os_string();
            ready.push(".ready");
            std::fs::write(ready, "").unwrap();
        };
        let how = std::env::var("SUBCHECK_TEST_HOW").unwrap_or_default();
        let _removing = match how.as_str() {
            "nohup" => {
                // SAFETY: setting a disposition to SIG_IGN.
                unsafe {
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                }
                remove_on_signal(&path)
            }
            // No thread can start, so none takes the signals.
            "no-thread" => {
                let none = Rlimit {
                    current: Some(0),
                    maximum: Some(0),
                };
                setrlimit(Resource::Nproc, none).unwrap();
                remove_on_signal(&path)
            }
            // Held far longer: only a second signal ends it before the hold does.
            // A group running, which holds the test's pipe (stderr) until it ends, and another one
            // whose start is stuck for 10 s (its program on a stalled mount, say).
            "stuck-start" => {
                let _running = spawn(Command::new("sleep").arg("20")).unwrap();
                // The group alone holds the pipe now: this process lives on, waiting for the
                // stuck start.
                let null = std::fs::File::create("/dev/null").unwrap();
                rustix::stdio::dup2_stderr(&null).unwrap();
                start_stuck(10);
                std::thread::sleep(Duration::from_millis(100));
                let _removing = remove_on_signal(&path);
                ready();
                std::thread::sleep(Duration::from_secs(10));
                return;
            }
            // A group running, which the main thread waits for and then returns, as subcheck's main
            // does when its mkvmerge dies; meanwhile another start is stuck in exec for 2 s, which
            // the signal waits for.
            "returns" => {
                let _removing = remove_on_signal(&path);
                let (mut running, _registered) = spawn(Command::new("sleep").arg("20")).unwrap();
                start_stuck(2);
                std::thread::sleep(Duration::from_millis(100));
                ready();
                let _ = running.wait();
                finish();
                return;
            }
            // A worker running a group to its end, then acting on how it ended (as `scan` records a
            // failed LLM command); meanwhile another start is stuck in exec for 2 s.
            "acts" => {
                let _removing = remove_on_signal(&path);
                let acted = path.with_extension("acted");
                std::thread::spawn(move || {
                    let _ = output(Command::new("sleep").arg("20"));
                    std::fs::write(acted, "").unwrap();
                });
                start_stuck(2);
                std::thread::sleep(Duration::from_millis(100));
                ready();
                std::thread::sleep(Duration::from_secs(10));
                return;
            }
            // A quit with work under way (review's screen quit mid-scan): a group running, which
            // holds the test's pipe (stderr) until it ends, and a worker running a group to its end
            // and then acting on how it ended. `end` kills both and removes the file; main returns
            // its own way.
            "quits" => {
                let removing = remove_on_signal(&path);
                let running = spawn(Command::new("sleep").arg("20")).unwrap();
                let acted = path.with_extension("acted");
                std::thread::spawn(move || {
                    let _ = output(Command::new("sleep").arg("20"));
                    std::fs::write(acted, "").unwrap();
                });
                std::thread::sleep(Duration::from_millis(100));
                ready();
                // Nothing of this module is dropped after `end`: review's screen drops its player
                // before it.
                std::mem::forget((removing, running));
                end();
                finish();
                return;
            }
            // And a group running, which holds the test's pipe (stderr) until it ends.
            "hold" => {
                let _running = spawn(Command::new("sleep").arg("20")).unwrap();
                remove_on_signal(&path).forget_after(|| {
                    ready();
                    std::thread::sleep(HELD_LONG);
                });
                std::thread::sleep(Duration::from_secs(10));
                return;
            }
            // The signal comes while the registry is held, the file still registered.
            "forget" => {
                let removing = remove_on_signal(&path);
                removing.forget_after(|| {
                    ready();
                    std::thread::sleep(HELD);
                });
                std::thread::sleep(Duration::from_secs(10));
                return;
            }
            // The signal comes while the registry is held, the file made but not yet registered;
            // held longer than PATIENCE in `create-long`.
            "create" | "create-long" => create_removing(&path, |path| {
                std::fs::write(path, "half a remux")?;
                ready();
                std::thread::sleep(if how == "create" { HELD } else { HELD_PAST });
                Ok(())
            })
            .unwrap(),
            _ => remove_on_signal(&path),
        };
        ready();
        std::thread::sleep(Duration::from_secs(10));
    }

    /// Start the child on `path` the `how` way, send it `sig` once it is ready, and return whether
    /// it died of it within `limit`.
    fn signal(path: &Path, how: &str, sig: Signal, limit: Duration) -> bool {
        let child = start(path, how);
        kill_process(Pid::from_child(&child), sig).unwrap();
        ended_within(child, limit)
    }

    /// Whether `child` ends within `limit`; killed if it does not.
    fn ended_within(mut child: Child, limit: Duration) -> bool {
        let died = died_within(&mut child, limit);
        if !died {
            child.kill().unwrap();
        }
        child.wait().unwrap();
        died
    }

    /// Whether `child` ends within `limit`.
    fn died_within(child: &mut Child, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// What `child`'s stderr says, read by read, then `None` once every process holding it has
    /// ended: the child and any group it started.
    fn pipe_ends(child: &mut Child) -> mpsc::Receiver<Option<Vec<u8>>> {
        let mut stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0; 256];
            while let Ok(n @ 1..) = stderr.read(&mut buf) {
                let _ = tx.send(Some(buf[..n].to_vec()));
            }
            let _ = tx.send(None);
        });
        rx
    }

    /// Whether `said` reaches its end within `limit`.
    fn ends_within(said: &mpsc::Receiver<Option<Vec<u8>>>, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            match said.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Some(_)) => {}
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }

    /// The child on `path`, started the `how` way and ready; its stderr is a pipe.
    fn start(path: &Path, how: &str) -> Child {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("SUBCHECK_TEST_REMOVE", path)
            .env("SUBCHECK_TEST_HOW", how)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let ready = path.with_extension("mkv.ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the child never got ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    #[test]
    fn a_second_signal_soon_after_the_first_still_lets_it_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        let child = start(&path, "create");
        let pid = Pid::from_child(&child);
        kill_process(pid, Signal::TERM).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        kill_process(pid, Signal::TERM).unwrap();
        assert!(ended_within(child, DIES));
        assert!(!path.exists(), "the first signal's cleanup ran");
    }

    #[test]
    fn a_hang_up_twice_waits_for_the_cleanup_however_long() {
        // Closing a terminal sends SIGHUP twice: the kernel's, and the shell's to its jobs. Neither
        // is anyone insisting, so the second does not cut the wait short.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        let child = start(&path, "create-long");
        let pid = Pid::from_child(&child);
        kill_process(pid, Signal::HUP).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        kill_process(pid, Signal::HUP).unwrap();
        assert!(ended_within(child, DIES));
        assert!(!path.exists(), "the first signal's cleanup ran");
    }

    #[test]
    fn a_second_signal_ends_subcheck_without_the_cleanup_while_the_first_waits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kept.mkv");
        std::fs::write(&path, "the original").unwrap();
        let mut child = start(&path, "hold");
        let group_ended = pipe_ends(&mut child);
        let pid = Pid::from_child(&child);
        kill_process(pid, Signal::TERM).unwrap();
        let waited = !died_within(&mut child, Duration::from_millis(200));
        // Again until one is read as a second: two sent before the first is read are one.
        let deadline = Instant::now() + DIES;
        while !died_within(&mut child, Duration::from_millis(100)) && Instant::now() < deadline {
            let _ = kill_process(pid, Signal::TERM);
        }
        // Well before the hold of 10 s ends, which would have let the first clean up.
        let ended = ended_within(child, Duration::ZERO);
        assert!(waited, "the first signal waits for the files");
        assert!(ended, "the second ends it");
        assert!(path.exists(), "with no cleanup of the files");
        assert!(
            ends_within(&group_ended, DIES),
            "the groups were killed all the same"
        );
    }

    #[test]
    fn a_signal_removes_a_registered_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        assert!(signal(&path, "", Signal::TERM, DIES), "SIGTERM ends it");
        assert!(!path.exists());
    }

    #[test]
    fn a_start_stuck_in_exec_does_not_keep_the_running_groups_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        let mut child = start(&path, "stuck-start");
        let group_ended = pipe_ends(&mut child);
        kill_process(Pid::from_child(&child), Signal::TERM).unwrap();
        let killed = ends_within(&group_ended, DIES);
        // It still waits for the stuck start, to kill that one too.
        ended_within(child, Duration::ZERO);
        assert!(killed, "the running group was killed at once");
    }

    #[test]
    fn a_main_that_returns_on_the_way_still_dies_of_the_signal_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        let mut child = start(&path, "returns");
        kill_process(Pid::from_child(&child), Signal::TERM).unwrap();
        if !died_within(&mut child, DIES) {
            child.kill().unwrap();
        }
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status}");
        assert!(!path.exists(), "the cleanup ran to its end");
    }

    #[test]
    fn a_thread_whose_group_the_signal_killed_does_not_act_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        let mut child = start(&path, "acts");
        kill_process(Pid::from_child(&child), Signal::TERM).unwrap();
        if !died_within(&mut child, DIES) {
            child.kill().unwrap();
        }
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status}");
        assert!(!path.with_extension("acted").exists(), "the worker went on");
        assert!(!path.exists());
    }

    #[test]
    fn a_quit_ends_the_groups_and_removes_the_files_then_returns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        let mut child = start(&path, "quits");
        let group_ended = pipe_ends(&mut child);
        let returned = died_within(&mut child, DIES);
        if !returned {
            child.kill().unwrap();
        }
        let status = child.wait().unwrap();
        assert!(returned, "the quit returns from main");
        assert!(status.success(), "{status}");
        assert!(ends_within(&group_ended, DIES), "the groups were killed");
        assert!(!path.exists(), "the file was removed");
        assert!(!path.with_extension("acted").exists(), "the worker went on");
    }

    #[test]
    fn a_signal_waits_for_a_file_being_made_and_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        assert!(
            signal(&path, "create", Signal::TERM, DIES),
            "SIGTERM ends it"
        );
        assert!(!path.exists());
    }

    #[test]
    fn a_signal_waits_for_a_file_being_forgotten_and_leaves_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kept.mkv");
        std::fs::write(&path, "the original").unwrap();
        assert!(
            signal(&path, "forget", Signal::TERM, DIES),
            "SIGTERM ends it"
        );
        assert!(path.exists());
    }

    #[test]
    fn without_the_signal_thread_a_signal_ends_subcheck_as_before() {
        if rustix::process::getuid().is_root() {
            eprintln!("skipped: root is not held to RLIMIT_NPROC");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        assert!(
            signal(&path, "no-thread", Signal::TERM, DIES),
            "SIGTERM ends it"
        );
    }

    #[test]
    fn a_hang_up_ignored_from_the_start_stays_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        assert!(
            !signal(&path, "nohup", Signal::HUP, Duration::from_secs(1)),
            "nohup: SIGHUP must not end it"
        );
        assert!(path.exists());
    }

    /// Run by `a_signal_while_a_group_starts_ends_that_group_too`, as the child it signals: starts
    /// `sleep 20` from another thread, the start held open for 300 ms after it writes `ready` to
    /// stderr, which the sleep goes on holding.
    #[test]
    #[ignore = "the child of the signal tests"]
    fn child_that_starts_a_group_slowly() {
        if std::env::var_os("SUBCHECK_TEST_START").is_none() {
            return;
        }
        let mut cmd = Command::new("sleep");
        cmd.arg("20");
        // SAFETY: write and nanosleep are async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::write(2, b"ready".as_ptr().cast(), 5);
                let held = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 300_000_000,
                };
                libc::nanosleep(&raw const held, std::ptr::null_mut());
                Ok(())
            });
        }
        std::thread::spawn(move || {
            let _started = spawn(&mut cmd);
            std::thread::sleep(Duration::from_secs(10));
        });
        std::thread::sleep(Duration::from_secs(10));
    }

    #[test]
    fn a_signal_while_a_group_starts_ends_that_group_too() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", STARTER, "--ignored", "--nocapture"])
            .env("SUBCHECK_TEST_START", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let rx = pipe_ends(&mut child);
        let mut said = Vec::new();
        while !said.windows(5).any(|w| w == b"ready") {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(bytes)) => said.extend(bytes),
                end => panic!("never ready ({end:?}): {}", String::from_utf8_lossy(&said)),
            }
        }
        kill_process(Pid::from_child(&child), Signal::TERM).unwrap();
        let ended = ends_within(&rx, DIES);
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
        }
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert!(ended, "the group it was starting outlived it");
    }

    #[test]
    fn a_dropped_registration_is_forgotten() {
        let (mut child, running) = spawn(&mut Command::new("true")).unwrap();
        let pgid = Pid::from_child(&child);
        assert!(lock(&GROUPS).entries.iter().any(|&(_, g)| g == pgid));
        drop(running);
        assert!(!lock(&GROUPS).entries.iter().any(|&(_, g)| g == pgid));
        child.wait().unwrap();

        let path = Path::new("/nonexistent/x");
        let removing = remove_on_signal(path);
        assert!(lock(&FILES).entries.iter().any(|(_, p)| p == path));
        drop(removing);
        assert!(!lock(&FILES).entries.iter().any(|(_, p)| p == path));
    }
}

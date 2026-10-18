//! The process groups subcheck started that are still running (LLM commands, review's mpv,
//! apply's mkvmerge), and the files to remove, so that a signal that ends subcheck ends those
//! groups too and leaves no half-written file behind.

use std::ffi::{c_char, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::Once;

/// More than `-j` will ever want; a command that finds no free slot just is not forwarded to.
const SLOTS: usize = 64;
static RUNNING: [AtomicI32; SLOTS] = [const { AtomicI32::new(0) }; SLOTS];
/// Apply has one file in the making at a time; a path that finds no free slot is not removed.
const PATH_SLOTS: usize = 8;
static REMOVE: [AtomicPtr<c_char>; PATH_SLOTS] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; PATH_SLOTS];
static INSTALL: Once = Once::new();

/// A registered group; dropping it frees the slot.
pub struct Registered(Option<usize>);

impl Drop for Registered {
    fn drop(&mut self) {
        if let Some(i) = self.0 {
            RUNNING[i].store(0, Ordering::SeqCst);
        }
    }
}

pub fn register(pgid: i32) -> Registered {
    INSTALL.call_once(install);
    if pgid <= 0 {
        return Registered(None);
    }
    Registered(RUNNING.iter().position(|slot| {
        slot.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }))
}

/// A file a signal removes; dropping it forgets the file (and removes nothing).
pub struct Removing(Option<usize>);

impl Drop for Removing {
    fn drop(&mut self) {
        if let Some(i) = self.0 {
            REMOVE[i].store(std::ptr::null_mut(), Ordering::SeqCst);
        }
    }
}

/// Remove `path` if a signal ends subcheck before the returned guard is dropped.
pub fn remove_on_signal(path: &Path) -> Removing {
    INSTALL.call_once(install);
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return Removing(None);
    };
    // Leaked, not freed with the guard: the handler may be reading it at that very moment. A few
    // bytes per file remuxed.
    let ptr = c.into_raw();
    Removing(REMOVE.iter().position(|slot| {
        slot.compare_exchange(
            std::ptr::null_mut(),
            ptr,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
    }))
}

pub fn kill(pgid: i32) {
    if pgid > 0 {
        // SAFETY: kill only reads its integer arguments.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// Kills every running group and removes every registered file, then dies of the signal as if
/// there were no handler. Only async-signal-safe calls: atomic loads, kill, unlink, signal, raise.
extern "C" fn on_signal(sig: libc::c_int) {
    for slot in &RUNNING {
        let pgid = slot.load(Ordering::SeqCst);
        if pgid > 0 {
            // SAFETY: as in `kill`.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    for slot in &REMOVE {
        let path = slot.load(Ordering::SeqCst);
        if !path.is_null() {
            // SAFETY: a registered path is a NUL-terminated string that is never freed.
            unsafe {
                libc::unlink(path);
            }
        }
    }
    // SAFETY: restoring the default action and re-raising are async-signal-safe.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install() {
    install_on(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]);
}

/// Handle each of `signals` that is not ignored. One ignored when subcheck started stays ignored:
/// under `nohup` a hang-up must not end it.
fn install_on(signals: &[libc::c_int]) {
    let handler = on_signal as extern "C" fn(libc::c_int);
    for &sig in signals {
        // SAFETY: a null new action only reads the current one into `old`; `on_signal` makes
        // only async-signal-safe calls.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &raw mut old);
            if old.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            libc::signal(sig, handler as libc::sighandler_t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD: &str = "group::tests::child_that_registers_a_file_and_waits";

    /// Run by `signal`, as the child it signals: registers $SUBCHECK_TEST_REMOVE (ignoring SIGHUP
    /// first if $SUBCHECK_TEST_NOHUP is set), says it is ready, and waits.
    #[test]
    #[ignore = "the child of the signal tests"]
    fn child_that_registers_a_file_and_waits() {
        let Some(path) = std::env::var_os("SUBCHECK_TEST_REMOVE") else {
            return;
        };
        if std::env::var_os("SUBCHECK_TEST_NOHUP").is_some() {
            // SAFETY: setting a disposition to SIG_IGN.
            unsafe {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
        }
        let _removing = remove_on_signal(Path::new(&path));
        let mut ready = path;
        ready.push(".ready");
        std::fs::write(ready, "").unwrap();
        std::thread::sleep(Duration::from_secs(10));
    }

    /// Start the child on `path`, send it `sig` once it is ready, and return whether it died of it.
    fn signal(path: &Path, nohup: bool, sig: libc::c_int) -> bool {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("SUBCHECK_TEST_REMOVE", path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if nohup {
            cmd.env("SUBCHECK_TEST_NOHUP", "1");
        }
        let mut child = cmd.spawn().unwrap();
        let ready = path.with_extension("mkv.ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the child never got ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = i32::try_from(child.id()).unwrap();
        // SAFETY: kill only reads its integer arguments.
        unsafe {
            libc::kill(pid, sig);
        }
        std::thread::sleep(Duration::from_millis(300));
        let died = child.try_wait().unwrap().is_some();
        if !died {
            child.kill().unwrap();
        }
        child.wait().unwrap();
        died
    }

    #[test]
    fn a_signal_removes_a_registered_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        assert!(signal(&path, false, libc::SIGTERM), "SIGTERM ends it");
        assert!(!path.exists());
    }

    #[test]
    fn a_hang_up_ignored_from_the_start_stays_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.mkv");
        std::fs::write(&path, "half a remux").unwrap();
        assert!(
            !signal(&path, true, libc::SIGHUP),
            "nohup: SIGHUP must not end it"
        );
        assert!(path.exists());
    }

    #[test]
    fn a_dropped_registration_frees_its_slot() {
        let free = || {
            REMOVE
                .iter()
                .filter(|s| s.load(Ordering::SeqCst).is_null())
                .count()
        };
        let before = free();
        let r = remove_on_signal(Path::new("/nonexistent/x"));
        assert_eq!(free(), before - 1);
        drop(r);
        assert_eq!(free(), before);
    }
}

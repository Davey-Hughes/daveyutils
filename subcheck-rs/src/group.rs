//! The process groups subcheck started that are still running (LLM commands, review's mpv), so a
//! signal that ends subcheck ends them too.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Once;

/// More than `-j` will ever want; a command that finds no free slot just is not forwarded to.
const SLOTS: usize = 64;
static RUNNING: [AtomicI32; SLOTS] = [const { AtomicI32::new(0) }; SLOTS];
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

pub fn kill(pgid: i32) {
    if pgid > 0 {
        // SAFETY: kill only reads its integer arguments.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// Kills every running group, then dies of the signal as if there were no handler. Only
/// async-signal-safe calls: atomic loads, kill, signal, raise.
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
    // SAFETY: restoring the default action and re-raising are async-signal-safe.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install() {
    let handler = on_signal as extern "C" fn(libc::c_int);
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `on_signal` makes only async-signal-safe calls.
        unsafe {
            libc::signal(sig, handler as libc::sighandler_t);
        }
    }
}

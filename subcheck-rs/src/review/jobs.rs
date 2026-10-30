//! The screen's threads: the terminal's input and the scan, each sending what happens to the
//! loop's channel as a `Msg`. Each runs to its end or the process's: quitting with the scan
//! running ends its LLM calls through `group::end`, and its threads with the process.

use std::any::Any;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::Sender;

use crossterm::event;

use super::update::Msg;
use crate::scan::{self, Options, Plan};
use crate::store::Store;

/// Read the terminal on a thread of its own, each key, click or resize sent as its `Msg`. A
/// terminal that cannot be read ends the screen (`Msg::Fatal`), and so does a panic: with this
/// thread gone no key reaches the loop, `q` and Ctrl-C among them.
pub fn input(tx: Sender<Msg>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            let read = panic::catch_unwind(AssertUnwindSafe(|| {
                loop {
                    let msg = super::next_msg(event::read)
                        .unwrap_or_else(|e| Msg::Fatal(format!("reading the terminal: {e}")));
                    let fatal = matches!(msg, Msg::Fatal(_));
                    if tx.send(msg).is_err() || fatal {
                        return;
                    }
                }
            }));
            if let Err(panic) = read {
                let _ = tx.send(Msg::Fatal(format!(
                    "reading the terminal panicked: {}",
                    panic_text(panic.as_ref())
                )));
            }
        })?;
    Ok(())
}

/// Run `plan` on a thread of its own, saving through `store`, a connection of its own: each step
/// sent as `Msg::Scan`, then how it ended as `Msg::ScanEnded`. What `scan` would log goes nowhere:
/// the screen has each file's lines from its events. A panic is `Msg::Fatal`.
pub fn scan(plan: Plan, opts: Options, mut store: Store, tx: Sender<Msg>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let report = |event: scan::Event| {
                let _ = tx.send(Msg::Scan(event));
            };
            let ran = panic::catch_unwind(AssertUnwindSafe(|| {
                scan::execute(plan, &opts, &mut store, &mut io::sink(), &report)
            }));
            let _ = tx.send(match ran {
                Ok(ended) => Msg::ScanEnded(ended.map_err(|e| format!("{e:#}"))),
                Err(panic) => {
                    Msg::Fatal(format!("the scan panicked: {}", panic_text(panic.as_ref())))
                }
            });
        })?;
    Ok(())
}

/// What a panic said.
fn panic_text(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".into())
}

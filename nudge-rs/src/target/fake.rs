//! A scripted [`Target`] for tests: it answers captures and sizes from a
//! script and records what is typed into it. Shared by the tests of every
//! module that drives a pane, so each does not grow its own.

use std::cell::RefCell;

use super::{Key, Target, WrongPane};

/// In-memory Target: returns a scripted screen, records what was sent.
pub(crate) struct FakeTarget {
    /// What successive `capture()` calls answer, in order; the last entry
    /// repeats forever, so one entry is a pane that never changes.
    pub(crate) screens: RefCell<Vec<String>>,
    /// What successive `dims()` calls answer, in order; the last entry
    /// repeats forever. A one-element script is therefore a pane whose size
    /// is not changing, however many times it is read.
    pub(crate) dims_reads: RefCell<Vec<Option<crate::target::PaneDims>>>,
    pub(crate) sent: RefCell<Vec<String>>,
    /// Each lone key pressed, with how many lines had been sent before it.
    pub(crate) keys: RefCell<Vec<(usize, Key)>>,
    /// Whether `capture()` fails, as it does for a pane tmux cannot read.
    pub(crate) capture_fails: bool,
    /// What `wrong_pane()` answers.
    pub(crate) wrong_pane: Option<WrongPane>,
}
impl FakeTarget {
    pub(crate) fn new(screen: &str) -> Self {
        FakeTarget {
            screens: RefCell::new(vec![screen.to_string()]),
            dims_reads: RefCell::new(vec![Some(DIMS)]),
            sent: RefCell::new(Vec::new()),
            keys: RefCell::new(Vec::new()),
            capture_fails: false,
            wrong_pane: None,
        }
    }
    /// A pane that shows each of `screens` in turn, one per capture.
    pub(crate) fn showing(screens: &[&str]) -> Self {
        FakeTarget {
            screens: RefCell::new(screens.iter().map(ToString::to_string).collect()),
            ..FakeTarget::new("")
        }
    }
    /// A pane that takes keys but will not give up its screen.
    pub(crate) fn unreadable() -> Self {
        FakeTarget {
            capture_fails: true,
            ..FakeTarget::new("")
        }
    }
    /// A pane whose size tmux will not report (it went away, or tmux
    /// answered with the empty fields it returns for a dead pane).
    pub(crate) fn with_unknown_dims(screen: &str) -> Self {
        FakeTarget {
            dims_reads: RefCell::new(vec![None]),
            ..FakeTarget::new(screen)
        }
    }
    /// The same pane, resized: tmux reflows the capture, so the text is
    /// different *and* so are the dims.
    pub(crate) fn resized(screen: &str, dims: crate::target::PaneDims) -> Self {
        FakeTarget {
            dims_reads: RefCell::new(vec![Some(dims)]),
            ..FakeTarget::new(screen)
        }
    }
    /// A pane resized *while the capture was in flight*: a read taken before
    /// it still answers the old size, the capture comes back already
    /// reflowed, and a read taken after sees the new size.
    pub(crate) fn resized_mid_capture(
        screen: &str,
        before: crate::target::PaneDims,
        after: crate::target::PaneDims,
    ) -> Self {
        FakeTarget {
            dims_reads: RefCell::new(vec![Some(before), Some(after)]),
            ..FakeTarget::new(screen)
        }
    }
}

pub(crate) const DIMS: crate::target::PaneDims = crate::target::PaneDims {
    width: 80,
    height: 24,
};
impl Target for FakeTarget {
    fn capture(&self) -> anyhow::Result<String> {
        if self.capture_fails {
            anyhow::bail!("can't find pane");
        }
        let mut screens = self.screens.borrow_mut();
        Ok(if screens.len() > 1 {
            screens.remove(0)
        } else {
            screens[0].clone()
        })
    }
    fn send_line(&self, text: &str) -> anyhow::Result<()> {
        self.sent.borrow_mut().push(text.to_string());
        Ok(())
    }
    fn capture_styled(&self) -> anyhow::Result<String> {
        self.capture()
    }
    fn settle(&self) {}
    fn send_key(&self, key: Key) -> anyhow::Result<()> {
        let lines_before = self.sent.borrow().len();
        self.keys.borrow_mut().push((lines_before, key));
        Ok(())
    }
    fn wrong_pane(&self) -> anyhow::Result<Option<WrongPane>> {
        Ok(self.wrong_pane)
    }
    fn dims(&self) -> Option<crate::target::PaneDims> {
        let mut reads = self.dims_reads.borrow_mut();
        if reads.len() > 1 {
            reads.remove(0)
        } else {
            reads[0]
        }
    }
}

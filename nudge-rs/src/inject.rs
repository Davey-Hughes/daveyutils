//! Run a scheduled nudge against a `Target`: an optional `--verify` gate, then
//! type each message in order.

use anyhow::Result;

use crate::detect::detect_reset;
use crate::job::Job;
use crate::target::Target;

/// The result of an injection attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum InjectOutcome {
    /// Messages were sent; carries how many.
    Sent(usize),
    /// `--verify` was on and the pane no longer showed a rate-limit banner, so
    /// nothing was sent (the session was likely already resumed).
    SkippedVerify,
}

/// Execute `job`'s injection against `target`.
///
/// With `job.verify`, the pane is captured and checked for a rate-limit banner
/// first; if none is present the send is skipped. Otherwise each message is
/// typed and submitted, pausing `job.send_delay_secs` between messages.
pub fn run_injection(
    target: &dyn Target,
    job: &Job,
    now: &jiff::Zoned,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
) -> Result<InjectOutcome> {
    if job.verify {
        let screen = target.capture()?;
        if detect_reset(&screen, now, clock_ext, dur_ext).is_none() {
            return Ok(InjectOutcome::SkippedVerify);
        }
    }

    let delay = std::time::Duration::from_secs_f64(job.send_delay_secs.max(0.0));
    for (i, msg) in job.messages.iter().enumerate() {
        if i > 0 && !delay.is_zero() {
            std::thread::sleep(delay);
        }
        target.send_line(msg)?;
    }
    Ok(InjectOutcome::Sent(job.messages.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, Target as TargetKind};
    use jiff::{civil::date, tz::TimeZone};
    use std::cell::RefCell;

    fn now() -> jiff::Zoned {
        date(2026, 7, 13)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap()
    }

    /// In-memory Target: returns a fixed screen, records what was sent.
    struct FakeTarget {
        screen: String,
        sent: RefCell<Vec<String>>,
    }
    impl FakeTarget {
        fn new(screen: &str) -> Self {
            FakeTarget {
                screen: screen.to_string(),
                sent: RefCell::new(Vec::new()),
            }
        }
    }
    impl Target for FakeTarget {
        fn capture(&self) -> anyhow::Result<String> {
            Ok(self.screen.clone())
        }
        fn send_line(&self, text: &str) -> anyhow::Result<()> {
            self.sent.borrow_mut().push(text.to_string());
            Ok(())
        }
    }

    fn job(verify: bool, messages: &[&str]) -> Job {
        Job {
            id: 1,
            target: TargetKind::Tmux { pane: "x".into() },
            messages: messages.iter().map(|s| s.to_string()).collect(),
            send_delay_secs: 0.0,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
        }
    }

    #[test]
    fn sends_all_messages_in_order_when_verify_off() {
        let t = FakeTarget::new("");
        let out = run_injection(&t, &job(false, &["one", "two"]), &now(), None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(2));
        assert_eq!(*t.sent.borrow(), vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn verify_sends_when_banner_present() {
        let t = FakeTarget::new("quota reached. Resets in 45m");
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
        assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
    }

    #[test]
    fn verify_skips_when_banner_gone() {
        let t = FakeTarget::new("all done, no limits here");
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None).unwrap();
        assert_eq!(out, InjectOutcome::SkippedVerify);
        assert!(t.sent.borrow().is_empty());
    }
}

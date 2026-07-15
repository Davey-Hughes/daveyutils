//! Run a scheduled nudge against a `Target`: an optional `--verify` gate, then
//! type each message in order.

use anyhow::Result;

use crate::detect::detect_reset;
use crate::job::Job;
use crate::target::Target;

/// The result of an injection attempt.
///
/// The two skips are separate variants because they are separate stories, and a
/// user staring at a nudge that did not fire needs to know which one they got:
/// "the banner was gone" says the session had already come back on its own,
/// while "the pane changed" says *you* touched it. Collapsed into one outcome,
/// the notification can only say "skipped", which reads uncomfortably close to
/// the failure this whole design exists to avoid -- nudge silently not firing.
#[derive(Debug, PartialEq, Eq)]
pub enum InjectOutcome {
    /// Messages were sent; carries how many.
    Sent(usize),
    /// `--verify` was on and the pane no longer showed a rate-limit banner, so
    /// nothing was sent (the session was likely already resumed).
    SkippedNoBanner,
    /// `--verify` was on and the pane had changed since the job was scheduled,
    /// so the user has resumed this session themselves.
    SkippedResumed,
}

/// Execute `job`'s injection against `target`.
///
/// With `job.verify`, the pane is gated twice before anything is typed: it must
/// not have moved since the job was scheduled (the recency gate), and it must
/// still show a rate-limit banner. Otherwise each message is typed and
/// submitted, pausing `job.send_delay_secs` between messages.
pub fn run_injection(
    target: &dyn Target,
    job: &Job,
    now: &jiff::Zoned,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
) -> Result<InjectOutcome> {
    if job.verify {
        // Dims either side of the capture, believed only when they agree.
        //
        // Reading them once left a window: a resize landing between the read
        // and the capture makes the dims describe a layout the capture no
        // longer has. They match the baseline, so the gate thinks the two
        // captures are comparable, while the fingerprint cannot match because
        // every line reflowed -- Changed, i.e. "the user resumed", i.e. the
        // nudge silently never fires. Agreement on both sides is what proves no
        // resize straddled the capture; disagreement is precisely the state we
        // cannot interpret, so it goes to Unknown and fails open, as does
        // either read coming back unreadable.
        //
        // `capture_baseline` deliberately does NOT mirror this and must not be
        // "made consistent" with it -- see the comment there for why its
        // dims-then-capture order is already the safe one at schedule time.
        //
        // A capture that fails is an Err (the pane is gone, tmux is down),
        // which the caller retries or reports. It is deliberately not a skip --
        // an unreachable pane is a failure to say out loud, not a decision that
        // the user resumed.
        let dims_before = target.dims();
        let screen = target.capture()?;
        let dims_after = target.dims();
        let now_dims = match (dims_before, dims_after) {
            (Some(before), Some(after)) if before == after => Some(before),
            _ => None,
        };

        // Has the pane moved since the user scheduled this? Only `Changed` --
        // same size, different content -- stops the nudge. Everything else,
        // including every way of being unsure, falls through to the banner
        // check below, which is exactly what nudge did before this gate existed.
        if crate::verify::recency(
            job.verify_baseline(),
            &crate::verify::fingerprint(&screen),
            now_dims,
        ) == crate::verify::Recency::Changed
        {
            return Ok(InjectOutcome::SkippedResumed);
        }

        if detect_reset(&screen, now, clock_ext, dur_ext).is_none() {
            return Ok(InjectOutcome::SkippedNoBanner);
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
    use crate::job::{Job, TargetSpec};
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
        /// What successive `dims()` calls answer, in order; the last entry
        /// repeats forever. A one-element script is therefore a pane whose size
        /// is not changing, however many times it is read.
        dims_reads: RefCell<Vec<Option<crate::target::PaneDims>>>,
        sent: RefCell<Vec<String>>,
    }
    impl FakeTarget {
        fn new(screen: &str) -> Self {
            FakeTarget {
                screen: screen.to_string(),
                dims_reads: RefCell::new(vec![Some(DIMS)]),
                sent: RefCell::new(Vec::new()),
            }
        }
        /// A pane whose size tmux will not report (it went away, or tmux
        /// answered with the empty fields it returns for a dead pane).
        fn with_unknown_dims(screen: &str) -> Self {
            FakeTarget {
                dims_reads: RefCell::new(vec![None]),
                ..FakeTarget::new(screen)
            }
        }
        /// The same pane, resized: tmux reflows the capture, so the text is
        /// different *and* so are the dims.
        fn resized(screen: &str, dims: crate::target::PaneDims) -> Self {
            FakeTarget {
                dims_reads: RefCell::new(vec![Some(dims)]),
                ..FakeTarget::new(screen)
            }
        }
        /// A pane resized *while the capture was in flight*: a read taken before
        /// it still answers the old size, the capture comes back already
        /// reflowed, and a read taken after sees the new size.
        fn resized_mid_capture(
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

    const DIMS: crate::target::PaneDims = crate::target::PaneDims {
        width: 80,
        height: 24,
    };
    impl Target for FakeTarget {
        fn capture(&self) -> anyhow::Result<String> {
            Ok(self.screen.clone())
        }
        fn send_line(&self, text: &str) -> anyhow::Result<()> {
            self.sent.borrow_mut().push(text.to_string());
            Ok(())
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

    fn job(verify: bool, messages: &[&str]) -> Job {
        Job {
            id: 1,
            target: TargetSpec::Tmux { pane: "x".into() },
            messages: messages.iter().map(|s| s.to_string()).collect(),
            send_delay_secs: 0.0,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
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
        assert_eq!(out, InjectOutcome::SkippedNoBanner);
        assert!(t.sent.borrow().is_empty());
    }

    // ---- the recency gate (finding I19) ----

    /// The pane as it looked when the user scheduled: parked at the banner.
    const PARKED: &str = "\
● Working on it...
⏸ session limit reached · resets 3:00am

❯ ";

    /// `job`, snapshotted against `screen` as `capture_baseline` would at
    /// schedule time.
    fn job_snapshotted_at(screen: &str, messages: &[&str]) -> Job {
        Job {
            verify_fingerprint: Some(crate::verify::fingerprint(screen)),
            verify_dims: Some(DIMS),
            ..job(true, messages)
        }
    }

    /// Finding I19 itself. 23:00: Claude prints the banner, the user schedules
    /// for 03:03. 03:01: the user resumes by hand and Claude answers. 03:03: the
    /// 03:00 banner is *still on screen* a few lines up, so the banner check
    /// passes and nudge types "please continue" into the session the user is
    /// actively using -- the exact outcome `--verify` advertises it prevents.
    #[test]
    fn verify_skips_when_the_user_resumed_and_the_stale_banner_is_still_on_screen() {
        let resumed = "\
● Working on it...
⏸ session limit reached · resets 3:00am
● Sure -- resuming now.
● All 40 tests pass.

❯ ";
        let t = FakeTarget::new(resumed);
        let out =
            run_injection(&t, &job_snapshotted_at(PARKED, &["go"]), &now(), None, None).unwrap();
        assert_eq!(
            out,
            InjectOutcome::SkippedResumed,
            "the banner is still on screen, but the pane moved since we scheduled: \
             the user resumed and nudge must not type into their live session"
        );
        assert!(
            t.sent.borrow().is_empty(),
            "nothing may be typed into a session the user already resumed"
        );
    }

    /// The disaster guard, and the reason the polarity above is not simply
    /// "skip whenever unsure": a pane still parked at its banner at fire time is
    /// the whole point of the tool. If this ever goes red, the overnight nudge
    /// silently never fires.
    #[test]
    fn verify_sends_when_the_pane_is_untouched_since_scheduling() {
        let t = FakeTarget::new(PARKED);
        let out =
            run_injection(&t, &job_snapshotted_at(PARKED, &["go"]), &now(), None, None).unwrap();
        assert_eq!(
            out,
            InjectOutcome::Sent(1),
            "an untouched pane still parked at its banner is exactly what nudge exists to resume"
        );
        assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
    }

    /// Why the fingerprint covers the whole capture rather than tracking the
    /// banner's row offset. The pane is not yet full, so the resumed output
    /// appends into blank space *below* the banner and nothing scrolls: the
    /// banner sits at the same offset in both captures, and an offset-tracking
    /// design would call this untouched and inject into a live session.
    #[test]
    fn verify_skips_when_output_appends_below_the_banner_without_scrolling() {
        let appended = "\
● Working on it...
⏸ session limit reached · resets 3:00am
● Sure -- resuming now.

❯ ";
        // The property the test rests on: the banner did not move.
        let banner_row = |s: &str| {
            s.lines()
                .position(|l| l.contains("session limit reached"))
                .unwrap()
        };
        assert_eq!(
            banner_row(PARKED),
            banner_row(appended),
            "precondition: nothing scrolled, so the banner is at the same offset in both"
        );
        let t = FakeTarget::new(appended);
        let out =
            run_injection(&t, &job_snapshotted_at(PARKED, &["go"]), &now(), None, None).unwrap();
        assert_eq!(
            out,
            InjectOutcome::SkippedResumed,
            "the banner never moved, so only a whole-capture hash can see this pane changed"
        );
    }

    // ---- fail-open paths: each one must INJECT, never skip ----

    /// The user resized the window between scheduling and firing. tmux reflowed
    /// the capture, so the fingerprint differs for reasons that have nothing to
    /// do with the user resuming. Unsure means inject.
    #[test]
    fn verify_fails_open_and_sends_when_the_pane_was_resized() {
        let reflowed = "● Working on it... ⏸ session limit reached · resets 3:00am\n\n❯ ";
        let t = FakeTarget::resized(
            reflowed,
            crate::target::PaneDims {
                width: 120,
                height: 24,
            },
        );
        let out =
            run_injection(&t, &job_snapshotted_at(PARKED, &["go"]), &now(), None, None).unwrap();
        assert_eq!(
            out,
            InjectOutcome::Sent(1),
            "a resize reflows every line; reading that as 'the user resumed' would \
             make a resized window silently never fire"
        );
    }

    /// The resize that lands *between* the dims read and the capture.
    ///
    /// Reading dims once, before capturing, leaves a window: `dims()` answers
    /// the size the pane had a moment ago, and the capture comes back already
    /// reflowed. The dims then *match* the baseline -- so the gate believes the
    /// two captures are comparable -- while the fingerprint cannot possibly
    /// match, because every line wrapped differently. That reads as "the user
    /// resumed" and the nudge silently never fires, which is the one failure
    /// this design will not trade for anything.
    ///
    /// The window is milliseconds wide, so this is hardening rather than a bug
    /// anyone has hit. It is also the disaster direction, and reading dims on
    /// both sides costs one more tmux call.
    #[test]
    fn verify_fails_open_when_a_resize_lands_between_the_dims_read_and_the_capture() {
        let reflowed = "● Working on it... ⏸ session limit reached · resets 3:00am\n\n❯ ";
        let t = FakeTarget::resized_mid_capture(
            reflowed,
            DIMS,
            crate::target::PaneDims {
                width: 120,
                height: 24,
            },
        );
        let out =
            run_injection(&t, &job_snapshotted_at(PARKED, &["go"]), &now(), None, None).unwrap();
        assert_eq!(
            out,
            InjectOutcome::Sent(1),
            "a resize straddling the capture makes the dims a lie: they match the \
             baseline while the text they describe has already reflowed. Believing \
             them turns an untouched pane into 'resumed' and never fires."
        );
    }

    /// A job scheduled by a build from before this gate existed, as the daemon
    /// reloads it from queue.json. It carries no snapshot and must behave
    /// exactly as it did then: banner present -> inject.
    #[test]
    fn verify_fails_open_and_sends_for_an_old_job_with_no_baseline() {
        let t = FakeTarget::new(PARKED);
        let mut j = job(true, &["go"]);
        j.verify_fingerprint = None;
        j.verify_dims = None;
        let out = run_injection(&t, &j, &now(), None, None).unwrap();
        assert_eq!(
            out,
            InjectOutcome::Sent(1),
            "no snapshot means no opinion about recency, which must not become a skip"
        );
    }

    /// tmux would not say how big the pane is, so the two captures are not
    /// comparable.
    #[test]
    fn verify_fails_open_and_sends_when_the_pane_dims_are_unreadable() {
        let t = FakeTarget::with_unknown_dims("● busy\n⏸ session limit reached · resets 3:00am");
        let out =
            run_injection(&t, &job_snapshotted_at(PARKED, &["go"]), &now(), None, None).unwrap();
        assert_eq!(
            out,
            InjectOutcome::Sent(1),
            "unknown dims means not comparable, which fails open to the banner check"
        );
    }

    /// Fail-open reaches the banner check; it does not bypass it. An old job
    /// whose pane shows no banner is still a skip -- just a differently-named
    /// one.
    #[test]
    fn failing_open_still_runs_the_banner_check() {
        let t = FakeTarget::new("all done, no limits here");
        let mut j = job(true, &["go"]);
        j.verify_fingerprint = None;
        let out = run_injection(&t, &j, &now(), None, None).unwrap();
        assert_eq!(out, InjectOutcome::SkippedNoBanner);
    }

    /// The gate is `--verify`'s alone: without the flag, a changed pane is not a
    /// reason to withhold anything.
    #[test]
    fn a_changed_pane_does_not_gate_a_job_without_verify() {
        let t = FakeTarget::new("totally different");
        let j = Job {
            verify_fingerprint: Some(crate::verify::fingerprint(PARKED)),
            verify_dims: Some(DIMS),
            ..job(false, &["go"])
        };
        assert_eq!(
            run_injection(&t, &j, &now(), None, None).unwrap(),
            InjectOutcome::Sent(1)
        );
    }
}

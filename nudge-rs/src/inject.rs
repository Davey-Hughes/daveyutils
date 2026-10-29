//! Run a scheduled nudge against a `Target`: an optional `--verify` gate, then
//! type each message in order.

use anyhow::Result;

use crate::composer::{BoxKind, ClaudeMode, InputBox};
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
    /// A message was typed and Enter pressed, and it was still in the input
    /// box afterwards. `submitted` messages went before it; none were typed
    /// after it, since they would only have joined it in the box.
    Unsubmitted { submitted: usize },
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
    weekly_ext: Option<&str>,
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

        // Detection::Unreadable counts as banner-present, i.e. inject. Something
        // limit-shaped is still on screen and we could not read its reset day;
        // per this module's polarity, every ambiguity resolves toward inject,
        // because a false skip is the nudge silently never firing.
        //
        // Detection::Elapsed does too, and is no ambiguity at all: a nudge fires
        // after the time its banner named, so for a Codex pane still parked at
        // its limit this is the answer every single time.
        if matches!(
            detect_reset(&screen, now, clock_ext, dur_ext, weekly_ext),
            crate::detect::Detection::None
        ) {
            return Ok(InjectOutcome::SkippedNoBanner);
        }
    }

    let delay = std::time::Duration::from_secs_f64(job.send_delay_secs.max(0.0));
    for (i, msg) in job.messages.iter().enumerate() {
        if i > 0 && !delay.is_zero() {
            std::thread::sleep(delay);
        }
        // A composer waiting in Vim Normal mode reads typed text as commands,
        // so it is put in Insert mode the way a person would do it. Read again
        // for every message, since submitting one is what changes the screen.
        //
        // A capture that fails here is not an error: not knowing the mode is no
        // reason to withhold the nudge, and if the pane is truly gone the send
        // below is what will say so.
        if let Ok(screen) = target.capture_styled() {
            if crate::composer::vim_normal(&screen) {
                target.send_key("i")?;
            } else if let Some(before) = crate::composer::input_box(&screen)
                && let BoxKind::Claude {
                    mode: mode @ (ClaudeMode::Visual | ClaudeMode::Unlabelled),
                } = before.kind
            {
                claude_insert_mode(target, &before, mode)?;
            }
        }
        target.send_line(msg)?;
        // Enter that does not submit leaves the message where it was typed, as
        // Codex once did by reading it as a newline inside a paste. Said
        // outright, that is a nudge that did not go out; read as `Sent`, it is
        // one that did. The evidence either way is the box: a read showing it
        // without the message says it went, and one showing it holding the
        // message says it has not yet. A read that shows no box says nothing.
        // Stuck means seen held and never seen clear; a pane no read shows a
        // box in counts as sent, since nothing says otherwise.
        let mut seen_held = false;
        let cleared = reread(target, |input| match input {
            Some(b) if b.holds(msg) => {
                seen_held = true;
                None
            }
            Some(_) => Some(()),
            None => None,
        });
        if cleared.is_none() && seen_held {
            return Ok(InjectOutcome::Unsubmitted { submitted: i });
        }
    }
    Ok(InjectOutcome::Sent(job.messages.len()))
}

/// How many more times a box is read, [`Target::settle`] apart, before what it
/// shows is taken as final: a long transcript or a busy machine can be slow to
/// redraw.
const REREADS: usize = 4;

/// Read the pane's input box up to `1 + REREADS` times, a settle apart, until
/// `decide` has an answer. `None` from `decide` means look again; a read that
/// fails, or shows no box nudge can read, is handed to it as `None`.
fn reread<T>(
    target: &dyn Target,
    mut decide: impl FnMut(Option<InputBox>) -> Option<T>,
) -> Option<T> {
    for read in 0..=REREADS {
        if read > 0 {
            target.settle();
        }
        let input = target
            .capture_styled()
            .ok()
            .and_then(|screen| crate::composer::input_box(&screen));
        if let Some(answer) = decide(input) {
            return Some(answer);
        }
    }
    None
}

/// Put a Claude Code box drawn without `-- INSERT --` into Insert mode, if it
/// has Vim keys at all.
///
/// Claude labels Insert mode and leaves Normal mode unlabelled, which is also
/// how a box with its Vim keys off looks, so the missing label cannot say
/// which this is. An `A` can. Normal mode takes it as the command to append,
/// draws the label, and leaves the box's text alone. A box without Vim keys
/// types it, and gets a Backspace to take it out again. Seen live on
/// 2026-10-27: two nudges typed into Normal mode arrived as "se continue".
///
/// `A` rather than `i` because a draft in the box puts Normal mode's cursor on
/// its last character, and `i` would insert the nudge in front of that. Visual
/// mode is escaped from first, since an `A` there is no way into Insert mode.
fn claude_insert_mode(target: &dyn Target, before: &InputBox, mode: ClaudeMode) -> Result<()> {
    if mode == ClaudeMode::Visual {
        target.send_key("Escape")?;
    }
    target.send_key("A")?;
    // Only the `A` itself is taken back: typed, it is one more `A` than the
    // box held, wherever the cursor put it. A box whose text changed some
    // other way keeps every character. Nothing changing at all is Normal mode
    // with its label hidden (`hideVimModeIndicator`), which took the `A` as a
    // command.
    let a_count = |text: &str| text.matches('A').count();
    let typed = reread(target, |input| {
        let after = input?;
        match after.kind {
            BoxKind::Claude {
                mode: ClaudeMode::Insert,
            } => Some(false),
            BoxKind::Claude {
                mode: ClaudeMode::Unlabelled,
            } if a_count(&after.text) == a_count(&before.text) + 1 => Some(true),
            _ => None,
        }
    });
    if typed == Some(true) {
        target.send_key("BSpace")?;
    }
    Ok(())
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

    /// In-memory Target: returns a scripted screen, records what was sent.
    struct FakeTarget {
        /// What successive `capture()` calls answer, in order; the last entry
        /// repeats forever, so one entry is a pane that never changes.
        screens: RefCell<Vec<String>>,
        /// What successive `dims()` calls answer, in order; the last entry
        /// repeats forever. A one-element script is therefore a pane whose size
        /// is not changing, however many times it is read.
        dims_reads: RefCell<Vec<Option<crate::target::PaneDims>>>,
        sent: RefCell<Vec<String>>,
        /// Each lone key pressed, with how many lines had been sent before it.
        keys: RefCell<Vec<(usize, String)>>,
        /// Whether `capture()` fails, as it does for a pane tmux cannot read.
        capture_fails: bool,
    }
    impl FakeTarget {
        fn new(screen: &str) -> Self {
            FakeTarget {
                screens: RefCell::new(vec![screen.to_string()]),
                dims_reads: RefCell::new(vec![Some(DIMS)]),
                sent: RefCell::new(Vec::new()),
                keys: RefCell::new(Vec::new()),
                capture_fails: false,
            }
        }
        /// A pane that shows each of `screens` in turn, one per capture.
        fn showing(screens: &[&str]) -> Self {
            FakeTarget {
                screens: RefCell::new(screens.iter().map(ToString::to_string).collect()),
                ..FakeTarget::new("")
            }
        }
        /// A pane that takes keys but will not give up its screen.
        fn unreadable() -> Self {
            FakeTarget {
                capture_fails: true,
                ..FakeTarget::new("")
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
        fn send_key(&self, key: &str) -> anyhow::Result<()> {
            let lines_before = self.sent.borrow().len();
            self.keys.borrow_mut().push((lines_before, key.to_string()));
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
            messages: messages.iter().map(ToString::to_string).collect(),
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
        let out =
            run_injection(&t, &job(false, &["one", "two"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(2));
        assert_eq!(*t.sent.borrow(), vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn verify_sends_when_banner_present() {
        let t = FakeTarget::new("quota reached. Resets in 45m");
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
        assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
    }

    #[test]
    fn verify_skips_when_banner_gone() {
        let t = FakeTarget::new("all done, no limits here");
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::SkippedNoBanner);
        assert!(t.sent.borrow().is_empty());
    }

    /// Codex names a wall-clock time and a nudge fires after it, so by the time
    /// this gate looks, the banner's own time has always gone by. That is the
    /// pane still parked at its limit -- the one case nudge exists for -- and
    /// must never read as "no banner".
    #[test]
    fn verify_sends_into_a_codex_pane_parked_past_its_banner_time() {
        // now() is 10:00; the banner said 9:57.
        let t = FakeTarget::new(
            "■ You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), \
             visit https://chatgpt.com/settings/usage to purchase more credits or try again at\n\
             9:57 AM.\n\n\n› Ask Codex to do anything",
        );
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
    }

    /// The dated form is past its time at fire time too, and is no more a
    /// reason to stand down than the bare one.
    #[test]
    fn verify_sends_into_a_codex_pane_parked_past_a_dated_banner() {
        // now() is 2026-07-13 10:00.
        let t = FakeTarget::new(
            "■ You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), \
             visit https://chatgpt.com/settings/usage to purchase more credits or try again at Jul\n\
             13th, 2026 9:57 AM.\n\n\n› Ask Codex to do anything",
        );
        let out = run_injection(&t, &job(true, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
    }

    // ---- a composer in Vim Normal mode ----

    /// The bottom of a parked Codex pane whose composer was left in Normal
    /// mode, as captured live on 2026-10-12 (the path on its status line
    /// abridged).
    const VIM_NORMAL: &str = "\
› Ask Codex to do anything

  GPT-6.1-Sol xhigh · ~/src                                        Vim: Normal
  ← for agents · ? for shortcuts
";

    /// Typed into Normal mode, "go" is two commands, not a message. An `i`
    /// first is what a person would press, and it has to come before the text.
    #[test]
    fn a_composer_in_vim_normal_mode_is_put_in_insert_mode_before_the_message() {
        let t = FakeTarget::new(VIM_NORMAL);
        let out = run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
        assert_eq!(*t.keys.borrow(), vec![(0, "i".to_string())]);
        assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
    }

    /// Everywhere else an `i` is a stray letter on the front of the message.
    #[test]
    fn a_composer_already_taking_text_gets_no_extra_key() {
        for screen in [
            VIM_NORMAL.replace("Vim: Normal", "Vim: Insert"),
            "❯ \n\n  ? for shortcuts\n".to_string(),
        ] {
            let t = FakeTarget::new(&screen);
            run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
            assert!(t.keys.borrow().is_empty(), "{screen:?}");
            assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
        }
    }

    /// The mode is read again for every message: submitting the first one is
    /// exactly the kind of thing that changes it.
    #[test]
    fn the_composer_mode_is_checked_before_each_message() {
        // Codex is back in Insert mode once the first message is submitted, and
        // a second `i` pressed on the strength of the first reading would be
        // typed: "itwo".
        let insert = VIM_NORMAL.replace("Vim: Normal", "Vim: Insert");
        let t = FakeTarget::showing(&[VIM_NORMAL, &insert]);
        run_injection(&t, &job(false, &["one", "two"]), &now(), None, None, None).unwrap();
        assert_eq!(*t.keys.borrow(), vec![(0, "i".to_string())]);
        assert_eq!(*t.sent.borrow(), vec!["one".to_string(), "two".to_string()]);
    }

    /// Not being able to read the mode is no reason to withhold the nudge. The
    /// send goes ahead exactly as it did before this check existed.
    #[test]
    fn a_pane_whose_mode_cannot_be_read_is_still_sent_to() {
        let t = FakeTarget::unreadable();
        let out = run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
        assert!(t.keys.borrow().is_empty());
        assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
    }

    // ---- a Claude Code box in Vim Normal mode ----

    /// The bottom of an idle Claude Code pane in Insert mode, as captured live
    /// on 2026-10-28 (the rules shortened).
    const CLAUDE_INSERT: &str = "\
✻ Brewed for 27s · done 10:29 PM

────────────────────────────────────────
❯\u{a0}
────────────────────────────────────────
  -- INSERT -- ⏵⏵ auto mode on (shift+tab to cycle) · ← 2 agents
";

    /// The same pane with no `-- INSERT --` under the box, holding `typed`.
    /// That is Normal mode, or a box with its Vim keys off: Claude draws the
    /// two alike.
    fn claude_unlabelled(typed: &str) -> String {
        CLAUDE_INSERT
            .replace("-- INSERT -- ", "")
            .replace("❯\u{a0}", &format!("❯\u{a0}{typed}"))
    }

    /// Seen live on 2026-10-27: two nudges typed into Claude boxes left in
    /// Normal mode arrived as "se continue". An `A` puts the box in Insert
    /// mode at the end of what it holds, and the label it then draws says
    /// that is what happened.
    #[test]
    fn a_claude_box_in_vim_normal_mode_is_put_in_insert_mode_first() {
        let t = FakeTarget::showing(&[&claude_unlabelled(""), CLAUDE_INSERT]);
        let out = run_injection(
            &t,
            &job(false, &["please continue"]),
            &now(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
        assert_eq!(*t.keys.borrow(), vec![(0, "A".to_string())]);
        assert_eq!(*t.sent.borrow(), vec!["please continue".to_string()]);
    }

    /// A box with no Vim keys types the `A`, which then has to come out
    /// again, or the nudge arrives as "Aplease continue".
    #[test]
    fn a_claude_box_without_vim_keys_gets_the_typed_key_taken_back() {
        let t = FakeTarget::showing(&[
            &claude_unlabelled(""),
            &claude_unlabelled("A"),
            &claude_unlabelled(""),
        ]);
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(
            *t.keys.borrow(),
            vec![(0, "A".to_string()), (0, "BSpace".to_string())]
        );
        assert_eq!(*t.sent.borrow(), vec!["go".to_string()]);
    }

    /// A pane slow to redraw shows nothing for the key at first. Read once,
    /// that looks like Normal mode, and the typed `A` stays in the nudge.
    #[test]
    fn a_box_slow_to_show_the_typed_key_still_gets_it_taken_back() {
        let t = FakeTarget::showing(&[
            &claude_unlabelled(""),
            &claude_unlabelled(""),
            &claude_unlabelled(""),
            &claude_unlabelled("A"),
            &claude_unlabelled(""),
        ]);
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(
            *t.keys.borrow(),
            vec![(0, "A".to_string()), (0, "BSpace".to_string())]
        );
    }

    /// So is a pane slow to draw the label, and that box needs nothing more.
    #[test]
    fn a_box_slow_to_show_the_label_gets_nothing_taken_back() {
        let t = FakeTarget::showing(&[
            &claude_unlabelled(""),
            &claude_unlabelled(""),
            CLAUDE_INSERT,
        ]);
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(*t.keys.borrow(), vec![(0, "A".to_string())]);
    }

    /// Only the `A` itself, typed at the end of what the box held, is taken
    /// back. A box whose text changed some other way (a stale first read, a
    /// redraw) keeps every character.
    #[test]
    fn a_box_that_changed_some_other_way_keeps_its_text() {
        let t = FakeTarget::showing(&[&claude_unlabelled("draft"), &claude_unlabelled("drafts")]);
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(*t.keys.borrow(), vec![(0, "A".to_string())]);
    }

    /// A box with no Vim keys types the `A` wherever its cursor is, which is
    /// not always the end of the draft.
    #[test]
    fn a_typed_a_inside_the_draft_is_taken_back() {
        let t = FakeTarget::showing(&[&claude_unlabelled("draft"), &claude_unlabelled("drAaft")]);
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(
            *t.keys.borrow(),
            vec![(0, "A".to_string()), (0, "BSpace".to_string())]
        );
    }

    /// With its label hidden (`hideVimModeIndicator`), Normal mode takes the
    /// `A` and shows nothing for it. There is nothing to take back, and a
    /// Backspace would eat whatever the box held.
    #[test]
    fn a_key_that_changes_nothing_on_screen_is_not_taken_back() {
        let t = FakeTarget::new(&claude_unlabelled("draft"));
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(*t.keys.borrow(), vec![(0, "A".to_string())]);
    }

    /// In Visual mode an `A` is no way into Insert mode. Escape first goes
    /// back to Normal mode, where it is.
    #[test]
    fn a_claude_box_in_visual_mode_is_escaped_first() {
        let visual = CLAUDE_INSERT.replace("-- INSERT --", "-- VISUAL --");
        let t = FakeTarget::showing(&[&visual, CLAUDE_INSERT]);
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(
            *t.keys.borrow(),
            vec![(0, "Escape".to_string()), (0, "A".to_string())]
        );
    }

    #[test]
    fn a_claude_box_labelled_insert_gets_no_key() {
        let t = FakeTarget::new(CLAUDE_INSERT);
        run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert!(t.keys.borrow().is_empty());
    }

    // ---- a message that stays in the box ----

    fn claude_holding(typed: &str) -> String {
        CLAUDE_INSERT.replace("❯\u{a0}", &format!("❯\u{a0}{typed}"))
    }

    /// Enter that does not submit leaves the message in the box: Codex once
    /// took it as a newline in a paste. That is not a nudge sent, and the log
    /// must not say it was.
    #[test]
    fn a_message_still_in_the_box_after_enter_is_reported_unsubmitted() {
        let t = FakeTarget::showing(&[CLAUDE_INSERT, &claude_holding("go")]);
        let out = run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Unsubmitted { submitted: 0 });
    }

    /// A pane slow to redraw still shows the message just after Enter, and
    /// clears it a moment later. That message went out.
    #[test]
    fn a_box_slow_to_clear_is_not_unsubmitted() {
        let t = FakeTarget::showing(&[CLAUDE_INSERT, &claude_holding("go"), CLAUDE_INSERT]);
        let out = run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(1));
    }

    /// A read that shows no box says nothing either way. Only a box seen
    /// clear says the message went.
    #[test]
    fn an_unreadable_read_does_not_make_a_stuck_message_sent() {
        let t = FakeTarget::showing(&[
            CLAUDE_INSERT,
            &claude_holding("go"),
            "mid-redraw, no box on screen",
            &claude_holding("go"),
        ]);
        let out = run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Unsubmitted { submitted: 0 });
    }

    /// Nor does a first read taken mid-redraw.
    #[test]
    fn a_first_read_with_no_box_does_not_make_a_stuck_message_sent() {
        let t = FakeTarget::showing(&[
            CLAUDE_INSERT,
            "mid-redraw, no box on screen",
            &claude_holding("go"),
        ]);
        let out = run_injection(&t, &job(false, &["go"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Unsubmitted { submitted: 0 });
    }

    /// The next message would only be typed in behind it.
    #[test]
    fn no_message_is_typed_behind_one_that_stuck() {
        let t = FakeTarget::showing(&[CLAUDE_INSERT, &claude_holding("one")]);
        let out =
            run_injection(&t, &job(false, &["one", "two"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Unsubmitted { submitted: 0 });
        assert_eq!(*t.sent.borrow(), vec!["one".to_string()]);
    }

    #[test]
    fn a_message_that_left_the_box_is_sent() {
        let t = FakeTarget::new(CLAUDE_INSERT);
        let out =
            run_injection(&t, &job(false, &["one", "two"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Sent(2));
    }

    #[test]
    fn the_second_message_sticking_counts_the_first_as_submitted() {
        let t = FakeTarget::showing(&[
            CLAUDE_INSERT,
            CLAUDE_INSERT,
            CLAUDE_INSERT,
            &claude_holding("two"),
        ]);
        let out =
            run_injection(&t, &job(false, &["one", "two"]), &now(), None, None, None).unwrap();
        assert_eq!(out, InjectOutcome::Unsubmitted { submitted: 1 });
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
    /// for 03:02. 03:01: the user resumes by hand and Claude answers. 03:02: the
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
        let out = run_injection(
            &t,
            &job_snapshotted_at(PARKED, &["go"]),
            &now(),
            None,
            None,
            None,
        )
        .unwrap();
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
        let out = run_injection(
            &t,
            &job_snapshotted_at(PARKED, &["go"]),
            &now(),
            None,
            None,
            None,
        )
        .unwrap();
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
        let out = run_injection(
            &t,
            &job_snapshotted_at(PARKED, &["go"]),
            &now(),
            None,
            None,
            None,
        )
        .unwrap();
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
        let out = run_injection(
            &t,
            &job_snapshotted_at(PARKED, &["go"]),
            &now(),
            None,
            None,
            None,
        )
        .unwrap();
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
        let out = run_injection(
            &t,
            &job_snapshotted_at(PARKED, &["go"]),
            &now(),
            None,
            None,
            None,
        )
        .unwrap();
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
        let out = run_injection(&t, &j, &now(), None, None, None).unwrap();
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
        let out = run_injection(
            &t,
            &job_snapshotted_at(PARKED, &["go"]),
            &now(),
            None,
            None,
            None,
        )
        .unwrap();
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
        let out = run_injection(&t, &j, &now(), None, None, None).unwrap();
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
            run_injection(&t, &j, &now(), None, None, None).unwrap(),
            InjectOutcome::Sent(1)
        );
    }
}

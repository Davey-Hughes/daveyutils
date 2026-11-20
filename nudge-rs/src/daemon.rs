//! The resident scheduler: serves IPC and fires due jobs. Firing happens off
//! the queue lock so a slow tmux inject never blocks IPC.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::{Span, ZonedDateTime};

use crate::inject::run_injection;
use crate::panes::Positions;
use crate::paths::Paths;
use crate::queue::Queue;
use crate::scheduler::{MAX_POLL, apply_outcome, next_wake, plan};

/// A retry never lands sooner than this, so a sub-second `settle_secs` can't
/// create a fire-storm (esp. with infinite retries).
const MIN_RETRY_SECS: f64 = 1.0;

/// Install a tracing subscriber. Safe to call more than once (a second call is
/// a no-op once a global subscriber exists).
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
}

/// Take the daemon singleton lock: an exclusive, non-blocking advisory lock on
/// `<state_dir>/nudge.lock`. The returned File MUST be held for the daemon's
/// lifetime — dropping it releases the lock. The OS releases it automatically if
/// the process dies, so a crashed daemon never wedges the next one.
pub fn acquire_singleton_lock(state_dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    use fs4::fs_std::FileExt;
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("nudge.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let acquired = file.try_lock_exclusive()?;
    if !acquired {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!("another nudge daemon already holds {}", path.display()),
        ));
    }
    Ok(file)
}

/// What the daemon does when its IPC server exits: say why, then take the
/// process down.
///
/// A daemon with no control plane still fires jobs but can't be listed,
/// cancelled or edited -- and the singleton lock (correctly) stops a
/// replacement from taking over. So take the whole process down, letting the
/// user or the service manager start a working daemon, rather than leaving a
/// headless one injecting into panes.
///
/// Named, and taking the outcome as a value, so the policy is one thing in one
/// place -- and so [`run_with`] can hand an in-process test something that does
/// not take the *test binary* down with it.
fn on_serve_exit(result: std::io::Result<()>) -> ! {
    match result {
        Ok(()) => tracing::error!("nudge: ipc server exited unexpectedly"),
        Err(e) => tracing::error!("nudge: ipc server exited: {e}"),
    }
    std::process::exit(1);
}

/// Run the daemon forever: IPC server thread + scheduler loop.
///
/// Call [`init_tracing`] first if you want the daemon's logs.
pub fn run(
    paths: &Paths,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
    weekly_ext: Option<&str>,
    grace: Span,
) -> std::io::Result<()> {
    run_with(paths, clock_ext, dur_ext, weekly_ext, grace, on_serve_exit)
}

/// [`run`], with the serve-exit policy injected.
///
/// Exists for tests that run a real daemon *in-process*. `run`'s policy is
/// `process::exit(1)`, which is right for a daemon and wrong for a cargo test
/// binary: a fatal `serve` there takes down every test in the file at once,
/// exit 1, with no attribution to any test. A test passes a policy that parks
/// instead, and then fails as a test -- on the assertion that the daemon's
/// socket never came up.
///
/// The `-> !` is the point: a serve-exit policy may not return to a daemon that
/// has already lost its control plane.
pub fn run_with(
    paths: &Paths,
    clock_ext: Option<&str>,
    dur_ext: Option<&str>,
    weekly_ext: Option<&str>,
    grace: Span,
    on_serve_exit: fn(std::io::Result<()>) -> !,
) -> std::io::Result<()> {
    // Refuse to start a second daemon: two schedulers on one queue.json double-fire
    // jobs and clobber each other's state.
    let _lock = acquire_singleton_lock(&paths.state_dir).map_err(|e| {
        tracing::error!("nudge: not starting: {e}");
        e
    })?;

    let queue = Arc::new(Mutex::new(Queue::load(paths.queue.clone())?));

    // IPC server on its own thread.
    let q_ipc = Arc::clone(&queue);
    let socket = paths.socket.clone();
    std::thread::spawn(move || on_serve_exit(crate::ipc::server::serve(&socket, &q_ipc)));

    loop {
        let now = ZonedDateTime::now();

        // 1. Snapshot due jobs and stale ids under the lock.
        let plan_now = {
            let q = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            plan(q.all(), &now, &grace)
        };

        // 2. Drop stale jobs under the lock.
        if !plan_now.drop_stale.is_empty() {
            let mut q = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_stale(&mut q, &plan_now.drop_stale);
        }

        // 3. Fire each due job WITHOUT the lock, applying results under it.
        // Where each pane sits is read once, before any of them fires, and
        // herdr is asked only if one of them is a herdr pane: a herdr that
        // has stopped answering must not hold up tmux's jobs.
        let positions = if plan_now.fire.is_empty() {
            Positions::new()
        } else {
            crate::panes::positions_for(plan_now.fire.iter().map(|job| &job.target))
        };
        for job in &plan_now.fire {
            let pane = describe_pane(job, &positions);
            let outcome = run_injection(
                &*job.target.connect(),
                job,
                &now,
                clock_ext,
                dur_ext,
                weekly_ext,
            );
            log_outcome(job, &pane, &outcome);
            if let Some(body) = notification(job, &pane, &outcome) {
                crate::notify::send(&body);
            }
            let retry_secs = job.settle_secs.max(MIN_RETRY_SECS);
            // `as` on a float is saturating (and NaN -> 0) since Rust 1.45, which
            // is the behavior we want from a settle_secs that arrived from a
            // config file: an absurd value pins the retry at the largest span
            // jiff will take and `checked_add` then declines it, falling back to
            // `now` below. Truncation here is the documented escape hatch, not a
            // lurking wrap.
            #[allow(clippy::cast_possible_truncation)]
            let retry_ms = (retry_secs * 1000.0) as i64;
            let retry_at = now
                .checked_add(Span::new().milliseconds(retry_ms))
                .map_or(now.timestamp(), |z| z.timestamp());
            let mut q = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = apply_outcome(&mut q, job, &outcome, retry_at) {
                tracing::warn!("nudge: persisting job {} outcome failed: {e}", job.id);
            }
        }

        // 4. Sleep until the next job is due (capped).
        let wake = {
            let q = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            next_wake(q.all(), &now, MAX_POLL)
        };
        std::thread::sleep(wake.max(Duration::from_millis(50)));
    }
}

/// Drop the stale jobs in `ids`, reporting what actually happened to each.
///
/// Split out of `run`'s loop purely to be reachable: the log line *is* the
/// behaviour here, and `run` is an infinite loop no test can call.
fn drop_stale(q: &mut Queue, ids: &[u64]) {
    for id in ids {
        match q.remove(*id) {
            // Only a removal that actually persisted may claim the drop.
            Ok(true) => tracing::info!("nudge: dropped stale job {id}"),
            // Already gone: nothing happened, so say nothing.
            Ok(false) => {}
            // `remove` rolls back, so the job is still live and still queued.
            // Saying "dropped" on the next line -- as this used to,
            // unconditionally -- tells whoever is debugging why stale jobs keep
            // reappearing that this half worked, and sends them elsewhere.
            Err(e) => tracing::warn!("nudge: removing stale job {id} failed: {e}"),
        }
    }
}

/// Log what firing `job` into `pane` came to.
///
/// Split out of `run`'s loop to be reachable, like [`drop_stale`]. The pane is
/// in the line because a job id alone cannot say where a nudge went, and
/// "where did it go" is the first question about one that never arrived.
fn log_outcome(
    job: &crate::job::Job,
    pane: &str,
    outcome: &anyhow::Result<crate::inject::InjectOutcome>,
) {
    match outcome {
        // Typed but never sent, or never typed because the pane is no longer
        // the one scheduled for: either way the nudge did not go out.
        Ok(
            o @ (crate::inject::InjectOutcome::Unsubmitted { .. }
            | crate::inject::InjectOutcome::WrongPane(_)),
        ) => {
            tracing::warn!("nudge: job {} -> {o:?} in {pane}", job.id);
        }
        // Not "fired": two of the three outcomes deliberately send
        // nothing, and a log that calls a skip a fire is how you spend
        // an evening looking for an injection that never happened.
        Ok(o) => tracing::info!("nudge: job {} -> {o:?} in {pane}", job.id),
        Err(e) => tracing::warn!("nudge: job {} in {pane} failed: {e}", job.id),
    }
}

/// A short human-readable description of a job's target, for the log and
/// notification text: the session's name, where its pane sits now, and the id
/// the job stores.
fn describe_pane(job: &crate::job::Job, positions: &Positions) -> String {
    crate::panes::named_label(&job.target, positions)
}

/// What to tell the user about firing `job`, or `None` for nothing.
///
/// `--notify` is still the opt-in, but it no longer means "tell me only when a
/// message went out". A `--verify` skip used to be silent, and silence is
/// indistinguishable from nudge never having run — which is precisely the
/// failure the recency design exists to prevent, so it is the outcome that most
/// needs saying out loud. Each skip names its own reason, because they have
/// different remedies: "the banner is gone" means the session came back on its
/// own and there was nothing to do; "the pane changed" means you resumed it
/// yourself, and is also the sentence that explains an unexpected skip.
///
/// An `Err` is logged at warn. While the scheduler will retry it
/// ([`Job::will_retry`]) it stays silent, because a notification would
/// announce a job as finished when it is not. Once it will not be retried the
/// job is removed, and that is announced with the error's text: it may have
/// come after part of a message was typed, so it does not say nothing was sent.
///
/// `pane` is how to name the job's pane, from [`describe_pane`].
pub fn notification(
    job: &crate::job::Job,
    pane: &str,
    outcome: &anyhow::Result<crate::inject::InjectOutcome>,
) -> Option<String> {
    use crate::inject::InjectOutcome::{
        Sent, SkippedNoBanner, SkippedResumed, Unsubmitted, WrongPane,
    };
    use crate::target::WrongPane::{OtherConversation, RunsHerdr, RunsTmux, ServerRestarted};
    if !job.notify {
        return None;
    }
    match outcome {
        Ok(Sent(_)) => Some(format!("nudge fired for {pane}")),
        Ok(SkippedNoBanner) => Some(format!(
            "nudge skipped {pane}: the rate-limit banner is gone, so the session had \
             already resumed"
        )),
        Ok(SkippedResumed) => Some(format!(
            "nudge skipped {pane}: the pane changed since you scheduled, so you had \
             already resumed this session"
        )),
        Ok(Unsubmitted { .. }) => Some(format!(
            "nudge typed into {pane}, but the message is still in its input box, unsent"
        )),
        Ok(WrongPane(ServerRestarted)) => Some(format!(
            "nudge sent nothing to {pane}: that pane was on a tmux server that is not \
             the one running now (tmux restarted since you scheduled it, or the pane is \
             on another tmux socket)"
        )),
        Ok(WrongPane(OtherConversation)) => Some(format!(
            "nudge sent nothing to {pane}: that pane now holds a different conversation \
             than the one you scheduled for"
        )),
        Ok(WrongPane(RunsHerdr)) => Some(format!(
            "nudge sent nothing to {pane}: that tmux pane now runs herdr, so a nudge \
             there would land in whichever herdr pane has focus"
        )),
        Ok(WrongPane(RunsTmux)) => Some(format!(
            "nudge sent nothing to {pane}: that herdr pane now runs tmux, so a nudge \
             there would land in whichever tmux pane is active"
        )),
        Err(_) if job.will_retry() => None,
        // With -r the user expects a retry, so say why there was none.
        Err(e) if job.auto_retry && job.one_try() => Some(format!(
            "nudge gave up on {pane} without a retry, as it had no banner to check: {e:#}"
        )),
        Err(e) if job.auto_retry => Some(format!(
            "nudge gave up on {pane} with no retries left: {e:#}"
        )),
        Err(e) => Some(format!("nudge gave up on {pane}: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{drop_stale, log_outcome, notification};
    use crate::inject::InjectOutcome;
    use crate::job::{JobSpec, TargetSpec};
    use crate::queue::Queue;
    use crate::target::WrongPane;

    fn spec(notify: bool) -> JobSpec {
        JobSpec {
            target: TargetSpec::Tmux {
                pane: "p".into(),
                server: None,
            },
            messages: vec!["go".into()],
            send_delay_secs: 0.0,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
            verify_no_banner: false,
        }
    }

    fn job(notify: bool) -> crate::job::Job {
        spec(notify).into_job(1)
    }

    /// A sink for one thread's tracing output.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for Capture {
        type Writer = Capture;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `f` and return what it logged.
    ///
    /// `with_default` installs the subscriber for *this thread only*, so this
    /// cannot race the rest of the binary the way a global subscriber would --
    /// cargo runs these tests on parallel threads of one process.
    fn logs_from(f: impl FnOnce()) -> String {
        let cap = Capture::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, f);
        let bytes = cap.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// A queue whose parent directory is a regular file, so every persist
    /// fails. Stands in for the ENOSPC / read-only state dir of the real
    /// failure, and unlike a chmod'd directory it still fails as root.
    fn block_saving(parent: &std::path::Path) {
        std::fs::remove_dir_all(parent).unwrap();
        std::fs::write(parent, b"not a directory").unwrap();
    }

    #[test]
    fn a_stale_drop_that_could_not_be_persisted_is_not_logged_as_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let mut q = Queue::load(sub.join("q.json")).unwrap();
        let id = q.add(spec(false)).unwrap();
        block_saving(&sub);

        let out = logs_from(|| drop_stale(&mut q, &[id]));

        assert!(
            out.contains("failed"),
            "the failure must still be reported: {out}"
        );
        assert!(
            q.get(id).is_some(),
            "precondition: the job must still be live for this test to mean anything"
        );
        assert!(
            !out.contains("dropped stale job"),
            "remove failed and the job is STILL LIVE, so claiming the drop right after \
             warning that it failed sends anyone debugging reappearing stale jobs to \
             the wrong place: {out}"
        );
    }

    #[test]
    fn an_already_gone_stale_job_claims_no_drop() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();

        let out = logs_from(|| drop_stale(&mut q, &[9999]));

        assert!(
            !out.contains("dropped stale job"),
            "nothing was there to drop, so nothing was dropped: {out}"
        );
    }

    #[test]
    fn a_stale_drop_that_persisted_is_logged() {
        // The other half of the claim: this must stay noisy on success, or
        // "don't log the failure" would pass by logging nothing ever.
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        let id = q.add(spec(false)).unwrap();

        let out = logs_from(|| drop_stale(&mut q, &[id]));

        assert!(
            out.contains("dropped stale job"),
            "a real drop must still be reported: {out}"
        );
        assert!(q.get(id).is_none(), "and the job must actually be gone");
    }

    #[test]
    fn notifies_on_sent_when_opted_in_and_never_when_not() {
        assert!(
            notification(&job(true), "p", &Ok(InjectOutcome::Sent(1)))
                .unwrap()
                .contains("fired")
        );
        assert_eq!(
            notification(&job(false), "p", &Ok(InjectOutcome::Sent(1))),
            None
        );
        assert_eq!(
            notification(&job(false), "p", &Ok(InjectOutcome::SkippedResumed)),
            None,
            "--notify is still the opt-in: a skip does not create a notification \
             the user never asked for"
        );
    }

    /// A skip used to be silent. That is indistinguishable, from the outside,
    /// from nudge never having run -- which is the failure this whole design is
    /// built to avoid, so it is the one outcome that most needs saying out loud.
    #[test]
    fn a_skip_is_reported_and_names_which_skip_it_was() {
        let banner = notification(&job(true), "p", &Ok(InjectOutcome::SkippedNoBanner)).expect(
            "a skip must be visible: silence here reads exactly like the nudge \
             never firing at all",
        );
        let resumed = notification(&job(true), "p", &Ok(InjectOutcome::SkippedResumed)).expect(
            "a skip must be visible: silence here reads exactly like the nudge \
             never firing at all",
        );

        for msg in [&banner, &resumed] {
            assert!(msg.contains("skipped"), "must say it skipped: {msg}");
            assert!(msg.contains('p'), "must name the pane: {msg}");
        }
        assert_ne!(
            banner, resumed,
            "the two skips have different remedies -- 'the banner was gone' means the \
             session came back on its own, 'the pane changed' means you touched it -- \
             so a user staring at a nudge that did not fire must be able to tell them apart"
        );
        assert!(
            resumed.contains("resumed") || resumed.contains("changed"),
            "the resumed skip must say why: {resumed}"
        );
        assert!(
            banner.contains("banner"),
            "the no-banner skip must say why: {banner}"
        );
    }

    /// An error the scheduler will retry is not the end of the job, and
    /// announcing it would say the job is finished when it is not.
    #[test]
    fn a_failure_that_will_be_retried_is_not_notified() {
        let mut j = job(true);
        j.auto_retry = true;
        for left in [-1, 2] {
            j.retries_left = left;
            assert_eq!(
                notification(&j, "p", &Err(anyhow::anyhow!("boom"))),
                None,
                "retries_left {left}"
            );
        }
    }

    /// Without a retry the job is removed after an error, and a removal
    /// nobody was told of is a job that vanished.
    #[test]
    fn a_failure_that_will_not_be_retried_is_notified_with_its_error() {
        let err = || Err(anyhow::anyhow!("boom").context("herdr said no"));
        let plain = job(true);
        let said = notification(&plain, "p", &err()).expect("no notification");
        assert!(
            said.contains("gave up on p") && said.contains("herdr said no: boom"),
            "{said}"
        );
        let mut spent = job(true);
        spent.auto_retry = true;
        spent.retries_left = 0;
        let said = notification(&spent, "p", &err()).expect("no notification");
        // "no retries left", not "used up": `-r 0` asked for none and never
        // had any to use.
        assert!(said.contains("with no retries left"), "{said}");
        assert!(said.contains("herdr said no: boom"), "{said}");
        // A verify job whose pane showed no banner gets no retries, whatever
        // -r says, so its failure is the end of it, and the notification says
        // why the -r was not honoured.
        let mut once = job(true);
        once.auto_retry = true;
        once.retries_left = -1;
        once.verify = true;
        once.verify_no_banner = true;
        let said = notification(&once, "p", &err()).expect("no notification");
        assert!(said.contains("no banner to check"), "{said}");
        assert!(said.contains("herdr said no: boom"), "{said}");
    }

    #[test]
    fn a_failure_is_not_notified_without_notify() {
        assert_eq!(
            notification(&job(false), "p", &Err(anyhow::anyhow!("boom"))),
            None
        );
    }

    /// The log is where to look when a nudge did not arrive, and a job id alone
    /// cannot say where it went. Each line names the pane: where it sat when
    /// the job fired, and its id.
    #[test]
    fn the_log_names_the_pane_each_fire_went_to() {
        let sent = logs_from(|| {
            log_outcome(&job(false), "main:12.0 (%19)", &Ok(InjectOutcome::Sent(1)));
        });
        assert!(sent.contains("job 1"), "{sent}");
        assert!(sent.contains("Sent(1)"), "{sent}");
        assert!(sent.contains("main:12.0 (%19)"), "{sent}");

        let failed = logs_from(|| {
            log_outcome(
                &job(false),
                "main:12.0 (%19)",
                &Err(anyhow::anyhow!("boom")),
            );
        });
        assert!(failed.contains("main:12.0 (%19)"), "{failed}");
        assert!(failed.contains("boom"), "{failed}");
    }

    #[test]
    fn a_notification_names_the_pane_it_was_given() {
        let msg = notification(&job(true), "main:12.0 (%19)", &Ok(InjectOutcome::Sent(1)));
        assert!(msg.unwrap().contains("main:12.0 (%19)"));
    }

    /// A message left in the box is a nudge that did not go out. It is
    /// logged as a warning, and --notify says so.
    #[test]
    fn a_message_left_in_the_box_is_a_warning_and_a_notification() {
        let stuck = Ok(InjectOutcome::Unsubmitted { submitted: 0 });
        let log = logs_from(|| log_outcome(&job(false), "main:12.0 (%19)", &stuck));
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains("Unsubmitted"), "{log}");

        let msg = notification(&job(true), "main:12.0 (%19)", &stuck)
            .expect("a nudge that did not go out must not be silent");
        assert!(msg.contains("main:12.0 (%19)"), "{msg}");
        assert!(msg.contains("input box"), "{msg}");
    }

    /// A nudge refused because its pane is no longer the one scheduled for did
    /// not go out either, and is said out loud the same way.
    #[test]
    fn a_wrong_pane_is_a_warning() {
        let wrong = Ok(InjectOutcome::WrongPane(WrongPane::RunsHerdr));
        let log = logs_from(|| log_outcome(&job(false), "main:12.0 (%19)", &wrong));
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains("WrongPane(RunsHerdr)"), "{log}");
    }

    /// Each way of being the wrong pane says which it is, since each sends you
    /// to look somewhere different: for the pane the session is in now, or
    /// inside the client that took the pane over.
    #[test]
    fn a_wrong_pane_notification_says_which_way_it_is_wrong() {
        let job = job(true);
        let n = |w| notification(&job, "herdr:w7:pX", &Ok(InjectOutcome::WrongPane(w))).unwrap();
        assert!(
            n(WrongPane::ServerRestarted).contains("tmux server that is not the one running now")
        );
        assert_eq!(
            n(WrongPane::OtherConversation),
            "nudge sent nothing to herdr:w7:pX: that pane now holds a different conversation than the one you scheduled for"
        );
        assert_eq!(
            n(WrongPane::RunsHerdr),
            "nudge sent nothing to herdr:w7:pX: that tmux pane now runs herdr, so a nudge there would land in whichever herdr pane has focus"
        );
        assert_eq!(
            n(WrongPane::RunsTmux),
            "nudge sent nothing to herdr:w7:pX: that herdr pane now runs tmux, so a nudge there would land in whichever tmux pane is active"
        );
    }
}

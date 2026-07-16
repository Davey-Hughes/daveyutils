//! The resident scheduler: serves IPC and fires due jobs. Firing happens off
//! the queue lock so a slow tmux inject never blocks IPC.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::{Span, Zoned};

use crate::inject::run_injection;
use crate::paths::Paths;
use crate::queue::Queue;
use crate::scheduler::{apply_outcome, next_wake, plan, MAX_POLL};

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
    clock_ext: Option<String>,
    dur_ext: Option<String>,
    weekly_ext: Option<String>,
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
    clock_ext: Option<String>,
    dur_ext: Option<String>,
    weekly_ext: Option<String>,
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
    std::thread::spawn(move || on_serve_exit(crate::ipc::server::serve(&socket, q_ipc)));

    loop {
        let now = Zoned::now();

        // 1. Snapshot due jobs and stale ids under the lock.
        let plan_now = {
            let q = queue.lock().unwrap_or_else(|e| e.into_inner());
            plan(q.all(), &now, &grace)
        };

        // 2. Drop stale jobs under the lock.
        if !plan_now.drop_stale.is_empty() {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            drop_stale(&mut q, &plan_now.drop_stale);
        }

        // 3. Fire each due job WITHOUT the lock, applying results under it.
        for job in &plan_now.fire {
            let outcome = run_injection(
                &*job.target.connect(),
                job,
                &now,
                clock_ext.as_deref(),
                dur_ext.as_deref(),
                weekly_ext.as_deref(),
            );
            match &outcome {
                // Not "fired": two of the three outcomes deliberately send
                // nothing, and a log that calls a skip a fire is how you spend
                // an evening looking for an injection that never happened.
                Ok(o) => tracing::info!("nudge: job {} -> {:?}", job.id, o),
                Err(e) => tracing::warn!("nudge: job {} failed: {e}", job.id),
            }
            if let Some(body) = notification(job, &outcome) {
                crate::notify::send(&body);
            }
            let retry_secs = job.settle_secs.max(MIN_RETRY_SECS);
            let retry_at = now
                .checked_add(Span::new().milliseconds((retry_secs * 1000.0) as i64))
                .map(|z| z.timestamp())
                .unwrap_or(now.timestamp());
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = apply_outcome(&mut q, job, &outcome, retry_at) {
                tracing::warn!("nudge: persisting job {} outcome failed: {e}", job.id);
            }
        }

        // 4. Sleep until the next job is due (capped).
        let wake = {
            let q = queue.lock().unwrap_or_else(|e| e.into_inner());
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

/// A short human-readable description of a job's target, for notification text.
fn describe_pane(job: &crate::job::Job) -> String {
    match &job.target {
        crate::job::TargetSpec::Tmux { pane } => pane.clone(),
    }
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
/// An `Err` stays silent: it is logged at warn, it may still be retried, and a
/// notification would announce a job as finished when it is not.
pub fn notification(
    job: &crate::job::Job,
    outcome: &anyhow::Result<crate::inject::InjectOutcome>,
) -> Option<String> {
    use crate::inject::InjectOutcome::*;
    if !job.notify {
        return None;
    }
    let pane = describe_pane(job);
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
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{drop_stale, notification};
    use crate::inject::InjectOutcome;
    use crate::job::{JobSpec, TargetSpec};
    use crate::queue::Queue;

    fn spec(notify: bool) -> JobSpec {
        JobSpec {
            target: TargetSpec::Tmux { pane: "p".into() },
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
        assert!(notification(&job(true), &Ok(InjectOutcome::Sent(1)))
            .unwrap()
            .contains("fired"));
        assert_eq!(notification(&job(false), &Ok(InjectOutcome::Sent(1))), None);
        assert_eq!(
            notification(&job(false), &Ok(InjectOutcome::SkippedResumed)),
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
        let banner = notification(&job(true), &Ok(InjectOutcome::SkippedNoBanner)).expect(
            "a skip must be visible: silence here reads exactly like the nudge \
             never firing at all",
        );
        let resumed = notification(&job(true), &Ok(InjectOutcome::SkippedResumed)).expect(
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

    /// An error is not a skip. It is already logged at warn, it may still be
    /// retried, and dressing it up as a notification would tell the user the job
    /// is finished when it is not.
    #[test]
    fn a_failure_is_not_notified() {
        assert_eq!(
            notification(&job(true), &Err(anyhow::anyhow!("boom"))),
            None
        );
    }
}

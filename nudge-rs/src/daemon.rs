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

/// Run the daemon forever: IPC server thread + scheduler loop.
///
/// Call [`init_tracing`] first if you want the daemon's logs.
pub fn run(
    paths: &Paths,
    clock_ext: Option<String>,
    dur_ext: Option<String>,
    grace: Span,
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
    std::thread::spawn(move || {
        if let Err(e) = crate::ipc::server::serve(&socket, q_ipc) {
            tracing::error!("nudge ipc server exited: {e}");
        }
    });

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
            for id in &plan_now.drop_stale {
                if let Err(e) = q.remove(*id) {
                    tracing::warn!("nudge: removing stale job {id} failed: {e}");
                }
                tracing::info!("nudge: dropped stale job {id}");
            }
        }

        // 3. Fire each due job WITHOUT the lock, applying results under it.
        for job in &plan_now.fire {
            let outcome = run_injection(
                &*job.target.connect(),
                job,
                &now,
                clock_ext.as_deref(),
                dur_ext.as_deref(),
            );
            match &outcome {
                Ok(o) => tracing::info!("nudge: fired job {} -> {:?}", job.id, o),
                Err(e) => tracing::warn!("nudge: job {} failed: {e}", job.id),
            }
            if should_notify(job, &outcome) {
                crate::notify::send(&format!("nudge fired for {}", describe_pane(job)));
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

/// A short human-readable description of a job's target, for notification text.
fn describe_pane(job: &crate::job::Job) -> String {
    match &job.target {
        crate::job::TargetSpec::Tmux { pane } => pane.clone(),
    }
}

/// Whether firing `job` with this `outcome` warrants a desktop notification:
/// only when the user asked for one AND a message was actually sent.
pub fn should_notify(
    job: &crate::job::Job,
    outcome: &anyhow::Result<crate::inject::InjectOutcome>,
) -> bool {
    job.notify && matches!(outcome, Ok(crate::inject::InjectOutcome::Sent(_)))
}

#[cfg(test)]
mod tests {
    use super::should_notify;
    use crate::inject::InjectOutcome;
    use crate::job::{JobSpec, TargetSpec};

    fn job(notify: bool) -> crate::job::Job {
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
        }
        .into_job(1)
    }

    #[test]
    fn notifies_only_on_sent_when_opted_in() {
        assert!(should_notify(&job(true), &Ok(InjectOutcome::Sent(1))));
        assert!(!should_notify(
            &job(true),
            &Ok(InjectOutcome::SkippedVerify)
        ));
        assert!(!should_notify(&job(false), &Ok(InjectOutcome::Sent(1))));
        assert!(!should_notify(&job(true), &Err(anyhow::anyhow!("boom"))));
    }
}

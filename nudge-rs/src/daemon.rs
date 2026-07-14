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

/// Run the daemon forever: IPC server thread + scheduler loop.
///
/// Call [`init_tracing`] first if you want the daemon's logs.
pub fn run(
    paths: &Paths,
    clock_ext: Option<String>,
    dur_ext: Option<String>,
    grace: Span,
) -> std::io::Result<()> {
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
                Ok(o) => {
                    tracing::info!("nudge: fired job {} -> {:?}", job.id, o);
                    if job.notify {
                        crate::notify::send(&format!("nudge fired for {}", describe_pane(job)));
                    }
                }
                Err(e) => tracing::warn!("nudge: job {} failed: {e}", job.id),
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

//! Pure scheduler decisions: which due jobs to fire vs drop, what to do after
//! firing, and how long to sleep. `daemon::run` orchestrates the effects.

use std::time::Duration;

use jiff::{Span, Zoned};

use crate::inject::InjectOutcome;
use crate::job::Job;
use crate::queue::Queue;

/// Longest the scheduler sleeps before re-checking the queue, so a job added
/// via IPC while it sleeps still fires within this bound.
pub const MAX_POLL: Duration = Duration::from_secs(30);

/// The jobs a single scheduler pass should act on at a given `now`.
pub struct Plan {
    pub fire: Vec<Job>,
    pub drop_stale: Vec<u64>,
}

/// Partition jobs at `now`: due jobs within `grace` are fired; due jobs older
/// than `grace` are dropped (stale catch-up); future jobs are left alone.
pub fn plan(jobs: &[Job], now: &Zoned, grace: &Span) -> Plan {
    let now_ts = now.timestamp();
    let cutoff = now
        .checked_sub(*grace)
        .map(|z| z.timestamp())
        .unwrap_or(now_ts);
    let mut fire = Vec::new();
    let mut drop_stale = Vec::new();
    for j in jobs {
        if j.fire_at > now_ts {
            continue; // not due yet
        }
        if j.fire_at < cutoff {
            drop_stale.push(j.id);
        } else {
            fire.push(j.clone());
        }
    }
    Plan { fire, drop_stale }
}

/// Apply the result of firing `job`: reschedule for a retry, or remove.
pub fn apply_outcome(
    queue: &mut Queue,
    job: &Job,
    outcome: &anyhow::Result<InjectOutcome>,
    retry_at: jiff::Timestamp,
) -> std::io::Result<()> {
    match outcome {
        // A send that landed with retries still budgeted, and a send that
        // *failed* while the user asked to retry, both reschedule -- only the
        // reason differs. Letting Err fall through to the catch-all would
        // delete the job, so one transient tmux error (the server restarting,
        // the pane briefly unavailable) turned `-a -r -1` into zero retries.
        //
        // Ok(SkippedVerify) is deliberately excluded: the pane no longer shows
        // a banner, so the job is done, not failed.
        //
        // Note the consequence of Err joining this arm: `-a -r -1` against a
        // pane that is permanently gone now retries forever, where before it
        // deleted the job on the first error. That is accepted, not overlooked.
        // It is literally what `-1` asks for; it is symmetric with the Sent
        // path, which has always retried a live pane forever on the same flag;
        // the retry is floored at MIN_RETRY_SECS so it cannot become a spin;
        // and `--cancel` now reaches the daemon, so the user has a way out.
        // Silently dropping the job on one transient tmux hiccup -- the server
        // restarting, the pane briefly unavailable -- was the worse failure,
        // because nothing said it had happened.
        Ok(InjectOutcome::Sent(_)) | Err(_) if job.auto_retry && job.retries_left != 0 => {
            let left = if job.retries_left > 0 {
                job.retries_left - 1
            } else {
                job.retries_left // -1 stays -1 (infinite)
            };
            queue.reschedule(job.id, retry_at, left).map(|_| ())
        }
        _ => queue.remove(job.id).map(|_| ()),
    }
}

/// How long to sleep before the next pass, capped at `max`.
pub fn next_wake(jobs: &[Job], now: &Zoned, max: Duration) -> Duration {
    let now_ts = now.timestamp();
    match jobs.iter().map(|j| j.fire_at).min() {
        None => max,
        Some(ts) if ts <= now_ts => Duration::ZERO,
        Some(ts) => {
            let until: Duration = now_ts.duration_until(ts).unsigned_abs();
            until.min(max)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::InjectOutcome;
    use crate::job::{Job, JobSpec, TargetSpec};
    use crate::queue::Queue;
    use jiff::{civil::date, tz::TimeZone, ToSpan};

    fn now() -> jiff::Zoned {
        date(2026, 7, 13)
            .at(12, 0, 0, 0)
            .to_zoned(TimeZone::fixed(jiff::tz::Offset::UTC))
            .unwrap()
    }

    // Build a Job at `fire_at` (an RFC3339 string) with the given retry config.
    fn job(id: u64, fire_at: &str, auto_retry: bool, retries_left: i64) -> Job {
        JobSpec {
            target: TargetSpec::Tmux { pane: "p".into() },
            messages: vec!["go".into()],
            send_delay_secs: 0.0,
            fire_at: fire_at.parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry,
            retries_left,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        }
        .into_job(id)
    }

    #[test]
    fn plan_fires_due_skips_future_drops_stale() {
        let grace = 6.hours();
        let jobs = vec![
            job(1, "2026-07-13T11:59:00Z", false, 0), // due (1 min ago)
            job(2, "2026-07-13T13:00:00Z", false, 0), // future
            job(3, "2026-07-13T02:00:00Z", false, 0), // 10h ago -> stale (> 6h)
        ];
        let p = plan(&jobs, &now(), &grace);
        assert_eq!(p.fire.iter().map(|j| j.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(p.drop_stale, vec![3]);
    }

    #[test]
    fn next_wake_variants() {
        assert_eq!(next_wake(&[], &now(), MAX_POLL), MAX_POLL);
        // overdue -> zero
        assert_eq!(
            next_wake(
                &[job(1, "2026-07-13T11:00:00Z", false, 0)],
                &now(),
                MAX_POLL
            ),
            std::time::Duration::ZERO
        );
        // 10s in the future -> ~10s (< 30s cap)
        let w = next_wake(
            &[job(1, "2026-07-13T12:00:10Z", false, 0)],
            &now(),
            MAX_POLL,
        );
        assert!(w <= std::time::Duration::from_secs(10) && w >= std::time::Duration::from_secs(9));
        // far future -> capped at MAX_POLL
        let w2 = next_wake(
            &[job(1, "2026-07-13T18:00:00Z", false, 0)],
            &now(),
            MAX_POLL,
        );
        assert_eq!(w2, MAX_POLL);
    }

    fn q_with(job: Job) -> (tempfile::TempDir, Queue) {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        // add via JobSpec then overwrite fields to match `job`
        q.add(JobSpec {
            target: job.target.clone(),
            messages: job.messages.clone(),
            send_delay_secs: job.send_delay_secs,
            fire_at: job.fire_at,
            notify: job.notify,
            verify: job.verify,
            auto_retry: job.auto_retry,
            retries_left: job.retries_left,
            settle_secs: job.settle_secs,
            verify_fingerprint: job.verify_fingerprint.clone(),
            verify_dims: job.verify_dims,
        })
        .unwrap();
        (dir, q)
    }

    #[test]
    fn apply_sent_without_retry_removes() {
        let j = job(1, "2026-07-13T11:59:00Z", false, 0);
        let (_d, mut q) = q_with(j.clone());
        let cur = q.get(1).unwrap().clone();
        apply_outcome(&mut q, &cur, &Ok(InjectOutcome::Sent(1)), now().timestamp()).unwrap();
        assert!(q.get(1).is_none());
    }

    #[test]
    fn apply_sent_with_retry_reschedules_and_decrements() {
        let j = job(1, "2026-07-13T11:59:00Z", true, 2);
        let (_d, mut q) = q_with(j);
        let retry_at: jiff::Timestamp = "2026-07-13T12:05:00Z".parse().unwrap();
        let cur = q.get(1).unwrap().clone();
        apply_outcome(&mut q, &cur, &Ok(InjectOutcome::Sent(1)), retry_at).unwrap();
        let job = q.get(1).unwrap();
        assert_eq!(job.retries_left, 1);
        assert_eq!(job.fire_at, retry_at);
    }

    #[test]
    fn apply_infinite_retry_stays_negative_one() {
        let j = job(1, "2026-07-13T11:59:00Z", true, -1);
        let (_d, mut q) = q_with(j);
        let cur = q.get(1).unwrap().clone();
        apply_outcome(&mut q, &cur, &Ok(InjectOutcome::Sent(1)), now().timestamp()).unwrap();
        assert_eq!(q.get(1).unwrap().retries_left, -1);
    }

    /// A transient injection failure: the tmux server was restarting, or the
    /// pane was momentarily unavailable.
    fn inject_err() -> anyhow::Result<InjectOutcome> {
        Err(anyhow::anyhow!("tmux send-keys: can't find pane: p"))
    }

    #[test]
    fn apply_err_with_retries_reschedules_instead_of_deleting() {
        let j = job(1, "2026-07-13T11:59:00Z", true, 2);
        let (_d, mut q) = q_with(j);
        let retry_at: jiff::Timestamp = "2026-07-13T12:05:00Z".parse().unwrap();
        let cur = q.get(1).unwrap().clone();
        apply_outcome(&mut q, &cur, &inject_err(), retry_at).unwrap();
        let job = q
            .get(1)
            .expect("a failed injection must not delete a job the user asked to retry");
        assert_eq!(job.retries_left, 1);
        assert_eq!(job.fire_at, retry_at);
    }

    #[test]
    fn apply_err_with_infinite_retries_keeps_retrying() {
        // The README's own `nudge -p bot:0.1 -a -r -1` -- "retry forever" must
        // not mean "deleted after the first transient tmux blip".
        let j = job(1, "2026-07-13T11:59:00Z", true, -1);
        let (_d, mut q) = q_with(j);
        let cur = q.get(1).unwrap().clone();
        apply_outcome(&mut q, &cur, &inject_err(), now().timestamp()).unwrap();
        assert_eq!(
            q.get(1)
                .expect("infinite retries must survive a failure")
                .retries_left,
            -1
        );
    }

    #[test]
    fn apply_err_with_retries_exhausted_removes() {
        let j = job(1, "2026-07-13T11:59:00Z", true, 0);
        let (_d, mut q) = q_with(j);
        let cur = q.get(1).unwrap().clone();
        apply_outcome(&mut q, &cur, &inject_err(), now().timestamp()).unwrap();
        assert!(q.get(1).is_none(), "a job out of retries is done");
    }

    #[test]
    fn apply_err_without_auto_retry_removes() {
        let j = job(1, "2026-07-13T11:59:00Z", false, 0);
        let (_d, mut q) = q_with(j);
        let cur = q.get(1).unwrap().clone();
        apply_outcome(&mut q, &cur, &inject_err(), now().timestamp()).unwrap();
        assert!(q.get(1).is_none(), "no retry was asked for");
    }

    #[test]
    fn apply_skipped_verify_removes_even_with_retry() {
        let j = job(1, "2026-07-13T11:59:00Z", true, 2);
        let (_d, mut q) = q_with(j);
        let cur = q.get(1).unwrap().clone();
        apply_outcome(
            &mut q,
            &cur,
            &Ok(InjectOutcome::SkippedVerify),
            now().timestamp(),
        )
        .unwrap();
        assert!(q.get(1).is_none());
    }
}

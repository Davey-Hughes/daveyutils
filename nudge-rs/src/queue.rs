//! Load and atomically persist the job list as JSON.

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::job::{Job, JobSpec, Progress};

/// What [`Queue::replace`] did.
#[derive(Debug, PartialEq)]
pub enum ReplaceOutcome {
    /// The job is swapped for its replacement, which has this id.
    Swapped(u64),
    /// No job has that id: it fired for the last time, or was cancelled or
    /// dropped, since the edit read it.
    Gone,
    /// The job has fired since the edit read it, and this is the job now.
    /// Nothing was swapped.
    Changed(Job),
    /// The daemon is firing the job right now. Nothing was swapped.
    Firing,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct State {
    next_id: u64,
    jobs: Vec<Job>,
}

pub struct Queue {
    path: PathBuf,
    state: State,
    /// The jobs the daemon has taken to fire and not yet settled. A fire runs
    /// without the queue lock, and a job reads as it was until its outcome is
    /// applied, so only this tells an edit that the job is about to change.
    /// Not persisted: a daemon that stops mid-fire leaves no fire running.
    firing: HashSet<u64>,
    /// Saves attempted since `load`, counted only to drive [`Queue::fail_nth_save`].
    #[cfg(test)]
    saves: std::cell::Cell<u64>,
    /// Which save to refuse, if any. See [`Queue::fail_nth_save`].
    #[cfg(test)]
    fail_save_at: Option<u64>,
}

impl Queue {
    /// Load the queue from `path`, or start empty if the file is absent.
    pub fn load(path: PathBuf) -> std::io::Result<Queue> {
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e),
        };
        Ok(Queue {
            path,
            state,
            firing: HashSet::new(),
            #[cfg(test)]
            saves: std::cell::Cell::new(0),
            #[cfg(test)]
            fail_save_at: None,
        })
    }

    /// Test-only: refuse the `n`th save this queue attempts (1-based, counted
    /// from `load`).
    ///
    /// Making the state dir unwritable can only fail *every* save, and so only
    /// ever reaches the first one — which is the one failure an operation built
    /// from two commits survives cleanly, because it errors before commit #1
    /// lands. The interesting state is the other one: commit #1 landed and
    /// commit #2 could not. Only a seam that picks the save can reach it.
    #[cfg(test)]
    fn fail_nth_save(&mut self, n: u64) {
        self.fail_save_at = Some(n);
    }

    pub fn all(&self) -> &[Job] {
        &self.state.jobs
    }

    pub fn get(&self, id: u64) -> Option<&Job> {
        self.state.jobs.iter().find(|j| j.id == id)
    }

    /// Persist `next`, and only then adopt it as the live state.
    ///
    /// Every mutation goes through here so that an `Err` always means *nothing
    /// changed*. Mutating first and persisting second let the two disagree: a
    /// failed `add` still fired a job the CLI had just reported as rejected,
    /// and a failed `remove` dropped from memory a job that queue.json still
    /// held, so a restart within the grace window fired it a second time.
    fn commit(&mut self, next: State) -> std::io::Result<()> {
        self.save(&next)?;
        self.state = next;
        Ok(())
    }

    /// Assign a monotonic id, append, persist, and return the new id.
    pub fn add(&mut self, spec: JobSpec) -> std::io::Result<u64> {
        let mut next = self.state.clone();
        next.next_id += 1;
        let id = next.next_id;
        next.jobs.push(spec.into_job(id));
        self.commit(next)?;
        Ok(id)
    }

    /// Remove the job with `id`. Returns whether one was removed.
    pub fn remove(&mut self, id: u64) -> std::io::Result<bool> {
        let mut next = self.state.clone();
        next.jobs.retain(|j| j.id != id);
        if next.jobs.len() == self.state.jobs.len() {
            return Ok(false); // no such job: nothing to persist
        }
        self.commit(next)?;
        Ok(true)
    }

    /// Swap job `id` for `spec` in one transaction, if the job is still as the
    /// edit read it: `seen` is its [`Progress`] then. Anything else changes
    /// nothing, and the outcome says why.
    ///
    /// One commit, so an edit can never publish a state where both the original
    /// and its replacement are live. Doing it as a separate `add` + `remove`
    /// (which is what the CLI used to do over two round-trips) leaves exactly
    /// that state in between, and anything that stops the second step — a daemon
    /// restart, a stolen socket, a Ctrl-C — makes the message fire twice.
    ///
    /// The `seen` check exists because an edit carries over the time and the
    /// retries it read. Swapped in after the job fired, it would restore a fire
    /// time that has passed and the retry just used, and fire again at once.
    pub fn replace(
        &mut self,
        id: u64,
        seen: Progress,
        spec: JobSpec,
    ) -> std::io::Result<ReplaceOutcome> {
        let Some(job) = self.get(id) else {
            // The original is gone (fired or cancelled since the caller listed
            // it). Scheduling the replacement now would resurrect a job the user
            // no longer has: all-or-nothing means nothing.
            return Ok(ReplaceOutcome::Gone);
        };
        // Firing first: mid-fire the job still reads as `seen` does, and
        // will not once the fire settles.
        if self.is_firing(id) {
            return Ok(ReplaceOutcome::Firing);
        }
        if job.progress() != seen {
            return Ok(ReplaceOutcome::Changed(job.clone()));
        }
        let mut next = self.state.clone();
        next.jobs.retain(|j| j.id != id);
        next.next_id += 1;
        let new_id = next.next_id;
        next.jobs.push(spec.into_job(new_id));
        self.commit(next)?;
        Ok(ReplaceOutcome::Swapped(new_id))
    }

    /// Take job `id` to fire: mark it as being fired, until
    /// [`Queue::stop_firing`], and return it as it is now. While it is
    /// marked, [`Queue::replace`] refuses to swap it.
    ///
    /// `None` if it is no longer queued: cancelled, or swapped for an edit,
    /// since the daemon planned to fire it. Then there is nothing to fire.
    pub fn start_firing(&mut self, id: u64) -> Option<Job> {
        let job = self.get(id)?.clone();
        self.firing.insert(id);
        Some(job)
    }

    /// The fire of job `id` is over, and its outcome applied or about to be.
    pub fn stop_firing(&mut self, id: u64) {
        self.firing.remove(&id);
    }

    /// Whether job `id` is marked as being fired.
    pub fn is_firing(&self, id: u64) -> bool {
        self.firing.contains(&id)
    }

    /// Update a job's fire time and remaining retries, dropping any `--verify`
    /// snapshot; persist. Returns whether a job with `id` existed.
    ///
    /// The snapshot goes because this is only ever called after a fire, and a
    /// fire is nudge typing into the pane. The snapshot records the pane as the
    /// *user* left it, so our own injection is precisely what invalidates it:
    /// compared against it, the pane has always "changed", and the recency gate
    /// would read that as the user resuming and skip every retry. `-a -r N -v`
    /// could then never fire more than once, however limited the session still
    /// was. With no snapshot the gate is `Unknown` and falls open to the banner
    /// check -- which is all `-a` ever did here, and is the right question for a
    /// retry to ask: is the banner *still* up?
    ///
    /// Re-snapshotting instead would be worse than useless: the daemon would be
    /// fingerprinting a pane it just typed into and that is very likely
    /// mid-response.
    pub fn reschedule(
        &mut self,
        id: u64,
        fire_at: jiff::Timestamp,
        retries_left: i64,
    ) -> std::io::Result<bool> {
        let mut next = self.state.clone();
        let Some(job) = next.jobs.iter_mut().find(|j| j.id == id) else {
            return Ok(false); // no such job: nothing to persist
        };
        job.fire_at = fire_at;
        job.retries_left = retries_left;
        job.verify_fingerprint = None;
        job.verify_dims = None;
        self.commit(next)?;
        Ok(true)
    }

    /// The temp file `save` writes before renaming into place. Process-unique:
    /// two daemons sharing one temp name could truncate each other mid-write and
    /// publish a corrupt queue.
    fn temp_path(&self) -> std::path::PathBuf {
        self.path
            .with_extension(format!("json.{}.tmp", std::process::id()))
    }

    /// Write `state` to a sibling temp file then rename, so a crash never
    /// leaves a half-written queue. Takes the state explicitly because it
    /// persists the *candidate*, before `commit` adopts it.
    fn save(&self, state: &State) -> std::io::Result<()> {
        #[cfg(test)]
        {
            let n = self.saves.get() + 1;
            self.saves.set(n);
            if self.fail_save_at == Some(n) {
                return Err(std::io::Error::other(format!(
                    "save #{n} refused by the test seam"
                )));
            }
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.temp_path();
        let written = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(state)?)?;
            f.sync_all()
        })();
        // A successful rename consumes the temp; every other path orphans it.
        // Reap it here rather than sweeping the directory later: this is the
        // call that created the file and it is named for this process, so
        // removing it by exact path cannot touch a *live* daemon's temp -- a
        // sweep matching `queue.json.*.tmp` could, and truncating another
        // writer's in-flight temp is the corruption the process-unique name
        // exists to prevent. Uniqueness is untouched; this only adds cleanup.
        let result = written.and_then(|()| std::fs::rename(&tmp, &self.path));
        if result.is_err() {
            // Best-effort: if this fails too, the orphan is exactly the single
            // file this pid would have reused anyway.
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::TargetSpec;

    fn spec() -> JobSpec {
        JobSpec {
            target: TargetSpec::Tmux {
                pane: "bot:0.1".into(),
                server: None,
            },
            messages: vec!["go".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 2,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
            verify_no_banner: false,
        }
    }

    #[test]
    fn add_assigns_monotonic_ids_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");

        let mut q = Queue::load(path.clone()).unwrap();
        let id1 = q.add(spec()).unwrap();
        let id2 = q.add(spec()).unwrap();
        assert_eq!((id1, id2), (1, 2));

        // Reloading sees both jobs and keeps counting from 2.
        let mut q2 = Queue::load(path.clone()).unwrap();
        assert_eq!(q2.all().len(), 2);
        assert_eq!(q2.add(spec()).unwrap(), 3);
    }

    #[test]
    fn tmux_and_herdr_jobs_save_and_reload_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut herdr = spec();
        herdr.target = TargetSpec::Herdr {
            pane: "w7:pX".into(),
            socket: "/h/.config/herdr/herdr.sock".into(),
            agent_session: Some("cfebd961".into()),
        };

        let mut q = Queue::load(path.clone()).unwrap();
        q.add(spec()).unwrap();
        q.add(herdr).unwrap();

        let reloaded = Queue::load(path).unwrap();
        assert_eq!(reloaded.all(), q.all());
        assert_eq!(reloaded.all().len(), 2);
    }

    #[test]
    fn remove_reports_hit_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        let id = q.add(spec()).unwrap();
        assert!(q.remove(id).unwrap());
        assert!(!q.remove(id).unwrap());
        assert!(q.get(id).is_none());
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::load(dir.path().join("nope.json")).unwrap();
        assert_eq!(q.all(), []);
    }

    #[test]
    fn reschedule_updates_fire_time_and_retries_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut q = Queue::load(path.clone()).unwrap();
        let id = q.add(spec()).unwrap();

        let new_ts: jiff::Timestamp = "2026-07-13T16:30:00Z".parse().unwrap();
        assert!(q.reschedule(id, new_ts, 1).unwrap());
        assert!(!q.reschedule(9999, new_ts, 1).unwrap()); // missing id

        // Persisted: reload and confirm.
        let q2 = Queue::load(path).unwrap();
        let job = q2.get(id).unwrap();
        assert_eq!(job.fire_at, new_ts);
        assert_eq!(job.retries_left, 1);
    }

    /// A queue whose parent directory is a regular file, so `save`'s
    /// `create_dir_all` — and thus every persist — fails. Stands in for the
    /// ENOSPC / read-only state dir of the real failure, and unlike a chmod'd
    /// directory it still fails when the suite runs as root.
    fn block_saving(parent: &std::path::Path) {
        if parent.exists() {
            std::fs::remove_dir_all(parent).unwrap();
        }
        std::fs::write(parent, b"not a directory").unwrap();
    }

    #[test]
    fn a_failed_add_leaves_no_live_job() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let mut q = Queue::load(sub.join("q.json")).unwrap();
        block_saving(&sub);

        assert!(
            q.add(spec()).is_err(),
            "save must fail for this test to mean anything"
        );
        // The CLI printed "daemon rejected the job" and exited 1. The daemon
        // must not then fire the job it just reported as rejected.
        assert!(
            q.all().is_empty(),
            "a rejected job must not stay live in the daemon's memory"
        );
    }

    #[test]
    fn a_failed_add_consumes_no_id() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let mut q = Queue::load(sub.join("q.json")).unwrap();
        block_saving(&sub);
        assert!(q.add(spec()).is_err());

        // Unblock: the next add is the first job this queue ever accepted.
        std::fs::remove_file(&sub).unwrap();
        assert_eq!(
            q.add(spec()).unwrap(),
            1,
            "a failed add must not burn an id"
        );
    }

    #[test]
    fn a_failed_remove_keeps_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let mut q = Queue::load(sub.join("q.json")).unwrap();
        let id = q.add(spec()).unwrap();
        block_saving(&sub);

        assert!(q.remove(id).is_err());
        // queue.json on disk still holds this job, so memory must agree --
        // otherwise a restart within the grace window re-fires it.
        assert!(
            q.get(id).is_some(),
            "a remove that could not be persisted must not drop the job from memory"
        );
    }

    #[test]
    fn a_failed_replace_leaves_exactly_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let mut q = Queue::load(sub.join("q.json")).unwrap();
        let id = q.add(spec()).unwrap();
        let seen = q.get(id).unwrap().progress();
        block_saving(&sub);

        assert!(
            q.replace(id, seen, spec()).is_err(),
            "save must fail for this test to mean anything"
        );
        // Note the modest claim: this covers the case where the FIRST save
        // fails, so it pins rollback, not atomicity. `block_saving` cannot fail
        // any later save, and an add-then-remove replace passes this test — it
        // errors on its own first commit, before either leg lands. The shape
        // that admits the double-fire is "commit #1 landed, commit #2's save
        // failed", and it takes the save seam to reach:
        // `a_replace_leaves_exactly_one_job_whichever_save_fails` is that test.
        assert_eq!(q.all().len(), 1, "a failed replace must not add a job");
        assert_eq!(q.all()[0].id, id, "and must not drop the original");
    }

    /// `replace` must be ONE commit, and this is what pins it.
    ///
    /// The double-fire I10 exists to eliminate is a live replacement alongside
    /// the original. A `replace` built from `add` + `remove` publishes exactly
    /// that between its two commits, so anything that stops the second — here a
    /// failed save, in the field a daemon restart or a full disk — leaves the
    /// message scheduled twice, at the old time and the new. It is the same bug
    /// the CLI used to have over two round-trips, just moved into the queue.
    ///
    /// So walk the save that fails across `replace`'s whole span. One commit
    /// means one save, so every failure point either leaves the original (the
    /// swap never happened) or the replacement (it did) — and never both,
    /// whatever the outcome. A two-commit `replace` cannot satisfy that: it has
    /// a failure point where both are live.
    #[test]
    fn a_replace_leaves_exactly_one_job_whichever_save_fails() {
        for fail_at in [2, 3] {
            let dir = tempfile::tempdir().unwrap();
            let mut q = Queue::load(dir.path().join("q.json")).unwrap();
            let id = q.add(spec()).unwrap(); // save #1
            let seen = q.get(id).unwrap().progress();
            q.fail_nth_save(fail_at);

            let outcome = q.replace(id, seen, spec());

            assert_eq!(
                q.all().len(),
                1,
                "fail_at={fail_at}: a replace left {} jobs live. Exactly one may \
                 survive: the original AND its replacement both pending is the \
                 double-fire, and neither loses the user's nudge outright",
                q.all().len()
            );
            match outcome {
                // The swap committed: only the replacement is live.
                Ok(ReplaceOutcome::Swapped(new_id)) => assert_eq!(
                    q.all()[0].id,
                    new_id,
                    "fail_at={fail_at}: a replace that reported success must leave \
                     its replacement, not the original"
                ),
                Ok(other) => {
                    panic!("fail_at={fail_at}: the original was there to replace, got {other:?}")
                }
                // It did not: only the original is, untouched.
                Err(_) => assert_eq!(
                    q.all()[0].id,
                    id,
                    "fail_at={fail_at}: a replace that could not be persisted must \
                     leave the original"
                ),
            }
        }
    }

    /// A job that retries twice, as `-a -r 2` schedules one.
    fn retrying() -> JobSpec {
        JobSpec {
            auto_retry: true,
            retries_left: 2,
            ..spec()
        }
    }

    /// The job fires while its edit form is open: the daemon moves it to the
    /// retry and takes a retry off, and it keeps its id. The edit, saved
    /// after, was made against the job as it was before the fire.
    #[test]
    fn an_edit_of_a_job_that_fired_since_it_was_read_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        let id = q.add(retrying()).unwrap();
        let seen = q.get(id).unwrap().progress();
        let retry_at = "2026-07-13T15:00:05Z".parse().unwrap();
        assert!(q.reschedule(id, retry_at, 1).unwrap());
        let fired = q.get(id).unwrap().clone();

        // The edit keeps the time and the retries as the form read them.
        let outcome = q.replace(id, seen, retrying()).unwrap();

        assert_eq!(
            outcome,
            ReplaceOutcome::Changed(fired.clone()),
            "an edit of an older copy must be refused, with the job as it is now"
        );
        assert_eq!(
            q.all(),
            [fired],
            "the job must stay as the fire left it. Swapped, it would be due \
             at once, at its old time, with the retry it just used back"
        );
    }

    /// The daemon fires without the queue lock, so an edit can arrive
    /// mid-fire, while the job still reads as it was. Swapped then, the fire's
    /// outcome finds no job to settle, and the replacement, at the old time,
    /// fires again at once.
    #[test]
    fn an_edit_of_a_job_the_daemon_is_firing_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        let id = q.add(retrying()).unwrap();
        let job = q.get(id).unwrap().clone();
        assert!(q.start_firing(id).is_some());

        assert_eq!(
            q.replace(id, job.progress(), retrying()).unwrap(),
            ReplaceOutcome::Firing
        );
        assert_eq!(
            q.all(),
            std::slice::from_ref(&job),
            "the job must be untouched"
        );

        q.stop_firing(id);
        assert_eq!(
            q.replace(id, job.progress(), retrying()).unwrap(),
            ReplaceOutcome::Swapped(2),
            "once the fire is settled, an edit of the job as it is goes through"
        );
    }

    /// A job cancelled mid-fire is gone, not firing: there is nothing left to
    /// edit, and the fire in progress settles nothing.
    #[test]
    fn a_job_cancelled_while_it_fires_is_gone_to_an_edit() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        let id = q.add(retrying()).unwrap();
        let seen = q.get(id).unwrap().progress();
        assert!(q.start_firing(id).is_some());
        assert!(q.remove(id).unwrap());

        assert_eq!(
            q.replace(id, seen, retrying()).unwrap(),
            ReplaceOutcome::Gone
        );
        assert_eq!(q.all(), [], "nothing must come back");
    }

    /// The daemon plans a batch, then claims each job just before it fires
    /// it. One cancelled or swapped for an edit in between is not claimed,
    /// so the daemon does not fire its own stale copy of it.
    #[test]
    fn only_a_job_still_queued_can_be_taken_to_fire() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        let kept = q.add(spec()).unwrap();
        let cancelled = q.add(spec()).unwrap();
        let edited = q.add(spec()).unwrap();
        assert!(q.remove(cancelled).unwrap());
        let seen = q.get(edited).unwrap().progress();
        assert!(matches!(
            q.replace(edited, seen, spec()).unwrap(),
            ReplaceOutcome::Swapped(_)
        ));

        assert_eq!(q.start_firing(kept).as_ref(), q.get(kept));
        assert!(q.is_firing(kept));
        for gone in [cancelled, edited] {
            assert_eq!(q.start_firing(gone), None, "job {gone}");
            assert!(!q.is_firing(gone), "job {gone}");
        }
    }

    #[test]
    fn a_failed_reschedule_keeps_the_old_values() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let mut q = Queue::load(sub.join("q.json")).unwrap();
        let id = q.add(spec()).unwrap();
        let before = q.get(id).unwrap().clone();
        block_saving(&sub);

        let new_ts: jiff::Timestamp = "2026-07-13T16:30:00Z".parse().unwrap();
        assert!(q.reschedule(id, new_ts, 1).is_err());
        let after = q.get(id).unwrap();
        assert_eq!(after.fire_at, before.fire_at, "fire time must not drift");
        assert_eq!(
            after.retries_left, before.retries_left,
            "retries must not drift"
        );
    }

    /// Every `.tmp` sibling of `path` left in `dir`.
    // case_sensitive_file_extension_comparisons: exact match is the point. These
    // are files `save` itself wrote, always lowercase `.tmp`; a case-insensitive
    // check would start counting unrelated files as leaked temps.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn temps_in(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    /// A save that dies *after* writing the temp must reap it.
    ///
    /// `block_saving` and `fail_nth_save` both fail before the temp exists, so
    /// neither can reach this: the temp is only orphaned when the write lands
    /// and the rename does not. A directory sitting where queue.json belongs
    /// does exactly that -- write fine, rename EISDIR -- and stands in for the
    /// real ENOSPC/EROFS/EXDEV cases.
    ///
    /// Pid-unique naming bounds a *single* process to one orphan, since it
    /// reuses the name; the unbounded growth is one file per crashed pid, with
    /// nothing to reap them.
    #[test]
    fn a_save_that_fails_after_writing_the_temp_reaps_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.json");
        // Load first: with a directory already in place, `load` itself would
        // fail (EISDIR) and never reach a save.
        let mut q = Queue::load(path.clone()).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(
            q.add(spec()).is_err(),
            "the rename must fail for this test to mean anything"
        );
        assert_eq!(
            temps_in(dir.path()),
            Vec::<String>::new(),
            "a save that failed must not leave its temp behind: nothing reaps these, \
             so one accumulates per crashed process"
        );
    }

    #[test]
    fn temp_path_is_process_unique() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::load(dir.path().join("q.json")).unwrap();
        let tmp = q.temp_path();
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.contains(&std::process::id().to_string()),
            "temp file must be process-unique, got {name}"
        );
        assert_ne!(
            tmp,
            q.path.with_extension("json.tmp"),
            "must not use the shared fixed temp name -- two daemons would truncate each other"
        );
    }
}

//! Load and atomically persist the job list as JSON.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::job::{Job, JobSpec};

#[derive(Serialize, Deserialize, Default)]
struct State {
    next_id: u64,
    jobs: Vec<Job>,
}

pub struct Queue {
    path: PathBuf,
    state: State,
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
        Ok(Queue { path, state })
    }

    pub fn all(&self) -> &[Job] {
        &self.state.jobs
    }

    pub fn get(&self, id: u64) -> Option<&Job> {
        self.state.jobs.iter().find(|j| j.id == id)
    }

    /// Assign a monotonic id, append, persist, and return the new id.
    pub fn add(&mut self, spec: JobSpec) -> std::io::Result<u64> {
        self.state.next_id += 1;
        let id = self.state.next_id;
        self.state.jobs.push(spec.into_job(id));
        self.save()?;
        Ok(id)
    }

    /// Remove the job with `id`. Returns whether one was removed.
    pub fn remove(&mut self, id: u64) -> std::io::Result<bool> {
        let before = self.state.jobs.len();
        self.state.jobs.retain(|j| j.id != id);
        let removed = self.state.jobs.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Write to a sibling temp file then rename, so a crash never leaves a
    /// half-written queue.
    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(&self.state)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
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
            },
            messages: vec!["go".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 2,
            settle_secs: 5.0,
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
        assert!(q.all().is_empty());
    }
}

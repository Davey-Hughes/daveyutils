//! The persisted job value type and its target descriptor.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum TargetSpec {
    Tmux { pane: String },
}

impl TargetSpec {
    /// Build the live, connectable target this descriptor names.
    pub fn connect(&self) -> Box<dyn crate::target::Target> {
        match self {
            TargetSpec::Tmux { pane } => {
                Box::new(crate::target::tmux::TmuxTarget::new(pane.clone()))
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Job {
    pub id: u64,
    pub target: TargetSpec,
    pub messages: Vec<String>,
    pub send_delay_secs: f64,
    pub fire_at: jiff::Timestamp,
    pub notify: bool,
    pub verify: bool,
    pub auto_retry: bool,
    pub retries_left: i64,
    pub settle_secs: f64,
}

/// What a caller supplies to `Queue::add` (everything but the id).
#[derive(Clone, Debug, PartialEq)]
pub struct JobSpec {
    pub target: TargetSpec,
    pub messages: Vec<String>,
    pub send_delay_secs: f64,
    pub fire_at: jiff::Timestamp,
    pub notify: bool,
    pub verify: bool,
    pub auto_retry: bool,
    pub retries_left: i64,
    pub settle_secs: f64,
}

impl JobSpec {
    pub fn into_job(self, id: u64) -> Job {
        Job {
            id,
            target: self.target,
            messages: self.messages,
            send_delay_secs: self.send_delay_secs,
            fire_at: self.fire_at,
            notify: self.notify,
            verify: self.verify,
            auto_retry: self.auto_retry,
            retries_left: self.retries_left,
            settle_secs: self.settle_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_roundtrips_through_json() {
        let job = Job {
            id: 7,
            target: TargetSpec::Tmux {
                pane: "bot:0.1".into(),
            },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: true,
            verify: false,
            auto_retry: true,
            retries_left: -1,
            settle_secs: 5.0,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: Job = serde_json::from_str(&json).unwrap();
        assert_eq!(job, back);
        // Target is externally tagged by `kind` for readable state files.
        assert!(json.contains(r#""kind":"Tmux""#));
    }
}

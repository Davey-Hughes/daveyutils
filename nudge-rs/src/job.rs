//! The persisted job value type and its target descriptor.

use serde::{Deserialize, Serialize};

use crate::target::PaneDims;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum TargetSpec {
    Tmux {
        pane: String,
        /// The tmux server `pane`'s id was read from, as
        /// [`SERVER_FORMAT`](crate::target::tmux::SERVER_FORMAT) reads it. A
        /// job queued before jobs kept it has none, and fires as it always did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<String>,
    },
}

impl TargetSpec {
    /// Whether this target's pane id was read from a tmux server other than
    /// `current`, the one running now. Only when both are known: a job queued
    /// before jobs kept their server, or a server tmux would not name, is no
    /// evidence either way.
    pub fn from_earlier_server(&self, current: Option<&str>) -> bool {
        let TargetSpec::Tmux { server, .. } = self;
        matches!((server.as_deref(), current), (Some(then), Some(now)) if then != now)
    }

    /// Build the live, connectable target this descriptor names.
    pub fn connect(&self) -> Box<dyn crate::target::Target> {
        match self {
            TargetSpec::Tmux { pane, server } => Box::new(
                crate::target::tmux::TmuxTarget::new(pane.clone()).expecting_server(server.clone()),
            ),
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
    /// Fingerprint of the pane when this `--verify` job was scheduled.
    pub verify_fingerprint: Option<String>,
    /// The size the fingerprinted capture was taken at.
    pub verify_dims: Option<PaneDims>,
}

impl Job {
    /// This job's `--verify` snapshot, if it has a complete and usable one.
    ///
    /// Demands *both* halves. They are only ever written together, so one
    /// without the other should be impossible — but "impossible" states reached
    /// anyway (a hand-edited queue.json, a future field-by-field migration)
    /// must not be guessed at. A fingerprint with no dims cannot be compared
    /// safely, so it is no baseline at all, and no baseline fails open.
    pub fn verify_baseline(&self) -> Option<(String, PaneDims)> {
        Some((self.verify_fingerprint.clone()?, self.verify_dims?))
    }
}

/// What a caller supplies to `Queue::add` (everything but the id).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
    /// The pane snapshot taken at schedule time; `None` when `--verify` is off
    /// or the pane could not be snapshotted.
    pub verify_fingerprint: Option<String>,
    pub verify_dims: Option<PaneDims>,
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
            verify_fingerprint: self.verify_fingerprint,
            verify_dims: self.verify_dims,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job queued by 0.6.0 has no `server`, and a target with none writes
    /// none, so a queue.json from before the stamp reads back as it was.
    #[test]
    fn a_tmux_target_without_a_server_reads_and_writes_as_before() {
        let json = r#"{"kind":"Tmux","pane":"%19"}"#;
        let t: TargetSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            t,
            TargetSpec::Tmux {
                pane: "%19".into(),
                server: None
            }
        );
        assert_eq!(serde_json::to_string(&t).unwrap(), json);
    }

    #[test]
    fn a_tmux_target_keeps_its_server() {
        let t = TargetSpec::Tmux {
            pane: "%19".into(),
            server: Some("1793226261-4820".into()),
        };
        let back: TargetSpec = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn job_roundtrips_through_json() {
        let job = Job {
            id: 7,
            target: TargetSpec::Tmux {
                pane: "bot:0.1".into(),
                server: None,
            },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: true,
            verify: false,
            auto_retry: true,
            retries_left: -1,
            settle_secs: 5.0,
            verify_fingerprint: Some("c46eabc781d005dc".into()),
            verify_dims: Some(PaneDims {
                width: 80,
                height: 24,
            }),
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: Job = serde_json::from_str(&json).unwrap();
        assert_eq!(job, back);
        // Target is externally tagged by `kind` for readable state files.
        assert!(json.contains(r#""kind":"Tmux""#));
        // The snapshot is written by the CLI and read hours later by the daemon,
        // possibly across a restart. A round-trip that silently dropped it would
        // leave every --verify job failing open, i.e. back to the I19 bug, with
        // nothing to show for it.
        assert_eq!(back.verify_baseline(), job.verify_baseline());
    }

    /// A job written by a build that predates the recency fields, as a resident
    /// daemon finds it in queue.json on reload.
    ///
    /// If this does not deserialize, `Queue::load` errors and the daemon never
    /// starts -- every pending job silently never fires. And it must land on
    /// `None`, not on some "unchanged" default: an old job carries no snapshot
    /// and must fail open to the pre-recency banner check.
    #[test]
    fn a_job_from_an_older_queue_json_loads_and_has_no_baseline() {
        let old = r#"{
            "id": 3,
            "target": {"kind": "Tmux", "pane": "bot:0.1"},
            "messages": ["please continue"],
            "send_delay_secs": 0.75,
            "fire_at": "2026-07-13T15:00:00Z",
            "notify": true,
            "verify": true,
            "auto_retry": false,
            "retries_left": 0,
            "settle_secs": 5.0
        }"#;
        let job: Job = serde_json::from_str(old).expect(
            "a queue.json written by an older build must still load: failing here \
             stops the daemon dead and every pending job silently never fires",
        );
        assert_eq!(job.verify_fingerprint, None);
        assert_eq!(job.verify_dims, None);
        assert_eq!(
            job.verify_baseline(),
            None,
            "no snapshot means not comparable, which must fail open to the banner check"
        );
    }

    /// The halves are written together and must be read together.
    #[test]
    fn half_a_baseline_is_no_baseline() {
        let mut j = Job {
            id: 1,
            target: TargetSpec::Tmux {
                pane: "p".into(),
                server: None,
            },
            messages: vec!["go".into()],
            send_delay_secs: 0.0,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: true,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: Some("deadbeef".into()),
            verify_dims: None,
        };
        assert_eq!(j.verify_baseline(), None, "fingerprint without dims");
        j.verify_fingerprint = None;
        j.verify_dims = Some(crate::target::PaneDims {
            width: 80,
            height: 24,
        });
        assert_eq!(j.verify_baseline(), None, "dims without fingerprint");
    }

    fn target(server: Option<&str>) -> TargetSpec {
        TargetSpec::Tmux {
            pane: "%1".into(),
            server: server.map(String::from),
        }
    }

    #[test]
    fn only_two_known_and_different_servers_are_an_earlier_one() {
        assert!(target(Some("100-1")).from_earlier_server(Some("200-2")));
        assert!(!target(Some("100-1")).from_earlier_server(Some("100-1")));
        assert!(!target(None).from_earlier_server(Some("200-2")));
        assert!(!target(Some("100-1")).from_earlier_server(None));
        assert!(!target(None).from_earlier_server(None));
    }
}

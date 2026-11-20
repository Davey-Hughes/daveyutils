//! The persisted job value type and its target descriptor.

use std::path::PathBuf;

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
    Herdr {
        /// herdr's id for the pane (`w7:pX`), which is stable across restarts of
        /// the session as long as the pane lives.
        pane: String,
        /// The API socket of the herdr session that holds `pane`: every session
        /// numbers its own panes, so the id alone does not say which.
        socket: PathBuf,
        /// The agent session `pane` held when the job was queued, so a job does
        /// not type into a pane that has since been given another conversation.
        /// A job queued without one fires as it always did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_session: Option<String>,
    },
}

impl TargetSpec {
    /// Whether this target's pane id was read from a tmux server other than
    /// `current`, the one running now. Only when both are known: a job queued
    /// before jobs kept their server, or a server tmux would not name, is no
    /// evidence either way.
    pub fn from_earlier_server(&self, current: Option<&str>) -> bool {
        let TargetSpec::Tmux { server, .. } = self else {
            return false;
        };
        matches!((server.as_deref(), current), (Some(then), Some(now)) if then != now)
    }

    /// Whether `self` and `other` name the same pane, whatever each was
    /// stamped with when it was queued: a tmux pane by its id, a herdr pane by
    /// its id and the session's socket.
    pub fn same_pane(&self, other: &TargetSpec) -> bool {
        match (self, other) {
            (TargetSpec::Tmux { pane: a, .. }, TargetSpec::Tmux { pane: b, .. }) => a == b,
            (
                TargetSpec::Herdr {
                    pane: a,
                    socket: sa,
                    ..
                },
                TargetSpec::Herdr {
                    pane: b,
                    socket: sb,
                    ..
                },
            ) => a == b && sa == sb,
            _ => false,
        }
    }

    /// Build the live, connectable target this descriptor names.
    pub fn connect(&self) -> Box<dyn crate::target::Target> {
        match self {
            TargetSpec::Tmux { pane, server } => Box::new(
                crate::target::tmux::TmuxTarget::new(pane.clone()).expecting_server(server.clone()),
            ),
            TargetSpec::Herdr {
                pane,
                socket,
                agent_session,
            } => Box::new(
                crate::target::herdr::HerdrTarget::new(pane.clone(), socket.clone())
                    .expecting_session(agent_session.clone()),
            ),
        }
    }
}

// struct_excessive_bools: each bool is a key of queue.json and of the
// Schedule request, read by daemons across versions. Grouping them into a
// sub-struct or an enum would change that format for no gain in clarity.
#[allow(clippy::struct_excessive_bools)]
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
    /// The `--verify` snapshot showed no rate-limit banner, so the banner
    /// check is skipped when the job fires: a pane nudge found no banner in
    /// then will show none it recognises later either. The recency check
    /// still applies. Absent in a job from 0.9.x or earlier, which keeps the
    /// banner check, and not written when off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verify_no_banner: bool,
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

    /// Whether a failed send is tried again: `--auto-retry` is on, the
    /// retries are not used up (`-1` is without end), and the job is not
    /// [`one_try`](Self::one_try). The scheduler's removal of the job and the
    /// daemon's notification both go by this, so a job is never removed
    /// unannounced.
    pub fn will_retry(&self) -> bool {
        self.auto_retry && self.retries_left != 0 && !self.one_try()
    }

    /// A `--verify` job whose pane showed no banner gets one try, whatever
    /// `-r` says. A retry would have neither of verify's checks: the
    /// reschedule drops the snapshot, and there is no banner to look for. So
    /// nothing could stop it typing into a session the user had resumed, and
    /// that holds after a failed send too, since with several messages some
    /// may have gone in before the error.
    pub fn one_try(&self) -> bool {
        one_try(self.verify, self.verify_no_banner)
    }
}

/// The rule behind [`Job::one_try`], kept in one place for `JobSpec` too.
fn one_try(verify: bool, verify_no_banner: bool) -> bool {
    verify && verify_no_banner
}

/// A retry count in words: `-1` (or any negative, which the scheduler treats
/// the same) is "no limit", 0 is "no retries".
pub fn retries_in_words(n: i64) -> String {
    match n {
        ..0 => "no limit".to_string(),
        0 => "no retries".to_string(),
        1 => "1 retry".to_string(),
        n => format!("{n} retries"),
    }
}

/// The retry count an edit of a job starts from, before any `-r`.
///
/// The job's own count, except where the edit should arm `default_retries`,
/// what a new schedule would:
/// - The job has no auto-retry, so it has no count of its own (it stores 0).
///   An edit that turns auto-retry on would otherwise store auto-retry with 0
///   retries, which never retries.
/// - The job has auto-retry with 0 left (`-r 0`, or its retries used up), and
///   the edit `asked` for auto-retry itself: `--auto-retry`, or ticking it again
///   in the dashboard. An edit that changes only the time or the message gives
///   no retries back.
pub fn edit_retry_base(
    auto_retry: bool,
    retries_left: i64,
    asked: bool,
    default_retries: i64,
) -> i64 {
    if !auto_retry || (retries_left == 0 && asked) {
        default_retries
    } else {
        retries_left
    }
}

/// What a caller supplies to `Queue::add` (everything but the id).
// struct_excessive_bools: as for `Job`.
#[allow(clippy::struct_excessive_bools)]
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
    /// See [`Job::verify_no_banner`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verify_no_banner: bool,
}

impl JobSpec {
    /// See [`Job::one_try`].
    pub fn one_try(&self) -> bool {
        one_try(self.verify, self.verify_no_banner)
    }

    /// What auto-retry will do for this job, for a summary of it; `None` with
    /// auto-retry off. A job that gets no retries says so in place of
    /// "auto-retry", whether `-r 0` asked for none, its retries are used up,
    /// or it is [`one_try`](Self::one_try).
    pub fn retry_note(&self) -> Option<String> {
        if !self.auto_retry {
            return None;
        }
        Some(if self.one_try() {
            "no retries (no banner to check)".to_string()
        } else if self.retries_left == 0 {
            retries_in_words(0)
        } else {
            format!("auto-retry ({})", retries_in_words(self.retries_left))
        })
    }

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
            verify_no_banner: self.verify_no_banner,
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
            verify_no_banner: false,
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
        assert!(
            !job.verify_no_banner,
            "an older job keeps the banner check it was scheduled with"
        );
    }

    /// The flag crosses the socket and survives a daemon restart. Off, it is
    /// not written at all, so a queue.json of jobs that never set it reads
    /// as an older build wrote it.
    #[test]
    fn the_no_banner_flag_round_trips_and_is_not_written_when_off() {
        let off = JobSpec {
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
            verify_dims: Some(PaneDims {
                width: 80,
                height: 24,
            }),
            verify_no_banner: false,
        }
        .into_job(1);
        let on = Job {
            verify_no_banner: true,
            ..off.clone()
        };

        let json = serde_json::to_string(&on).unwrap();
        assert_eq!(serde_json::from_str::<Job>(&json).unwrap(), on);
        let json = serde_json::to_string(&off).unwrap();
        assert!(!json.contains("verify_no_banner"), "{json}");
        assert_eq!(serde_json::from_str::<Job>(&json).unwrap(), off);
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
            verify_no_banner: false,
        };
        assert_eq!(j.verify_baseline(), None, "fingerprint without dims");
        j.verify_fingerprint = None;
        j.verify_dims = Some(crate::target::PaneDims {
            width: 80,
            height: 24,
        });
        assert_eq!(j.verify_baseline(), None, "dims without fingerprint");
    }

    #[test]
    fn a_herdr_target_round_trips() {
        let t = TargetSpec::Herdr {
            pane: "w7:pX".into(),
            socket: "/s/herdr.sock".into(),
            agent_session: Some("abc".into()),
        };
        let j = serde_json::to_string(&t).unwrap();
        assert!(j.contains(r#""kind":"Herdr""#));
        assert_eq!(serde_json::from_str::<TargetSpec>(&j).unwrap(), t);
        let bare = TargetSpec::Herdr {
            pane: "w7:pX".into(),
            socket: "/s/herdr.sock".into(),
            agent_session: None,
        };
        assert!(
            !serde_json::to_string(&bare)
                .unwrap()
                .contains("agent_session")
        );
    }

    #[test]
    fn same_pane_ignores_stamps() {
        let tmux = |server: &str| TargetSpec::Tmux {
            pane: "%3".into(),
            server: Some(server.into()),
        };
        let herdr = |socket: &str, stamp: Option<&str>| TargetSpec::Herdr {
            pane: "w1:p1".into(),
            socket: socket.into(),
            agent_session: stamp.map(String::from),
        };
        assert!(tmux("a").same_pane(&tmux("b")));
        assert!(herdr("/a", Some("x")).same_pane(&herdr("/a", None)));
        assert!(!herdr("/a", None).same_pane(&herdr("/b", None)));
        assert!(!herdr("/a", None).same_pane(&tmux("a")));
        assert!(!tmux("a").same_pane(&herdr("/a", None)));
    }

    #[test]
    fn a_herdr_job_is_never_from_an_earlier_tmux_server() {
        let t = TargetSpec::Herdr {
            pane: "w1:p1".into(),
            socket: "/a".into(),
            agent_session: None,
        };
        assert!(!t.from_earlier_server(Some("200-2")));
    }

    fn target(server: Option<&str>) -> TargetSpec {
        TargetSpec::Tmux {
            pane: "%1".into(),
            server: server.map(String::from),
        }
    }

    /// Which count an edit starts from, case by case.
    #[test]
    fn an_edit_arms_the_default_without_auto_retry_or_when_asked_with_none_left() {
        // (auto_retry, retries_left, asked) -> base, with a default of 2.
        for (auto_retry, left, asked, want) in [
            (true, 3, false, 3),
            (true, 3, true, 3), // asking keeps a count the job still has
            (true, -1, true, -1),
            (true, 0, false, 0), // used up stays used up
            (true, 0, true, 2),  // unless the edit asks for retries
            (false, 0, false, 2),
            // A job without auto-retry has no count of its own, whatever a
            // hand-edited queue.json says; -1 here would mean no limit.
            (false, -1, false, 2),
        ] {
            assert_eq!(
                edit_retry_base(auto_retry, left, asked, 2),
                want,
                "auto_retry {auto_retry}, {left} left, asked {asked}"
            );
        }
    }

    /// `-r -1` is no limit, not "-1 retries", and a count of 0 is none.
    #[test]
    fn a_retry_count_reads_as_words() {
        assert_eq!(retries_in_words(-1), "no limit");
        assert_eq!(retries_in_words(0), "no retries");
        assert_eq!(retries_in_words(1), "1 retry");
        assert_eq!(retries_in_words(2), "2 retries");
    }

    fn retrying(retries_left: i64) -> JobSpec {
        JobSpec {
            target: target(None),
            messages: vec!["please continue".into()],
            send_delay_secs: 0.5,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: true,
            retries_left,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
            verify_no_banner: false,
        }
    }

    /// A summary says what the daemon will do: `-r 0` used to read
    /// "auto-retry (0)", and `-r -1` "auto-retry (-1)".
    #[test]
    fn the_retry_note_says_what_the_daemon_will_do() {
        assert_eq!(
            retrying(2).retry_note().as_deref(),
            Some("auto-retry (2 retries)")
        );
        assert_eq!(
            retrying(-1).retry_note().as_deref(),
            Some("auto-retry (no limit)")
        );
        assert_eq!(retrying(0).retry_note().as_deref(), Some("no retries"));
        let once = JobSpec {
            verify: true,
            verify_no_banner: true,
            ..retrying(2)
        };
        assert_eq!(
            once.retry_note().as_deref(),
            Some("no retries (no banner to check)")
        );
        let off = JobSpec {
            auto_retry: false,
            ..retrying(0)
        };
        assert_eq!(off.retry_note(), None, "nothing to say without -r");
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

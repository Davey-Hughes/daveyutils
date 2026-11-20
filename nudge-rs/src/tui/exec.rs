//! The blocking executor: one `Effect` -> one `Msg`. The only place the
//! dashboard touches the socket or shells out. Swapping this for a threaded
//! version is the whole cost of going async later.

use std::path::Path;

use super::update::{Effect, Msg};
use crate::detect::detect_reset;
use crate::ipc::{Request, Response, client};
use crate::job::TargetSpec;

pub fn run_effect(effect: Effect, socket: &Path) -> Msg {
    match effect {
        Effect::PollJobs => match client::request(socket, &Request::List) {
            Ok(Response::Jobs(jobs)) => {
                let positions = crate::panes::positions_for(jobs.iter().map(|j| &j.target));
                Msg::JobsLoaded(jobs, positions)
            }
            Ok(other) => Msg::ActionFailed(format!("unexpected response: {other:?}")),
            Err(e) => Msg::ActionFailed(format!("daemon request failed: {e}")),
        },
        Effect::ListPanes => match crate::panes::list() {
            Ok((panes, default_idx)) => Msg::PanesLoaded { panes, default_idx },
            Err(e) => Msg::ActionFailed(format!("{e:#}")),
        },
        Effect::CapturePane { target } => {
            let patterns = crate::detect::Patterns::from_env();
            match target.connect().capture_styled() {
                Ok(raw) => {
                    let now = jiff::ZonedDateTime::now();
                    let detection = detect_reset(
                        &raw,
                        &now,
                        patterns.clock(),
                        patterns.duration(),
                        patterns.weekly(),
                    );
                    // Keep the escapes — the view parses them into styled text
                    // (parsing confines them to SGR styling; no raw control
                    // sequence reaches a widget). detect_reset strips internally.
                    Msg::PaneCaptured {
                        screen: Some(raw),
                        detection,
                    }
                }
                // Silent on failure — the live preview must not spam the status
                // line every 1.5s. The panel shows "(preview unavailable)".
                Err(_) => Msg::PaneCaptured {
                    screen: None,
                    detection: crate::detect::Detection::None,
                },
            }
        }
        Effect::Schedule {
            mut spec,
            snapshot_pane,
        } => {
            attach_baseline(&mut spec, snapshot_pane);
            let no_banner = spec.verify_no_banner;
            match client::request(socket, &Request::Schedule(spec)) {
                Ok(Response::Scheduled(id)) => Msg::Scheduled { id, no_banner },
                Ok(Response::Error(e)) => {
                    Msg::ActionFailed(format!("daemon rejected the job: {e}"))
                }
                Ok(other) => Msg::ActionFailed(format!("unexpected response: {other:?}")),
                Err(e) => Msg::ActionFailed(format!("daemon request failed: {e}")),
            }
        }
        Effect::Cancel(id) => match client::request(socket, &Request::Cancel(id)) {
            Ok(Response::Cancelled(b)) => Msg::Cancelled(b),
            Ok(Response::Error(e)) => Msg::ActionFailed(format!("failed to cancel: {e}")),
            Ok(other) => Msg::ActionFailed(format!("unexpected response: {other:?}")),
            Err(e) => Msg::ActionFailed(format!("daemon request failed: {e}")),
        },
        Effect::Replace {
            id,
            mut spec,
            snapshot_pane,
        } => {
            attach_baseline(&mut spec, snapshot_pane);
            let no_banner = spec.verify_no_banner;
            match client::request(socket, &Request::Replace { id, spec }) {
                Ok(Response::Replaced(new_id)) => Msg::Replaced {
                    id: new_id,
                    no_banner,
                },
                Ok(Response::Error(e)) => Msg::ActionFailed(format!("failed to edit: {e}")),
                Ok(other) => Msg::ActionFailed(format!("unexpected response: {other:?}")),
                Err(e) => Msg::ActionFailed(format!("daemon request failed: {e}")),
            }
        }
    }
}

/// Fill in the `--verify` recency baseline when the form asked for one, and
/// whether the pane showed a banner (`verify_no_banner`). Never
/// fails the schedule: a pane that will not snapshot arms no gate and fails open
/// at fire time, exactly as `app::snapshot_pane` does for the CLI.
fn attach_baseline(spec: &mut crate::job::JobSpec, snapshot_pane: Option<TargetSpec>) {
    if let Some(pane) = snapshot_pane
        && let Some(b) = crate::verify::capture_baseline_now(pane.connect().as_ref())
    {
        (
            spec.verify_fingerprint,
            spec.verify_dims,
            spec.verify_no_banner,
        ) = crate::app::baseline_fields(Some(b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::ipc::server;
    use crate::job::JobSpec;
    use crate::queue::Queue;

    fn spec() -> JobSpec {
        JobSpec {
            target: TargetSpec::Tmux {
                pane: "bot:0.1".into(),
                server: None,
            },
            messages: vec!["go".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-16T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
            verify_no_banner: false,
        }
    }

    /// Serve a queue on a temp socket and return its path + tempdir guard.
    fn serving_queue(seed: bool) -> (std::path::PathBuf, tempfile::TempDir) {
        let tmp = tempfile::Builder::new()
            .prefix("nudge-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = tmp.path().join("s.sock");
        let queue = Arc::new(Mutex::new(Queue::load(tmp.path().join("q.json")).unwrap()));
        if seed {
            queue.lock().unwrap().add(spec()).unwrap();
        }
        let q = Arc::clone(&queue);
        let s = socket.clone();
        std::thread::spawn(move || {
            let _ = server::serve(&s, &q);
        });
        // Give the server a beat to bind.
        std::thread::sleep(std::time::Duration::from_millis(200));
        (socket, tmp)
    }

    #[test]
    fn poll_jobs_returns_the_served_queue() {
        let (socket, _tmp) = serving_queue(true);
        match run_effect(Effect::PollJobs, &socket) {
            Msg::JobsLoaded(jobs, _) => assert_eq!(jobs.len(), 1),
            other => panic!("expected JobsLoaded, got {other:?}"),
        }
    }

    #[test]
    fn cancel_of_a_missing_job_reports_false_not_an_error() {
        let (socket, _tmp) = serving_queue(false);
        assert_eq!(
            run_effect(Effect::Cancel(999), &socket),
            Msg::Cancelled(false)
        );
    }

    #[test]
    fn a_dead_socket_becomes_action_failed_not_a_panic() {
        let socket = std::path::Path::new("/tmp/nudge-does-not-exist.sock");
        assert!(matches!(
            run_effect(Effect::PollJobs, socket),
            Msg::ActionFailed(_)
        ));
    }
}

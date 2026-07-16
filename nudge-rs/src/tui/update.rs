//! The pure state transition: `update(&mut Model, Msg) -> Vec<Effect>`.

use crossterm::event::KeyCode;
use jiff::{Timestamp, ToSpan};

use super::model::{Model, Tab};
use crate::detect::Detection;
use crate::job::{Job, JobSpec};
use crate::tmux_panes::Pane;

/// How stale the job set may get before a Tick triggers a refresh.
const POLL_SECS: i64 = 2;

/// Everything that can change the model.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Key(KeyCode),
    Tick(Timestamp),
    JobsLoaded(Vec<Job>),
    PanesLoaded(Vec<Pane>),
    Detected(Detection),
    Scheduled(u64),
    Cancelled(bool),
    Replaced(Option<u64>),
    ActionFailed(String),
}

/// Work for the outside world. `exec::run_effect` turns each into a `Msg`.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    PollJobs,
    ListPanes,
    AutoDetect { pane: String },
    Schedule { spec: JobSpec, snapshot_pane: Option<String> },
    Cancel(u64),
    Replace { id: u64, spec: JobSpec, snapshot_pane: Option<String> },
}

pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Tick(now) => {
            model.now = now;
            if now.duration_since(model.last_poll).as_secs() >= POLL_SECS {
                model.last_poll = now;
                vec![Effect::PollJobs]
            } else {
                vec![]
            }
        }
        Msg::JobsLoaded(jobs) => {
            model.jobs = jobs;
            model.clamp_selection();
            vec![]
        }
        Msg::Key(code) => match model.tab {
            Tab::Jobs => jobs_key(model, code),
            Tab::NewNudge => vec![], // filled in Task 3
        },
        // Filled in Tasks 3-4.
        Msg::PanesLoaded(_)
        | Msg::Detected(_)
        | Msg::Scheduled(_)
        | Msg::Cancelled(_)
        | Msg::Replaced(_)
        | Msg::ActionFailed(_) => vec![],
    }
}

fn jobs_key(model: &mut Model, code: KeyCode) -> Vec<Effect> {
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            model.selected = model.selected.saturating_sub(1);
            vec![]
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !model.jobs.is_empty() {
                model.selected = (model.selected + 1).min(model.jobs.len() - 1);
            }
            vec![]
        }
        KeyCode::Char('r') => vec![Effect::PollJobs],
        KeyCode::Char('q') => {
            model.should_quit = true;
            vec![]
        }
        KeyCode::Tab => {
            model.tab = Tab::NewNudge;
            if model.form.panes.is_empty() {
                vec![Effect::ListPanes]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::*;
    use super::*;
    use crate::job::{Job, TargetSpec};

    fn defaults() -> ScheduleDefaults {
        ScheduleDefaults { send_delay_secs: 0.75, settle_secs: 5.0, retries: 2 }
    }

    fn t0() -> jiff::Timestamp {
        "2026-07-16T12:00:00Z".parse().unwrap()
    }

    fn job(id: u64, secs_out: i64) -> Job {
        Job {
            id,
            target: TargetSpec::Tmux { pane: format!("s:0.{id}") },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: t0().checked_add(jiff::ToSpan::seconds(secs_out)).unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        }
    }

    fn with_jobs(n: u64) -> Model {
        let mut m = Model::new(defaults(), t0());
        m.jobs = (1..=n).map(|i| job(i, 3600)).collect();
        m
    }

    #[test]
    fn down_and_up_move_the_selection_and_saturate() {
        let mut m = with_jobs(3);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down));
        assert_eq!(m.selected, 1);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down));
        assert_eq!(m.selected, 2, "past the end stays on the last row");
        for _ in 0..5 {
            update(&mut m, Msg::Key(crossterm::event::KeyCode::Up));
        }
        assert_eq!(m.selected, 0, "past the top stays on the first row");
    }

    #[test]
    fn tab_switches_to_the_form_and_asks_for_panes_once() {
        let mut m = with_jobs(1);
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Tab));
        assert_eq!(m.tab, Tab::NewNudge);
        assert!(matches!(fx.as_slice(), [Effect::ListPanes]));
        // Already have panes -> no second fetch.
        m.form.panes = vec![crate::tmux_panes::Pane { target: "s:0.1".into(), title: String::new() }];
        m.tab = Tab::Jobs;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Tab));
        assert!(fx.is_empty(), "panes already loaded, do not refetch");
    }

    #[test]
    fn q_quits_from_the_jobs_tab() {
        let mut m = with_jobs(1);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('q')));
        assert!(m.should_quit);
    }

    #[test]
    fn a_tick_past_the_poll_window_asks_for_jobs() {
        let mut m = with_jobs(1);
        let soon = m.now.checked_add(jiff::ToSpan::seconds(1)).unwrap();
        assert!(update(&mut m, Msg::Tick(soon)).is_empty(), "1s < 2s window");
        let later = m.now.checked_add(jiff::ToSpan::seconds(3)).unwrap();
        let fx = update(&mut m, Msg::Tick(later));
        assert!(fx.contains(&Effect::PollJobs));
        assert_eq!(m.now, later, "the tick advances the clock");
    }

    #[test]
    fn jobs_loaded_replaces_the_set_and_clamps_selection() {
        let mut m = with_jobs(3);
        m.selected = 2;
        update(&mut m, Msg::JobsLoaded(vec![job(1, 3600)]));
        assert_eq!(m.jobs.len(), 1);
        assert_eq!(m.selected, 0, "selection clamps into the shorter list");
    }
}

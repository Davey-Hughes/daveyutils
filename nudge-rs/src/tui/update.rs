//! The pure state transition: `update(&mut Model, Msg) -> Vec<Effect>`.

use crossterm::event::KeyCode;
use jiff::Timestamp;

use super::model::{CarriedEdit, Form, FormField, MessageField, Mode, Model, Tab, WhenMode};
use crate::detect::Detection;
use crate::job::{Job, JobSpec, TargetSpec};
use crate::timespec::parse_timespec;
use crate::tmux_panes::Pane;

/// How stale the job set may get before a Tick triggers a refresh.
const POLL_SECS: i64 = 2;

/// Everything that can change the model.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Key(KeyCode),
    Quit,
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
    AutoDetect {
        pane: String,
    },
    Schedule {
        spec: JobSpec,
        snapshot_pane: Option<String>,
    },
    Cancel(u64),
    Replace {
        id: u64,
        spec: JobSpec,
        snapshot_pane: Option<String>,
    },
}

pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Quit => {
            model.should_quit = true;
            vec![]
        }
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
            Tab::NewNudge => form_key(model, code),
        },
        Msg::PanesLoaded(panes) => {
            model.form.panes = panes;
            if model.form.pane_idx >= model.form.panes.len() {
                model.form.pane_idx = 0;
            }
            vec![]
        }
        Msg::Detected(d) => {
            model.form.detected = Some(d);
            vec![]
        }
        Msg::Scheduled(id) => {
            model.status.set(format!("scheduled job {id}"));
            model.tab = Tab::Jobs;
            model.form = super::model::Form::fresh();
            vec![Effect::PollJobs]
        }
        Msg::Replaced(Some(id)) => {
            model.status.set(format!("edited — now job {id}"));
            model.tab = Tab::Jobs;
            model.form = super::model::Form::fresh();
            vec![Effect::PollJobs]
        }
        Msg::Replaced(None) => {
            model.status.set("that job is already gone");
            vec![Effect::PollJobs]
        }
        Msg::Cancelled(true) => {
            model.status.set("cancelled");
            vec![Effect::PollJobs]
        }
        Msg::Cancelled(false) => {
            model.status.set("no such job");
            vec![Effect::PollJobs]
        }
        Msg::ActionFailed(e) => {
            model.status.set(e);
            vec![]
        }
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
        KeyCode::Char('c') => match model.jobs.get(model.selected) {
            Some(j) => vec![Effect::Cancel(j.id)],
            None => vec![],
        },
        KeyCode::Char('e') => {
            if let Some(j) = model.jobs.get(model.selected).cloned() {
                enter_edit(model, &j);
            }
            vec![]
        }
        _ => vec![],
    }
}

/// Switch to the New-nudge form pre-filled from `job`, carrying through the
/// fields the form does not show (mirrors `app::merge_edit`'s "unset stays put").
fn enter_edit(model: &mut Model, job: &Job) {
    let TargetSpec::Tmux { pane } = &job.target;
    let panes = std::mem::take(&mut model.form.panes);
    let pane_idx = panes.iter().position(|p| &p.target == pane).unwrap_or(0);
    let message = match job.messages.len() {
        0 | 1 => MessageField::Editable(
            job.messages
                .first()
                .cloned()
                .unwrap_or_else(|| "please continue".into()),
        ),
        n => MessageField::Preserved(n),
    };
    model.form = Form {
        panes,
        pane_idx,
        when: WhenMode::Keep,
        manual_time: String::new(),
        detected: None,
        message,
        verify: job.verify,
        notify: job.notify,
        auto_retry: job.auto_retry,
        focus: FormField::Pane,
        mode: Mode::Editing(job.id),
        carried: Some(CarriedEdit {
            fire_at: job.fire_at,
            messages: job.messages.clone(),
            send_delay_secs: job.send_delay_secs,
            settle_secs: job.settle_secs,
            retries_left: job.retries_left,
        }),
        preview: None,
        last_capture: None,
    };
    model.tab = Tab::NewNudge;
}

const FORM_ORDER: [FormField; 8] = [
    FormField::Pane,
    FormField::When,
    FormField::ManualTime,
    FormField::Message,
    FormField::Verify,
    FormField::Notify,
    FormField::AutoRetry,
    FormField::Submit,
];

fn move_focus(form: &mut super::model::Form, delta: i32) {
    let i = FORM_ORDER
        .iter()
        .position(|f| *f == form.focus)
        .unwrap_or(0) as i32;
    let n = FORM_ORDER.len() as i32;
    let next = (i + delta).rem_euclid(n) as usize;
    form.focus = FORM_ORDER[next];
}

fn form_key(model: &mut Model, code: KeyCode) -> Vec<Effect> {
    let form = &mut model.form;
    match code {
        KeyCode::Tab | KeyCode::Esc => {
            model.tab = Tab::Jobs;
            vec![]
        }
        KeyCode::Down => {
            move_focus(form, 1);
            vec![]
        }
        KeyCode::Up => {
            move_focus(form, -1);
            vec![]
        }
        KeyCode::Left | KeyCode::Right => {
            let dir: i32 = if code == KeyCode::Right { 1 } else { -1 };
            match form.focus {
                FormField::Pane if !form.panes.is_empty() => {
                    let n = form.panes.len() as i32;
                    form.pane_idx = ((form.pane_idx as i32 + dir).rem_euclid(n)) as usize;
                    if form.when == WhenMode::Auto {
                        return detect_selected(form);
                    }
                    vec![]
                }
                FormField::When => {
                    form.when = cycle_when(form.when, dir, form.mode);
                    if form.when == WhenMode::Auto {
                        return detect_selected(form);
                    }
                    vec![]
                }
                _ => vec![],
            }
        }
        KeyCode::Char(' ') => {
            match form.focus {
                FormField::Verify => form.verify = !form.verify,
                FormField::Notify => form.notify = !form.notify,
                FormField::AutoRetry => form.auto_retry = !form.auto_retry,
                _ => {}
            }
            vec![]
        }
        KeyCode::Char(c) => {
            edit_text(form, |s| s.push(c));
            vec![]
        }
        KeyCode::Backspace => {
            edit_text(form, |s| {
                s.pop();
            });
            vec![]
        }
        KeyCode::Enter => submit(model),
        _ => vec![],
    }
}

/// Clear the now-stale detection and ask exec to re-detect the selected pane.
/// Returns no effect when there is no pane to detect.
fn detect_selected(form: &mut super::model::Form) -> Vec<Effect> {
    form.detected = None;
    match form.selected_pane() {
        Some(p) => vec![Effect::AutoDetect {
            pane: p.target.clone(),
        }],
        None => vec![],
    }
}

/// Keep is only offered while editing; new nudges cycle Auto<->Manual only.
fn cycle_when(cur: WhenMode, dir: i32, mode: super::model::Mode) -> WhenMode {
    let opts: &[WhenMode] = match mode {
        super::model::Mode::Editing(_) => &[WhenMode::Keep, WhenMode::Auto, WhenMode::Manual],
        super::model::Mode::New => &[WhenMode::Auto, WhenMode::Manual],
    };
    let i = opts.iter().position(|w| *w == cur).unwrap_or(0) as i32;
    opts[((i + dir).rem_euclid(opts.len() as i32)) as usize]
}

/// Apply `f` to whichever text buffer the focused field owns, if any.
fn edit_text(form: &mut super::model::Form, f: impl FnOnce(&mut String)) {
    match form.focus {
        FormField::ManualTime => f(&mut form.manual_time),
        FormField::Message => {
            if let MessageField::Editable(s) = &mut form.message {
                f(s);
            }
        }
        _ => {}
    }
}

fn submit(model: &mut Model) -> Vec<Effect> {
    let now_zoned = model.now.to_zoned(model.defaults.tz.clone());
    let Some(pane) = model.form.selected_pane().map(|p| p.target.clone()) else {
        model
            .status
            .set("no tmux pane selected (none found — schedule from the CLI with -p)");
        return vec![];
    };
    // fire time
    let fire_at = match model.form.when {
        WhenMode::Keep => match &model.form.carried {
            Some(c) => c.fire_at,
            None => {
                model.status.set("nothing to keep — pick Auto or Manual");
                return vec![];
            }
        },
        WhenMode::Auto => match &model.form.detected {
            Some(Detection::Reset(z)) => z.timestamp(),
            Some(Detection::None) => {
                model
                    .status
                    .set("no rate-limit banner on that pane — enter a time manually");
                return vec![];
            }
            Some(Detection::Unreadable { gap, .. }) => {
                model.status.set(format!(
                    "weekly banner day unreadable ({gap:?}) — enter a time manually"
                ));
                return vec![];
            }
            None => {
                model
                    .status
                    .set("no time detected yet — select a pane or switch to Manual");
                return vec![];
            }
        },
        WhenMode::Manual => match parse_timespec(&model.form.manual_time, &now_zoned) {
            Ok(z) => z.timestamp(),
            Err(e) => {
                model.status.set(format!("could not parse time: {e}"));
                return vec![];
            }
        },
    };

    let spec = build_spec(model, &pane, fire_at);
    let snapshot_pane = model.form.verify.then(|| pane.clone());
    match model.form.mode {
        super::model::Mode::New => vec![Effect::Schedule {
            spec,
            snapshot_pane,
        }],
        super::model::Mode::Editing(id) => vec![Effect::Replace {
            id,
            spec,
            snapshot_pane,
        }],
    }
}

/// Build the JobSpec from the form + carried edit fields. Baseline is left
/// `None` here; `exec` fills it when `snapshot_pane` is set.
fn build_spec(model: &Model, pane: &str, fire_at: Timestamp) -> JobSpec {
    let form = &model.form;
    let messages = match &form.message {
        MessageField::Editable(s) if s.trim().is_empty() => vec!["please continue".to_string()],
        MessageField::Editable(s) => vec![s.clone()],
        MessageField::Preserved(_) => form
            .carried
            .as_ref()
            .map(|c| c.messages.clone())
            .unwrap_or_else(|| vec!["please continue".to_string()]),
    };
    // Retry base mirrors app::merge_edit: a job with 0 left re-arms the default.
    let retry_base = match &form.carried {
        Some(c) if c.retries_left != 0 => c.retries_left,
        _ => model.defaults.retries,
    };
    let (send_delay_secs, settle_secs) = match &form.carried {
        Some(c) => (c.send_delay_secs, c.settle_secs),
        None => (model.defaults.send_delay_secs, model.defaults.settle_secs),
    };
    JobSpec {
        target: TargetSpec::Tmux {
            pane: pane.to_string(),
        },
        messages,
        send_delay_secs,
        fire_at,
        notify: form.notify,
        verify: form.verify,
        auto_retry: form.auto_retry,
        retries_left: if form.auto_retry { retry_base } else { 0 },
        settle_secs,
        verify_fingerprint: None,
        verify_dims: None,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::super::model::*;
    use super::*;
    use crate::job::{Job, TargetSpec};

    fn defaults() -> ScheduleDefaults {
        ScheduleDefaults {
            send_delay_secs: 0.75,
            settle_secs: 5.0,
            retries: 2,
            tz: jiff::tz::TimeZone::UTC,
        }
    }

    fn t0() -> jiff::Timestamp {
        "2026-07-16T12:00:00Z".parse().unwrap()
    }

    fn job(id: u64, secs_out: i64) -> Job {
        Job {
            id,
            target: TargetSpec::Tmux {
                pane: format!("s:0.{id}"),
            },
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
        m.tab = Tab::Jobs;
        m.jobs = (1..=n).map(|i| job(i, 3600)).collect();
        m
    }

    fn form_model() -> Model {
        let mut m = Model::new(defaults(), t0());
        m.tab = Tab::NewNudge;
        m.form.panes = vec![
            Pane {
                target: "s:0.1".into(),
                title: "claude".into(),
            },
            Pane {
                target: "s:0.2".into(),
                title: "agy".into(),
            },
        ];
        m
    }

    fn multi_msg_job() -> Job {
        let mut j = job(7, 4000);
        j.messages = vec!["one".into(), "two".into()];
        j.send_delay_secs = 1.5;
        j.settle_secs = 9.0;
        j.retries_left = 3;
        j.auto_retry = true;
        j
    }

    #[test]
    fn panes_loaded_populates_the_dropdown_and_clamps_index() {
        let mut m = form_model();
        m.form.pane_idx = 5;
        update(
            &mut m,
            Msg::PanesLoaded(vec![Pane {
                target: "s:0.9".into(),
                title: String::new(),
            }]),
        );
        assert_eq!(m.form.panes.len(), 1);
        assert_eq!(m.form.pane_idx, 0);
    }

    #[test]
    fn changing_pane_requests_autodetect_when_when_is_auto() {
        let mut m = form_model();
        m.form.focus = FormField::Pane;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Right));
        assert_eq!(m.form.pane_idx, 1);
        assert_eq!(
            fx,
            vec![Effect::AutoDetect {
                pane: "s:0.2".into()
            }]
        );
    }

    #[test]
    fn changing_pane_clears_stale_detection_and_redetects() {
        let mut m = form_model(); // has panes s:0.1 and s:0.2, When defaults to Auto
        m.form.focus = FormField::Pane;
        m.form.detected = Some(Detection::Reset(
            jiff::Timestamp::from_str("2026-07-16T15:00:00Z")
                .unwrap()
                .to_zoned(jiff::tz::TimeZone::UTC),
        ));
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Right));
        assert!(
            m.form.detected.is_none(),
            "stale detection must be cleared on pane change"
        );
        assert_eq!(
            fx,
            vec![Effect::AutoDetect {
                pane: "s:0.2".into()
            }]
        );
    }

    #[test]
    fn focus_cycles_with_arrows() {
        let mut m = form_model();
        assert_eq!(m.form.focus, FormField::Pane);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down));
        assert_eq!(m.form.focus, FormField::When);
    }

    #[test]
    fn space_toggles_the_focused_flag() {
        let mut m = form_model();
        m.form.focus = FormField::Verify;
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char(' ')));
        assert!(m.form.verify);
    }

    #[test]
    fn typing_edits_the_focused_message_field() {
        let mut m = form_model();
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable(String::new());
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('h')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('i')));
        assert_eq!(m.form.message, MessageField::Editable("hi".into()));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Backspace));
        assert_eq!(m.form.message, MessageField::Editable("h".into()));
    }

    #[test]
    fn submit_with_a_detected_reset_emits_schedule() {
        let mut m = form_model();
        m.form.verify = true;
        m.form.detected = Some(Detection::Reset(
            jiff::Timestamp::from_str("2026-07-16T15:00:00Z")
                .unwrap()
                .to_zoned(jiff::tz::TimeZone::UTC),
        ));
        m.form.focus = FormField::Submit;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Enter));
        match fx.as_slice() {
            [Effect::Schedule {
                spec,
                snapshot_pane,
            }] => {
                assert_eq!(spec.messages, vec!["please continue".to_string()]);
                assert!(spec.verify);
                assert_eq!(spec.fire_at.to_string(), "2026-07-16T15:00:00Z");
                assert_eq!(
                    snapshot_pane.as_deref(),
                    Some("s:0.1"),
                    "verify wants a baseline"
                );
            }
            other => panic!("expected one Schedule effect, got {other:?}"),
        }
    }

    #[test]
    fn submit_manual_time_parses_relative_to_now() {
        let mut m = form_model();
        m.form.when = WhenMode::Manual;
        m.form.manual_time = "now + 2 hours".into();
        m.form.focus = FormField::Submit;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Enter));
        match fx.as_slice() {
            [Effect::Schedule { spec, .. }] => {
                assert_eq!(spec.fire_at.to_string(), "2026-07-16T14:00:00Z");
            }
            other => panic!("expected Schedule, got {other:?}"),
        }
    }

    #[test]
    fn submit_manual_clock_time_resolves_in_the_local_zone_not_utc() {
        // Regression: `submit` used to hardcode UTC when resolving absolute
        // clock times ("3pm"), hours off from the CLI's system-local
        // resolution. t0 is noon UTC, which is 04:00 in a fixed UTC-8 zone,
        // so "3pm" there must land at 23:00 UTC — not 15:00 UTC (the bug).
        let mut m = form_model();
        m.defaults.tz = jiff::tz::TimeZone::fixed(jiff::tz::offset(-8));
        m.form.when = WhenMode::Manual;
        m.form.manual_time = "3pm".into();
        m.form.focus = FormField::Submit;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Enter));
        match fx.as_slice() {
            [Effect::Schedule { spec, .. }] => {
                assert_eq!(spec.fire_at.to_string(), "2026-07-16T23:00:00Z");
            }
            other => panic!("expected Schedule, got {other:?}"),
        }
    }

    #[test]
    fn submit_without_a_time_sets_status_and_emits_nothing() {
        let mut m = form_model();
        m.form.focus = FormField::Submit; // when=Auto, detected=None
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Enter));
        assert!(fx.is_empty());
        assert!(
            m.status.0.is_some(),
            "must explain why nothing was scheduled"
        );
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
        m.form.panes = vec![crate::tmux_panes::Pane {
            target: "s:0.1".into(),
            title: String::new(),
        }];
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
    fn quit_msg_quits_from_either_tab() {
        let mut m = with_jobs(1);
        m.tab = Tab::NewNudge;
        update(&mut m, Msg::Quit);
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

    #[test]
    fn e_enters_edit_prefilled_and_preserves_hidden_fields_on_save() {
        let mut m = with_jobs(0);
        m.jobs = vec![multi_msg_job()];
        m.form.panes = vec![Pane {
            target: "s:0.7".into(),
            title: String::new(),
        }];
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('e')));
        assert_eq!(m.tab, Tab::NewNudge);
        assert!(matches!(m.form.mode, Mode::Editing(7)));
        assert_eq!(
            m.form.when,
            WhenMode::Keep,
            "an edit keeps the time by default"
        );
        assert_eq!(
            m.form.message,
            MessageField::Preserved(2),
            "multi-message is not editable in the TUI"
        );
        assert!(m.form.auto_retry);

        // Save with nothing changed -> Replace preserving delay/settle/retries/messages.
        m.form.focus = FormField::Submit;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Enter));
        match fx.as_slice() {
            [Effect::Replace { id, spec, .. }] => {
                assert_eq!(*id, 7);
                assert_eq!(spec.messages, vec!["one".to_string(), "two".to_string()]);
                assert_eq!(spec.send_delay_secs, 1.5);
                assert_eq!(spec.settle_secs, 9.0);
                assert_eq!(
                    spec.retries_left, 3,
                    "auto-retry on -> keep the job's budget"
                );
                assert_eq!(
                    spec.fire_at,
                    multi_msg_job().fire_at,
                    "Keep -> unchanged time"
                );
            }
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    #[test]
    fn c_cancels_the_selected_job() {
        let mut m = with_jobs(2);
        m.selected = 1;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('c')));
        assert_eq!(fx, vec![Effect::Cancel(2)]);
    }

    #[test]
    fn scheduled_confirms_switches_to_jobs_and_refreshes() {
        let mut m = form_model();
        let fx = update(&mut m, Msg::Scheduled(12));
        assert_eq!(m.tab, Tab::Jobs);
        assert!(m.status.0.as_ref().unwrap().contains("12"));
        assert!(fx.contains(&Effect::PollJobs));
        assert!(
            matches!(m.form.mode, Mode::New),
            "the form resets after a successful schedule"
        );
    }

    #[test]
    fn action_failed_lands_in_the_status_line_and_does_not_quit() {
        let mut m = with_jobs(1);
        update(&mut m, Msg::ActionFailed("daemon is not this build".into()));
        assert!(m
            .status
            .0
            .as_ref()
            .unwrap()
            .contains("daemon is not this build"));
        assert!(!m.should_quit);
    }
}

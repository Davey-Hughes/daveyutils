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

/// How often the New-nudge tab re-captures the selected pane for the live preview.
const CAPTURE_MILLIS: i64 = 1500;

/// Everything that can change the model.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Key(KeyCode),
    Quit,
    Tick(Timestamp),
    JobsLoaded(Vec<Job>),
    PanesLoaded(Vec<Pane>),
    PaneCaptured {
        screen: Option<String>,
        detection: Detection,
    },
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
    CapturePane {
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
            let mut effects = vec![];
            if now.duration_since(model.last_poll).as_secs() >= POLL_SECS {
                model.last_poll = now;
                effects.push(Effect::PollJobs);
            }
            // Live pane preview: while the form is open, re-capture the selected
            // pane about every 1.5s. Gated by last_capture so it fires at the
            // cadence, not on every 250ms idle tick.
            if model.tab == Tab::NewNudge && model.form.selected_pane().is_some() {
                let due = model
                    .form
                    .last_capture
                    .is_none_or(|t| now.duration_since(t).as_millis() >= CAPTURE_MILLIS as i128);
                if due {
                    effects.extend(capture_selected(&mut model.form, now));
                }
            }
            effects
        }
        Msg::JobsLoaded(jobs) => {
            model.jobs = jobs;
            model.clamp_selection();
            vec![]
        }
        Msg::Key(code) => match model.tab {
            Tab::Jobs => jobs_key(model, code),
            Tab::NewNudge if model.form.picker.is_some() => picker_key(model, code),
            Tab::NewNudge => form_key(model, code),
        },
        Msg::PanesLoaded(panes) => {
            model.form.panes = panes;
            if model.form.pane_idx >= model.form.panes.len() {
                model.form.pane_idx = 0;
            }
            if model.tab == Tab::NewNudge {
                return capture_selected(&mut model.form, model.now);
            }
            vec![]
        }
        Msg::PaneCaptured { screen, detection } => {
            model.form.preview = screen;
            model.form.detected = Some(detection);
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
        picker: None,
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
                    // Always refresh: the preview title tracks the selected
                    // pane live, so a stale screen from the old pane would
                    // otherwise show under the new pane's name until the next
                    // tick. Re-detecting in non-Auto is harmless — `submit`
                    // only reads `detected` in Auto mode.
                    capture_selected(form, model.now)
                }
                FormField::When => {
                    form.when = cycle_when(form.when, dir, form.mode);
                    if form.when == WhenMode::Auto {
                        return capture_selected(form, model.now);
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
        KeyCode::Char('q') if !matches!(form.focus, FormField::ManualTime | FormField::Message) => {
            model.should_quit = true;
            vec![]
        }
        KeyCode::Char('/') if !matches!(form.focus, FormField::ManualTime | FormField::Message) => {
            open_picker(model)
        }
        KeyCode::Enter if form.focus == FormField::Pane => open_picker(model),
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

/// Emit a capture of the selected pane and mark the time, clearing the stale
/// preview + detection so a submit can't read them mid-refresh. Takes
/// `&mut Form` + `now` (not `&mut Model`) so the pane-change arms that already
/// hold `&mut model.form` can call it without a second whole-model borrow;
/// `now` is a `Copy` `Timestamp` from a disjoint field.
fn capture_selected(form: &mut super::model::Form, now: Timestamp) -> Vec<Effect> {
    form.preview = None;
    form.detected = None;
    form.last_capture = Some(now);
    match form.active_pane() {
        Some(p) => vec![Effect::CapturePane {
            pane: p.target.clone(),
        }],
        None => vec![],
    }
}

/// The strings the fuzzy matcher searches — "<target> <title>" per pane.
fn pane_labels(panes: &[crate::tmux_panes::Pane]) -> Vec<String> {
    panes
        .iter()
        .map(|p| format!("{} {}", p.target, p.title))
        .collect()
}

/// Open the fzf picker over the current panes and capture the highlight.
fn open_picker(model: &mut Model) -> Vec<Effect> {
    let matches = crate::tui::fuzzy::filter("", &pane_labels(&model.form.panes));
    model.form.picker = Some(super::model::Picker {
        query: String::new(),
        matches,
        highlight: 0,
        mode: super::model::PickerMode::Insert,
    });
    capture_selected(&mut model.form, model.now)
}

fn picker_key(model: &mut Model, code: KeyCode) -> Vec<Effect> {
    use super::model::PickerMode;
    let mode = match model.form.picker.as_ref() {
        Some(p) => p.mode,
        None => return vec![],
    };
    // Keys shared by both modes: pick, and arrow navigation.
    match code {
        KeyCode::Enter => {
            if let Some(p) = &model.form.picker {
                if let Some(&idx) = p.matches.get(p.highlight) {
                    model.form.pane_idx = idx;
                }
            }
            model.form.picker = None;
            return capture_selected(&mut model.form, model.now);
        }
        KeyCode::Up => return picker_move(model, -1),
        KeyCode::Down => return picker_move(model, 1),
        _ => {}
    }
    match mode {
        PickerMode::Insert => match code {
            // Esc leaves Insert for Normal — it does NOT close the picker.
            KeyCode::Esc => {
                if let Some(p) = model.form.picker.as_mut() {
                    p.mode = PickerMode::Normal;
                }
                vec![]
            }
            KeyCode::Backspace => picker_filter(model, |q| {
                q.pop();
            }),
            KeyCode::Char(c) => picker_filter(model, |q| q.push(c)),
            _ => vec![],
        },
        PickerMode::Normal => match code {
            KeyCode::Char('j') => picker_move(model, 1),
            KeyCode::Char('k') => picker_move(model, -1),
            // The usual vim ways back into Insert; a single-line search box has
            // no distinct cursor position, so i/a/A/I are all equivalent.
            KeyCode::Char('i' | 'a' | 'A' | 'I') => {
                if let Some(p) = model.form.picker.as_mut() {
                    p.mode = PickerMode::Insert;
                }
                vec![]
            }
            // In Normal mode, Esc or q cancels the picker (back to the form).
            KeyCode::Esc | KeyCode::Char('q') => {
                model.form.picker = None;
                capture_selected(&mut model.form, model.now)
            }
            _ => vec![],
        },
    }
}

/// Move the picker highlight by `delta` (clamped to the match list), re-capturing
/// the preview only when the highlighted pane actually changes.
fn picker_move(model: &mut Model, delta: i32) -> Vec<Effect> {
    let before = model.form.active_pane().map(|p| p.target.clone());
    if let Some(picker) = model.form.picker.as_mut() {
        if !picker.matches.is_empty() {
            let n = picker.matches.len() as i32;
            picker.highlight = (picker.highlight as i32 + delta).clamp(0, n - 1) as usize;
        }
    }
    if model.form.active_pane().map(|p| p.target.clone()) != before {
        capture_selected(&mut model.form, model.now)
    } else {
        vec![]
    }
}

/// Edit the picker query, re-filter, reset the highlight, and re-capture the
/// preview only when the new top match differs from the previous highlight.
fn picker_filter(model: &mut Model, edit: impl FnOnce(&mut String)) -> Vec<Effect> {
    let labels = pane_labels(&model.form.panes);
    let before = model.form.active_pane().map(|p| p.target.clone());
    if let Some(picker) = model.form.picker.as_mut() {
        edit(&mut picker.query);
        picker.matches = crate::tui::fuzzy::filter(&picker.query, &labels);
        picker.highlight = 0;
    }
    if model.form.active_pane().map(|p| p.target.clone()) != before {
        capture_selected(&mut model.form, model.now)
    } else {
        vec![]
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
            vec![Effect::CapturePane {
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
        m.form.preview = Some("stale screen".into());
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Right));
        assert!(
            m.form.detected.is_none(),
            "stale detection must be cleared on pane change"
        );
        assert!(m.form.preview.is_none());
        assert_eq!(
            fx,
            vec![Effect::CapturePane {
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

    #[test]
    fn a_tick_on_the_new_nudge_tab_captures_the_pane_at_the_interval() {
        let mut m = form_model(); // NewNudge tab, panes present
        m.form.last_capture = Some(m.now);
        let soon = m.now.checked_add(jiff::ToSpan::milliseconds(500)).unwrap();
        assert!(
            update(&mut m, Msg::Tick(soon))
                .iter()
                .all(|e| !matches!(e, Effect::CapturePane { .. })),
            "before the interval, no capture"
        );
        let later = m.now.checked_add(jiff::ToSpan::milliseconds(1600)).unwrap();
        let fx = update(&mut m, Msg::Tick(later));
        assert!(
            fx.iter().any(|e| matches!(e, Effect::CapturePane { .. })),
            "after the interval, capture"
        );
    }

    #[test]
    fn a_tick_on_the_jobs_tab_does_not_capture() {
        let mut m = form_model();
        m.tab = Tab::Jobs;
        let later = m.now.checked_add(jiff::ToSpan::seconds(3)).unwrap();
        let fx = update(&mut m, Msg::Tick(later));
        assert!(
            fx.iter().all(|e| !matches!(e, Effect::CapturePane { .. })),
            "Jobs tab has no preview"
        );
    }

    #[test]
    fn panes_loaded_on_the_form_captures_the_selected_pane() {
        let mut m = Model::new(defaults(), t0()); // NewNudge default
        let fx = update(
            &mut m,
            Msg::PanesLoaded(vec![Pane {
                target: "s:0.1".into(),
                title: String::new(),
            }]),
        );
        assert_eq!(
            fx,
            vec![Effect::CapturePane {
                pane: "s:0.1".into()
            }]
        );
    }

    #[test]
    fn q_quits_from_the_new_nudge_tab_unless_editing_text() {
        let mut m = form_model(); // NewNudge tab, focus defaults to Pane
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('q')));
        assert!(
            m.should_quit,
            "q quits from the form when not on a text field"
        );

        let mut m2 = form_model();
        m2.form.focus = FormField::Message;
        m2.form.message = MessageField::Editable(String::new());
        update(&mut m2, Msg::Key(crossterm::event::KeyCode::Char('q')));
        assert!(
            !m2.should_quit,
            "q types, not quits, while editing a text field"
        );
        assert_eq!(m2.form.message, MessageField::Editable("q".into()));
    }

    #[test]
    fn changing_pane_refreshes_the_preview_even_in_manual_mode() {
        let mut m = form_model(); // has panes s:0.1, s:0.2
        m.form.when = WhenMode::Manual;
        m.form.focus = FormField::Pane;
        m.form.preview = Some("old pane screen".into());
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Right));
        assert!(
            m.form.preview.is_none(),
            "stale preview cleared on pane change even in Manual"
        );
        assert_eq!(
            fx,
            vec![Effect::CapturePane {
                pane: "s:0.2".into()
            }]
        );
    }

    #[test]
    fn pane_captured_stores_preview_and_detection() {
        let mut m = form_model();
        update(
            &mut m,
            Msg::PaneCaptured {
                screen: Some("current session limit · resets 3:00pm".into()),
                detection: Detection::None,
            },
        );
        assert!(m.form.preview.as_deref().unwrap().contains("resets 3:00pm"));
        assert!(m.form.detected.is_some());
    }

    #[test]
    fn slash_opens_the_picker_and_captures_the_highlight() {
        let mut m = form_model(); // NewNudge tab, panes s:0.1, s:0.2, focus defaults to Pane
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/')));
        assert!(m.form.picker.is_some(), "/ opens the picker");
        assert!(fx.iter().any(|e| matches!(e, Effect::CapturePane { .. })));
    }

    #[test]
    fn enter_on_the_pane_field_opens_the_picker_not_submit() {
        let mut m = form_model();
        m.form.focus = FormField::Pane;
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Enter));
        assert!(m.form.picker.is_some());
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Schedule { .. })),
            "not a submit"
        );
    }

    #[test]
    fn typing_in_the_picker_filters_and_navigating_moves_the_highlight() {
        let mut m = form_model();
        m.form.panes = vec![
            Pane {
                target: "s:0.1".into(),
                title: "claude".into(),
            },
            Pane {
                target: "s:0.2".into(),
                title: "vim".into(),
            },
        ];
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('v')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('i')));
        let p = m.form.picker.as_ref().unwrap();
        assert_eq!(p.query, "vi");
        assert_eq!(p.matches, vec![1], "only the vim pane matches 'vi'");
    }

    #[test]
    fn enter_in_the_picker_picks_the_highlight_and_closes() {
        let mut m = form_model();
        m.form.panes = vec![
            Pane {
                target: "s:0.1".into(),
                title: "claude".into(),
            },
            Pane {
                target: "s:0.2".into(),
                title: "vim".into(),
            },
        ];
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('v'))); // filters to vim (idx 1)
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Enter));
        assert!(m.form.picker.is_none(), "picker closes on Enter");
        assert_eq!(m.form.pane_idx, 1, "the highlighted pane is now selected");
    }

    #[test]
    fn esc_then_esc_cancels_the_picker_keeping_the_prior_pane() {
        let mut m = form_model();
        m.form.pane_idx = 0;
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('v')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc)); // Insert -> Normal
        assert!(
            m.form.picker.is_some(),
            "the first Esc leaves Insert for Normal, it does not close"
        );
        assert_eq!(m.form.picker.as_ref().unwrap().mode, PickerMode::Normal);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc)); // Normal -> cancel
        assert!(m.form.picker.is_none(), "Esc in Normal cancels the picker");
        assert_eq!(
            m.form.pane_idx, 0,
            "cancel keeps the pane that was selected before"
        );
    }

    #[test]
    fn the_picker_opens_in_insert_mode() {
        let mut m = form_model();
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/')));
        assert_eq!(m.form.picker.as_ref().unwrap().mode, PickerMode::Insert);
    }

    #[test]
    fn normal_mode_jk_navigate_and_typing_does_not_filter() {
        let mut m = form_model();
        m.form.panes = vec![
            Pane {
                target: "s:0.1".into(),
                title: "claude".into(),
            },
            Pane {
                target: "s:0.2".into(),
                title: "vim".into(),
            },
        ];
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/'))); // open (Insert)
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc)); // -> Normal
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('j')));
        assert_eq!(
            m.form.picker.as_ref().unwrap().highlight,
            1,
            "j moves down in Normal"
        );
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('k')));
        assert_eq!(
            m.form.picker.as_ref().unwrap().highlight,
            0,
            "k moves up in Normal"
        );
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('z')));
        assert_eq!(
            m.form.picker.as_ref().unwrap().query,
            "",
            "typing does not filter in Normal mode"
        );
    }

    #[test]
    fn normal_mode_i_a_return_to_insert() {
        for key in ['i', 'a', 'A', 'I'] {
            let mut m = form_model();
            update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/')));
            update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc)); // Normal
            update(&mut m, Msg::Key(crossterm::event::KeyCode::Char(key)));
            assert_eq!(
                m.form.picker.as_ref().unwrap().mode,
                PickerMode::Insert,
                "{key} re-enters Insert"
            );
        }
    }

    #[test]
    fn normal_mode_q_cancels_the_picker() {
        let mut m = form_model();
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc)); // Normal
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('q')));
        assert!(
            m.form.picker.is_none(),
            "q cancels the picker in Normal mode"
        );
    }

    #[test]
    fn the_picker_captures_only_when_the_highlighted_pane_changes() {
        let mut m = form_model();
        m.form.panes = vec![
            Pane {
                target: "s:0.1".into(),
                title: "claude".into(),
            },
            Pane {
                target: "s:0.2".into(),
                title: "vim".into(),
            },
        ];
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/'))); // open; highlight = s:0.1
                                                                        // Typing 'v' filters to [vim] -> highlighted pane changes to s:0.2 -> capture.
        let fx = update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('v')));
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::CapturePane { pane } if pane == "s:0.2")));
        // Typing 'i' keeps the match [vim] -> highlighted pane unchanged -> no capture.
        let fx2 = update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('i')));
        assert!(
            !fx2.iter().any(|e| matches!(e, Effect::CapturePane { .. })),
            "no re-capture when the pane is unchanged"
        );
    }
}

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
        nav_mode: super::model::VimMode::Insert,
        cursor: 0,
        pending_op: None,
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

const FORM_ORDER: [FormField; 7] = [
    FormField::Pane,
    FormField::When,
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
    // Leaving a blank manual time abandons it back to Auto (don't strand an
    // empty Manual behind you). Then reconcile the field we land on.
    if form.focus == FormField::When && form.when == WhenMode::Manual && form.manual_time.is_empty()
    {
        form.when = WhenMode::Auto;
    }
    form.focus = FORM_ORDER[next];
    // Landing on the When field in Insert makes it a manual input.
    sync_when_editing(form);
    // Land the edit cursor at the end of the new field's text (0 for non-text)
    // and drop any half-typed operator — it doesn't carry across fields.
    form.cursor = form.text_len();
    // In Normal the cursor sits ON a char, so pull it back off the append slot;
    // otherwise the first x/D/i after a field change is an off-by-one no-op.
    if form.nav_mode == super::model::VimMode::Normal {
        clamp_cursor_normal(form);
    }
    form.pending_op = None;
}

/// Change the focused field's value (←→ or h/l): cycle the selected pane or the
/// When mode. Toggle fields are handled by Space; text fields aren't changed
/// here (Insert mode edits them via typing).
fn change_value(model: &mut Model, dir: i32) -> Vec<Effect> {
    match model.form.focus {
        FormField::Pane if !model.form.panes.is_empty() => {
            let n = model.form.panes.len() as i32;
            model.form.pane_idx = ((model.form.pane_idx as i32 + dir).rem_euclid(n)) as usize;
            // Always refresh: the preview title tracks the selected pane live, so
            // a stale screen from the old pane would otherwise show under the new
            // pane's name until the next tick. Re-detecting in non-Auto is
            // harmless — `submit` only reads `detected` in Auto mode.
            capture_selected(&mut model.form, model.now)
        }
        FormField::When => {
            // The selector ring is Keep/Auto only — Manual is entered by typing,
            // so change_value never reaches it (When-while-Manual is a text field
            // where h/l move the cursor instead).
            model.form.when = cycle_when(model.form.when, dir, model.form.mode);
            if model.form.when == WhenMode::Auto {
                capture_selected(&mut model.form, model.now)
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Handle a key on the New-nudge form. Modal, vim-style: the form opens in
/// `Insert`. On the text fields (message, When-while-Manual) the modes act as a
/// vim line editor with a cursor; on the selector fields (pane, toggles,
/// When-as-Auto/Keep) `h/l`/`←→` cycle the value. Keys that mean the same
/// everywhere (leave the tab, ↑↓ fields, Enter) are handled first.
fn form_key(model: &mut Model, code: KeyCode) -> Vec<Effect> {
    use super::model::VimMode;
    let text = model.form.focused_text().is_some();

    // --- universal keys (both modes, any field) ---
    // These leave the field (or the tab), so a half-typed operator is abandoned;
    // Up/Down clear it via move_focus, the rest clear it here so a stale `d`
    // can't eat the next keypress after a round-trip.
    match code {
        KeyCode::Tab => {
            model.form.pending_op = None;
            model.tab = Tab::Jobs;
            return vec![];
        }
        KeyCode::Up => {
            move_focus(&mut model.form, -1);
            return vec![];
        }
        KeyCode::Down => {
            move_focus(&mut model.form, 1);
            return vec![];
        }
        // Enter opens the picker on the Pane field, otherwise schedules.
        KeyCode::Enter if model.form.focus == FormField::Pane => {
            model.form.pending_op = None;
            return open_picker(model);
        }
        KeyCode::Enter => {
            model.form.pending_op = None;
            return submit(model);
        }
        _ => {}
    }

    match model.form.nav_mode {
        VimMode::Insert => insert_key(model, code, text),
        VimMode::Normal => normal_key(model, code, text),
    }
}

/// Insert mode: type into the focused text field at the cursor. On selector
/// fields, `←→` still cycle the value and Space toggles a flag.
fn insert_key(model: &mut Model, code: KeyCode, text: bool) -> Vec<Effect> {
    match code {
        // Esc drops to Normal — it does NOT leave the tab (that's Normal's Esc).
        // The cursor steps left onto a character, as vim does leaving insert.
        KeyCode::Esc => {
            model.form.nav_mode = super::model::VimMode::Normal;
            // Back in Normal, a blank manual time reverts to Auto.
            sync_when_editing(&mut model.form);
            clamp_cursor_normal(&mut model.form);
            vec![]
        }
        KeyCode::Left if text => {
            cursor_left(&mut model.form);
            vec![]
        }
        KeyCode::Right if text => {
            cursor_right_insert(&mut model.form);
            vec![]
        }
        KeyCode::Left => change_value(model, -1),
        KeyCode::Right => change_value(model, 1),
        KeyCode::Backspace => {
            backspace(&mut model.form);
            vec![]
        }
        KeyCode::Char(' ') if is_flag(model.form.focus) => {
            toggle_flag(&mut model.form);
            vec![]
        }
        // `/` opens the pane picker unless you're typing into a text field.
        KeyCode::Char('/') if !text => open_picker(model),
        // Typing on the When field starts (or continues) the manual time: any
        // text switches it out of Auto/Keep into Manual, at a fresh cursor.
        KeyCode::Char(c) if model.form.focus == FormField::When => {
            if model.form.when != WhenMode::Manual {
                model.form.when = WhenMode::Manual;
                model.form.cursor = 0;
            }
            insert_char(&mut model.form, c);
            vec![]
        }
        KeyCode::Char(c) if text => {
            insert_char(&mut model.form, c);
            vec![]
        }
        _ => vec![],
    }
}

/// Normal mode: vim motions/operators on a text field; value cycling on a
/// selector field. `j/k` move between fields either way.
fn normal_key(model: &mut Model, code: KeyCode, text: bool) -> Vec<Effect> {
    // A pending operator (`d`/`c`) consumes this key as its motion.
    if let Some(op) = model.form.pending_op.take() {
        return apply_operator(model, op, code);
    }
    match code {
        KeyCode::Char('j') => {
            move_focus(&mut model.form, 1);
            vec![]
        }
        KeyCode::Char('k') => {
            move_focus(&mut model.form, -1);
            vec![]
        }
        KeyCode::Char('i') => enter_insert(model, InsertAt::Here),
        KeyCode::Char('a') => enter_insert(model, InsertAt::After),
        KeyCode::Char('A') => enter_insert(model, InsertAt::End),
        KeyCode::Char('I') => enter_insert(model, InsertAt::Start),
        KeyCode::Char('/') => open_picker(model),
        KeyCode::Char('q') => {
            model.should_quit = true;
            vec![]
        }
        // Esc from Normal leaves the form for the Jobs tab.
        KeyCode::Esc => {
            model.tab = Tab::Jobs;
            vec![]
        }
        _ if text => normal_text_key(model, code),
        _ => normal_selector_key(model, code),
    }
}

/// Normal-mode keys on a text field: cursor motions, single-key deletes, and the
/// `d`/`c` operators (which stash `pending_op` for the next key).
fn normal_text_key(model: &mut Model, code: KeyCode) -> Vec<Effect> {
    let chars: Vec<char> = model.form.focused_text().unwrap_or("").chars().collect();
    let cur = model.form.cursor.min(chars.len());
    match code {
        KeyCode::Char('h') | KeyCode::Left => cursor_left(&mut model.form),
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') => {
            cursor_right_normal(&mut model.form)
        }
        KeyCode::Char('0') | KeyCode::Home => model.form.cursor = 0,
        KeyCode::Char('$') | KeyCode::End => model.form.cursor = chars.len().saturating_sub(1),
        KeyCode::Char('w') => {
            model.form.cursor = next_word(&chars, cur).min(chars.len().saturating_sub(1))
        }
        KeyCode::Char('b') => model.form.cursor = prev_word(&chars, cur),
        // x deletes the char under the cursor; D/C delete to end (C then inserts).
        KeyCode::Char('x') => {
            delete_char_range(&mut model.form, cur, cur + 1);
            clamp_cursor_normal(&mut model.form);
        }
        KeyCode::Char('D') => {
            delete_char_range(&mut model.form, cur, chars.len());
            clamp_cursor_normal(&mut model.form);
        }
        KeyCode::Char('C') => {
            // Enter Insert first so emptying the manual time keeps it Manual.
            model.form.nav_mode = super::model::VimMode::Insert;
            delete_char_range(&mut model.form, cur, chars.len());
        }
        KeyCode::Char(c @ ('d' | 'c')) => model.form.pending_op = Some(c),
        _ => {}
    }
    vec![]
}

/// Normal-mode keys on a selector field: cycle the value or toggle a flag.
fn normal_selector_key(model: &mut Model, code: KeyCode) -> Vec<Effect> {
    match code {
        KeyCode::Char('h') | KeyCode::Left => change_value(model, -1),
        KeyCode::Char('l') | KeyCode::Right => change_value(model, 1),
        KeyCode::Char(' ') if is_flag(model.form.focus) => {
            toggle_flag(&mut model.form);
            vec![]
        }
        _ => vec![],
    }
}

/// Complete a `d`/`c` operator with `code` as its motion. Deletes the motion's
/// char range (whole line when the operator key repeats, e.g. `dd`); `c` then
/// enters Insert. An unrecognized motion cancels the operator.
fn apply_operator(model: &mut Model, op: char, code: KeyCode) -> Vec<Effect> {
    let Some(text) = model.form.focused_text() else {
        return vec![];
    };
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let cur = model.form.cursor.min(n);
    let range = match code {
        // `dd` / `cc` — the whole line.
        KeyCode::Char(c) if c == op => Some((0, n)),
        KeyCode::Char('w') => Some((cur, next_word(&chars, cur))),
        KeyCode::Char('b') => Some((prev_word(&chars, cur), cur)),
        KeyCode::Char('0') | KeyCode::Home => Some((0, cur)),
        KeyCode::Char('$') | KeyCode::End => Some((cur, n)),
        KeyCode::Char('h') | KeyCode::Left => Some((cur.saturating_sub(1), cur)),
        KeyCode::Char('l') | KeyCode::Right => Some((cur, (cur + 1).min(n))),
        _ => None, // unrecognized motion cancels the operator
    };
    let Some((a, b)) = range else {
        return vec![];
    };
    // `c` enters Insert before the delete, so emptying a manual time keeps it
    // Manual (a blank manual input) rather than snapping to Auto.
    if op == 'c' {
        model.form.nav_mode = super::model::VimMode::Insert;
    }
    delete_char_range(&mut model.form, a, b);
    if op == 'd' {
        clamp_cursor_normal(&mut model.form);
    }
    vec![]
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
        mode: super::model::VimMode::Insert,
    });
    capture_selected(&mut model.form, model.now)
}

fn picker_key(model: &mut Model, code: KeyCode) -> Vec<Effect> {
    use super::model::VimMode;
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
        VimMode::Insert => match code {
            // Esc leaves Insert for Normal — it does NOT close the picker.
            KeyCode::Esc => {
                if let Some(p) = model.form.picker.as_mut() {
                    p.mode = VimMode::Normal;
                }
                vec![]
            }
            KeyCode::Backspace => picker_filter(model, |q| {
                q.pop();
            }),
            KeyCode::Char(c) => picker_filter(model, |q| q.push(c)),
            _ => vec![],
        },
        VimMode::Normal => match code {
            KeyCode::Char('j') => picker_move(model, 1),
            KeyCode::Char('k') => picker_move(model, -1),
            // The usual vim ways back into Insert; a single-line search box has
            // no distinct cursor position, so i/a/A/I are all equivalent.
            KeyCode::Char('i' | 'a' | 'A' | 'I') => {
                if let Some(p) = model.form.picker.as_mut() {
                    p.mode = VimMode::Insert;
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

/// Move the picker highlight by `delta`, wrapping around the ends of the match
/// list, re-capturing the preview only when the highlighted pane actually changes.
fn picker_move(model: &mut Model, delta: i32) -> Vec<Effect> {
    let before = model.form.active_pane().map(|p| p.target.clone());
    if let Some(picker) = model.form.picker.as_mut() {
        if !picker.matches.is_empty() {
            let n = picker.matches.len() as i32;
            picker.highlight = (picker.highlight as i32 + delta).rem_euclid(n) as usize;
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

/// The selector ring for the When field: Keep/Auto while editing, Auto alone for
/// a new nudge. Manual is deliberately excluded — you enter it by typing a time
/// (and leave it by erasing), so it is never reached by cycling.
fn cycle_when(cur: WhenMode, dir: i32, mode: super::model::Mode) -> WhenMode {
    let opts: &[WhenMode] = match mode {
        super::model::Mode::Editing(_) => &[WhenMode::Keep, WhenMode::Auto],
        super::model::Mode::New => &[WhenMode::Auto],
    };
    let i = opts.iter().position(|w| *w == cur).unwrap_or(0) as i32;
    opts[((i + dir).rem_euclid(opts.len() as i32)) as usize]
}

fn is_flag(f: FormField) -> bool {
    matches!(
        f,
        FormField::Verify | FormField::Notify | FormField::AutoRetry
    )
}

fn toggle_flag(form: &mut super::model::Form) {
    match form.focus {
        FormField::Verify => form.verify = !form.verify,
        FormField::Notify => form.notify = !form.notify,
        FormField::AutoRetry => form.auto_retry = !form.auto_retry,
        _ => {}
    }
}

/// Where `i`/`a`/`A`/`I` place the cursor before entering Insert.
enum InsertAt {
    Here,
    After,
    End,
    Start,
}

fn enter_insert(model: &mut Model, at: InsertAt) -> Vec<Effect> {
    model.form.nav_mode = super::model::VimMode::Insert;
    // Focusing the When field in Insert turns it into a manual input (even blank).
    sync_when_editing(&mut model.form);
    let len = model.form.text_len();
    if model.form.focused_text().is_some() {
        model.form.cursor = match at {
            InsertAt::Here => model.form.cursor.min(len),
            InsertAt::After => (model.form.cursor + 1).min(len),
            InsertAt::End => len,
            InsertAt::Start => 0,
        };
    }
    vec![]
}

// --- text field editing (operates on the focused buffer + cursor) ---

fn cursor_left(form: &mut super::model::Form) {
    form.cursor = form.cursor.saturating_sub(1);
}

/// Insert mode lets the cursor sit one past the last char (append position).
fn cursor_right_insert(form: &mut super::model::Form) {
    form.cursor = (form.cursor + 1).min(form.text_len());
}

/// Normal mode keeps the cursor on a character (never past the last one).
fn cursor_right_normal(form: &mut super::model::Form) {
    form.cursor = (form.cursor + 1).min(form.text_len().saturating_sub(1));
}

/// Pull the cursor back onto a character (used when leaving Insert / after a
/// delete): clamp to the last char, or 0 for an empty field.
fn clamp_cursor_normal(form: &mut super::model::Form) {
    form.cursor = form.cursor.min(form.text_len().saturating_sub(1));
}

/// Byte offset of char index `idx` in `s` (its length when `idx` is past the end).
fn char_byte(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map_or(s.len(), |(b, _)| b)
}

/// Reconcile the When field's Auto/Manual state with how it's being edited:
/// focusing it in Insert shows a (possibly empty) manual input for clarity, and
/// a blank time reverts to Auto only once you're back in Normal or focused
/// elsewhere. `Keep` is committed (only typing leaves it) and other fields are
/// untouched, so this is safe to call after any focus / mode / buffer change.
fn sync_when_editing(form: &mut super::model::Form) {
    if form.focus != FormField::When || form.when == WhenMode::Keep {
        return;
    }
    let editing = form.nav_mode == super::model::VimMode::Insert;
    form.when = if editing || !form.manual_time.is_empty() {
        WhenMode::Manual
    } else {
        WhenMode::Auto
    };
}

/// Apply `f` to (the focused text buffer, the cursor). Editing the When field's
/// time then reconciles the mode via [`sync_when_editing`]. Callers only run this
/// in a text context, so `Keep` (a selector state) is never touched here.
fn edit_focused(form: &mut super::model::Form, f: impl FnOnce(&mut String, &mut usize)) {
    match form.focus {
        FormField::Message => {
            if let MessageField::Editable(s) = &mut form.message {
                f(s, &mut form.cursor);
            }
        }
        FormField::When if form.when == WhenMode::Manual => {
            f(&mut form.manual_time, &mut form.cursor);
            sync_when_editing(form);
        }
        _ => {}
    }
}

fn insert_char(form: &mut super::model::Form, c: char) {
    edit_focused(form, |s, cur| {
        let at = char_byte(s, *cur);
        s.insert(at, c);
        *cur += 1;
    });
}

fn backspace(form: &mut super::model::Form) {
    edit_focused(form, |s, cur| {
        if *cur > 0 {
            let start = char_byte(s, *cur - 1);
            let end = char_byte(s, *cur);
            s.replace_range(start..end, "");
            *cur -= 1;
        }
    });
}

/// Delete char range `[a, b)` from the focused buffer and leave the cursor at `a`.
fn delete_char_range(form: &mut super::model::Form, a: usize, b: usize) {
    edit_focused(form, |s, cur| {
        let n = s.chars().count();
        let (a, b) = (a.min(n), b.min(n));
        if a < b {
            let (ba, bb) = (char_byte(s, a), char_byte(s, b));
            s.replace_range(ba..bb, "");
        }
        *cur = a;
    });
}

/// vim word class: word chars (alphanumeric + `_`), whitespace, or punctuation.
fn char_class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// Char index of the next word start (vim `w`): skip the current run, then spaces.
fn next_word(chars: &[char], i: usize) -> usize {
    let n = chars.len();
    if i >= n {
        return n;
    }
    let mut j = i;
    let cls = char_class(chars[j]);
    if cls != 0 {
        while j < n && char_class(chars[j]) == cls {
            j += 1;
        }
    }
    while j < n && char_class(chars[j]) == 0 {
        j += 1;
    }
    j
}

/// Char index of the previous word start (vim `b`): skip spaces back, then the run.
fn prev_word(chars: &[char], i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    let mut j = i - 1;
    while j > 0 && char_class(chars[j]) == 0 {
        j -= 1;
    }
    if char_class(chars[j]) == 0 {
        return 0;
    }
    let cls = char_class(chars[j]);
    while j > 0 && char_class(chars[j - 1]) == cls {
        j -= 1;
    }
    j
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
    let retry_base = form.retry_base(model.defaults.retries);
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
    fn typing_on_when_enters_manual_and_erasing_reverts_to_auto_in_normal() {
        let mut m = form_model(); // opens in Insert
        m.form.focus = FormField::When;
        assert_eq!(m.form.when, WhenMode::Auto, "a new nudge starts on Auto");

        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('3')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('p')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('m')));
        assert_eq!(m.form.when, WhenMode::Manual, "typing a time enters Manual");
        assert_eq!(m.form.manual_time, "3pm");

        // Erasing in Insert keeps a (blank) manual input for clarity — it does
        // NOT snap back to Auto while you're still typing.
        for _ in 0..3 {
            update(&mut m, Msg::Key(crossterm::event::KeyCode::Backspace));
        }
        assert_eq!(m.form.manual_time, "");
        assert_eq!(
            m.form.when,
            WhenMode::Manual,
            "a blank manual stays Manual while in Insert"
        );

        // Only on returning to Normal does a blank time revert to Auto.
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc));
        assert_eq!(
            m.form.when,
            WhenMode::Auto,
            "blank + Normal reverts to Auto"
        );
    }

    #[test]
    fn entering_insert_on_the_when_field_shows_a_manual_input() {
        // Pressing `i` on an Auto When shows an (empty) manual input, so it's
        // clear you can type a time.
        let mut m = form_model();
        m.form.focus = FormField::When;
        m.form.nav_mode = VimMode::Normal; // shows "auto → …"
        assert_eq!(m.form.when, WhenMode::Auto);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('i')));
        assert_eq!(
            m.form.when,
            WhenMode::Manual,
            "insert on When → manual input"
        );
        assert_eq!(m.form.manual_time, "");
        // Leaving without typing reverts to Auto.
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc));
        assert_eq!(m.form.when, WhenMode::Auto, "blank on Esc → Auto");
    }

    #[test]
    fn leaving_a_blank_manual_input_reverts_to_auto() {
        // Navigating onto When in Insert shows a blank manual input; navigating
        // away without typing must not leave an empty Manual behind.
        let mut m = form_model(); // Insert, focus Pane
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down)); // Pane -> When
        assert_eq!(m.form.focus, FormField::When);
        assert_eq!(
            m.form.when,
            WhenMode::Manual,
            "on When in Insert → manual input"
        );
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down)); // When -> Message
        assert_eq!(
            m.form.when,
            WhenMode::Auto,
            "blank Manual abandoned on leave"
        );
    }

    #[test]
    fn a_no_op_backspace_on_when_preserves_keep() {
        // Regression: editing an existing job opens on Keep with an empty time.
        // A stray Backspace on the When field (nothing to erase) must NOT flip
        // Keep → Auto and quietly re-time the job.
        let mut m = form_model();
        m.form.mode = Mode::Editing(7);
        m.form.when = WhenMode::Keep;
        m.form.focus = FormField::When;
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Backspace));
        assert_eq!(
            m.form.when,
            WhenMode::Keep,
            "Keep survives a no-op Backspace"
        );
        // But typing still overrides Keep → Manual.
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('3')));
        assert_eq!(m.form.when, WhenMode::Manual);
        assert_eq!(m.form.manual_time, "3");
    }

    #[test]
    fn a_space_is_a_valid_manual_time_character() {
        let mut m = form_model();
        m.form.focus = FormField::When;
        m.form.when = WhenMode::Manual;
        m.form.manual_time = "in 90".into();
        m.form.cursor = 5; // at the end
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char(' ')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('m')));
        assert_eq!(m.form.manual_time, "in 90 m");
    }

    #[test]
    fn hl_move_the_cursor_within_a_manual_time() {
        // When it's Manual, the When field is a text field: h/l move the cursor
        // (they no longer cycle the mode), and inserts land at the cursor.
        let mut m = form_model();
        m.form.focus = FormField::When;
        m.form.when = WhenMode::Manual;
        m.form.manual_time = "3pm".into();
        m.form.cursor = 3;
        m.form.nav_mode = VimMode::Normal;
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('h')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('h')));
        assert_eq!(m.form.cursor, 1, "h moves left, staying on a char");
        assert_eq!(m.form.when, WhenMode::Manual, "h does not cycle the mode");
        // Insert before the cursor: "3" | "pm" -> "3<X>pm".
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('i')));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('0')));
        assert_eq!(m.form.manual_time, "30pm");
    }

    #[test]
    fn space_toggles_the_focused_flag() {
        let mut m = form_model();
        m.form.focus = FormField::Verify;
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char(' ')));
        assert!(m.form.verify);
    }

    /// A Normal-mode message field at a given cursor, for the vim-editing tests.
    fn msg_normal(text: &str, cursor: usize) -> Model {
        let mut m = form_model();
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable(text.into());
        m.form.cursor = cursor;
        m.form.nav_mode = VimMode::Normal;
        m
    }

    fn press(m: &mut Model, c: char) {
        update(m, Msg::Key(crossterm::event::KeyCode::Char(c)));
    }

    #[test]
    fn normal_hl_0_dollar_move_the_cursor() {
        let mut m = msg_normal("hello", 0);
        press(&mut m, '$');
        assert_eq!(m.form.cursor, 4, "$ goes to the last char");
        press(&mut m, 'h');
        assert_eq!(m.form.cursor, 3, "h moves left");
        press(&mut m, '0');
        assert_eq!(m.form.cursor, 0, "0 goes to the start");
        press(&mut m, 'l');
        assert_eq!(m.form.cursor, 1, "l moves right");
        // l stops on the last char, never past it.
        for _ in 0..10 {
            press(&mut m, 'l');
        }
        assert_eq!(m.form.cursor, 4, "l never moves past the last char");
    }

    #[test]
    fn normal_wb_move_by_word() {
        let mut m = msg_normal("hello world", 0);
        press(&mut m, 'w');
        assert_eq!(m.form.cursor, 6, "w jumps to the next word");
        press(&mut m, 'b');
        assert_eq!(m.form.cursor, 0, "b jumps back to the word start");
    }

    #[test]
    fn x_deletes_the_char_under_the_cursor() {
        let mut m = msg_normal("abc", 1);
        press(&mut m, 'x');
        assert_eq!(m.form.message, MessageField::Editable("ac".into()));
        assert_eq!(m.form.cursor, 1, "cursor stays on a char");
    }

    #[test]
    fn cap_d_deletes_to_end_of_line() {
        let mut m = msg_normal("hello", 2);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('D')));
        assert_eq!(m.form.message, MessageField::Editable("he".into()));
        assert_eq!(m.form.cursor, 1);
    }

    #[test]
    fn cap_c_changes_to_end_then_inserts() {
        let mut m = msg_normal("hello", 2);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('C')));
        assert_eq!(m.form.message, MessageField::Editable("he".into()));
        assert_eq!(m.form.nav_mode, VimMode::Insert, "C enters Insert");
        press(&mut m, 'y');
        assert_eq!(m.form.message, MessageField::Editable("hey".into()));
    }

    #[test]
    fn dd_clears_the_whole_line() {
        let mut m = msg_normal("hello", 3);
        press(&mut m, 'd');
        assert_eq!(m.form.pending_op, Some('d'), "d waits for a motion");
        press(&mut m, 'd');
        assert_eq!(m.form.message, MessageField::Editable(String::new()));
        assert_eq!(m.form.cursor, 0);
        assert_eq!(m.form.pending_op, None);
    }

    #[test]
    fn dw_deletes_a_word_and_db_deletes_the_one_before() {
        let mut m = msg_normal("foo bar", 0);
        press(&mut m, 'd');
        press(&mut m, 'w');
        assert_eq!(m.form.message, MessageField::Editable("bar".into()));

        let mut m2 = msg_normal("foo bar baz", 8); // cursor on the "baz" b
        press(&mut m2, 'd');
        press(&mut m2, 'b');
        assert_eq!(m2.form.message, MessageField::Editable("foo baz".into()));
    }

    #[test]
    fn an_operator_then_a_bad_motion_cancels_harmlessly() {
        let mut m = msg_normal("hello", 2);
        press(&mut m, 'd');
        press(&mut m, 'z'); // not a motion
        assert_eq!(m.form.message, MessageField::Editable("hello".into()));
        assert_eq!(m.form.pending_op, None, "the operator is cancelled");
    }

    #[test]
    fn insert_keys_land_at_the_right_place() {
        // i — before the cursor.
        let mut m = msg_normal("cat", 1);
        press(&mut m, 'i');
        press(&mut m, 'X');
        assert_eq!(m.form.message, MessageField::Editable("cXat".into()));
        // a — after the cursor.
        let mut m = msg_normal("cat", 1);
        press(&mut m, 'a');
        press(&mut m, 'X');
        assert_eq!(m.form.message, MessageField::Editable("caXt".into()));
        // A — at the end.
        let mut m = msg_normal("cat", 1);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('A')));
        press(&mut m, 'X');
        assert_eq!(m.form.message, MessageField::Editable("catX".into()));
        // I — at the start.
        let mut m = msg_normal("cat", 1);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('I')));
        press(&mut m, 'X');
        assert_eq!(m.form.message, MessageField::Editable("Xcat".into()));
    }

    #[test]
    fn insert_backspace_deletes_before_the_cursor() {
        let mut m = form_model();
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable("abc".into());
        m.form.cursor = 2; // between b and c
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Backspace));
        assert_eq!(m.form.message, MessageField::Editable("ac".into()));
        assert_eq!(m.form.cursor, 1);
    }

    #[test]
    fn changing_field_clears_a_pending_operator() {
        let mut m = msg_normal("hello", 2);
        press(&mut m, 'd');
        assert_eq!(m.form.pending_op, Some('d'));
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down));
        assert_eq!(m.form.pending_op, None, "moving fields drops the operator");
    }

    #[test]
    fn leaving_the_tab_clears_a_pending_operator() {
        let mut m = msg_normal("hello", 2);
        press(&mut m, 'd');
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Tab));
        assert_eq!(m.form.pending_op, None, "Tab away drops the operator");
    }

    #[test]
    fn the_first_edit_after_a_field_change_is_not_off_by_one() {
        // Regression: move_focus left the cursor one past the end, so the first
        // x/D landed on nothing. Navigating onto a text field must leave the
        // cursor on the last char in Normal mode.
        let mut m = form_model();
        m.form.message = MessageField::Editable("hello".into());
        m.form.nav_mode = VimMode::Normal;
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down)); // Pane -> When
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down)); // When -> Message
        assert_eq!(m.form.focus, FormField::Message);
        assert_eq!(
            m.form.cursor, 4,
            "cursor rests on the last char, not past it"
        );
        press(&mut m, 'x');
        assert_eq!(m.form.message, MessageField::Editable("hell".into()));
    }

    #[test]
    fn editing_handles_multibyte_chars() {
        // "café" — 'é' is 2 bytes, so char/byte indexing must not split it.
        let mut m = msg_normal("café", 3); // cursor on 'é'
        press(&mut m, 'x');
        assert_eq!(m.form.message, MessageField::Editable("caf".into()));

        // Insert a multibyte char at the cursor, then backspace it.
        let mut m = form_model();
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable("ab".into());
        m.form.cursor = 1; // between a and b
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('é')));
        assert_eq!(m.form.message, MessageField::Editable("aéb".into()));
        assert_eq!(m.form.cursor, 2);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Backspace));
        assert_eq!(m.form.message, MessageField::Editable("ab".into()));
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
    fn the_form_opens_in_insert_mode() {
        let m = form_model();
        assert_eq!(m.form.nav_mode, VimMode::Insert);
    }

    #[test]
    fn q_quits_only_in_normal_mode() {
        // Insert (the default): q on a non-text field is a no-op, not a quit.
        let mut m = form_model(); // focus defaults to Pane
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('q')));
        assert!(!m.should_quit, "q does not quit while in Insert mode");

        // Normal mode: q quits.
        m.form.nav_mode = VimMode::Normal;
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('q')));
        assert!(m.should_quit, "q quits from Normal mode");

        // Insert mode on a text field: q types, never quits.
        let mut m2 = form_model();
        m2.form.focus = FormField::Message;
        m2.form.message = MessageField::Editable(String::new());
        update(&mut m2, Msg::Key(crossterm::event::KeyCode::Char('q')));
        assert!(!m2.should_quit, "q types, not quits, in a text field");
        assert_eq!(m2.form.message, MessageField::Editable("q".into()));
    }

    #[test]
    fn esc_drops_to_normal_then_leaves_the_form() {
        let mut m = form_model(); // Insert
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc));
        assert_eq!(m.form.nav_mode, VimMode::Normal, "first Esc → Normal");
        assert_eq!(m.tab, Tab::NewNudge, "first Esc stays on the form");

        update(&mut m, Msg::Key(crossterm::event::KeyCode::Esc));
        assert_eq!(m.tab, Tab::Jobs, "Esc from Normal leaves for Jobs");
    }

    #[test]
    fn insert_keys_all_return_to_insert_from_normal() {
        for key in ['i', 'a', 'A', 'I'] {
            let mut m = form_model();
            m.form.nav_mode = VimMode::Normal;
            update(&mut m, Msg::Key(crossterm::event::KeyCode::Char(key)));
            assert_eq!(m.form.nav_mode, VimMode::Insert, "{key} → Insert");
        }
    }

    #[test]
    fn normal_jk_move_the_focused_field() {
        let mut m = form_model();
        m.form.nav_mode = VimMode::Normal;
        assert_eq!(m.form.focus, FormField::Pane);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('j')));
        assert_eq!(m.form.focus, FormField::When, "j moves down a field");
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('k')));
        assert_eq!(m.form.focus, FormField::Pane, "k moves back up");
    }

    #[test]
    fn normal_hl_change_the_focused_value() {
        let mut m = form_model();
        m.form.nav_mode = VimMode::Normal;
        m.form.focus = FormField::Pane; // panes: s:0.1, s:0.2
        assert_eq!(m.form.pane_idx, 0);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('l')));
        assert_eq!(m.form.pane_idx, 1, "l cycles the pane forward");
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('h')));
        assert_eq!(m.form.pane_idx, 0, "h cycles the pane back");
    }

    #[test]
    fn normal_jkhl_are_literal_text_in_insert() {
        let mut m = form_model();
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable(String::new());
        for key in ['h', 'j', 'k', 'l'] {
            update(&mut m, Msg::Key(crossterm::event::KeyCode::Char(key)));
        }
        assert_eq!(m.form.message, MessageField::Editable("hjkl".into()));
    }

    #[test]
    fn space_types_into_a_message_but_toggles_a_flag() {
        // Insert mode on the message: Space inserts a literal space at the cursor.
        let mut m = form_model();
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable("go".into());
        m.form.cursor = 2; // at the end
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char(' ')));
        assert_eq!(m.form.message, MessageField::Editable("go ".into()));

        // Space on a flag field toggles it, in either mode.
        let mut m2 = form_model();
        m2.form.focus = FormField::Verify;
        assert!(!m2.form.verify);
        update(&mut m2, Msg::Key(crossterm::event::KeyCode::Char(' ')));
        assert!(m2.form.verify, "space toggles verify in Insert");
        m2.form.nav_mode = VimMode::Normal;
        update(&mut m2, Msg::Key(crossterm::event::KeyCode::Char(' ')));
        assert!(!m2.form.verify, "space toggles verify in Normal too");
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
    fn picker_up_down_wrap_around_the_ends() {
        let mut m = form_model();
        m.form.panes = vec![
            Pane {
                target: "s:0.1".into(),
                title: "a".into(),
            },
            Pane {
                target: "s:0.2".into(),
                title: "b".into(),
            },
            Pane {
                target: "s:0.3".into(),
                title: "c".into(),
            },
        ];
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Char('/'))); // opens, highlight 0
        assert_eq!(m.form.picker.as_ref().unwrap().highlight, 0);
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Up));
        assert_eq!(
            m.form.picker.as_ref().unwrap().highlight,
            2,
            "Up from the top wraps to the bottom"
        );
        update(&mut m, Msg::Key(crossterm::event::KeyCode::Down));
        assert_eq!(
            m.form.picker.as_ref().unwrap().highlight,
            0,
            "Down from the bottom wraps to the top"
        );
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
        assert_eq!(m.form.picker.as_ref().unwrap().mode, VimMode::Normal);
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
        assert_eq!(m.form.picker.as_ref().unwrap().mode, VimMode::Insert);
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
                VimMode::Insert,
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

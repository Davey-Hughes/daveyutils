//! Dashboard state and its pure helpers.

/// Format a signed seconds delta as a compact countdown: `2h 14m`, `6d 3h`,
/// `45m`, `12s`. A non-positive delta (the job's time is here or past) renders
/// as `now` — the daemon fires within its grace window, and a dashboard that
/// showed a negative countdown would look broken.
pub fn human_countdown(delta_secs: i64) -> String {
    if delta_secs <= 0 {
        return "now".to_string();
    }
    let d = delta_secs / 86_400;
    let h = (delta_secs % 86_400) / 3_600;
    let m = (delta_secs % 3_600) / 60;
    let s = delta_secs % 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_formats_by_largest_unit() {
        assert_eq!(human_countdown(45 * 60), "45m");
        assert_eq!(human_countdown(2 * 3600 + 14 * 60), "2h 14m");
        assert_eq!(human_countdown(6 * 86400 + 3 * 3600), "6d 3h");
        assert_eq!(human_countdown(12), "12s");
    }

    #[test]
    fn a_past_or_zero_delta_reads_now_not_a_negative() {
        assert_eq!(human_countdown(0), "now");
        assert_eq!(human_countdown(-500), "now");
    }

    #[test]
    fn the_dashboard_opens_on_the_new_nudge_tab() {
        let m = Model::new(
            ScheduleDefaults {
                send_delay_secs: 0.75,
                settle_secs: 5.0,
                retries: 2,
                tz: jiff::tz::TimeZone::UTC,
            },
            "2026-07-16T12:00:00Z".parse().unwrap(),
        );
        assert_eq!(m.tab, Tab::NewNudge);
        assert!(m.form.preview.is_none());
        assert!(m.form.last_capture.is_none());
    }

    #[test]
    fn active_pane_tracks_the_picker_highlight_then_falls_back_to_pane_idx() {
        let mut form = Form::fresh();
        form.panes = vec![
            Pane {
                target: "s:0.1".into(),
                title: "claude".into(),
            },
            Pane {
                target: "s:0.2".into(),
                title: "vim".into(),
            },
        ];
        form.pane_idx = 0;
        assert_eq!(
            form.active_pane().unwrap().target,
            "s:0.1",
            "no picker → pane_idx"
        );
        form.picker = Some(Picker {
            query: String::new(),
            matches: vec![1, 0],
            highlight: 0,
            mode: VimMode::Insert,
        });
        assert_eq!(
            form.active_pane().unwrap().target,
            "s:0.2",
            "picker → highlighted match"
        );
    }
}

use jiff::Timestamp;

use crate::detect::Detection;
use crate::job::Job;
use crate::tmux_panes::Pane;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Jobs,
    NewNudge,
}

/// Defaults for fields the form does not show, read once from the environment
/// in `tui::run` and then held in the (pure) model so `update` never reads env.
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduleDefaults {
    pub send_delay_secs: f64,
    pub settle_secs: f64,
    pub retries: i64,
    /// The system/local time zone, read once at the impure edge (`tui::run`).
    /// `submit` resolves manual clock times ("3pm") in this zone so the TUI
    /// matches the CLI's `Zoned::now()`-based resolution.
    pub tz: jiff::tz::TimeZone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormField {
    Pane,
    When,
    Message,
    Verify,
    Notify,
    AutoRetry,
    Submit,
}

/// `Keep` is only reachable while editing (leave the job's fire time alone);
/// a new nudge starts on `Auto`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WhenMode {
    Keep,
    Auto,
    Manual,
}

/// A single editable message, or — for a multi-message job being edited — the
/// count carried through unchanged (the TUI never collapses them to one line).
#[derive(Clone, PartialEq, Debug)]
pub enum MessageField {
    Editable(String),
    Preserved(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    New,
    Editing(u64),
}

/// A vim-style editing mode, shared by the pane picker and the New-nudge form:
/// Insert (typing edits the field / filters the list) or Normal (h/j/k/l
/// navigate; i/a/A/I return to Insert). Both open in `Insert`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VimMode {
    Insert,
    Normal,
}

/// The fzf-style pane picker's state, present only while the picker is open.
#[derive(Clone, PartialEq, Debug)]
pub struct Picker {
    pub query: String,
    /// Indices into `Form.panes`, best fuzzy match first.
    pub matches: Vec<usize>,
    /// Index into `matches` of the highlighted row.
    pub highlight: usize,
    /// vim-style editing mode; the picker opens in `Insert`.
    pub mode: VimMode,
}

/// The transient bottom line: last error or confirmation.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Status(pub Option<String>);

impl Status {
    pub fn set(&mut self, msg: impl Into<String>) {
        self.0 = Some(msg.into());
    }
}

/// The fields an edit carries through unchanged because the form does not show
/// them (mirrors `app::merge_edit` preserving what no flag overrode).
#[derive(Clone, PartialEq, Debug)]
pub struct CarriedEdit {
    pub fire_at: Timestamp,
    pub messages: Vec<String>,
    pub send_delay_secs: f64,
    pub settle_secs: f64,
    pub retries_left: i64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Form {
    pub panes: Vec<Pane>,
    pub pane_idx: usize,
    pub when: WhenMode,
    pub manual_time: String,
    pub detected: Option<Detection>,
    pub message: MessageField,
    pub verify: bool,
    pub notify: bool,
    pub auto_retry: bool,
    pub focus: FormField,
    pub mode: Mode,
    /// vim-style navigation mode for the form; opens in `Insert` so typing a
    /// message / time works immediately. Esc drops to `Normal` (h/j/k/l move
    /// and change fields); i/a/A/I return to `Insert`. The picker carries its
    /// own [`VimMode`] independently while open.
    pub nav_mode: VimMode,
    /// Char index of the edit cursor within the focused text field (the message,
    /// or the When field's manual time). Meaningful only while [`Form::focused_text`]
    /// is `Some`; consumers clamp it into range, so a stale value is harmless.
    pub cursor: usize,
    /// A vim operator (`d` or `c`) awaiting its motion: after `d` the next key
    /// completes `dd`, `dw`, `d$`, … `None` unless mid-operator.
    pub pending_op: Option<char>,
    pub carried: Option<CarriedEdit>,
    /// The selected pane's captured screen for the live preview; `None` when the
    /// pane could not be captured (rendered as "(preview unavailable)").
    ///
    /// Holds RAW SGR escape sequences (captured with `tmux capture-pane -e`).
    /// Only `view::preview_text` may render it — it parses the escapes into
    /// styled spans. Never feed this to a plain widget, or a pane's control
    /// bytes would reach the terminal and corrupt the dashboard.
    pub preview: Option<String>,
    /// When the preview was last (re)captured; gates the ~1.5s refresh cadence.
    pub last_capture: Option<jiff::Timestamp>,
    /// Present while the fzf pane picker is open; `None` in the normal form.
    pub picker: Option<Picker>,
}

impl Form {
    pub fn fresh() -> Form {
        Form {
            panes: Vec::new(),
            pane_idx: 0,
            when: WhenMode::Auto,
            manual_time: String::new(),
            detected: None,
            message: MessageField::Editable("please continue".to_string()),
            verify: false,
            notify: false,
            auto_retry: false,
            focus: FormField::Pane,
            mode: Mode::New,
            nav_mode: VimMode::Insert,
            cursor: 0,
            pending_op: None,
            carried: None,
            preview: None,
            last_capture: None,
            picker: None,
        }
    }

    pub fn selected_pane(&self) -> Option<&Pane> {
        self.panes.get(self.pane_idx)
    }

    /// The buffer the user is editing, if the focused field is a text field: the
    /// editable message, or the When field while it is `Manual`. `None` for
    /// selector fields (pane, toggles, When-as-Auto/Keep) and preserved messages.
    pub fn focused_text(&self) -> Option<&str> {
        match self.focus {
            FormField::Message => match &self.message {
                MessageField::Editable(s) => Some(s.as_str()),
                MessageField::Preserved(_) => None,
            },
            FormField::When if self.when == WhenMode::Manual => Some(self.manual_time.as_str()),
            _ => None,
        }
    }

    /// Char length of the focused text buffer (0 when not editing text).
    pub fn text_len(&self) -> usize {
        self.focused_text().map_or(0, |s| s.chars().count())
    }

    /// The pane the preview/detection should track: the picker's highlighted
    /// match while it is open, else the `pane_idx` selection.
    pub fn active_pane(&self) -> Option<&Pane> {
        match &self.picker {
            Some(p) => p.matches.get(p.highlight).and_then(|&i| self.panes.get(i)),
            None => self.panes.get(self.pane_idx),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Model {
    pub tab: Tab,
    pub jobs: Vec<Job>,
    pub selected: usize,
    pub form: Form,
    pub status: Status,
    pub now: Timestamp,
    pub last_poll: Timestamp,
    pub defaults: ScheduleDefaults,
    pub should_quit: bool,
}

impl Model {
    pub fn new(defaults: ScheduleDefaults, now: Timestamp) -> Model {
        Model {
            tab: Tab::NewNudge,
            jobs: Vec::new(),
            selected: 0,
            form: Form::fresh(),
            status: Status::default(),
            now,
            last_poll: now,
            defaults,
            should_quit: false,
        }
    }

    /// Keep `selected` inside `jobs` after the list changes.
    pub fn clamp_selection(&mut self) {
        if self.jobs.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.jobs.len() {
            self.selected = self.jobs.len() - 1;
        }
    }
}

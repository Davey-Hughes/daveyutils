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
    ManualTime,
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
    pub carried: Option<CarriedEdit>,
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
            carried: None,
        }
    }

    pub fn selected_pane(&self) -> Option<&Pane> {
        self.panes.get(self.pane_idx)
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
            tab: Tab::Jobs,
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

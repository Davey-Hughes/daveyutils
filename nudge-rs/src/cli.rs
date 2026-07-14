//! Command-line interface: the `clap` parser and option resolution.

use clap::Parser;

use crate::config::{env_bool, resolve, FlagOverrides, Toggles};

/// nudge — inject messages into a tmux pane at a rate-limit reset.
#[derive(Parser, Debug, Default)]
#[command(name = "nudge", version, about)]
pub struct Cli {
    /// Target tmux pane (e.g. bot:0.1). Prompts interactively if omitted.
    #[arg(short, long)]
    pub pane: Option<String>,

    /// Specific target time (e.g. "14:30" or "now + 45 min"); else auto-detect.
    #[arg(short = 'm', long = "time")]
    pub time: Option<String>,

    /// Message to inject; repeat to send several (default: "please continue").
    #[arg(short = 'i', long = "input")]
    pub input: Vec<String>,

    /// Pause between multiple sends, seconds (default 0.75).
    #[arg(short = 'w', long = "delay")]
    pub delay: Option<f64>,

    #[arg(short = 'n', long = "notify")]
    pub notify: bool,
    #[arg(long = "no-notify")]
    pub no_notify: bool,

    #[arg(short = 'a', long = "auto-retry")]
    pub auto_retry: bool,
    #[arg(long = "no-auto-retry")]
    pub no_auto_retry: bool,

    /// Exact retry count (-1 = forever). Implies --auto-retry.
    #[arg(short = 'r', long = "retries")]
    pub retries: Option<i64>,

    #[arg(short = 'v', long = "verify")]
    pub verify: bool,
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Review pending jobs (interactive).
    #[arg(short = 'l', long, visible_alias = "jobs")]
    pub list: bool,
    /// Review pending jobs as a plain table (non-interactive).
    #[arg(long = "list-plain")]
    pub list_plain: bool,
    /// Cancel a pending job by id.
    #[arg(long = "cancel", value_name = "ID")]
    pub cancel: Option<u64>,
    /// Edit a pending job by id.
    #[arg(long = "edit", value_name = "ID")]
    pub edit: Option<u64>,

    /// Run the resident scheduler daemon (foreground).
    #[arg(long = "daemon")]
    pub daemon: bool,
    /// Register the daemon with the OS service manager.
    #[arg(long = "install-daemon")]
    pub install_daemon: bool,
    /// Unregister the daemon.
    #[arg(long = "uninstall-daemon")]
    pub uninstall_daemon: bool,
}

pub(crate) fn tri(on: bool, off: bool) -> Option<bool> {
    if off {
        Some(false)
    } else if on {
        Some(true)
    } else {
        None
    }
}

/// Env defaults (`NUDGE_*`) overlaid with the CLI's flags.
pub fn resolve_options(cli: &Cli) -> Toggles {
    let env = Toggles {
        notify: env_bool(std::env::var("NUDGE_NOTIFY").ok().as_deref()),
        verify: env_bool(std::env::var("NUDGE_VERIFY").ok().as_deref()),
        auto_retry: env_bool(std::env::var("NUDGE_AUTO_RETRY").ok().as_deref()),
        retries: std::env::var("NUDGE_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2),
        settle_secs: std::env::var("NUDGE_SETTLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5.0),
    };
    let overrides = FlagOverrides {
        notify: tri(cli.notify, cli.no_notify),
        verify: tri(cli.verify, cli.no_verify),
        auto_retry: tri(cli.auto_retry, cli.no_auto_retry),
        retries: cli.retries,
        settle_secs: None,
    };
    resolve(&env, &overrides)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn parses_core_scheduling_flags() {
        let c = parse(&[
            "nudge", "-p", "bot:0.1", "-m", "3pm", "-i", "a", "-i", "b", "-w", "0.5", "-v",
        ]);
        assert_eq!(c.pane.as_deref(), Some("bot:0.1"));
        assert_eq!(c.time.as_deref(), Some("3pm"));
        assert_eq!(c.input, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(c.delay, Some(0.5));
        assert!(c.verify);
    }

    #[test]
    fn no_flags_override_env_defaults() {
        // With no CLI flags, resolve reflects the (test-provided) env absence -> false/defaults.
        let c = parse(&["nudge", "-p", "x"]);
        // Clear any inherited env for determinism.
        std::env::remove_var("NUDGE_NOTIFY");
        std::env::remove_var("NUDGE_VERIFY");
        std::env::remove_var("NUDGE_AUTO_RETRY");
        std::env::remove_var("NUDGE_RETRIES");
        let t = resolve_options(&c);
        assert!(!t.notify);
        assert_eq!(t.retries, 2);
    }

    #[test]
    fn retries_flag_implies_auto_retry() {
        let c = parse(&["nudge", "-p", "x", "-r", "5"]);
        let t = resolve_options(&c);
        assert!(t.auto_retry);
        assert_eq!(t.retries, 5);
    }

    #[test]
    fn no_notify_beats_a_bare_notify_env() {
        std::env::set_var("NUDGE_NOTIFY", "1");
        let c = parse(&["nudge", "-p", "x", "--no-notify"]);
        let t = resolve_options(&c);
        assert!(!t.notify);
        std::env::remove_var("NUDGE_NOTIFY");
    }
}

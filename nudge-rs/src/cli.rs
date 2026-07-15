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

    /// Send a desktop notification when the injection fires.
    #[arg(short = 'n', long = "notify")]
    pub notify: bool,
    /// Disable notifications (overrides NUDGE_NOTIFY).
    #[arg(long = "no-notify")]
    pub no_notify: bool,

    /// If rate-limited, autonomously schedule another nudge (default 2 retries).
    #[arg(short = 'a', long = "auto-retry")]
    pub auto_retry: bool,
    /// Disable auto-retry (overrides NUDGE_AUTO_RETRY).
    #[arg(long = "no-auto-retry")]
    pub no_auto_retry: bool,

    /// Exact retry count (-1 = forever). Implies --auto-retry.
    #[arg(short = 'r', long = "retries")]
    pub retries: Option<i64>,

    /// Before injecting, confirm the pane still shows a rate-limit banner; skip the send if not.
    #[arg(short = 'v', long = "verify")]
    pub verify: bool,
    /// Disable verification (overrides NUDGE_VERIFY).
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

    /// Print a shell completion script for SHELL (bash, zsh, fish, …) to stdout.
    #[arg(long = "completions", value_name = "SHELL")]
    pub completions: Option<clap_complete::Shell>,
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

/// The CLI's flags as overrides, with an absent flag left `None` so the env
/// default survives.
///
/// Split out of `resolve_options` as a pure seam: this mapping is the part
/// `cli.rs` actually owns, and testing it through the process environment means
/// `set_var`, which races other tests in the same binary (and is UB alongside a
/// concurrent `var`).
pub(crate) fn flag_overrides(cli: &Cli) -> FlagOverrides {
    FlagOverrides {
        notify: tri(cli.notify, cli.no_notify),
        verify: tri(cli.verify, cli.no_verify),
        auto_retry: tri(cli.auto_retry, cli.no_auto_retry),
        retries: cli.retries,
        settle_secs: None,
    }
}

/// The `NUDGE_*` environment defaults.
fn env_toggles() -> Toggles {
    Toggles {
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
    }
}

/// Env defaults (`NUDGE_*`) overlaid with the CLI's flags.
pub fn resolve_options(cli: &Cli) -> Toggles {
    resolve(&env_toggles(), &flag_overrides(cli))
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

    /// A stand-in for the `NUDGE_*` environment.
    ///
    /// These tests pass the env in explicitly rather than calling
    /// `resolve_options`, which reads the real one. Mutating the process
    /// environment to set up a test races every other test in this binary --
    /// they share one process and cargo runs them on parallel threads -- and a
    /// `set_var` concurrent with another thread's `var` is documented UB
    /// besides. Nothing here touches process-global state, so nothing here can
    /// race.
    fn env(notify: bool) -> Toggles {
        Toggles {
            notify,
            verify: false,
            auto_retry: false,
            retries: 2,
            settle_secs: 5.0,
        }
    }

    /// The env -> flags path `resolve_options` runs, minus the env read.
    fn resolve_flags(env: &Toggles, args: &[&str]) -> Toggles {
        resolve(env, &flag_overrides(&parse(args)))
    }

    #[test]
    fn no_flags_leave_the_env_defaults_alone() {
        let t = resolve_flags(&env(false), &["nudge", "-p", "x"]);
        assert!(!t.notify);
        assert_eq!(t.retries, 2);
    }

    #[test]
    fn a_bare_notify_env_survives_when_no_flag_is_given() {
        // Pins that an absent flag maps to None, not Some(false): mapping it to
        // Some(false) would silently override NUDGE_NOTIFY=1.
        let t = resolve_flags(&env(true), &["nudge", "-p", "x"]);
        assert!(t.notify);
    }

    #[test]
    fn retries_flag_implies_auto_retry() {
        let t = resolve_flags(&env(false), &["nudge", "-p", "x", "-r", "5"]);
        assert!(t.auto_retry);
        assert_eq!(t.retries, 5);
    }

    #[test]
    fn no_auto_retry_beats_a_retry_count_in_either_order() {
        // `-r` implies auto-retry, but only when the user did not say otherwise.
        // Order must not matter: both spellings are the same stated intent, and
        // clap hands them to us as flags either way.
        for args in [
            ["nudge", "-p", "x", "-r", "5", "--no-auto-retry"],
            ["nudge", "-p", "x", "--no-auto-retry", "-r", "5"],
        ] {
            let t = resolve_flags(&env(false), &args);
            assert!(
                !t.auto_retry,
                "--no-auto-retry must disable auto-retry given {args:?}"
            );
            assert_eq!(t.retries, 5, "the count is still recorded given {args:?}");
        }
    }

    #[test]
    fn no_notify_beats_a_bare_notify_env() {
        // NUDGE_NOTIFY=1 in the environment, --no-notify on the command line.
        let t = resolve_flags(&env(true), &["nudge", "-p", "x", "--no-notify"]);
        assert!(!t.notify);
    }
}

//! Core library for nudge. Side-effect-free logic used by the CLI and daemon.

/// This build's version, as carried in the IPC handshake.
///
/// The daemon is resident and auto-started: rebuilding nudge does not replace
/// the one already running, and it will not restart itself. So the CLI asks
/// what it is talking to rather than assuming it is itself.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod app;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod detect;
pub mod inject;
pub mod ipc;
pub mod job;
pub mod notify;
pub mod paths;
pub mod queue;
pub mod register;
pub mod scheduler;
pub mod target;
pub mod timespec;
pub mod tmux_panes;

/// Dispatch a parsed CLI to the right mode.
pub fn run(cli: cli::Cli) -> anyhow::Result<()> {
    if let Some(shell) = cli.completions {
        app::print_completions(shell);
        return Ok(());
    }
    if cli.daemon {
        daemon::init_tracing();
        let p = paths::resolve();
        return match daemon::run(
            &p,
            std::env::var("NUDGE_CLOCK_PATTERN").ok(),
            std::env::var("NUDGE_DURATION_PATTERN").ok(),
            jiff::ToSpan::hours(6),
        ) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Another daemon already owns this state. That is not a failure:
                // exiting non-zero makes systemd's Restart=on-failure retry every
                // RestartSec forever against a lock it can never win.
                eprintln!("nudge: {e}");
                Ok(())
            }
            Err(e) => Err(e.into()),
        };
    }
    if cli.install_daemon {
        return register::install(&std::env::current_exe()?);
    }
    if cli.uninstall_daemon {
        return register::uninstall();
    }
    // scheduling / job-management dispatch is added in Tasks 3-4 & 6.
    app::dispatch(cli)
}

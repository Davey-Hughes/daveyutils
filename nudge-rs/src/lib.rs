//! Core library for nudge. Side-effect-free logic used by the CLI and daemon.

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

/// Dispatch a parsed CLI to the right mode.
pub fn run(cli: cli::Cli) -> anyhow::Result<()> {
    if cli.daemon {
        daemon::init_tracing();
        let p = paths::resolve();
        return Ok(daemon::run(
            &p,
            std::env::var("NUDGE_CLOCK_PATTERN").ok(),
            std::env::var("NUDGE_DURATION_PATTERN").ok(),
            jiff::ToSpan::hours(6),
        )?);
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

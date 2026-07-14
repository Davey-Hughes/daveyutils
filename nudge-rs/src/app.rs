//! Command implementations for the CLI modes.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use jiff::Zoned;

use crate::cli::{resolve_options, Cli};
use crate::config::Toggles;
use crate::detect::detect_reset;
use crate::ipc::{client, Request, Response};
use crate::job::{JobSpec, TargetSpec};
use crate::paths;
use crate::target::{tmux::TmuxTarget, Target};
use crate::timespec::parse_timespec;

/// Assemble a JobSpec from resolved options.
pub fn build_spec(pane: &str, fire_at: jiff::Timestamp, cli: &Cli, opts: &Toggles) -> JobSpec {
    let messages = if cli.input.is_empty() {
        vec!["please continue".to_string()]
    } else {
        cli.input.clone()
    };
    JobSpec {
        target: TargetSpec::Tmux {
            pane: pane.to_string(),
        },
        messages,
        send_delay_secs: cli.delay.unwrap_or(0.75),
        fire_at,
        notify: opts.notify,
        verify: opts.verify,
        auto_retry: opts.auto_retry,
        retries_left: if opts.auto_retry { opts.retries } else { 0 },
        settle_secs: opts.settle_secs,
    }
}

/// Determine the fire time: explicit `-m`, else auto-detect from the pane.
pub fn fire_time(cli: &Cli, pane: &str, now: &Zoned) -> anyhow::Result<jiff::Timestamp> {
    if let Some(t) = &cli.time {
        return Ok(parse_timespec(t, now)
            .map_err(|e| anyhow::anyhow!("could not parse time '{t}': {e}"))?
            .timestamp());
    }
    let screen = TmuxTarget::new(pane)
        .capture()
        .context("capturing pane to auto-detect the rate limit")?;
    let clock_ext = std::env::var("NUDGE_CLOCK_PATTERN").ok();
    let dur_ext = std::env::var("NUDGE_DURATION_PATTERN").ok();
    match detect_reset(&screen, now, clock_ext.as_deref(), dur_ext.as_deref()) {
        Some(z) => Ok(z.timestamp()),
        None => bail!("no rate-limit banner detected in {pane}; pass -m to set a time"),
    }
}

/// Ping the daemon; if it's not answering, start it and wait for the socket.
pub fn ensure_daemon(socket: &Path) -> anyhow::Result<()> {
    if client::request(socket, &Request::Ping).is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe).arg("--daemon").spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if client::request(socket, &Request::Ping).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("daemon did not come up on {}", socket.display())
}

/// Schedule a nudge.
pub fn schedule(cli: &Cli) -> anyhow::Result<()> {
    let pane = match &cli.pane {
        Some(p) => p.clone(),
        None => crate::app::pick_pane()?,
    };
    let opts = resolve_options(cli);
    let now = Zoned::now();
    let fire_at = fire_time(cli, &pane, &now)?;
    let spec = build_spec(&pane, fire_at, cli, &opts);

    let paths = paths::resolve();
    ensure_daemon(&paths.socket)?;
    match client::request(&paths.socket, &Request::Schedule(spec))? {
        Response::Scheduled(id) => {
            println!("nudge: scheduled job {id} for {}", fire_at);
            Ok(())
        }
        Response::Error(e) => bail!("daemon rejected the job: {e}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Dispatch non-daemon modes.
pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    // list / cancel / edit are added in Task 4; interactive pick in Task 6.
    schedule(&cli)
}

// pick_pane is provided in Task 6; a temporary stub keeps this compiling until then.
pub fn pick_pane() -> anyhow::Result<String> {
    anyhow::bail!("no pane given (-p); interactive picker lands in a later task")
}

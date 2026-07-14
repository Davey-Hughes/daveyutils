//! Command implementations for the CLI modes.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use jiff::Zoned;

use crate::cli::{resolve_options, Cli};
use crate::config::Toggles;
use crate::detect::detect_reset;
use crate::ipc::{client, Request, Response};
use crate::job::{Job, JobSpec, TargetSpec};
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
    std::process::Command::new(exe)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
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

/// Render pending jobs as a plain table.
pub fn format_jobs(jobs: &[Job]) -> String {
    if jobs.is_empty() {
        return "no pending nudge jobs".to_string();
    }
    let mut out = String::from("ID   PANE                 FIRE (UTC)            MSGS\n");
    for j in jobs {
        let TargetSpec::Tmux { pane } = &j.target;
        out.push_str(&format!(
            "{:<4} {:<20} {:<20} {}\n",
            j.id,
            pane,
            j.fire_at,
            j.messages.len()
        ));
    }
    out
}

fn socket() -> std::path::PathBuf {
    paths::resolve().socket
}

/// List pending jobs (interactive picker lands in Task 6; both modes print
/// the table for now).
pub fn list(_plain: bool) -> anyhow::Result<()> {
    match client::request(&socket(), &Request::List)? {
        Response::Jobs(jobs) => {
            print!("{}", format_jobs(&jobs));
            Ok(())
        }
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Cancel a pending job by id.
pub fn cancel(id: u64) -> anyhow::Result<()> {
    match client::request(&socket(), &Request::Cancel(id))? {
        Response::Cancelled(true) => {
            println!("nudge: cancelled job {id}");
            Ok(())
        }
        Response::Cancelled(false) => bail!("no pending job with id {id}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Edit a pending job: find it via `List`, overlay explicitly-passed CLI
/// flags onto its existing fields, `Cancel` the old job, and `Schedule` the
/// merged spec.
pub fn edit(id: u64, cli: &Cli) -> anyhow::Result<()> {
    let jobs = match client::request(&socket(), &Request::List)? {
        Response::Jobs(j) => j,
        other => bail!("unexpected response: {other:?}"),
    };
    let job = jobs
        .into_iter()
        .find(|j| j.id == id)
        .ok_or_else(|| anyhow::anyhow!("no pending job with id {id}"))?;

    // Overlay explicitly-passed flags onto the existing job.
    let opts = resolve_options(cli);
    let pane = match &cli.pane {
        Some(p) => p.clone(),
        None => {
            let TargetSpec::Tmux { pane } = &job.target;
            pane.clone()
        }
    };
    let now = Zoned::now();
    let fire_at = match &cli.time {
        Some(_) => fire_time(cli, &pane, &now)?,
        None => job.fire_at,
    };
    let messages = if cli.input.is_empty() {
        job.messages.clone()
    } else {
        cli.input.clone()
    };
    let spec = JobSpec {
        target: TargetSpec::Tmux { pane },
        messages,
        send_delay_secs: cli.delay.unwrap_or(job.send_delay_secs),
        fire_at,
        notify: opts.notify,
        verify: opts.verify,
        auto_retry: opts.auto_retry,
        retries_left: if opts.auto_retry { opts.retries } else { 0 },
        settle_secs: opts.settle_secs,
    };

    client::request(&socket(), &Request::Cancel(id))?;
    match client::request(&socket(), &Request::Schedule(spec))? {
        Response::Scheduled(new_id) => {
            println!("nudge: edited job {id} -> {new_id}");
            Ok(())
        }
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Dispatch non-daemon modes.
pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    if let Some(id) = cli.cancel {
        return cancel(id);
    }
    if let Some(id) = cli.edit {
        return edit(id, &cli);
    }
    if cli.list || cli.list_plain {
        return list(cli.list_plain);
    }
    schedule(&cli)
}

// pick_pane is provided in Task 6; a temporary stub keeps this compiling until then.
pub fn pick_pane() -> anyhow::Result<String> {
    anyhow::bail!("no pane given (-p); interactive picker lands in a later task")
}

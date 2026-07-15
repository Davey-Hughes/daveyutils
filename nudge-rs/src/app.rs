//! Command implementations for the CLI modes.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use clap::CommandFactory;
use jiff::Zoned;

use crate::cli::{resolve_options, tri, Cli};
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

    match client::request(&live_socket()?, &Request::Schedule(spec))? {
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

/// The daemon socket, with a daemon guaranteed to be answering on it.
///
/// Every command that talks to the queue goes through here. Jobs are persisted
/// and outlive the daemon, so the job-management commands need a daemon exactly
/// as `schedule` does: with one down (a reboot, or an ad-hoc daemon that exited
/// — nothing restarts it), `--list`/`--cancel`/`--edit` otherwise die on a
/// socket that isn't there, and the user cannot cancel a job that queue.json
/// still holds and that fires as soon as anything starts a daemon again.
fn live_socket() -> anyhow::Result<std::path::PathBuf> {
    let socket = paths::resolve().socket;
    ensure_daemon(&socket)?;
    Ok(socket)
}

/// List pending jobs (interactive picker lands in Task 6; both modes print
/// the table for now).
pub fn list(_plain: bool) -> anyhow::Result<()> {
    match client::request(&live_socket()?, &Request::List)? {
        Response::Jobs(jobs) => {
            print!("{}", format_jobs(&jobs));
            Ok(())
        }
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Cancel a pending job by id.
pub fn cancel(id: u64) -> anyhow::Result<()> {
    match client::request(&live_socket()?, &Request::Cancel(id))? {
        Response::Cancelled(true) => {
            println!("nudge: cancelled job {id}");
            Ok(())
        }
        Response::Cancelled(false) => bail!("no pending job with id {id}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Merge a pending job with the edit CLI's explicitly-passed flags, preserving
/// the job's existing values for anything not passed. Env defaults are NOT
/// consulted (unlike a fresh schedule) — only the job, the explicit flags, and
/// `default_retries` (the caller's `NUDGE_RETRIES` default, passed in rather
/// than read here so this stays pure and its tests cannot race the env).
pub fn merge_edit(
    job: &Job,
    cli: &Cli,
    now: &Zoned,
    default_retries: i64,
) -> anyhow::Result<JobSpec> {
    let base = Toggles {
        notify: job.notify,
        verify: job.verify,
        auto_retry: job.auto_retry,
        // A job scheduled without auto-retry stores retries_left == 0, so seeding
        // the base straight from the job made `--edit <id> --auto-retry` (no -r)
        // resolve to auto_retry=true with a budget of 0 — which apply_outcome
        // reads as exhausted, deleting the job on its first fire while the CLI
        // reported a successful edit. Fall back to the count a fresh schedule
        // would arm; an explicit -r still overrides it below.
        retries: if job.retries_left == 0 {
            default_retries
        } else {
            job.retries_left
        },
        settle_secs: job.settle_secs,
    };
    let overrides = crate::config::FlagOverrides {
        notify: tri(cli.notify, cli.no_notify),
        verify: tri(cli.verify, cli.no_verify),
        auto_retry: tri(cli.auto_retry, cli.no_auto_retry),
        retries: cli.retries,
        settle_secs: None,
    };
    let opts = crate::config::resolve(&base, &overrides);
    let pane = match &cli.pane {
        Some(p) => p.clone(),
        None => {
            let TargetSpec::Tmux { pane } = &job.target;
            pane.clone()
        }
    };
    let fire_at = match &cli.time {
        Some(_) => fire_time(cli, &pane, now)?,
        None => job.fire_at,
    };
    let messages = if cli.input.is_empty() {
        job.messages.clone()
    } else {
        cli.input.clone()
    };
    Ok(JobSpec {
        target: TargetSpec::Tmux { pane },
        messages,
        send_delay_secs: cli.delay.unwrap_or(job.send_delay_secs),
        fire_at,
        notify: opts.notify,
        verify: opts.verify,
        auto_retry: opts.auto_retry,
        retries_left: if opts.auto_retry { opts.retries } else { 0 },
        settle_secs: opts.settle_secs,
    })
}

/// Edit a pending job: find it via `List`, overlay explicitly-passed CLI
/// flags onto its existing fields (preserving anything not passed), then
/// `Schedule` the merged spec BEFORE `Cancel`-ing the original — so a
/// Schedule failure can never lose the job.
pub fn edit(id: u64, cli: &Cli) -> anyhow::Result<()> {
    let socket = live_socket()?;
    let jobs = match client::request(&socket, &Request::List)? {
        Response::Jobs(j) => j,
        other => bail!("unexpected response: {other:?}"),
    };
    let job = jobs
        .into_iter()
        .find(|j| j.id == id)
        .ok_or_else(|| anyhow::anyhow!("no pending job with id {id}"))?;

    let now = Zoned::now();
    let spec = merge_edit(&job, cli, &now, crate::cli::default_retries())?;

    // Schedule the replacement FIRST so a failure can't lose the original.
    let new_id = match client::request(&socket, &Request::Schedule(spec))? {
        Response::Scheduled(new_id) => new_id,
        Response::Error(e) => bail!("failed to reschedule job {id}: {e}"),
        other => bail!("failed to reschedule job {id}: {other:?}"),
    };
    match client::request(&socket, &Request::Cancel(id))? {
        Response::Cancelled(true) => {}
        Response::Cancelled(false) => {
            tracing::warn!("nudge: original job {id} was already gone")
        }
        other => bail!("scheduled replacement {new_id} but could not cancel {id}: {other:?}"),
    }
    println!("nudge: edited job {id} -> {new_id}");
    Ok(())
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

/// Interactively choose a tmux pane.
pub fn pick_pane() -> anyhow::Result<String> {
    let panes = crate::tmux_panes::list()?;
    if panes.is_empty() {
        anyhow::bail!("no tmux panes found; pass -p <pane>");
    }
    let labels: Vec<String> = panes
        .iter()
        .map(|p| {
            if p.title.is_empty() {
                p.target.clone()
            } else {
                format!("{}  ({})", p.target, p.title)
            }
        })
        .collect();
    let choice = inquire::Select::new("Target pane:", labels.clone())
        .prompt()
        .context("pane selection cancelled")?;
    let idx = labels.iter().position(|l| l == &choice).unwrap();
    Ok(panes[idx].target.clone())
}

/// Write a shell completion script for the `nudge` binary.
pub fn write_completions<W: std::io::Write>(shell: clap_complete::Shell, w: &mut W) {
    clap_complete::generate(shell, &mut Cli::command(), "nudge", w);
}

/// Print a shell completion script to stdout.
pub fn print_completions(shell: clap_complete::Shell) {
    write_completions(shell, &mut std::io::stdout());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_completions_mention_the_binary() {
        let mut buf: Vec<u8> = Vec::new();
        write_completions(clap_complete::Shell::Bash, &mut buf);
        let script = String::from_utf8(buf).unwrap();
        assert!(
            script.contains("nudge"),
            "completion script should mention the binary"
        );
        assert!(!script.is_empty());
    }

    #[test]
    fn zsh_and_fish_generate_nonempty_scripts() {
        for sh in [clap_complete::Shell::Zsh, clap_complete::Shell::Fish] {
            let mut buf: Vec<u8> = Vec::new();
            write_completions(sh, &mut buf);
            assert!(!buf.is_empty(), "{sh} script must be non-empty");
        }
    }
}

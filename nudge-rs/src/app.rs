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

/// What to tell a user whose resident daemon is not this build.
///
/// Worth spelling out, because nothing else will: the daemon is long-lived and
/// auto-started, so rebuilding nudge does not replace it, and there is no
/// `--stop-daemon` to do it with. Naming the command is the whole value here.
fn stale_daemon(detail: &str) -> String {
    format!(
        "the nudge daemon already running is not this build ({detail}).\n\
         It will not restart itself, so until it is stopped it is the old code \
         that runs your jobs, whatever this binary does.\n\
         Stop it and retry:\n    \
         pkill -f 'nudge --daemon'\n\
         or, if you installed it with --install-daemon:\n    \
         systemctl --user restart nudged.service            (Linux)\n    \
         launchctl kickstart -k gui/$UID/com.nudge.daemon   (macOS)"
    )
}

/// Whether a failed Ping means "no daemon is there" — so start one, as nudge
/// always has — rather than "something is there, and it is not us".
///
/// ENOENT (no socket file) and ECONNREFUSED (a socket file nobody is listening
/// on) are the two shapes of not-running. Everything else means somebody
/// answered: most of all the InvalidData of an old daemon's unit `"Pong"`,
/// which this build cannot parse. Spawning a daemon on top of one of those
/// only loses the singleton lock and reports `daemon did not come up`, which
/// tells the user nothing about the daemon they actually have.
fn daemon_not_running(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Hold a versioned Pong to this build, or explain the daemon that isn't.
fn check_handshake(resp: Response) -> anyhow::Result<()> {
    match resp {
        Response::Pong { version } if version == crate::VERSION => Ok(()),
        Response::Pong { version } => bail!(
            "{}",
            stale_daemon(&format!(
                "it is running version {version}; this is {}",
                crate::VERSION
            ))
        ),
        other => bail!(
            "{}",
            stale_daemon(&format!("it answered a ping with {other:?}"))
        ),
    }
}

/// One request to a daemon we have already handshaken with.
///
/// A transport failure here is not a bare errno to hand the user: something is
/// listening (it answered a Ping moments ago) and it has now either hung up
/// mid-exchange or spoken a protocol this build does not read. That is the same
/// wrong-build daemon `ensure_daemon` guards against, racing into the window,
/// and it has the same remedy — so give them the remedy, not the errno. This is
/// the `nudge: no response` that `--edit` against an old daemon used to print.
fn request(socket: &Path, req: &Request) -> anyhow::Result<Response> {
    client::request(socket, req).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData => {
            anyhow::anyhow!("{}", stale_daemon(&format!("the request failed: {e}")))
        }
        _ => anyhow::anyhow!("nudge daemon request failed: {e}"),
    })
}

/// Ping the daemon; if it's not answering, start it and wait for the socket.
///
/// The Ping is a version handshake, not just a liveness check. A daemon from
/// another build answers a plain "are you there?" perfectly well, so this used
/// to adopt it and hand it requests written against code it does not run.
pub fn ensure_daemon(socket: &Path) -> anyhow::Result<()> {
    match client::request(socket, &Request::Ping) {
        Ok(resp) => return check_handshake(resp),
        // Nothing is listening. Starting one is ours to do, and always was.
        Err(e) if daemon_not_running(&e) => {}
        // Something answered, just not in a way this build understands. An old
        // daemon's unit `"Pong"` arrives here as a parse error, which is as
        // reliable a signal of one as the version field itself.
        Err(e) => bail!(
            "{}",
            stale_daemon(&format!(
                "it did not answer a ping this build can read: {e}"
            ))
        ),
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
        // The daemon we just spawned is this exe, so a mismatch here means
        // somebody else won the socket — still worth saying out loud.
        if let Ok(resp) = client::request(socket, &Request::Ping) {
            return check_handshake(resp);
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

    match request(&live_socket()?, &Request::Schedule(spec))? {
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
    match request(&live_socket()?, &Request::List)? {
        Response::Jobs(jobs) => {
            print!("{}", format_jobs(&jobs));
            Ok(())
        }
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Cancel a pending job by id.
pub fn cancel(id: u64) -> anyhow::Result<()> {
    match request(&live_socket()?, &Request::Cancel(id))? {
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

/// Edit a pending job: find it via `List`, overlay explicitly-passed CLI flags
/// onto its existing fields (preserving anything not passed), then `Replace` it
/// with the merged spec.
///
/// The mutation is one request, applied under the daemon's queue lock. It used
/// to be Schedule-then-Cancel: scheduled first so a Schedule failure couldn't
/// lose the job, but that leaves both jobs live in between, and a Cancel leg
/// that never landed — daemon restarted or killed in the window, socket stolen,
/// Ctrl-C — fired the message twice, at the old time and the new, while the
/// error read as though the edit hadn't happened at all. A single round-trip
/// cannot half-apply: either the swap is committed or nothing is.
pub fn edit(id: u64, cli: &Cli) -> anyhow::Result<()> {
    let socket = live_socket()?;
    let jobs = match request(&socket, &Request::List)? {
        Response::Jobs(j) => j,
        other => bail!("unexpected response: {other:?}"),
    };
    let job = jobs
        .into_iter()
        .find(|j| j.id == id)
        .ok_or_else(|| anyhow::anyhow!("no pending job with id {id}"))?;

    let now = Zoned::now();
    let spec = merge_edit(&job, cli, &now, crate::cli::default_retries())?;

    match request(&socket, &Request::Replace { id, spec })? {
        Response::Replaced(Some(new_id)) => {
            println!("nudge: edited job {id} -> {new_id}");
            Ok(())
        }
        // It fired or was cancelled between the List above and now. Nothing was
        // scheduled in its place.
        Response::Replaced(None) => bail!("no pending job with id {id}"),
        Response::Error(e) => bail!("failed to edit job {id}: {e}; it is unchanged"),
        other => bail!("failed to edit job {id}: {other:?}; it is unchanged"),
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
    use std::io::ErrorKind;

    /// The distinction the whole handshake rests on. Get it wrong one way and a
    /// first-ever `nudge` stops auto-starting its daemon; wrong the other way
    /// and we spawn a second daemon on top of an old one that is answering
    /// perfectly well, lose the singleton lock, and report `daemon did not come
    /// up` — which names neither the problem nor the fix.
    #[test]
    fn only_enoent_and_econnrefused_mean_no_daemon_is_running() {
        for kind in [ErrorKind::NotFound, ErrorKind::ConnectionRefused] {
            assert!(
                daemon_not_running(&std::io::Error::new(kind, "x")),
                "{kind:?} means nothing is listening: start one, as nudge always has"
            );
        }
        // InvalidData is precisely an old daemon's unit-variant `"Pong"` failing
        // to parse: something IS there, and starting a rival is not the answer.
        for kind in [
            ErrorKind::InvalidData,
            ErrorKind::UnexpectedEof,
            ErrorKind::TimedOut,
            ErrorKind::PermissionDenied,
        ] {
            assert!(
                !daemon_not_running(&std::io::Error::new(kind, "x")),
                "{kind:?} means something answered; it must not be read as an absent daemon"
            );
        }
    }

    #[test]
    fn a_pong_from_this_build_is_accepted_and_any_other_is_not() {
        assert!(check_handshake(Response::Pong {
            version: crate::VERSION.to_string()
        })
        .is_ok());

        let err = check_handshake(Response::Pong {
            version: "0.0.1-ancient".to_string(),
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("0.0.1-ancient"),
            "must name the version: {err}"
        );
        assert!(
            err.contains("pkill -f 'nudge --daemon'"),
            "must name the remedy: {err}"
        );
    }

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

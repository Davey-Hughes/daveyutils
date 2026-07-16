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
use crate::target::{tmux::TmuxTarget, PaneDims, Target};
use crate::timespec::parse_timespec;

/// The mode `dispatch` resolves a parsed CLI + TTY state into.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Cancel(u64),
    Edit(u64),
    StaticList,
    Dashboard,
    Schedule,
}

/// True if the user expressed scheduling intent — any flag `schedule` consumes.
/// Bare `nudge` (none of these) is the dashboard's front door instead.
pub fn has_scheduling_flags(cli: &Cli) -> bool {
    cli.pane.is_some()
        || cli.time.is_some()
        || !cli.input.is_empty()
        || cli.delay.is_some()
        || cli.notify
        || cli.no_notify
        || cli.auto_retry
        || cli.no_auto_retry
        || cli.retries.is_some()
        || cli.verify
        || cli.no_verify
}

/// Pure routing: mode flags first, then the dashboard/table/schedule split.
pub fn route(cli: &Cli, is_tty: bool) -> Route {
    if let Some(id) = cli.cancel {
        return Route::Cancel(id);
    }
    if let Some(id) = cli.edit {
        return Route::Edit(id);
    }
    if cli.list_plain {
        return Route::StaticList;
    }
    if cli.list {
        return if is_tty {
            Route::Dashboard
        } else {
            Route::StaticList
        };
    }
    if has_scheduling_flags(cli) {
        return Route::Schedule;
    }
    if is_tty {
        Route::Dashboard
    } else {
        Route::StaticList
    }
}

/// Split a `--verify` snapshot into the two fields a JobSpec stores.
///
/// `None` in, `None`s out — a pane that could not be snapshotted arms no
/// recency gate, and a job with no snapshot fails open at fire time to the
/// banner check nudge has always done. Scheduling never fails over this.
fn baseline_fields(b: Option<crate::verify::Baseline>) -> (Option<String>, Option<PaneDims>) {
    match b {
        Some(b) => (Some(b.fingerprint), Some(b.dims)),
        None => (None, None),
    }
}

/// Assemble a JobSpec from resolved options.
///
/// `baseline` is the pane snapshot the recency gate compares against at fire
/// time; the caller takes it (only when `--verify` is on) so this stays pure.
pub fn build_spec(
    pane: &str,
    fire_at: jiff::Timestamp,
    cli: &Cli,
    opts: &Toggles,
    baseline: Option<crate::verify::Baseline>,
) -> JobSpec {
    let messages = if cli.input.is_empty() {
        vec!["please continue".to_string()]
    } else {
        cli.input.clone()
    };
    // Without --verify nothing ever reads these, and storing a snapshot for a
    // job that will not consult one is just a stale fingerprint in queue.json.
    let (verify_fingerprint, verify_dims) = baseline_fields(baseline.filter(|_| opts.verify));
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
        verify_fingerprint,
        verify_dims,
    }
}

/// Snapshot `pane` for the recency gate, but only when `--verify` is on.
///
/// The one production seam that takes a real capture for the gate. Never
/// fallible: a pane that will not answer yields no snapshot, and a job with no
/// snapshot fails open. `--verify` failing to arm must never be a reason the
/// user's nudge does not get scheduled at all.
fn snapshot_pane(pane: &str, opts: &Toggles) -> Option<crate::verify::Baseline> {
    snapshot_gate(opts, || {
        crate::verify::capture_baseline(&TmuxTarget::new(pane))
    })
}

/// The `--verify`-only guard around taking a capture: `take` runs only when the
/// flag is on.
///
/// Split out from [`snapshot_pane`] so the guard is testable at all — its own
/// body shells out to tmux, so nothing could assert this without a live server.
/// Both consumers (`build_spec`, `merge_edit`) now independently drop a
/// baseline when `--verify` is off, which is what makes the *stored* job right;
/// what this guard alone still decides is whether nudge shells out to tmux
/// twice for a snapshot no job will ever consult. That is invisible in the
/// JobSpec, so it takes a test of its own.
fn snapshot_gate(
    opts: &Toggles,
    take: impl FnOnce() -> Option<crate::verify::Baseline>,
) -> Option<crate::verify::Baseline> {
    if !opts.verify {
        return None;
    }
    take()
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
    let weekly_ext = std::env::var("NUDGE_WEEKLY_PATTERN").ok();
    match detect_reset(
        &screen,
        now,
        clock_ext.as_deref(),
        dur_ext.as_deref(),
        weekly_ext.as_deref(),
    ) {
        crate::detect::Detection::Reset(z) => Ok(z.timestamp()),
        crate::detect::Detection::None => {
            bail!("no rate-limit banner detected in {pane}; pass -m to set a time")
        }
        crate::detect::Detection::Unreadable { banner, gap } => bail!(
            "weekly limit banner found in {pane}, but I can't read its reset day: {gap:?}\n\
             (from {banner:?})\n\
             Schedule it by hand with -m, and please file this text."
        ),
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
pub fn ensure_daemon(paths: &paths::Paths) -> anyhow::Result<()> {
    let socket = &paths.socket;
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
    spawn_daemon(&std::env::current_exe()?, paths)?;
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

/// The auto-started daemon's log lives here, in the state dir beside the queue.
pub fn log_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("nudge.log")
}

/// Start over rather than grow past this. See [`daemon_log`].
const LOG_MAX_BYTES: u64 = 1 << 20;

/// Open the auto-started daemon's log for append, or `None` if it cannot be.
///
/// The daemon is spawned in the background by whatever `nudge` command the user
/// happened to run, so its stderr has nowhere to go by default and used to go
/// to `/dev/null`. That silently discarded every diagnostic it has — including
/// the `--verify` skip report, which the recency design requires be visible
/// precisely because a silent skip is indistinguishable from nudge never having
/// run. `--install-daemon` is unaffected: systemd and launchd capture stderr
/// themselves, and this only redirects the daemon nudge starts for you.
///
/// Growth: the daemon writes a line per job fired plus the occasional error, so
/// this is kilobytes a year in normal use — but normal use is not a bound, and
/// an unbounded file in the state dir is a bug waiting for a loop to find it.
/// Rotation is more machinery than the volume justifies, so the log simply
/// starts over once it passes [`LOG_MAX_BYTES`]. That check happens only here,
/// at spawn, so nothing ever truncates under a running daemon's append handle;
/// and the 1 MiB it discards is thousands of lines older than the daemon the
/// user is currently trying to explain.
fn daemon_log(state_dir: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(state_dir).ok()?;
    let path = log_path(state_dir);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    opts.open(&path).ok()
}

/// Start `exe --daemon` detached, with its diagnostics going to the log.
///
/// `exe` is a parameter rather than `current_exe()` inside because a test
/// harness *is* `current_exe()` and would have to stub the whole spawn to work
/// around it — leaving the one thing worth testing, where the daemon's stderr
/// ends up, untested. Production passes its own path.
///
/// A log that will not open falls back to `/dev/null` rather than failing the
/// spawn: losing the diagnostics is bad, but it is nothing next to a read-only
/// state dir meaning no nudge ever fires again.
pub fn spawn_daemon(exe: &Path, paths: &paths::Paths) -> std::io::Result<std::process::Child> {
    let log = daemon_log(&paths.state_dir)
        .map(Stdio::from)
        .unwrap_or_else(Stdio::null);
    std::process::Command::new(exe)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
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
    // Snapshot last, and only for --verify: this is the "before" the fire-time
    // gate compares against, so it wants to be as close to the user's actual
    // parked-at-the-banner pane as we can get it.
    let spec = build_spec(&pane, fire_at, cli, &opts, snapshot_pane(&pane, &opts));

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
    let paths = paths::resolve();
    ensure_daemon(&paths)?;
    Ok(paths.socket)
}

/// List pending jobs.
///
/// Takes no `plain` flag on purpose: it had one, ignored it, and the help text
/// promised an interactive `--list` on the strength of it. There is one
/// renderer; when a picker lands, that is when this grows a parameter that
/// means something.
pub fn list() -> anyhow::Result<()> {
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
/// `snapshot` is the effect `merge_edit` cannot take itself: it is handed the
/// *resolved* pane and toggles, because an edit may move the job to a new pane
/// (`-p`) or turn `--verify` on, and the snapshot has to describe the pane the
/// job will actually watch. Production passes [`snapshot_pane`].
pub fn merge_edit(
    job: &Job,
    cli: &Cli,
    now: &Zoned,
    default_retries: i64,
    snapshot: &dyn Fn(&str, &Toggles) -> Option<crate::verify::Baseline>,
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
    // Re-snapshot rather than carry the job's old one over. An edit is a
    // re-schedule, and the pane the user is looking at *now* is the "before"
    // they mean. Inheriting the original snapshot would compare the pane
    // against however it looked hours ago, so any edit of a job whose pane had
    // since moved would arm a gate that skips on sight.
    //
    // Filtered here as well as in `snapshot_pane`, and for the same reason
    // `build_spec` filters: `snapshot` is a parameter, so "no snapshot when
    // --verify is off" is this function's own invariant to keep and not one to
    // borrow from whatever the caller happened to pass.
    let (verify_fingerprint, verify_dims) =
        baseline_fields(snapshot(&pane, &opts).filter(|_| opts.verify));
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
        verify_fingerprint,
        verify_dims,
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
    let spec = merge_edit(
        &job,
        cli,
        &now,
        crate::cli::default_retries(),
        &snapshot_pane,
    )?;

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
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    match route(&cli, is_tty) {
        Route::Cancel(id) => cancel(id),
        Route::Edit(id) => edit(id, &cli),
        Route::StaticList => list(),
        Route::Dashboard => crate::tui::run(),
        Route::Schedule => schedule(&cli),
    }
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

    use crate::cli::Cli;

    fn cli(args: &[&str]) -> Cli {
        <Cli as clap::Parser>::try_parse_from(args).unwrap()
    }

    #[test]
    fn bare_nudge_on_a_tty_opens_the_dashboard() {
        assert_eq!(route(&cli(&["nudge"]), true), Route::Dashboard);
    }

    #[test]
    fn bare_nudge_without_a_tty_prints_the_static_table() {
        assert_eq!(route(&cli(&["nudge"]), false), Route::StaticList);
    }

    #[test]
    fn list_opens_the_dashboard_on_a_tty_and_the_table_otherwise() {
        assert_eq!(route(&cli(&["nudge", "--list"]), true), Route::Dashboard);
        assert_eq!(route(&cli(&["nudge", "--list"]), false), Route::StaticList);
    }

    #[test]
    fn list_plain_is_always_the_static_table() {
        assert_eq!(
            route(&cli(&["nudge", "--list-plain"]), true),
            Route::StaticList
        );
    }

    #[test]
    fn any_scheduling_flag_schedules_directly_even_on_a_tty() {
        assert_eq!(
            route(&cli(&["nudge", "-p", "bot:0.1"]), true),
            Route::Schedule
        );
        assert_eq!(route(&cli(&["nudge", "-m", "3pm"]), true), Route::Schedule);
        assert_eq!(route(&cli(&["nudge", "-i", "go"]), true), Route::Schedule);
        assert_eq!(route(&cli(&["nudge", "-v"]), true), Route::Schedule);
    }

    #[test]
    fn cancel_and_edit_still_win() {
        assert_eq!(
            route(&cli(&["nudge", "--cancel", "1"]), true),
            Route::Cancel(1)
        );
        assert_eq!(route(&cli(&["nudge", "--edit", "2"]), true), Route::Edit(2));
    }

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

    use std::io::Write as _;

    /// The daemon appends across restarts. Nothing else restarts it, so the
    /// last thing the *previous* daemon said is often the whole explanation
    /// for what the user is looking at now.
    #[test]
    fn the_daemon_log_is_created_and_then_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("nudge");

        let mut first = daemon_log(&state).expect("a writable state dir must yield a log");
        writeln!(first, "older daemon").unwrap();
        drop(first);

        let mut second = daemon_log(&state).expect("reopen");
        writeln!(second, "this daemon").unwrap();
        drop(second);

        let text = std::fs::read_to_string(log_path(&state)).unwrap();
        assert!(
            text.contains("older daemon") && text.contains("this daemon"),
            "a reopen must not truncate what the last daemon reported: {text:?}"
        );
    }

    /// The growth bound, since there is no rotation. Checked at spawn only, so
    /// it can never truncate under a running daemon's append handle.
    #[test]
    fn the_daemon_log_starts_over_once_it_grows_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("nudge");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(log_path(&state), vec![b'x'; (LOG_MAX_BYTES + 1) as usize]).unwrap();

        let mut f = daemon_log(&state).expect("an oversized log must still open");
        writeln!(f, "fresh start").unwrap();
        drop(f);

        let text = std::fs::read_to_string(log_path(&state)).unwrap();
        assert_eq!(
            text, "fresh start\n",
            "past the cap the log starts over rather than growing forever"
        );
    }

    /// Fail open. Losing the diagnostics is bad; a state dir that cannot be
    /// written meaning no nudge ever fires again is very much worse.
    #[test]
    fn a_log_that_cannot_be_opened_does_not_stop_the_daemon_starting() {
        let dir = tempfile::tempdir().unwrap();
        // A *file* where the state dir should be: create_dir_all cannot win.
        let state = dir.path().join("blocked");
        std::fs::write(&state, b"not a directory").unwrap();
        assert!(
            daemon_log(&state).is_none(),
            "an unopenable log must degrade to None, which spawn_daemon turns \
             into /dev/null -- never into a failure to start the daemon"
        );
    }

    fn toggles(verify: bool) -> Toggles {
        Toggles {
            notify: false,
            verify,
            auto_retry: false,
            retries: 0,
            settle_secs: 5.0,
        }
    }

    /// Without `--verify`, nudge must not capture the pane at all.
    ///
    /// `build_spec` and `merge_edit` both drop a baseline when the flag is off,
    /// so the *stored job* is right either way and no JobSpec assertion can see
    /// this. What is left for the guard to decide is whether scheduling shells
    /// out to tmux twice (`display-message`, `capture-pane`) to build a
    /// snapshot that is then thrown away — on every plain `nudge`, which is the
    /// common case. Asserting the returned `None` would not catch that; only
    /// counting the calls does.
    #[test]
    fn without_verify_no_capture_is_ever_taken() {
        let taken = std::cell::Cell::new(false);
        let out = snapshot_gate(&toggles(false), || {
            taken.set(true);
            Some(crate::verify::Baseline {
                fingerprint: "x".into(),
                dims: PaneDims {
                    width: 80,
                    height: 24,
                },
            })
        });
        assert!(
            !taken.get(),
            "--verify is off: nothing will ever read this snapshot, so taking it \
             is two tmux subprocesses spent on nothing"
        );
        assert!(out.is_none());
    }

    /// The other half: with the flag on, the capture is taken and returned. A
    /// guard that swallowed this would disarm the gate on every job, which
    /// fails open — so it is silent, and only this catches it.
    #[test]
    fn with_verify_the_capture_is_taken_and_returned() {
        let taken = std::cell::Cell::new(false);
        let out = snapshot_gate(&toggles(true), || {
            taken.set(true);
            Some(crate::verify::Baseline {
                fingerprint: "abc".into(),
                dims: PaneDims {
                    width: 80,
                    height: 24,
                },
            })
        });
        assert!(
            taken.get(),
            "--verify is on: the snapshot is the gate's whole input"
        );
        assert_eq!(out.map(|b| b.fingerprint), Some("abc".to_string()));
    }

    /// `0.1.0` is the last version whose `JobSpec` has no `verify_fingerprint`
    /// / `verify_dims`. Serde drops unknown fields silently and nothing in the
    /// crate sets `deny_unknown_fields`, so a 0.1.0 daemon accepts this build's
    /// Schedule, throws the snapshot away, and runs the banner-only logic the
    /// recency gate exists to replace — while every report says it worked.
    ///
    /// The handshake is the only thing that can refuse it, and it can only do
    /// that if this build stops calling itself 0.1.0. So the version this crate
    /// carries is load-bearing behavior, not release hygiene: shipping the
    /// recency gate under 0.1.0 ships it inert.
    #[test]
    fn the_handshake_refuses_the_pre_recency_version() {
        let err = check_handshake(Response::Pong {
            version: "0.1.0".to_string(),
        })
        .expect_err(
            "0.1.0 predates verify_fingerprint/verify_dims: adopting that daemon \
             silently drops the snapshot off every Schedule and runs the old \
             banner-only check, which is the whole bug this branch fixes",
        )
        .to_string();
        assert!(err.contains("0.1.0"), "must name the version: {err}");
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

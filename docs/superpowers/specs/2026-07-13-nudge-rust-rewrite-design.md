# nudge — Rust rewrite (design)

**Date:** 2026-07-13
**Status:** Approved for planning
**Scope:** nudge only. The media-script cleanup (batch_img2pdf, bisect_img,
video_pcm_to_flac, batch_makemkvcon, mkvpropedit_set_name, cue2flac → bash +
tests + descriptions) is a separate, independent effort with its own spec.

## Motivation

`nudge` is a rate-limit auto-resumer: it watches a tmux pane for a Claude
Code / Antigravity "resets in …" / "resets 3:00pm" banner, computes the reset
time, and re-injects the user's messages when the limit clears — with retries,
`--verify`, desktop notifications, and full job management.

The current implementation (`scripts/nudge`, ~1357 lines of bash) works well and
the user is broadly happy with its behavior. The pain is not the feature set —
it is the **dependencies and cross-platform scheduling**:

1. **macOS install friction** — the `at` scheduler (`atrun`) is disabled by
   default and needs a one-time `sudo launchctl` dance.
2. **Per-schedule friction** — fighting `atrun` (disabled-by-default, ~30s poll)
   feels like "approving" every schedule.
3. **Daemon-liveness checks** — atd (Linux) vs atrun (launchd, macOS) have
   different liveness semantics, and nudge has to guess whether the thing that
   fires jobs is even alive. The user has hit "daemon wasn't running."
4. **Separately-installed pieces** — `at` and `fzf` are both "go install this
   other thing first." nudge should be self-contained.

All four trace to two root dependencies: the `at` daemon and `fzf`. The fix is
to stop depending on them.

## Goals

- **Feature parity** with today's bash nudge. The bash script is the reference
  oracle and stays in the repo for feature comparison during the port.
- **Self-contained single binary** — no `at`, no `fzf`, no coreutils/gnu-sed.
- **Smooth, uniform cross-platform scheduling** with no per-schedule friction
  and no daemon-liveness guessing.
- **Reboot-safe** scheduling (jobs survive a restart, with catch-up).
- A **sophisticated, offloaded** implementation: lean on high-quality crates for
  the hard parts (date math, arg parsing, plist generation, TUI) rather than
  hand-rolling.

## Non-goals / honest limitations

- **No GUI auto-injection.** The Claude Code desktop/web app exposes no pty or
  terminal to write to or read from; robust auto-injection is not feasible and
  will not be faked via accessibility automation. The honest floor there is
  reminder mode (see Target tiers).
- **Phase 1 is tmux-only.** The Target trait is designed so pty mode, notify
  mode, and other multiplexers are additive, but they are Phase 2.
- Not a rewrite of the media scripts (separate spec).

## Language decision (recorded)

Rust was chosen over "modern bash." The deciding factors, specific to nudge:

- A single self-contained binary directly kills the install/dependency friction.
- `jiff` (native date/time) **deletes nudge's entire reason for dual GNU/BSD
  code** — no `gdate`/`gsed`, no C-locale forcing to parse `atq`.
- A built-in picker (ratatui) removes the `fzf` dependency, which is painful to
  reproduce in bash but nearly free in Rust.
- The pure logic (banner parsing, time parsing, job (de)serialization) is
  pleasant to unit-test with `cargo test`.

Performance is explicitly **not** a motivation — the work is external-tool
orchestration (tmux); there is nothing to speed up.

## Architecture — own-daemon

nudge ships its own tiny user-level scheduler daemon (`nudged`) — conceptually
"nudge's own `atd`," but user-level, cross-platform, and self-registering.

### Registration (one-time, per machine)

- **Linux:** write a `systemd --user` service unit and `systemctl --user enable
  --now` it. `loginctl enable-linger` (one-time) lets it run without an active
  login session — needed for the headless / SSH-tmux use case.
- **macOS:** write a LaunchAgent plist to `~/Library/LaunchAgents` and
  `launchctl bootstrap gui/$UID` it. User domain → **no sudo, no approval
  dialog**.

Both mechanisms are always present in a user session and the OS relaunches the
daemon on boot, so "is the daemon running?" stops being a question nudge has to
answer by probing an external `at` daemon.

### CLI ↔ daemon IPC

- The daemon owns the **authoritative job queue** (a JSON state file) and exposes
  it over a **Unix domain socket**.
- `nudge` (schedule), `--list`, `--edit`, `--cancel` are thin socket clients
  (serde-encoded requests). The daemon is the single source of truth → no
  file-locking races, and `--list`/`--cancel` are always consistent.
- If the socket is not answering, the CLI **auto-starts the daemon** (and
  performs first-run registration if needed). nudge owns its own lifecycle.

### Scheduling, catch-up, reboot

- Scheduling is an append to the daemon's queue — **zero per-schedule OS
  interaction**.
- On startup (including post-reboot), the daemon:
  1. Loads the persistent queue.
  2. Fires jobs whose reset time has passed, subject to a configurable **grace
     window** (how-late is too-late).
  3. Schedules the rest with in-process timers.
- **Catch-up × `--verify` synergy:** a stale catch-up job (e.g. the session was
  already resumed while the machine was off) is naturally guarded by `--verify`,
  which re-captures the pane and skips the send if the rate-limit banner is gone.

## Target abstraction (answers "what if not tmux?")

Injecting a nudge requires two capabilities against a stable, addressable
handle: **write** keystrokes into the program's input, and **read** its screen
(for banner auto-detection and `--verify`). Rather than hardcode tmux, both live
behind a trait:

```rust
trait Target {
    fn capture(&self) -> Result<String>;            // read screen: detect + verify
    fn send_line(&self, text: &str) -> Result<()>;  // inject a message
}
```

The daemon, scheduler, and job management operate on a serialized `Target`
*descriptor* stored in each job; they never know what backend is behind it.
Adding a backend does not touch the core. Three tiers:

- **Tier 1 — Multiplexers (full support).** `tmux` (`send-keys` + `capture-pane`)
  ships in Phase 1. `zellij`, `wezterm`, `screen` are addable `Target` impls
  later.
- **Tier 2 — Plain terminal, no multiplexer (Phase 2).** `nudge run -- claude`
  launches the CLI under a pty nudge owns (`portable-pty`), relaying I/O to the
  user's terminal while retaining read+write. nudge becomes the persistence
  layer instead of requiring tmux. (Optional macOS-only convenience: best-effort
  iTerm2/Terminal.app AppleScript injection — brittle, not the recommended path.)
- **Tier 3 — GUI / web (Phase 2, degraded).** No pty, no terminal. Robust
  injection is not feasible. Graceful floor = **reminder mode**: user supplies
  the reset time manually (`-m`), no auto-detect, and at reset time nudge fires a
  desktop notification instead of injecting.

## Crate selection

| Concern | Crate | Rationale |
|---|---|---|
| CLI + completions | `clap` (derive) + `clap_complete` | best-in-class parsing; free shell completions |
| Date/time | `jiff` | "3pm" / "now + 45 min" / catch-up math, TZ-aware; deletes the GNU/BSD `date` split |
| Queue / state | `serde` + `serde_json` | typed persistence |
| macOS registration | `plist` (serde) | type-safe LaunchAgent generation |
| Linux registration | (subprocess) `systemctl --user` + written unit file | one-time; no D-Bus needed |
| pty hosting (Phase 2) | `portable-pty` | `nudge run -- claude` |
| Picker / dashboard | `ratatui` + `crossterm` (+ `inquire` for simple prompts) | built-in, replaces fzf |
| Notifications | `notify-rust` | cross-platform; replaces osascript/notify-send split |
| Banner regex / ANSI | `regex` + `strip-ansi-escapes` | pattern shapes + clean capture |
| Errors / logging | `anyhow` + `thiserror`, `tracing` (+`tracing-subscriber`) | app vs typed errors; daemon logs |
| Test tooling | `assert_cmd`, `predicates`, `insta`, `tempfile` | CLI integration, snapshots, isolated state |

## Module layout (isolation-first)

Each module maps to a cluster of bash functions so parity is a checklist:

- `cli` — clap definitions.
- `config` — env < flag < `--no-*` precedence resolution (mirrors bash).
- `timespec` — jiff-based parsing of absolute/named/relative times.
- `detect` — banner detection; clock vs duration shapes; `NUDGE_CLOCK_PATTERN`
  / `NUDGE_DURATION_PATTERN` extensions.
- `target/` — the `Target` trait + `tmux` impl (later `pty`, `notify`).
- `daemon/` — `queue`, `scheduler` (in-process timers + catch-up), `ipc` (Unix
  socket server).
- `register/` — `systemd` and `launchd` one-time registration.
- `inject` — the fire path: send messages, `-w` delay, `--verify`, auto-retry.
- `notify` — notify-rust wrapper.
- `tui/` — ratatui job/pane dashboard + a plain `--list-plain` fallback for
  non-TTY / CI.

## Feature parity checklist (port target)

The bash `tests/README.md` is a strong behavioral spec and will be mined for
edge cases. Parity surface:

- **Targeting:** `-p/--pane`; interactive pane picker when omitted.
- **Messages:** `-i/--input` repeatable (each on its own line); `-w/--delay`
  between sends; default `please continue`.
- **Auto-detect:** Claude clock banner + Antigravity duration banner; 3-minute
  safety padding; `NUDGE_CLOCK_PATTERN` / `NUDGE_DURATION_PATTERN` extensions.
- **Manual time:** `-m/--time` absolute / named / relative — now uniformly via
  jiff (removes the BSD relative-time rejection special case).
- **Notifications:** `-n/--notify`, `--no-notify`.
- **Auto-retry:** `-a/--auto-retry`, `-r/--retries <n>` incl. `-1` = forever;
  `--no-auto-retry`; settle window (`NUDGE_SETTLE_SECS`).
- **Verify:** `-v/--verify`, `--no-verify`.
- **Env defaults:** `NUDGE_AUTO_RETRY`, `NUDGE_VERIFY`, `NUDGE_NOTIFY`,
  `NUDGE_RETRIES`, `NUDGE_SETTLE_SECS`, `NUDGE_CLOCK_PATTERN`,
  `NUDGE_DURATION_PATTERN` — same precedence (env < flag < interactive toggle).
- **Job management:** `--list`/`--jobs` (interactive dashboard + `--list-plain`
  non-TTY fallback), `--edit <id>` (interactive + non-interactive per-flag
  overrides that overlay only explicitly-passed flags), `--cancel <id>`.

Behaviors that become obsolete (dropped, not ported): `atrun_hint` and the whole
macOS atrun setup, the GNU/BSD `date` split, C-locale forcing for `atq`, the BSD
relative-time rejection, and `at -c` dump parsing (nudge owns its own queue now).

## Testing / CI / distribution

- **Unit (`cargo test`):** timespec parsing, banner detection, queue
  (de)serialization, catch-up policy, config precedence. `insta` snapshots for
  parser/formatter output.
- **Integration:** `assert_cmd` + `predicates`; tmux-dependent tests self-skip
  when tmux is absent (mirrors today's e2e). Isolated `tempfile` state dirs +
  per-test socket paths.
- **CI:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on
  Linux + macOS.
- **Distribution:** `cargo install` and a Homebrew formula. One self-contained
  binary; no coreutils, gnu-sed, fzf, or `at`.

## Repo layout & phasing

- New `nudge-rs/` crate in the repo. `scripts/nudge` (bash) and `tests/` stay
  put as the reference oracle; retire them only once parity is proven.
- **Phase 1:** daemon (registration + IPC + queue + scheduler + catch-up) +
  tmux Target + full feature parity + install path + tests/CI.
- **Phase 2:** `nudge run` pty mode, notify-only reminder mode, and additional
  multiplexer backends.

## Open questions (defer to planning)

- Exact state/socket paths (XDG `~/.local/state/nudge/` + `$XDG_RUNTIME_DIR` for
  the socket on Linux; `~/Library/Application Support/nudge/` on macOS).
- Default catch-up grace window value.
- Whether `nudge-rs/` is a plain crate now or a Cargo workspace anticipating the
  media tools (leaning: plain crate; revisit if media tools ever go Rust).

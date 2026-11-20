# nudge

Rate-limit auto-resumer for AI CLIs (Claude Code, Codex, Antigravity) running in
tmux or herdr.

nudge watches a tmux or herdr pane for a rate-limit banner ("resets 3:00pm" /
"try again at 6:26 PM" / "resets in 1h30m"), and re-injects your messages when
the limit clears — via a small resident daemon it manages itself. No `at`, no
`fzf`, no coreutils; Linux + macOS.

## Install

**cargo**
```sh
cargo install --path nudge-rs
```

**Arch (AUR)** — `nudge` (release) or `nudge-git` (latest):
```sh
# from packaging/aur/nudge-git
makepkg -si
```

**Homebrew**
```sh
brew install --HEAD packaging/homebrew/nudge.rb
```

Shell completions: `nudge --completions bash|zsh|fish` (the packages install these
automatically).

## Usage

```sh
nudge -a                         # no prompts: last pane, its reset time, default message
nudge -p bot:0.1                 # auto-detect the reset time from the pane
nudge -p bot:0.1 -m "14:30"      # explicit time
nudge -p bot:0.1 --auto-retry -r -1 -v   # retry forever, verify before each send
nudge -p herdr:w7:pX             # a herdr pane (see herdr below for which session)
nudge                            # interactive pane picker
nudge --list                     # pending jobs
nudge --cancel 3 / --edit 3      # manage a job
```

A job stores tmux's id for its pane (`%19`), not the position you named
(`main:12.0`). Positions move: closing a window renumbers the ones after it
when `renumber-windows` is on, and swapping or rotating moves them too. A nudge
stored by position fired into whichever pane took the slot. An id stays with
its pane until the pane closes or the tmux server stops. A new server numbers
its panes from `%0` again, so a job also remembers which server its pane was on
(the server's start time and PID). One that comes due on a different server,
after a reboot or a `kill-server`, types nothing: the log says
`WrongPane(ServerRestarted)`, `--notify` says so, `--list` shows its pane as
`%19 (earlier tmux server)`, and the dashboard won't edit it. A job queued
before 0.7.0 has no record of its server, so cancel any of those left over from
before a reboot. The confirmation, `--list` and the dashboard show both, as
`main:12.0 (%19)`, with the position read at that moment. A pane tmux doesn't
have is refused when you schedule.

A job is also named after the session in its pane, read from the pane's title
whenever the job is shown, so a `/rename` shows up from then on. Claude Code
titles its pane with the session's name, or with a title it makes up until you
name it. Codex titles it with the items its `terminal_title` setting lists,
and the thread name is the one that names a session. A thread id or a status
glyph is left out. `--list` and the dashboard show the name in a NAME column,
and the confirmation, the log and `--notify` put it first:
`ra-scan-3 · main:5.0 (%13)`. A pane nothing has titled has no name. The NAME
column is left out when no job has a name, and in the dashboard when the
terminal is too narrow for it.

### `-a` / `--auto`

`nudge -a` schedules without asking anything. It targets the pane you were last
in — the same one the dashboard preselects, the last-active pane of nudge's own
tmux window, or the pane under nudge's popup — reads the reset time off it, and
queues the default message (`please continue`). The table under
[herdr](#herdr) below covers every place nudge can run.

It refuses rather than guesses. If nudge is running inside neither tmux nor
herdr, or in a herdr split, or its tmux window has no other recently-used pane,
`--auto` says so and names `-p`; it will not fall back to an arbitrary pane,
because unlike the dashboard's preselection nobody sees its choice before the
message is delivered. A pane with no recognisable rate-limit banner is the same
story — pass `-m` to set the time yourself.

It composes with the other scheduling flags (`-m`, `-i`, `-v`, `-n`, `-r`), and
conflicts with `-p` and with `--list`/`--cancel`/`--edit`.

> **Note** — `-a` used to be short for `--auto-retry`, which is now long-only.
> `-r <n>` still implies it. Anything passing `-p <pane> -a` together will now
> fail to parse rather than change meaning.

### herdr

nudge types into [herdr](https://herdr.dev) panes as well as tmux panes. Name
one by herdr's id for it after a `herdr:` prefix, and put the herdr session it
is in between the two when that needs saying:

```sh
nudge -p herdr:w7:pX             # w7:pX in the herdr session nudge runs in, or herdr's default session
nudge -p herdr:beaver:w1:p2      # w1:p2 in the herdr session named beaver, wherever nudge runs
```

The prefix is required, because `w7:pX` is also a tmux target (session `w7`,
window `pX`).

With no session in it, `herdr:<pane>` means the herdr session nudge is
running in when nudge runs in a herdr pane or popup, and herdr's default
session anywhere else: outside herdr, and also in a tmux running inside a
herdr pane. A tmux pane carries the herdr variables its tmux server was
started with, which can name a herdr long gone, so nudge does not go by them.
`herdr:<session>:<pane>` names the session outright; herdr calls its default
session `default`. In a herdr pane or popup that has lost `HERDR_SOCKET_PATH`,
nudge cannot tell its session, and refuses `herdr:<pane>` and `-a` rather than
guess the default session: every session numbers its panes from `w1:p1`. Name
the session then.

The confirmation, `--list`, the dashboard and the log show a herdr pane with
its session always named, `herdr:default:w7:pX` for the default session's, so a
label works as `-p` from anywhere, inside a named herdr session or out. The
picker and the dashboard list the panes of every running herdr session beside
tmux's.

For `nudge -a`, bind it to a popup in herdr's `config.toml`:

```toml
[[keys.command]]
key = "prefix+u"
type = "popup"
command = "nudge -a"
width = "60%"
height = "40%"
```

What `-a` targets depends on where nudge is running:

| nudge runs in | `-a` targets |
|---|---|
| a tmux pane | the last-active pane of its tmux window |
| a tmux popup (`display-popup`) | the pane under the popup |
| a herdr popup | the pane under the popup |
| a herdr split | nothing: herdr does not say which pane had focus before, so `-a` refuses and names the popup and `-p` |
| neither | nothing: `-a` refuses and names `-p` |

A pane that runs the other multiplexer's client passes whatever is typed into
it on to a pane of its own, so `-a` looks through it, once, to that pane:

- A tmux pane running herdr means the pane that has focus in that herdr. So
  `-a` from a tmux split beside herdr, or from a tmux popup over it, finds the
  agent you were looking at in herdr. A herdr client started with `--remote`
  or `--machine` shows a herdr on another machine, which nudge does not reach,
  and is refused.
- A herdr pane running tmux means the active pane of that tmux client. Only a
  client of the default tmux server: a job stores no tmux socket, so a client
  started with `-L` or `-S` is refused.

`-p` looks through a client the same way: `-p main:1.0`, naming the tmux pane
that runs herdr, schedules for the herdr pane focused behind it. Either way the
job stores the pane behind the client, never the client's own pane, and the
picker leaves client panes out.

A popup closes as soon as its command exits, and would take nudge's
confirmation or error with it. So in a tmux or herdr popup, nudge prints it and
waits with `Press any key to close`.

When a job comes due, nudge checks that its pane is still the one you
scheduled for before typing anything:

- A pane that now runs the other multiplexer's client gets nothing. What nudge
  typed would land in whichever pane that client shows.
- A herdr job also remembers the conversation its pane held when you scheduled
  it (herdr's `agent_session` for the pane). If the pane holds a different
  conversation when the job comes due, it gets nothing. If herdr reports no
  conversation for the pane, nudge cannot tell and treats it as a failed send:
  retried with `-r`, and without `-r` dropped, with a notification (under
  `--notify`) and a log line that say why. A `/clear` in
  that pane counts, and so does quitting the agent and starting another. herdr
  keeps a pane's id and its conversation across a restart, so a plain herdr
  restart does not drop the job; the check is there because a closed
  workspace's id can be handed to a new one after a restart. A pane herdr knew
  no conversation for when you scheduled gets no check.

Each of these ends the job like a tmux restart does: the log says
`WrongPane(RunsHerdr)`, `WrongPane(RunsTmux)` or
`WrongPane(OtherConversation)` (and `WrongPane(ServerRestarted)` for a tmux
restart), `--notify` says why in words, and the job is removed, even with
`--auto-retry -r -1`.

### Dashboard

Run `nudge` with no arguments in a terminal to open the interactive dashboard:

- **New nudge** — the tab it opens on, focused on `[ Schedule ]`: auto-detection
  has already read the reset off the pane it preselected, so a bare `Enter`
  schedules it, prints a one-line summary, and quits. `↑↓` first to change the
  pane, the time, the message, or the verify/notify/auto-retry toggles; `^S`
  schedules without quitting and drops you on the Jobs tab.
- **Jobs** — `Tab` to it: a live table of pending nudges with a countdown to
  each; `↑↓` to select, `c` to cancel, `e` to edit, `r` to refresh.

`q` quits from anywhere it isn't a character you are typing: the message field, a
manual time, and the pane picker's query keep it as text, and in the picker's
Normal mode it closes the picker rather than the dashboard. Everywhere else it
quits. `^C` always quits, and `?` toggles the full key reference in the footer.

Passing any scheduling flag (`-p`, `-m`, `-i`, …) schedules directly and skips
the dashboard, as before. `nudge --list-plain`, or `nudge`/`--list` with output
piped, prints a static table instead.

### Weekly limits

nudge also detects Claude's weekly banner:

    You've hit your weekly limit · resets Jul 28 at 8am (America/Los_Angeles)

Its per-model limits are weekly too, and read the same way:
`You've hit your Opus limit` (or `Sonnet`). The session banner is
`You've hit your 5-hour limit` since Claude Code 2.1.312, and
`You've hit your session limit` before it; nudge reads both.

The stated time zone is honored, so the reset resolves on the clock the banner
quotes rather than your machine's.

A weekly reset may be days away, so nudge reads the day off the banner's line.
The wording there is not a stable interface — it has already drifted from
`resets Jul 16, 8am` to `resets Jul 28 at 8am` — so nudge scans the line for a
date rather than matching one phrasing, and the connective tissue around it does
not matter:

    resets Jul 28 at 8am          resets on the 28th of July at 8am
    resets Jul 28, 8am            resets Tue, Jul 28 at 8am
    resets 28 Jul at 8am          resets at 8am on Jul 28
    resets Wed 8am                resets tomorrow at 8am

A banner with no day at all (`resets 8am`) means the next such hour. Anything
trailing the time in its own `·` segment — the usual `/upgrade to increase your
limits` — is prose, and is ignored rather than mistaken for a day.

All-numeric dates read too, and nudge works out which field is the month rather
than assuming a convention:

    resets 7/16 at 8am            resets 2026-07-16 at 8am
    resets 16/7 at 8am            resets 7/16/2026 at 8am

Three things decide `7/16` against `16/7`, in that order:

1. **The calendar.** There are only twelve months, so a field above twelve can
   only be a day. That alone settles most real dates.
2. **The week.** A weekly reset is at most seven days out, which rules out the
   reading that lands months away — and it does so identically everywhere, so a
   US and a UK machine agree.
3. **The locale.** Only if both readings are real *and* both fall inside the week
   does convention get a vote: `M/D` for an `en_US` locale, `D/M` otherwise.

In practice step 3 never runs. The two readings of `a/b` sit in month `a` and
month `b`, so when they differ at all they are about a month apart — and two
dates a month apart cannot both be a week away. It is kept as a backstop, and an
unset or `C` locale is treated as *no* answer rather than as US.

What nudge will **not** do is guess. A day it cannot read unambiguously makes it
**refuse to schedule** — guessing would fire the nudge days early, silently. A
two-digit year is the standing example: `7/28/26` is three fields that could each
be the month, the day, or the year. The error quotes the text it could not read:

    rate-limit banner found in bot:0.1, but I can't read its reset day: " 7/28/26 at "
    (from "You've hit your weekly limit · resets 7/28/26 at 8am")
    Schedule it by hand with -m, and please file this text.

That text is exactly what an issue needs. A date is also refused if it lands
more than a month out, since no rate-limit window is that long — that guard is
what keeps a bare `Jul 28` from silently resolving into next year.

`NUDGE_WEEKLY_PATTERN` extends the weekly banner pattern the same way
`NUDGE_CLOCK_PATTERN` extends the clock one.

One gap worth knowing: only the IANA `(Region/City)` zone form is understood. A
banner that says `8am PT` resolves 8am on *your* machine's clock, not Pacific.

### Codex

nudge reads Codex's usage-limit banner:

    ■ You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit
    https://chatgpt.com/settings/usage to purchase more credits or try again at 6:26 PM.

Codex wraps that to the pane itself, so where it breaks depends on how wide the
pane is, and the time can land alone on the next line. nudge reads the banner as
the paragraph it is, so the width does not change the answer as long as Codex
breaks it between words, which is what it does by default. The sentence between
`hit your usage limit` and `try again at` varies with your plan and is not part
of the match. Nothing after the time is read, so a line that happens to sit
directly under the banner cannot change the day.

The `/status` card that Codex's "Run /status for a breakdown" points you to has
a `Weekly limit: ... (resets ...)` row. That is a usage report, not a limit you
have hit, and nudge ignores it.

A reset that is not today carries its date, which goes through the same day
scanner as the weekly banner above, with the same refusal for a day it cannot
read:

    ... or try again at Oct 14th, 2026 6:26 PM.

Because Codex dates every reset that is not today, a bare time means *today's*.
One whose hour has already gone by today is a reset that has happened, not
tomorrow's, and nudge says so rather than schedule a day late. A dated banner
whose time has passed gets the same answer:

    the rate-limit banner in bot:0.1 says to try again at 4:20 AM, and that has already passed
    (from "■ You’ve hit your usage limit. ... or try again at 4:20 AM.")
    Its reset has come and gone, so the pane no longer says when to nudge. Pass -m to set a time.

What nudge cannot catch is a bare banner left on screen from an earlier day
whose hour has not yet come round today. `try again at 9:49 PM` from last night,
read this morning, is identical to one printed this morning, and is scheduled
for tonight. Check the time nudge reports when the pane has been sitting a
while. The time is also read on your machine's clock, so a Codex running in
another time zone, over ssh for instance, needs `-m`.

`try again later`, which Codex prints when it has no reset to quote, names no
time and is not detected. Use `-m`, and leave `-v` off for that job: with no
banner it recognises, the verify gate would skip it.

If Codex's Vim keys are on and you left the composer in Normal mode, typed text
would be read as commands: `please continue` arrives as `se continue`. nudge
checks for a status line ending in `Vim: Normal` and presses `A` first, which
puts the message after a draft left in the composer rather than inside it. If
the status line still says Normal after that (a remapped `A`, or a Codex
without it), nudge presses `i`. It looks only at the last two lines of the
pane, where Codex draws that line, so a pane that merely mentions those words,
or a draft that ends in them, is left alone. This relies on the Vim mode being
shown in Codex's status line.

When a nudge fires, nudge waits half a second between typing the message and
pressing Enter. Codex treats keystrokes that arrive faster than anyone types as
a paste, and an Enter within 120ms of one as a newline inside it, so an Enter
sent straight after the text left the message sitting in the composer, unsent.

After Enter, nudge reads the input box again (Claude's or Codex's). If the
message is still there, the job ends as `Unsubmitted`: a warning in the daemon
log rather than `Sent`, and a notification with `--notify`. Nothing more is
typed in behind it, neither later messages nor a retry.

### Vim mode in Claude Code

Claude Code's Vim keys (`editorMode: "vim"`) have the same problem as Codex's:
typed into Normal mode, `please continue` arrives as `se continue`. Claude
draws `-- INSERT --` under its input box in Insert mode and nothing in Normal
mode, which is also how the box looks with Vim keys off. So when the label is
missing, nudge presses `A` and looks again. If the `A` showed up as text, the
box has no Vim keys, and nudge presses Backspace to take it out. Either way the
box is taking text when the message is typed. `A` rather than `i` puts the
message after a draft left in the box rather than inside it, and a box left in
Visual mode (`-- VISUAL --`) gets Escape first.

### `--verify`

`-v` means "don't type into this session if I've already come back to it myself".

Scheduling with `-v` fingerprints the pane as you leave it — parked at its
banner. At fire time nudge injects only if the pane is **unchanged** since then
*and* still shows a rate-limit banner. If you resumed the session in the
meantime the pane has moved, and nudge stands down and says so. Checking only
for a banner is not enough: the banner that made you schedule the nudge is still
sitting there hours later, so it would happily inject into the session you are
in the middle of using.

Anything it cannot judge, it injects. Resizing the window reflows the pane, a
job scheduled by an older build carries no fingerprint, and a pane that will not
report its size cannot be compared — all of these fall back to the plain banner
check. That is deliberate and it is the whole trade: a stray "please continue"
is an annoyance, whereas an overnight nudge that silently never fires defeats
the point of the tool. `-v` will not cost you a nudge; it only ever declines one
it is sure about.

With `--notify`, a skip notifies too, naming which one it was — nudge not firing
should never be something you have to guess at. `--edit` re-fingerprints, so the
pane at edit time becomes the new baseline.

The daemon is auto-started on first schedule. To run it at login:

```sh
nudge --install-daemon           # register with systemd --user / launchd
```

## Development

```sh
cd nudge-rs
cargo test
cargo run -- --help
```

`cargo test` drives a real `tmux` server, so tmux must be installed. The herdr
tests start a private herdr server of their own, and skip themselves where
herdr is not installed.

This started life as a bash script at `scripts/nudge`. That original was kept as
a reference oracle for the duration of the port and removed once the rewrite
overtook it.

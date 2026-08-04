# nudge

Rate-limit auto-resumer for AI CLIs (Claude Code, Antigravity) running in tmux.

nudge watches a tmux pane for a rate-limit banner ("resets 3:00pm" / "resets in
1h30m"), and re-injects your messages when the limit clears — via a small
resident daemon it manages itself. No `at`, no `fzf`, no coreutils; Linux + macOS.

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
nudge                            # interactive pane picker
nudge --list                     # pending jobs
nudge --cancel 3 / --edit 3      # manage a job
```

### `-a` / `--auto`

`nudge -a` schedules without asking anything. It targets the pane you were last
in — the same one the dashboard preselects, the last-active pane of nudge's own
tmux window — reads the reset time off it, and queues the default message
(`please continue`).

It refuses rather than guesses. If nudge is not running inside tmux, or that
window has no other recently-used pane, `--auto` says so and names `-p`; it will
not fall back to an arbitrary pane, because unlike the dashboard's preselection
nobody sees its choice before the message is delivered. A pane with no
recognisable rate-limit banner is the same story — pass `-m` to set the time
yourself.

It composes with the other scheduling flags (`-m`, `-i`, `-v`, `-n`, `-r`), and
conflicts with `-p` and with `--list`/`--cancel`/`--edit`.

> **Note** — `-a` used to be short for `--auto-retry`, which is now long-only.
> `-r <n>` still implies it. Anything passing `-p <pane> -a` together will now
> fail to parse rather than change meaning.

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

    weekly limit banner found in bot:0.1, but I can't read its reset day: " 7/28/26 at "
    (from "You've hit your weekly limit · resets 7/28/26 at 8am")
    Schedule it by hand with -m, and please file this text.

That text is exactly what an issue needs. A date is also refused if it lands
more than a month out, since no rate-limit window is that long — that guard is
what keeps a bare `Jul 28` from silently resolving into next year.

`NUDGE_WEEKLY_PATTERN` extends the weekly banner pattern the same way
`NUDGE_CLOCK_PATTERN` extends the clock one.

One gap worth knowing: only the IANA `(Region/City)` zone form is understood. A
banner that says `8am PT` resolves 8am on *your* machine's clock, not Pacific.

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

`cargo test` drives a real `tmux` server, so tmux must be installed.

This started life as a bash script at `scripts/nudge`. That original was kept as
a reference oracle for the duration of the port and removed once the rewrite
overtook it.

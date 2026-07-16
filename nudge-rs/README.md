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
nudge -p bot:0.1                 # auto-detect the reset time from the pane
nudge -p bot:0.1 -m "14:30"      # explicit time
nudge -p bot:0.1 -a -r -1 -v     # auto-retry forever, verify before each send
nudge                            # interactive pane picker
nudge --list                     # pending jobs
nudge --cancel 3 / --edit 3      # manage a job
```

### Dashboard

Run `nudge` with no arguments in a terminal to open the interactive dashboard:

- **Jobs** — a live table of pending nudges with a countdown to each; `↑↓` to
  select, `c` to cancel, `e` to edit, `r` to refresh, `q` to quit.
- **New nudge** — `Tab` to it: pick a pane, let nudge auto-detect the reset (or
  type a time), set a message and the verify/notify/auto-retry toggles, `Enter`
  to schedule.

Passing any scheduling flag (`-p`, `-m`, `-i`, …) schedules directly and skips
the dashboard, as before. `nudge --list-plain`, or `nudge`/`--list` with output
piped, prints a static table instead.

### Weekly limits

nudge also detects Claude's weekly banner:

    You've hit your weekly limit · resets 8am (America/Los_Angeles)

The stated time zone is honored, so the reset resolves on the clock the banner
quotes rather than your machine's.

A weekly reset may be days away, and the bare form above names no day. nudge
reads a day when the banner gives one (`resets Wed 8am`, `resets Wednesday at
8am`, `tomorrow`), and treats the bare form as the next such hour.

If the banner names its day in a form nudge does not recognize, it **refuses to
schedule** rather than guessing — guessing would fire the nudge up to six days
early, silently. The error quotes the text it could not read:

    weekly limit banner found in bot:0.1, but I can't read its reset day: " Jul 16, "
    (from "You've hit your weekly limit · resets Jul 16, 8am")
    Schedule it by hand with -m, and please file this text.

That text is exactly what an issue needs. `NUDGE_WEEKLY_PATTERN` extends the
weekly banner pattern the same way `NUDGE_CLOCK_PATTERN` extends the clock one.

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

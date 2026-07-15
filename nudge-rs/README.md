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

The bash predecessor lives at `scripts/nudge` (kept as the reference oracle).

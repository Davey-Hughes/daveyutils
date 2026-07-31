# daveyutils

Random command-line utilities. Each one has a `--help`; run it for usage.

| Utility | Does | Requires |
|--------|------|----------|
| `cue2flac` | Split a CUE/BIN disc image into per-track FLAC files | `ffmpeg` |
| `batch_img2pdf` | Unzip image archives and make one PDF per folder | `unar`, `img2pdf`, `file` |
| `bisect_img` | Split landscape JPEGs into left/right halves | ImageMagick |
| `video_pcm_to_flac` | Convert PCM⇄FLAC audio streams in MKVs | `fd`, `ffprobe`, `ffmpeg` |
| `batch_makemkvcon` | Rip every Blu-ray/DVD disc image under a directory to MKV | `makemkvcon` |
| `mkvpropedit_set_name` | Set each MKV's title tag from its filename | `mkvpropedit` |
| `nudge` | Rate-limit auto-resumer for AI CLIs in tmux (Rust — see `nudge-rs/`) | `tmux` |

## Install

`make all` collects every utility into `./bin` (gitignored). Put that on your
PATH once:

```sh
make all
export PATH="$PWD/bin:$PATH"     # add to your shell rc
```

The bash scripts are **symlinked**, so editing one takes effect immediately.
`nudge` is built from `nudge-rs/` (`cargo build --release`) and linked in — and
since it is the only utility here that needs compiling, bare `make` rebuilds and
relinks just it.

```sh
make          # build nudge + link it into ./bin
make all      # that, plus a symlink for every script
make check    # run the bash and Rust test suites
make clean    # remove ./bin
make help     # list targets
```

## Layout

- `scripts/` — the bash utilities.
- `nudge-rs/` — the Rust `nudge` (a rewrite of the original bash version: no `at`
  daemon, no `fzf`, no coreutils; it runs its own user-level scheduler).
  Its jobs are run by a resident daemon, auto-started on first use, which reports
  what it did with each one — fired, or skipped because you had already resumed
  the pane — to `<state dir>/nudge.log` (`~/.local/state/nudge/` on Linux,
  `~/Library/Application Support/nudge/` on macOS). That is where to look when a
  nudge did not fire; `--notify` reports the same outcomes at the time.
- `tests/` — bash test-suite (`bash tests/run.sh`); Rust tests live in `nudge-rs/`.
- `dev/` — tooling for working on this repo, deliberately NOT in `scripts/`: the Makefile
  links every `scripts/*` onto your PATH, so a dev script placed there would ship as a
  utility.
- `packaging/` — Homebrew formula and AUR PKGBUILDs for `nudge`.

## Contributing

`main` is **linear** — no merge commits — and every commit on it is an **atomic unit**: it
builds and passes the gate on its own. Work happens on a feature branch, which lands as
**one squashed commit**.

    dev/check-all.sh            # the local mirror of CI — syntax sweep, bash suite, nudge-rs, make
    dev/land.sh                 # land the current branch (opens an editor for the subject)
    dev/land.sh -- --no-rust    # same, skipping the nudge-rs leg

`make check` is **not** this gate: it runs the bash suite and nudge-rs's tests, but not
rustfmt, not clippy, and not the syntax sweep. Landing on `make check` alone misses three
things CI fails on.

`land.sh` refuses a dirty tree, a `main` that differs from `origin/main`, and a branch that
is behind `main`. It then squash-merges, runs `dev/check-all.sh` **on the merged tree before
the commit exists**, and commits only if that passes — which is what makes "every commit on
`main` passes CI" a property rather than a hope. The branch is deleted once its tree matches
`main`.

A plain `git merge --squash` discards every commit message on the branch, so `land.sh`
prefills the message with all of them under a `--- Squashed from N commits ---` marker.
Delete what you do not want; what is left is kept verbatim.

Once per clone, so a non-fast-forward merge fails rather than quietly creating a merge
commit:

    git config merge.ff only && git config pull.ff only

That is convenience — `.git/config` is untracked and binds nobody. The remote is the real
constraint: it allows only squash merges, and deletes the branch on merge.

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
builds and passes the gate on its own. Work happens on a feature branch and lands as **one
squashed commit**, through a **pull request**. `main` is protected: it does not accept a
direct push, so the PR is the only way in.

    git switch -c my-change
    dev/check-all.sh            # the local mirror of CI — syntax sweep, bash suite, nudge-rs, make
    dev/check-all.sh --no-rust  # same, skipping the nudge-rs leg
    git push -u origin my-change
    tea pr create               # or open it in the web UI

`make check` is **not** this gate: it runs the bash suite and nudge-rs's tests, but not
rustfmt, not clippy, and not the syntax sweep. Opening a PR on `make check` alone misses
three things CI fails on.

Run `dev/check-all.sh` before pushing anyway, even though CI runs the same jobs on the PR.
It is the same gate several minutes earlier, and it is the reason a red PR is unusual rather
than routine.

The forge enforces the shape that used to be a local script's job: merges are squash-only, so
a PR cannot produce a merge commit or a string of fixup commits on `main`, and the branch is
deleted on merge. The `land.sh` guarantee survives intact by a different route: a PR cannot
merge while its branch is behind `main`, so the head that CI tested and the tree that lands
are the same tree. Falling behind does not quietly weaken the gate — it blocks the merge
until you rebase.

**The PR description is the commit message.** `.forgejo/default_merge_message/` overrides the
default squash message, which would otherwise be a list of the branch's commit subjects with
`Co-authored-by` trailers appended. The squashed commit is instead the PR title as its subject
and the PR description as its body, verbatim — so write the description as the commit message
you want on `main`, not as a note to the reviewer.

Once per clone, so a non-fast-forward `git pull` fails rather than quietly creating a merge
commit:

    git config merge.ff only && git config pull.ff only

That is convenience — `.git/config` is untracked and binds nobody. Branch protection on the
forge is the real constraint.

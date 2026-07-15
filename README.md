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

`make` collects every utility into `./bin` (gitignored). Put that on your PATH once:

```sh
make
export PATH="$PWD/bin:$PATH"     # add to your shell rc
```

The bash scripts are **symlinked**, so editing one takes effect immediately.
`nudge` is built from `nudge-rs/` (`cargo build --release`) and linked in.

```sh
make          # build nudge + link everything into ./bin
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
- `packaging/` — Homebrew formula and AUR PKGBUILDs for `nudge`.

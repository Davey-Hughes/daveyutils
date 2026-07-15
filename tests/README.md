# Tests

Test-suite for the bash utilities in `scripts/`. Plain bash, no framework.

```sh
bash tests/run.sh              # run everything (exits non-zero on any failure)
bash tests/test_cue2flac.sh    # run one file
```

CI runs the same `tests/run.sh` on every push / PR, on both Linux (GNU
coreutils) and macOS (BSD `date`/`sed`/`grep`/`awk`), so the scripts' dual
GNU/BSD code paths are both exercised (see `.github/workflows/tests.yml`).

The suite needs nothing but bash and coreutils. Tests that would otherwise
shell out to a heavy dependency (`ffmpeg`, `img2pdf`, ImageMagick, …) stub it
on `PATH` instead, so no test requires the real tool to be installed.

The Rust `nudge` has its own tests in `nudge-rs/` (`cargo test`); those do
require `tmux`. `make check` runs both suites.

## Layout

| File | Covers |
|------|--------|
| `test_assert.sh` | the harness's own contract: `finish` TERMINATES with the tally's status, so no caller can accidentally discard a failure; plus the suite's file modes (every `test_*.sh` is executable, `assert.sh` is sourced-only) |
| `test_batch_img2pdf.sh` | archive extraction, per-folder PDF grouping, and the image-file filtering |
| `test_batch_makemkvcon.sh` | disc-image discovery and the `makemkvcon` invocation |
| `test_bisect_img.sh` | landscape detection and the left/right split geometry |
| `test_cue2flac.sh` | CUE parsing and per-track FLAC splitting; drives the real script (it has no source guard, so it cannot be sourced) |
| `test_mkvpropedit_set_name.sh` | deriving each MKV's title tag from its filename |
| `test_video_pcm_to_flac.sh` | PCM⇄FLAC stream selection, output paths, and metadata handling (incl. the bash 4.3 nounset-safe array expansion) |
| `assert.sh` | shared `check`/`finish` assertions; sourced by every file above |

Most files source their script under test and call its functions directly;
`assert.sh` is the only shared helper.

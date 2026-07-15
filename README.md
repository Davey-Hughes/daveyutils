# daveyutils

Random command-line utilities. Each script has a `--help`; run it for usage.

| Script | Does | Requires |
|--------|------|----------|
| `cue2flac` | Split a CUE/BIN disc image into per-track FLAC files | `ffmpeg` |
| `batch_img2pdf` | Unzip image archives and make one PDF per folder | `unar`, `img2pdf`, `file` |
| `bisect_img` | Split landscape JPEGs into left/right halves | ImageMagick |
| `video_pcm_to_flac` | Convert PCM⇄FLAC audio streams in MKVs | `fd`, `ffprobe`, `ffmpeg` |
| `batch_makemkvcon` | Rip every Blu-ray/DVD disc image under a directory to MKV | `makemkvcon` |
| `mkvpropedit_set_name` | Set each MKV's title tag from its filename | `mkvpropedit` |
| `nudge` | Rate-limit auto-resumer for AI CLIs in tmux | `tmux`, `at` |

All scripts are `bash` and live in `scripts/`. Tests are in `tests/` — run `bash tests/run.sh`.

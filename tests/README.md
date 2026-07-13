# Tests

Test-suite for the `nudge` script. Plain bash, no framework.

```sh
bash tests/run.sh          # run everything (exits non-zero on any failure)
bash tests/test_helpers.sh # run one file
```

CI runs the same `tests/run.sh` on every push / PR, on both Linux (GNU
coreutils) and macOS (BSD `date`/`sed`/`grep`/`awk`), so the script's dual
GNU/BSD code paths are both exercised (see `.github/workflows/tests.yml`).

## Layout

| File | Needs | Covers |
|------|-------|--------|
| `test_helpers.sh` | bash + coreutils | pure helpers: `env_bool`, `options_summary`, `pane_after_marker` + `detect_reset_epoch` (the retry marker fix), `normalize_clock` + loosened clock parsing (bare-hour `3pm`, lowercase / spaced meridiem), the `NUDGE_CLOCK_PATTERN` / `NUDGE_DURATION_PATTERN` banner extensions, `has_limit_banner`, `build_next_cmd`, interactive `prompt_options` toggling, `is_relative_timespec` (rejecting relative `-m` times BSD `at` mishandles), `atrun_hint` (the macOS schedule-time reminder + `NUDGE_NO_ATRUN_HINT` suppression), fully case-insensitive duration countdowns (`Resets in 1H30M` / all-caps), a clock-time banner whose reset time sits on the *following* line (`grep -A1` context), and `print_help`'s modern `launchctl` verbs |
| `test_jobs.sh` | bash + coreutils | job-management parsers that reverse `at_pipe`/`build_next_cmd`: `job_inner_cmd`, `job_summary`, `job_detail` (recovering pane / message count / messages from an `at -c` dump, incl. the printf `%q` → `'\''`-escape round-trip), `atq_time_str` (formatting the GNU/BSD ctime date), plus the `--edit` helpers `load_job` (repopulating the scheduling globals), `atq_ctime` / `ctime_to_epoch` (recovering a pending job's fire time), and `edit_has_flags` / `apply_edit_overrides` (overlaying only the explicitly-passed flags for non-interactive edits, so env defaults don't leak in). Also the flag-walk hang guard (a payload ending in a dangling `-p`/`-i`/`-w`/`-r` must reject, not spin — run under `timeout`), `--edit`'s desktop-env preservation (`load_job` capturing the original job's `export …;` prefix + `at_pipe` re-emitting it instead of the editing shell's env), and `ctime_to_epoch`'s locale independence (parsing atq's C-locale English date under `de_DE.UTF-8` — self-skips where no German locale is installed; macOS CI ships one, and that BSD `strptime` leg is exactly where an un-forced locale would fail) |
| `test_resolution.sh` | bash + coreutils | option precedence — env vars < CLI flags < `--no-*` overrides — plus the hermetic `--execute-nudge` guard |
| `test_e2e_tmux.sh` | `tmux` + `at` | headless `--execute-nudge` against a real tmux pane: the `--verify` gate (skips clean); self-skips if the binaries are missing |
| `test_jobs_e2e.sh` | `at`/`atq`/`atrm` | job-management CLI against a **real** `at` spool (isolated in a throwaway queue): `--list`/`--list-plain`, `list_jobs`, `--preview-job`, non-interactive `--edit`, `--cancel`, and the flag-swallow guards. Also the `--preview-job` queue-membership guard (a job in another queue isn't rendered), the non-TTY `--list` plain-table fallback (not the fzf dashboard), the false-"Done!" guard when `at` rejects a time (non-zero exit, nothing queued), and the out-of-queue nudge-job note (staged in a second throwaway queue). This is what actually validates the real `atq` column format on macOS/BSD — `test_jobs.sh` only parses synthetic dumps. Never waits for a job to fire, so the daemon needn't run; self-skips if it can't queue |
| `lib.sh` | — | shared `check`/`finish` assertions; loads helper functions and provides `resolve_case` |

`lib.sh` sources the script's top matter (with the dependency check stripped) so
the helper functions and option-resolution logic can be exercised directly,
without invoking the full script.

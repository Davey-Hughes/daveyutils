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
| `test_helpers.sh` | bash + coreutils | pure helpers: `env_bool`, `options_summary`, `pane_after_marker` + `detect_reset_epoch` (the retry marker fix), `has_limit_banner`, `build_next_cmd`, interactive `prompt_options` toggling, `is_relative_timespec` (rejecting relative `-m` times BSD `at` mishandles), `atrun_hint` (the macOS schedule-time reminder + `NUDGE_NO_ATRUN_HINT` suppression), and `print_help`'s modern `launchctl` verbs |
| `test_jobs.sh` | bash + coreutils | job-management parsers that reverse `at_pipe`/`build_next_cmd`: `job_inner_cmd`, `job_summary`, `job_detail` (recovering pane / message count / messages from an `at -c` dump, incl. the printf `%q` → `'\''`-escape round-trip), and `atq_time_str` (formatting the GNU/BSD ctime date) |
| `test_resolution.sh` | bash + coreutils | option precedence — env vars < CLI flags < `--no-*` overrides — plus the hermetic `--execute-nudge` guard |
| `test_e2e_tmux.sh` | `tmux` + `at` | headless `--execute-nudge` against a real tmux pane: the `--verify` gate (skips clean); self-skips if the binaries are missing |
| `lib.sh` | — | shared `check`/`finish` assertions; loads helper functions and provides `resolve_case` |

`lib.sh` sources the script's top matter (with the dependency check stripped) so
the helper functions and option-resolution logic can be exercised directly,
without invoking the full script.

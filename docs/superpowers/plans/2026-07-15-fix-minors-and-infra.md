# Increment 4 — minors and infra

The last of the whole-repo review: `M1`–`M16`, plus `F1`–`F9` (the follow-ups the
Increment 1/2/3 reviews raised). Findings:
`docs/superpowers/reviews/2026-07-15-whole-repo-review-findings.md`.

**Base:** `main` — flat, no stack. Everything before it (#14–#17, #19) is merged.

## Why this before the bash I19 port (5b)

`scripts/nudge` is not installed into `bin/`; it is only the port's comparison oracle,
and the Rust is now *ahead* of it. Porting I19's snapshot design through the `at`
payload is expensive and lands on a script nobody runs. These findings land on what
does run — and M2 turns out not to be minor at all.

## M2 is the headline, and it is not minor

`nudge-rs/README.md:35` advertises `nudge -p bot:0.1 -a -r -1 -v` — "auto-retry
forever". Against the built binary:

```
$ ./bin/nudge -p fake:0.0 -m '11:59pm' -a -r -1 -i 'x'
error: unexpected argument '-1' found
```

The arg lacks `allow_negative_numbers`, so clap reads `-1` as an unknown short flag.
"Retry forever" is reachable only via the undiscoverable `--retries=-1` equals form.
Increment 2 shipped I2, which makes the retry path honour `retries_left == -1` — a
value the CLI cannot accept. The feature was dead on both ends, and each half looked
correct in isolation.

## Global constraints

- Every fix needs a regression test that **FAILS against the current code**. Reviewers
  here mutation-test; three tautological tests have already been caught on this project
  (`temp_path_is_process_unique`, `a_failed_replace_leaves_exactly_the_original`'s
  comment, `edit_with_no_verify_drops_the_snapshot`'s stub). Prove the bite.
- No attribution in commits.
- **Never run a bare `tmux` command** — the developer is inside tmux; a subagent's
  `kill-server` killed their live session. Isolate with `-L` or `TMUX_TMPDIR`.
- macOS ships **bash 3.2**; socket paths are capped at **SUN_LEN** (104). Use
  `tests/common::short_tempdir()`.
- Rust: `cargo test --no-fail-fast` green, fmt + clippy clean. Bash: `bash -n` clean,
  `bash tests/run.sh` green (now safe — the e2e is isolated).

## Wave 1 — Rust

| # | File | Defect |
|---|------|--------|
| M2 | `cli.rs:42` | `-r -1` rejected by clap; the documented "retry forever" is unreachable. |
| M3 | `paths.rs:26` | A set-but-empty `XDG_STATE_HOME`/`XDG_RUNTIME_DIR` yields **CWD-relative** state and socket paths, so scheduling from one directory and listing from another silently talk to different queues. Empty means unset per the XDG spec. |
| M4 | `timespec.rs:77` | `13pm` silently parses as 13:00; the unanchored meridiem search lets arbitrary text parse as a time. |
| M1 | `daemon.rs:62` | "dropped stale job" logs unconditionally, including right after `remove` failed — the log claims the drop succeeded. |
| M5 | `register/mod.rs:103` | `--uninstall` reports success it didn't achieve. |
| M6 | `app.rs:132` | `--list` ignores `_plain`. Fix the help text, **not** by building a dashboard. |
| M7 | `cli_jobs.rs:33` | Tautological assertion. |
| M8 | `launchd.rs:65` | Weak assertion — may already be closed by #14's `KeepAlive{SuccessfulExit:false}` work; verify before "fixing". |
| F7 | `queue.rs:87` | Pid-unique temp files accumulate on failure with nothing to reap them. |
| F8 | `daemon_singleton.rs:163` | `process::exit(1)` inside a test binary. |
| F9 | `daemon.rs:81` | The fatal `serve` exit has no test. |

## Wave 2 — bash

| # | File | Defect |
|---|------|--------|
| M13 | `cue2flac:110` | Tracklist count wrong. |
| M12 | `cue2flac:162` | Unguarded pipeline. |
| M11 | `batch_img2pdf:23` | Argument parsing. |
| M9 | `batch_img2pdf:59` | Portability. |
| M10 | `video_pcm_to_flac:186` | Portability. |
| F2 | `batch_img2pdf:20` | Symlinks invisible to both sides of the `--clean` coverage check. |
| F3 | `batch_img2pdf:20` | One dotfile disables `--clean` for a folder forever; the WARN doesn't say why. |
| F4 | `test_batch_img2pdf.sh:78` | The C3 test discards stderr, so the WARN is unasserted. |
| F5 | `test_jobs_e2e.sh:31` | Scoped purge leaks when id-parsing breaks. |
| F6 | `test_jobs_e2e.sh:31` | `remember_id` returns 1 on empty input. |
| F1 | `daemon_singleton.rs:86` | Hangs instead of failing on regression. |

## Wave 3 — infra

| # | Defect |
|---|--------|
| M15 | **No workflow invokes the Makefile**, so `bin/` — the whole deliverable of #13 — has no CI at all. A dangling `bin/nudge` would ship green. |
| M14 | `.gitignore` misses the packaging artifacts. |
| M16 | The README's Layout omits the tracked `docs/` tree. |
| M13b | `cue2flac` has no CI coverage. |

## Open question, not for this increment

With the Rust now ahead of the oracle, does `scripts/nudge` still earn its place? Its
stated purpose was feature comparison during the port. Worth a decision before anyone
spends the 5b effort on it.

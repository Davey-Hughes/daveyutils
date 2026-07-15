# Increment 3 — bash correctness

Fixes the Important bash findings from the whole-repo review
(`docs/superpowers/reviews/2026-07-15-whole-repo-review-findings.md`).

**Base:** `fix/critical-data-loss` (#14) — a **sibling** of `fix/rust-correctness` (#15),
not stacked on it. Increment 3 is bash-only and #15 is pure Rust, so the two are
disjoint; both need #14 because the bash changes live there.

**Scope:** I12–I16, I18, I19. **I17 is already done** — Increment 1 scoped the e2e
purge to only the ids the test creates.

## Global constraints

- Every fix needs a regression test that **FAILS against the current code**. Prove the
  bite. Increment 1 shipped a tautological test and the reviewer caught it by
  mutation-testing; Increment 2's fixes were all mutation-verified. Hold that bar.
- No attribution in commits (CLAUDE.md).
- House style: pure helpers at top level, side effects inside `main()`, source-guard
  `if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then main "$@"; fi`. Batch scripts
  warn+continue+summary. macOS ships **bash 3.2** — no namerefs (`local -n`).
- `bash -n` clean; `bash tests/run.sh` green.

## Wave 1 — the small independent ones

| # | File | Defect |
|---|------|--------|
| I12 | `mkvpropedit_set_name` | `find -iname '*.mkv'` matches case-insensitively but `${base%.mkv}` strips case-sensitively → `Movie.MKV` gets the extension **baked into the title tag**, written into the media file, reported as success. |
| I14 | `mkvpropedit_set_name` | `-d`/`-f` as the final argument makes `shift 2` fail under `set -e` → silent abort, exit 1, **no message at all**. Also `-f` without `-d` is accepted then silently ignored. |
| I13 | `bisect_img` | The default (no-argument) invocation writes output as **hidden dotfiles** (`./._foo-1.jpg`), because `basename "$(dirname './foo.jpg')"` is `.`. Reports success; user can't find the halves. |
| I18 | `test_video_pcm_to_flac.sh` | The bash<4.3 skip branch runs `finish` then `exit 0`, discarding finish's status → on macOS's system bash 3.2 a **genuine failure reports PASSED**. Only place in the harness that breaks the contract. |
| I16 | `test_jobs_e2e.sh` | The self-skip probes `at` *through nudge*, so a scheduling regression silently disables the whole file and the suite still says PASSED — on macOS, the platform the file exists to cover. Probe `at` directly instead. |

## Wave 2 — `scripts/nudge`

Sequential after Wave 1 (same file). The bash `nudge` is not installed, but it is the
port's reference oracle, so it stays correct.

| # | Defect |
|---|--------|
| I15 | A value-taking flag with its value omitted (`nudge -p`) **spins forever at 100% CPU** — bash's `shift 2` with `$#`==1 is a silent failing no-op. `-i` is worse: it appends an empty array element per iteration (100k measured) until the OOM killer steps in. The script's own payload walkers already guard this (`shift 2 \|\| return 1`, proven under `timeout` in test_jobs.sh F2); the CLI parser they mirror was never fixed. |
| I19 | `--verify` greps the whole captured pane for a banner with no notion of recency, so **the very banner that motivated the nudge re-triggers the gate** and it injects into a session the user already resumed — exactly what the flag advertises it prevents. `pane_after_marker` already exists for this reason and the auto-retry re-scan uses it; `--verify` doesn't. The existing test calls `fresh_pane`, so its pane never had a banner — it can only catch the trivial case. |

## Verification

`bash -n` on every script, `bash tests/run.sh`, then a whole-increment adversarial
review before the PR.

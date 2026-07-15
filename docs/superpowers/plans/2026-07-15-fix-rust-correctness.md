# Increment 2 — Rust correctness

Fixes the Important Rust findings from the whole-repo review
(`docs/superpowers/reviews/2026-07-15-whole-repo-review-findings.md`).

**Base:** `fix/critical-data-loss` (stacked — several findings touch files Increment 1
just changed; will need a restack if #14 is squash-merged).

**Scope:** I2–I11. **I1 is already done** — it was pulled forward into Increment 1,
because Increment 1's fatal-`serve` exit was only sound once transient `accept()`
errors stopped reaching it.

## Global constraints

- Every fix needs a regression test that **FAILS against the current code**. Prove the
  bite; a test that passes before the fix is worthless (Increment 1 shipped one such
  tautology and the review caught it — see `temp_path_is_process_unique`).
- No attribution in commits (CLAUDE.md).
- Do not undo Increment 1: `ipc/server.rs`'s accept loop already retries transient
  errors, and `serve` returning is deliberately fatal to the process.
- `cargo fmt` + `cargo clippy --all-targets -- -D warnings` clean; full suite green.

## Wave 1 — outside the `app.rs` cluster

| # | File | Defect |
|---|------|--------|
| I5 | `detect.rs` | `find` is leftmost, and pane text is chronological, so the **oldest** banner wins → nudge fires hours early. Use the last match; pick clock-vs-duration by which banner sits later, not by branch order. |
| I6 | `detect.rs` | `NUDGE_*_PATTERN` is interpolated raw and compiled with `.expect()` → an invalid pattern panics and kills the daemon's scheduler thread. Fall back to the built-in with a `tracing::warn!` (explicitly sanctioned by the finding, and keeps the fix inside `detect.rs`). |
| I2 | `scheduler.rs` | `apply_outcome`'s catch-all deletes the job when injection **fails**, so `-a -r -1` ("retry forever") gets zero retries after one transient tmux blip. Add an explicit `Err(_)` arm that reschedules while retries remain. |
| I4 | `queue.rs` | `add`/`remove`/`reschedule` mutate memory then persist, with no rollback → the CLI says "rejected" while the daemon still fires the job. Commit to `self.state` only after `save()` succeeds. |
| I3 | `ipc` | No read/write timeout anywhere, and `accept` handles connections serially → one client that connects and never sends a newline wedges the whole control plane forever. Timeouts on both ends. |
| I11 | `cli.rs` | Two tests race on the process-global `NUDGE_NOTIFY` (~3% failure over 200k interleavings) and the concurrent `set_var`/`var` is UB. Call the pure `config::resolve` directly, as `config.rs`'s own tests already do. |

## Wave 2 — the `app.rs` / `config.rs` cluster

Sequential after Wave 1: these overlap each other's files, and parallel agents would
race on the git index.

| # | File | Defect |
|---|------|--------|
| I7 | `config.rs` | `--retries N` is applied after the auto-retry override and unconditionally sets `auto_retry = true`, so `--no-auto-retry` silently does nothing whenever `-r` is present. |
| I8 | `app.rs` | `--edit <id> --auto-retry` yields `auto_retry=true, retries_left=0` → never retries, and the CLI reports success. Inconsistent with a fresh schedule, which gets the default count. |
| I9 | `app.rs` | `--list/--cancel/--edit` never call `ensure_daemon` → with the daemon down they fail with a bare errno, and a persisted job **cannot be cancelled even though it will still fire** on the next daemon start. |
| I10 | `app.rs` | `edit`'s Schedule-then-Cancel is not atomic: if the Cancel leg fails, both jobs live and the message is injected **twice**, while the error names neither. |

## Verification

`cargo test`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`bash tests/run.sh`. Then a whole-increment adversarial review before the PR.

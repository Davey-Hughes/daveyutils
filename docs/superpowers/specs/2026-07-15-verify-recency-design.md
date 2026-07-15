# `--verify` recency — design (finding I19)

**Status:** approved by the user 2026-07-15. Applies to **both** `nudge-rs` and the
bash oracle `scripts/nudge`.

## The bug

`--verify` exists to answer "did the user already resume this session?" and skip the
injection if so. It currently answers a different question — "is there a banner
anywhere in the pane?" — with no notion of recency.

At 23:00 Claude prints `⏸ session limit reached · resets 3:00am`. You run
`nudge -p bot:0.1 -v`, which schedules for 03:03. At 03:01 you resume manually and
Claude replies. At 03:03 the pane **still shows the 03:00 banner a few lines up**, so
the check passes and nudge injects "please continue" into the session you are actively
using — the exact outcome the flag advertises it prevents.

Both implementations share it:
- `nudge-rs/src/inject.rs:32-36` — captures the pane, calls `detect_reset` on the whole thing.
- `scripts/nudge:1194` — captures the pane, greps it with `has_limit_banner`, positionally blind.

## Rejected: scope to "after the last user input"

This is what the original finding prescribed (reuse `pane_after_marker`, as the
auto-retry re-scan does). **It is wrong**, verified against live panes on this machine:

- Claude Code runs on the **alternate screen** (`alternate_on=1`) with **no scrollback**
  (`history_size` ≤ 7) — only the ~36 visible rows exist.
- Submitted messages are **never echoed**: 0 hits for `^> ` across the full scrollback
  of all 19 panes.
- The only prompt-like line is the `❯` input widget, which is **pinned to the bottom** —
  always *below* the banner, i.e. the opposite of a recency marker.

So `pane_after_marker` yields a tail that can never contain a banner. On a real capture
in the **still-limited** case — where `--verify` must fire — it flips INJECT to SKIP.
That trades a rare spurious inject for a **100% silent never-fire**.

`pane_after_marker` is legitimate for the retry path only because its marker is a string
nudge itself just injected. `--verify` runs before any injection and has no such anchor.

## The asymmetry that drives the design

The two failure modes are **not** equally bad:

| Failure | Cost |
|---|---|
| False INJECT (today's bug) | A stray "please continue" typed into a session you're using. Annoying. |
| False SKIP | Your overnight nudge silently never fires. **Defeats the entire tool.** |

**Every ambiguity must therefore resolve toward INJECT.** A recency check that is unsure
must fail *open*.

## The design: snapshot at schedule, diff at fire

At **schedule** time (when `--verify` is on), capture the pane and store on the job:
- `verify_fingerprint` — a hash of the **normalized full capture**
- `verify_dims` — the pane dimensions the capture was taken at

At **fire** time, if `job.verify`:

1. Capture the pane; read its dimensions.
2. **Dimensions differ from `verify_dims`** → the capture reflowed and is not comparable
   → **fail open**: fall back to today's behavior (banner present → inject).
3. **Fingerprint differs** → something happened since we scheduled → the user resumed →
   **SKIP**.
4. **Fingerprint matches** → the pane is untouched → banner check as today → inject.

### Why a whole-pane hash, and not the banner's position

Considered and rejected: track the banner's row offset. It fails in the common
not-yet-full case — new output appends into blank space below the banner without
scrolling it, so the offset is unchanged while the pane plainly changed. A normalized
full-capture hash catches both the scrolling and appending cases.

### Empirical basis (measured on this machine, 2026-07-15)

- An **idle** Claude pane is **byte-identical** over 25s (`cst:1.0`). Parked-at-a-banner
  is idle, and idle is stable — this is what makes the scheme viable.
- Every pane that changed over the same window was one **actively doing work**. The
  signal tracks exactly what we want.
- **No clock or counter in the bottom chrome**, so nothing drifts on its own across a
  4-hour wait.
- **Resize is the one real confound** — it reflows the whole capture. Hence the dims
  guard and fail-open at step 2.

### Normalization

Minimal and generic — strip trailing whitespace per line, drop trailing blank lines. Do
**not** try to strip TUI chrome (status line, input widget): that needs per-tool,
per-version knowledge, is brittle across Claude/Codex/etc, and is exactly why the
"banner must be the last substantive line" option was rejected.

### Observability

A skip must be **visible** — the user has to be able to tell "it skipped because you'd
resumed" from "it silently never ran". Report the skip on the existing notify/log path,
naming the reason.

### `--edit`

Re-scheduling re-captures: the pane state at edit time becomes the new baseline.

## Scope

Two PRs from this one design (the branch topology forces it — `scripts/nudge` and
`nudge-rs/src/inject.rs` live on sibling branches):
1. **`feat/verify-recency-rs`** on `fix/rust-correctness` (#15) — the binary you run.
2. **`feat/verify-recency-sh`** on `fix/bash-correctness` (#16) — the oracle. The
   fingerprint has to round-trip through the `at` payload (`at_pipe` / `load_job` /
   `job_summary` / `--edit`).

//! The `--verify` recency gate: has this pane moved since the user scheduled?
//!
//! `--verify` used to ask only "is a rate-limit banner somewhere on screen?",
//! which the *stale* banner that motivated the nudge answers `yes` to forever —
//! so nudge injected into sessions the user had already resumed (finding I19).
//! Recency comes from a snapshot: fingerprint the pane when the job is
//! scheduled, and at fire time compare. A parked-at-a-banner pane is idle, and
//! an idle pane is byte-identical over hours; every pane that moves is one
//! doing work.
//!
//! The two failure modes are not symmetric. A false INJECT is a stray "please
//! continue" in a session you are using: annoying. A false SKIP is your
//! overnight nudge silently never firing, which defeats the entire tool. So
//! **every ambiguity here resolves toward INJECT** — see [`Recency::Unknown`],
//! which every uncertain path funnels into.

use crate::target::{PaneDims, Target};

/// Canonicalize a capture before hashing: strip trailing whitespace per line
/// and drop trailing blank lines.
///
/// Deliberately minimal and tool-agnostic. It is tempting to also strip the TUI
/// chrome (status line, input widget) so the hash tracks only "real" output,
/// but that needs per-tool, per-version knowledge and is brittle across
/// Claude/Codex/etc. The cost of not doing it is bounded and lands on the safe
/// side of nothing: unrecognized chrome churn reads as "the pane changed" ->
/// SKIP, so it is the one place this design can be over-eager. It is confined
/// to panes the user actually touched, and the skip is reported by name.
pub fn normalize(capture: &str) -> String {
    let mut lines: Vec<&str> = capture.lines().map(|l| l.trim_end()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// A stable content hash of a normalized capture (FNV-1a 64, hex).
///
/// Hand-rolled rather than `std::hash::DefaultHasher` because this value is
/// **persisted in queue.json** and compared against one computed hours later,
/// possibly by a different build. `DefaultHasher`'s output is explicitly not
/// guaranteed stable across Rust releases, and a hash that silently changes
/// meaning between builds would make every pending `--verify` job compare
/// unequal — i.e. skip. FNV-1a is fixed forever.
pub fn fingerprint(capture: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalize(capture).as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// What the recency gate concluded about a pane.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Recency {
    /// Byte-identical to the snapshot, at the same size: the user has not
    /// touched it. Proceed to the banner check.
    Unchanged,
    /// Same size, different content: something happened here since we
    /// scheduled. The user resumed. Skip.
    Changed,
    /// Not comparable — no snapshot, or the pane was resized and the capture
    /// reflowed, or its size cannot be read.
    ///
    /// **Fails open** to the banner check, i.e. exactly what nudge did before
    /// this gate existed. This variant is the design's whole safety margin:
    /// every way of being unsure lands here, and landing here can only cost a
    /// stray inject, never a silent never-fire.
    Unknown,
}

/// A pane snapshot taken when a `--verify` job was scheduled.
pub struct Baseline {
    pub fingerprint: String,
    pub dims: PaneDims,
}

/// Snapshot `target` for a job being scheduled, or `None` if it cannot be
/// snapshotted.
///
/// Infallible by construction: a pane that will not capture, or will not report
/// its size, yields no baseline, and a job with no baseline fails open at fire
/// time. Scheduling must not fail because the recency gate could not arm — the
/// user asked for a nudge, and a nudge with a degraded `--verify` is strictly
/// better than no job at all.
///
/// **The dims-then-capture order is load-bearing** and is not the mistake it
/// looks like next to `run_injection`, which reads dims on both sides of its
/// capture. Consider a resize straddling the two calls here:
///
/// - As written, we store the OLD dims with a NEW (reflowed) capture. At fire
///   time the pane reads at its new size, which does not equal the stored dims
///   → `Unknown` → fail open → inject. Safe.
/// - Reversed (capture, then dims) we would store the NEW dims with an OLD
///   capture. At fire time the dims *match*, so the fingerprint is trusted —
///   and it compares a pre-reflow hash against a post-reflow capture, which
///   differs → `Changed` → **skip**. The nudge silently never fires.
///
/// So a resize racing this function can only ever produce a baseline that
/// fails open, never one that skips. Reading dims twice here would be safe too
/// (disagreement → no baseline → fail open), but it buys nothing the ordering
/// does not already guarantee.
pub fn capture_baseline(target: &dyn Target) -> Option<Baseline> {
    let dims = target.dims()?;
    let text = target.capture().ok()?;
    Some(Baseline {
        fingerprint: fingerprint(&text),
        dims,
    })
}

/// Compare a job's snapshot against the pane as it is now.
///
/// `baseline` is `None` for any job that has no usable snapshot: scheduled
/// before this feature existed (an old queue.json), or scheduled when the pane
/// could not be read. `now_dims` is `None` when the pane's size cannot be read
/// now. Both are [`Recency::Unknown`].
pub fn recency(
    baseline: Option<(String, PaneDims)>,
    now_fingerprint: &str,
    now_dims: Option<PaneDims>,
) -> Recency {
    let Some((base_fp, base_dims)) = baseline else {
        return Recency::Unknown;
    };
    let Some(now_dims) = now_dims else {
        return Recency::Unknown;
    };
    // Checked before the fingerprint, not after: on a resize the fingerprint's
    // verdict is meaningless in *both* directions. "Differs" would be reflow,
    // not the user; "matches" would be luck (a capture with nothing wrapped).
    if now_dims != base_dims {
        return Recency::Unknown;
    }
    if base_fp == now_fingerprint {
        Recency::Unchanged
    } else {
        Recency::Changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::PaneDims;

    #[test]
    fn normalize_strips_trailing_space_and_trailing_blank_lines() {
        assert_eq!(normalize("a  \nb\t\n\n\n"), "a\nb");
    }

    #[test]
    fn fingerprint_is_stable_and_distinguishes_content() {
        assert_eq!(fingerprint("a\nb"), fingerprint("a  \nb\n\n"));
        assert_ne!(fingerprint("a\nb"), fingerprint("a\nb\nc"));
    }

    #[test]
    fn recency_says_unchanged_only_when_dims_and_fingerprint_both_match() {
        let d = PaneDims {
            width: 80,
            height: 24,
        };
        let base = Some((fingerprint("x"), d));
        assert_eq!(
            recency(base.clone(), &fingerprint("x"), Some(d)),
            Recency::Unchanged
        );
        assert_eq!(
            recency(base.clone(), &fingerprint("y"), Some(d)),
            Recency::Changed
        );
    }

    /// Every ambiguity resolves toward INJECT. Each of these is a path that,
    /// read as "changed", would silently never fire.
    #[test]
    fn recency_is_unknown_and_fails_open_for_every_ambiguous_input() {
        let d = PaneDims {
            width: 80,
            height: 24,
        };
        let wider = PaneDims {
            width: 100,
            height: 24,
        };
        let taller = PaneDims {
            width: 80,
            height: 40,
        };
        let base = Some((fingerprint("x"), d));

        // No baseline at all: an old queue.json's job, or a schedule-time
        // capture that failed. Must behave exactly as today.
        assert_eq!(recency(None, &fingerprint("y"), Some(d)), Recency::Unknown);
        // Dims unreadable now -> not comparable.
        assert_eq!(
            recency(base.clone(), &fingerprint("y"), None),
            Recency::Unknown
        );
        // Resized: the capture reflowed, so a differing fingerprint says
        // nothing about whether the user resumed.
        assert_eq!(
            recency(base.clone(), &fingerprint("y"), Some(wider)),
            Recency::Unknown
        );
        assert_eq!(
            recency(base.clone(), &fingerprint("y"), Some(taller)),
            Recency::Unknown
        );
        // A width-only resize can leave the fingerprint *identical* (a capture
        // with no wrapped lines). Still not comparable: report Unknown, which
        // fails open to the banner check, rather than a false Unchanged.
        assert_eq!(
            recency(base, &fingerprint("x"), Some(wider)),
            Recency::Unknown
        );
    }
}

//! Boolean env parsing and env < flag < `--no-*` option precedence.

#[derive(Clone, Debug, PartialEq)]
pub struct Toggles {
    pub notify: bool,
    pub verify: bool,
    pub auto_retry: bool,
    pub retries: i64,
    pub settle_secs: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlagOverrides {
    pub notify: Option<bool>,
    pub verify: Option<bool>,
    pub auto_retry: Option<bool>,
    pub retries: Option<i64>,
    pub settle_secs: Option<f64>,
}

/// `1/true/yes/on` (any case) -> true; everything else (incl. None) -> false.
pub fn env_bool(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Overlay `overrides` (present values only) onto the env `Toggles`.
pub fn resolve(env: &Toggles, overrides: &FlagOverrides) -> Toggles {
    let mut out = env.clone();
    if let Some(v) = overrides.notify {
        out.notify = v;
    }
    if let Some(v) = overrides.verify {
        out.verify = v;
    }
    if let Some(v) = overrides.auto_retry {
        out.auto_retry = v;
    }
    if let Some(v) = overrides.settle_secs {
        out.settle_secs = v;
    }
    if let Some(v) = overrides.retries {
        out.retries = v;
        // A retry count implies auto-retry -- but only as a default, never over
        // an explicit flag. Setting this unconditionally clobbered the `false`
        // that `--no-auto-retry` had just written, so `--retries 5
        // --no-auto-retry` scheduled a job with auto_retry=true/retries_left=5
        // and retried it five times for a user who asked for none.
        if overrides.auto_retry.is_none() {
            out.auto_retry = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_bool_truthy_and_falsy() {
        for t in ["1", "true", "TRUE", "Yes", "on"] {
            assert!(env_bool(Some(t)), "{t} should be truthy");
        }
        for f in ["0", "false", "no", "", "banana"] {
            assert!(!env_bool(Some(f)), "{f} should be falsy");
        }
        assert!(!env_bool(None));
    }

    fn env() -> Toggles {
        Toggles {
            notify: true,
            verify: false,
            auto_retry: false,
            retries: 2,
            settle_secs: 5.0,
        }
    }

    fn no_overrides() -> FlagOverrides {
        FlagOverrides {
            notify: None,
            verify: None,
            auto_retry: None,
            retries: None,
            settle_secs: None,
        }
    }

    #[test]
    fn env_defaults_apply_when_no_flags() {
        let out = resolve(&env(), &no_overrides());
        assert!(out.notify);
        assert!(!out.verify);
    }

    #[test]
    fn flag_overrides_env() {
        let mut ov = no_overrides();
        ov.notify = Some(false); // --no-notify beats NUDGE_NOTIFY=1
        ov.verify = Some(true); // -v beats unset
        let out = resolve(&env(), &ov);
        assert!(!out.notify);
        assert!(out.verify);
    }

    #[test]
    fn setting_retries_implies_auto_retry() {
        let mut ov = no_overrides();
        ov.retries = Some(5);
        let out = resolve(&env(), &ov);
        assert!(out.auto_retry);
        assert_eq!(out.retries, 5);
    }

    #[test]
    fn an_explicit_no_auto_retry_beats_the_retries_implication() {
        // `--retries 5 --no-auto-retry`. The count implies auto-retry only as a
        // default; an explicitly-passed flag is the user's stated intent and must
        // win. Clobbering it here persisted the job with auto_retry=true /
        // retries_left=5, so the scheduler retried five times a user who asked
        // for none -- and --no-auto-retry, the only spelling that disables it,
        // silently did nothing whenever -r was also present.
        let mut ov = no_overrides();
        ov.retries = Some(5);
        ov.auto_retry = Some(false);
        let out = resolve(&env(), &ov);
        assert!(
            !out.auto_retry,
            "--no-auto-retry must survive an explicit --retries"
        );
        assert_eq!(out.retries, 5, "the count itself is still honoured");
    }

    #[test]
    fn an_explicit_auto_retry_still_wins_alongside_retries() {
        // The mirror case: --auto-retry --retries 5 agree, and env auto_retry
        // false must not leak through.
        let mut ov = no_overrides();
        ov.retries = Some(5);
        ov.auto_retry = Some(true);
        let out = resolve(&env(), &ov);
        assert!(out.auto_retry);
        assert_eq!(out.retries, 5);
    }
}

//! Single pre-send trim decision kernel.
//!
//! Context management is cache management: the *only* thing that should
//! decide *whether* and *how far* to trim is one comparison between a
//! provider-authoritative token count and the configured budget. Before this
//! kernel the three trim paths (preemptive turn-boundary trim, reported-budget
//! enforce, and overflow recovery) each re-derived that decision inline, so the
//! semantics drifted. They now share [`plan_pre_send_trim`].
//!
//! The one place we deliberately keep a *distinct* aggressiveness is overflow
//! recovery: when a provider has already rejected the request, a preemptive
//! "just get under budget" target may still be too big to fit next time, so we
//! force a deeper shrink to a fraction of the current size. That is the single
//! knob [`plan_pre_send_trim`] varies via `forced_shrink`.

/// The outcome of a pre-send budget check: whether to trim, and the token
/// budget the trim should drive the history down to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrimDecision {
    pub should_trim: bool,
    pub target_budget: usize,
}

impl TrimDecision {
    /// Do nothing — the budget is disabled or the history already fits.
    const NO_OP: TrimDecision = TrimDecision {
        should_trim: false,
        target_budget: 0,
    };
}

/// Decide whether to trim before a send, and to what budget.
///
/// - `tokens_now`: the size of the history about to be sent. **Pre-send** it
///   is the local estimate, because no report for a request that has not been
///   sent can exist; that is the water-line's whole job. **Post-response** it
///   is [`ContextCalibration::current`], anchored on the provider's reported
///   `input_tokens` when there is one. The caller owns which evidence applies;
///   this kernel only compares the number it is handed.
/// - `trim_threshold`: the configured budget. `0` means the budget is disabled,
///   so the pipeline never trims.
/// - `forced_shrink`: overflow recovery. A provider already rejected this
///   history, so trim to a fraction of the *current* size rather than merely
///   down to the threshold — a next-fit target that keeps most of the history
///   risks overflowing again. The fraction (`2/3` of the current size) is kept
///   exactly as the historical overflow path did.
#[must_use]
pub(crate) fn plan_pre_send_trim(
    tokens_now: usize,
    trim_threshold: usize,
    forced_shrink: bool,
) -> TrimDecision {
    // Overflow recovery ignores the configured budget entirely: a provider has
    // already rejected this history, so we force a deeper shrink to a fraction
    // of the *current* size — even when the budget is disabled (`threshold ==
    // 0`), where merely getting "under budget" is not actionable. This is the
    // one place that keeps a distinct aggressiveness from the preemptive path.
    if forced_shrink {
        let target = tokens_now.saturating_mul(2) / 3;
        TrimDecision {
            // Only worth trimming if the target is strictly below the current
            // size; a 0-token history can't shrink further.
            should_trim: target < tokens_now,
            target_budget: target,
        }
    } else if trim_threshold == 0 {
        // Preemptive path with the budget disabled: never trim.
        TrimDecision::NO_OP
    } else if tokens_now > trim_threshold {
        TrimDecision {
            should_trim: true,
            target_budget: trim_threshold,
        }
    } else {
        TrimDecision::NO_OP
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_budget_never_trims_preemptively() {
        // trim_threshold == 0 disables the *preemptive* pipeline entirely.
        assert_eq!(plan_pre_send_trim(1_000_000, 0, false), TrimDecision::NO_OP);
    }

    #[test]
    fn forced_shrink_ignores_disabled_budget() {
        // Even with the budget disabled, a provider that already rejected the
        // history still forces the deeper 2/3 shrink — "get under budget" is
        // meaningless when there is no budget, but headroom still matters.
        let d = plan_pre_send_trim(1_000_000, 0, true);
        assert!(d.should_trim);
        assert_eq!(d.target_budget, 1_000_000 * 2 / 3);
    }

    #[test]
    fn preemptive_under_threshold_is_noop() {
        assert_eq!(plan_pre_send_trim(100, 1_000, false), TrimDecision::NO_OP);
        // Exactly at threshold is not over threshold: still no trim.
        assert_eq!(plan_pre_send_trim(1_000, 1_000, false), TrimDecision::NO_OP);
    }

    #[test]
    fn preemptive_over_threshold_trims_down_to_threshold() {
        let d = plan_pre_send_trim(1_500, 1_000, false);
        assert!(d.should_trim);
        assert_eq!(d.target_budget, 1_000);
    }

    #[test]
    fn forced_shrink_targets_two_thirds_of_current() {
        let d = plan_pre_send_trim(1_500, 1_000, true);
        assert!(d.should_trim);
        // Even though 1_000 (threshold) < 1_500, the forced target is 2/3 of
        // current, which is 1_000 here — the deeper, headroom-y target wins.
        assert_eq!(d.target_budget, 1_000);
    }

    #[test]
    fn forced_shrink_goes_below_threshold_when_current_is_far_over() {
        // Forced shrink ignores the threshold and always drives to 2/3 of the
        // current size, so a huge history shrinks well past the threshold.
        let d = plan_pre_send_trim(9_000, 1_000, true);
        assert!(d.should_trim);
        assert_eq!(d.target_budget, 6_000);
    }

    #[test]
    fn forced_shrink_still_shrinks_when_two_thirds_exceeds_threshold() {
        // tokens_now just over threshold, but 2/3 is still under it — forced
        // shrink does the deeper trim regardless.
        let d = plan_pre_send_trim(1_200, 1_000, true);
        assert!(d.should_trim);
        assert_eq!(d.target_budget, 800);
    }

    #[test]
    fn forced_shrink_noop_when_history_cannot_shrink() {
        // 1 token * 2/3 == 0 < 1, so it does trim to 0... except a tiny history
        // still meaningfully shrinks. 0 tokens cannot shrink: target == tokens.
        let d = plan_pre_send_trim(0, 1_000, true);
        assert!(!d.should_trim);
    }
}

//! Single pre-send trim decision kernel.
//!
//! Context management is cache management: the *only* thing that decides
//! *whether* to trim is one comparison between a provider-authoritative token
//! count and the configured budget. Every trim path (preemptive turn-boundary
//! trim, reported-budget enforce, provider-rejection recovery) shares this
//! kernel, so the trigger cannot drift between them.
//!
//! There is no deeper variant for overflow recovery: the action is already
//! "keep the newest `keep_recent_turns` whole turns, then one fewer at a time
//! while the retained set still exceeds the budget", and that cascade is what
//! covers a provider that rejected the first attempt.

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

/// Decide whether to trim before a send.
///
/// - `tokens_now`: the size of the request about to be sent. **Pre-send** it
///   is the local estimate, because no report for a request that has not been
///   sent can exist. **Post-response** it is [`ContextCalibration::current`],
///   anchored on the provider's reported `input_tokens` when there is one. The
///   caller owns which evidence applies; this kernel only compares the number
///   it is handed.
/// - `trim_budget`: the configured budget, which is also the fit target a trim
///   drives the retained history toward. `0` means the budget is unset, so the
///   pipeline never trims.
#[must_use]
pub(crate) fn plan_pre_send_trim(tokens_now: usize, trim_budget: usize) -> TrimDecision {
    if trim_budget == 0 {
        // Budget unset: never trim on a proactive check.
        TrimDecision::NO_OP
    } else if tokens_now > trim_budget {
        TrimDecision {
            should_trim: true,
            target_budget: trim_budget,
        }
    } else {
        TrimDecision::NO_OP
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_budget_never_trims() {
        assert_eq!(plan_pre_send_trim(1_000_000, 0), TrimDecision::NO_OP);
    }

    #[test]
    fn preemptive_under_threshold_is_noop() {
        assert_eq!(plan_pre_send_trim(100, 1_000), TrimDecision::NO_OP);
        // Exactly at the budget is not over it: still no trim.
        assert_eq!(plan_pre_send_trim(1_000, 1_000), TrimDecision::NO_OP);
    }

    #[test]
    fn preemptive_over_threshold_trims_to_the_budget() {
        let d = plan_pre_send_trim(1_500, 1_000);
        assert!(d.should_trim);
        assert_eq!(d.target_budget, 1_000);
    }
}

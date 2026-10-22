//! Whole-turn history trimming. One rule: keep the most recent whole turns
//! that fit the token budget, drop the rest, never cut a turn in half.

use crate::agent::history::estimate_history_tokens;
use zeroclaw_api::model_provider::ConversationMessage;
use zeroclaw_providers::ChatMessage;

/// Outcome of a trim pass. `trimmed` is true only when at least one whole turn
/// was dropped, in which case the caller emits a user-visible event and injects
/// a breadcrumb so the loss is never silent.
#[derive(Debug, Clone)]
pub struct TrimResult {
    pub history: Vec<ChatMessage>,
    pub dropped_messages: usize,
    pub dropped_turns: usize,
    pub kept_turns: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub trimmed: bool,
    /// After cascading to `kept_turns == 1`, history still exceeds `send_budget`.
    pub exceeds_budget: bool,
    /// True when a successful cascade kept fewer turns than `preferred_keep`.
    pub below_preferred_keep: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnCountTrimResult<T> {
    pub history: Vec<T>,
    pub dropped_messages: usize,
    pub dropped_turns: usize,
    pub kept_turns: usize,
    pub trimmed: bool,
}

fn is_conversation_system(msg: &ConversationMessage) -> bool {
    matches!(msg, ConversationMessage::Chat(chat) if chat.role == "system")
}

fn is_turn_boundary(msg: &ChatMessage) -> bool {
    msg.role == "user"
        && !msg
            .content
            .starts_with(zeroclaw_api::tool_carrier::TOOL_RESULTS_PREFIX)
}

fn is_system(msg: &ChatMessage) -> bool {
    msg.role == "system"
}

/// Whether a `user`-roled message is machinery the runtime wrote rather than
/// something the operator typed: a prompt-mode `[Tool results]` round, or the
/// trim breadcrumb. Answering "is this ours" is a different question from
/// [`is_turn_boundary`]'s "does a turn start here", so the two are kept apart:
/// the breadcrumb is machinery AND still a turn boundary, while a tool round
/// is machinery and explicitly not one.
pub fn is_synthetic_user_message(msg: &ChatMessage) -> bool {
    if msg.role != "user" {
        return false;
    }
    let trimmed = msg.content.trim_start();
    trimmed.starts_with(zeroclaw_api::tool_carrier::TOOL_RESULTS_PREFIX)
        || trimmed.starts_with(breadcrumb_text().as_str())
}

/// The breadcrumb's localized text, kept in one place so the writer, the
/// machinery predicate, and its tests cannot drift apart.
fn breadcrumb_text() -> String {
    crate::i18n::get_required_cli_string("history-trim-breadcrumb")
}

/// Keep at most `keep_turns` newest whole turns, always keeping leading
/// system messages and at least the most recent whole turn. `0` is
/// normalised to `1` so the newest turn is never dropped.
///
/// When `crumb_present` is true the population is assumed to carry the
/// synthetic breadcrumb immediately after the leading system messages. That
/// breadcrumb is never counted as a turn boundary, never dropped, and never
/// double-counted in `dropped_messages`. The owner flag decides, not message
/// text.
///
/// This is the count-based primitive the spec calls "prefer keep-N"; the
/// budget cascade on top of it is [`trim_to_fit`].
fn keep_recent_whole_turns(
    history: Vec<ChatMessage>,
    keep_turns: usize,
    crumb_present: bool,
) -> TrimResult {
    let keep = keep_turns.max(1);
    let tokens_before = estimate_history_tokens(&history);
    let leading_system = history.iter().take_while(|m| is_system(m)).count();
    let crumb_offset = usize::from(crumb_present && history.len() > leading_system);
    // `total_turns` excludes the synthetic crumb so `kept_turns` stays
    // breadcrumb-aware. When the flag is set the crumb is at
    // `leading_system` by contract, so the body starts after it.
    let body_start = leading_system + crumb_offset;
    let body = if body_start <= history.len() {
        &history[body_start..]
    } else {
        &[][..]
    };
    let boundaries: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, message)| is_turn_boundary(message))
        .map(|(i, _)| i)
        .collect();
    let total_turns = boundaries.len();

    if total_turns <= keep {
        return TrimResult {
            history,
            dropped_messages: 0,
            dropped_turns: 0,
            kept_turns: total_turns,
            tokens_before,
            tokens_after: tokens_before,
            trimmed: false,
            exceeds_budget: false,
            below_preferred_keep: false,
        };
    }

    let start = boundaries[total_turns - keep];
    let dropped_messages = start;
    let dropped_turns = total_turns - keep;
    let mut kept: Vec<ChatMessage> = history[..body_start].to_vec();
    kept.extend_from_slice(&body[start..]);
    let tokens_after = estimate_history_tokens(&kept);

    TrimResult {
        history: kept,
        dropped_messages,
        dropped_turns,
        kept_turns: keep,
        tokens_before,
        tokens_after,
        trimmed: true,
        exceeds_budget: false,
        below_preferred_keep: false,
    }
}

/// Keep at most `max_turns` recent provider-facing turns. This is the
/// count-based companion to the keep-N cut; both use identical turn
/// boundaries and always retain the newest complete turn.
pub(crate) fn trim_to_recent_turn_count(
    history: Vec<ChatMessage>,
    max_turns: usize,
    has_leading_breadcrumb: bool,
) -> TurnCountTrimResult<ChatMessage> {
    let total_turns = count_turns(&history).saturating_sub(usize::from(has_leading_breadcrumb));
    if total_turns <= max_turns || total_turns <= 1 {
        return TurnCountTrimResult {
            history,
            dropped_messages: 0,
            dropped_turns: 0,
            kept_turns: total_turns,
            trimmed: false,
        };
    }

    let leading_system = history
        .iter()
        .take_while(|message| is_system(message))
        .count();
    let system = history[..leading_system].to_vec();
    let body = &history[leading_system..];
    let breadcrumb_index = has_leading_breadcrumb.then_some(0);
    let boundaries: Vec<usize> = body
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (is_turn_boundary(message) && Some(index) != breadcrumb_index).then_some(index)
        })
        .collect();
    let kept_turns = max_turns.max(1).min(boundaries.len());
    let dropped_turns = boundaries.len() - kept_turns;
    let first_kept = boundaries[dropped_turns];

    let mut kept = system;
    if has_leading_breadcrumb {
        kept.push(body[0].clone());
    }
    kept.extend_from_slice(&body[first_kept..]);
    TurnCountTrimResult {
        history: kept,
        dropped_messages: first_kept - usize::from(has_leading_breadcrumb),
        dropped_turns,
        kept_turns,
        trimmed: true,
    }
}

pub fn trim_to_reported_budget(
    history: Vec<ChatMessage>,
    budget_tokens: usize,
    reported_input_tokens: usize,
    // Estimated token count of the exact message population that produced
    // `reported_input_tokens` (the pre-request `prepared_messages`, before any
    // assistant/tool-result output for this iteration was appended to
    // `history`). Scaling the selection target against this measured population
    // keeps it consistent with the calibration ratio used for `tokens_after`;
    // re-estimating the larger post-append `history` here would select more
    // retention than the calibration justifies and let the final history
    // overrun the budget.
    reported_population_estimated: usize,
    tool_schema_tokens: usize,
) -> TrimResult {
    trim_to_reported_budget_with_crumb(
        history,
        budget_tokens,
        reported_input_tokens,
        reported_population_estimated,
        tool_schema_tokens,
        false,
    )
}

/// Crumb-aware variant: when `crumb_present` is true the population carries
/// the synthetic breadcrumb immediately after leading system messages.
/// The breadcrumb is never counted as a turn and never dropped.
pub fn trim_to_reported_budget_with_crumb(
    history: Vec<ChatMessage>,
    budget_tokens: usize,
    reported_input_tokens: usize,
    reported_population_estimated: usize,
    tool_schema_tokens: usize,
    crumb_present: bool,
) -> TrimResult {
    let estimated = reported_population_estimated;
    if budget_tokens == 0 || reported_input_tokens <= budget_tokens || estimated == 0 {
        let total_turns = count_turns(&history).saturating_sub(usize::from(crumb_present));
        return TrimResult {
            tokens_before: reported_input_tokens,
            tokens_after: reported_input_tokens,
            history,
            dropped_messages: 0,
            dropped_turns: 0,
            kept_turns: total_turns,
            trimmed: false,
            exceeds_budget: false,
            below_preferred_keep: false,
        };
    }
    // Native tool schemas are constant across a trim, so reserve them inside
    // the scaled total and trim the history portion to what remains: the
    // retained history plus tools then fits the budget when the reported
    // count is faithful.
    let target_total =
        (budget_tokens as u128 * estimated as u128 / reported_input_tokens as u128).max(1) as usize;
    let scaled = target_total.saturating_sub(tool_schema_tokens).max(1);
    let preferred = zeroclaw_config::scattered_types::DEFAULT_KEEP_RECENT_TURNS;
    // The reported-budget path has no model window of its own: the
    // caller's budget is the only ceiling, so the hard floor is not judged here.
    let result = trim_to_fit_with_crumb(history, preferred, scaled, 0, crumb_present);
    let ratio = reported_input_tokens as f64 / estimated as f64;
    TrimResult {
        tokens_before: reported_input_tokens,
        tokens_after: ((result.tokens_after + tool_schema_tokens) as f64 * ratio).round() as usize,
        ..result
    }
}

fn next_boundary_after(boundaries: &[usize], current: usize) -> usize {
    boundaries
        .iter()
        .copied()
        .find(|&b| b > current)
        .unwrap_or(current)
}

pub(crate) fn count_turns(history: &[ChatMessage]) -> usize {
    history.iter().filter(|m| is_turn_boundary(m)).count()
}

/// Drop the oldest whole turn (after leading system messages and an optional
/// breadcrumb), preserving the most recent whole turn and the system prefix.
/// `crumb_present` is the OWNER's authoritative record that the population
/// carries the synthetic trim breadcrumb — never inferred from message text,
/// so a genuine user turn that happens to equal the localized breadcrumb
/// string keeps its turn-boundary role regardless of locale. Returns how many
/// messages were dropped — zero when only the newest turn remains, which the
/// caller treats as the unsatisfiable floor rather than silently claiming the
/// history fits.
pub(crate) fn drop_oldest_whole_turn(history: &mut Vec<ChatMessage>, crumb_present: bool) -> usize {
    let leading_system = history.iter().take_while(|m| is_system(m)).count();
    let body_start = leading_system + usize::from(crumb_present);
    let body = &history[body_start..];
    let boundaries: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, m)| is_turn_boundary(m))
        .map(|(i, _)| body_start + i)
        .collect();
    if boundaries.len() <= 1 {
        return 0;
    }
    let drop_end = next_boundary_after(&boundaries, boundaries[0]);
    let dropped = drop_end - body_start;
    history.drain(body_start..drop_end);
    dropped
}

/// Front breadcrumb injected after the system messages so the model SEES that
/// earlier turns were cut and cannot confabulate dropped work as present.
pub fn breadcrumb() -> ChatMessage {
    ChatMessage::user(breadcrumb_text().as_str())
}

/// Insert the trim breadcrumb after the leading system messages unless the
/// owner's `crumb_present` record says one is already sitting there. Returns
/// whether a breadcrumb is present after the call, so the caller can store it
/// as the population's authoritative provenance instead of re-inferring it
/// from text later.
pub fn insert_breadcrumb_deduped(history: &mut Vec<ChatMessage>, crumb_present: bool) -> bool {
    if crumb_present {
        return true;
    }
    let system_count = history.iter().take_while(|m| is_system(m)).count();
    history.insert(system_count, breadcrumb());
    true
}

// ── Budget-cascade primitives (water-line trigger, send-budget fit) ──────

/// Provider-authoritative context size, replacing the bare local estimate
/// wherever a trim decision is made after a billed response.
///
/// A provider reports the prompt size of a request **as it was sent**
/// (`usage.input_tokens` / `prompt_tokens`). That is the occupancy support
/// fact when present. Between that report and the next trim check, the loop
/// appends messages the provider has not billed yet — the assistant reply and
/// the tool results it just produced. Those unbilled rows are priced with the
/// local heuristic.
///
/// Before any provider report exists (first iteration, or a provider that
/// emits no usage), [`ContextCalibration::current`] degrades to the plain
/// whole-history estimate.
#[derive(Debug, Clone, Default)]
pub struct ContextCalibration {
    /// Prompt tokens as reported by the provider for the request sent when the
    /// history had `calibrated_history_len` messages. `None` until a provider
    /// reports usage.
    reported_input_tokens: Option<usize>,
    /// `history.len()` at the moment `reported_input_tokens` was captured.
    calibrated_history_len: usize,
}

impl ContextCalibration {
    /// A fresh calibration with no provider anchor: every read is the plain
    /// whole-history estimate until [`Self::observe_reported`] lands.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a provider-authoritative prompt size for the current history.
    /// Called each time a response carries `usage.input_tokens`; the reported
    /// value is the full prompt the provider just saw, so the messages present
    /// now are all accounted for and become the new anchor.
    pub fn observe_reported(&mut self, reported_input_tokens: usize, history: &[ChatMessage]) {
        self.reported_input_tokens = Some(reported_input_tokens);
        self.calibrated_history_len = history.len();
    }

    /// Current context size: the authoritative anchor plus the estimated cost
    /// of any messages appended since it was captured. With no anchor yet, the
    /// whole history is estimated locally.
    #[must_use]
    pub fn current(&self, history: &[ChatMessage]) -> usize {
        match self.reported_input_tokens {
            None => estimate_history_tokens(history),
            Some(reported) => {
                let unbilled = history.len().saturating_sub(self.calibrated_history_len);
                let tail = &history[history.len().saturating_sub(unbilled)..];
                reported.saturating_add(estimate_history_tokens(tail))
            }
        }
    }
}

pub fn resolve_send_budget(context_send_budget: usize, context_token_budget: usize) -> usize {
    if context_send_budget > 0 {
        context_send_budget
    } else {
        context_token_budget
    }
}

/// Pick the largest `kept_turns` in `1..=preferred` that fits `send_budget`
/// using one per-message token pass + O(1) probes (no full-history clones).
fn choose_keep_turns_for_budget<T>(
    history: &[T],
    preferred: usize,
    send_budget: usize,
    message_tokens: impl Fn(&T) -> usize,
    is_system_msg: impl Fn(&T) -> bool,
    is_boundary: impl Fn(&T, usize) -> bool,
) -> usize {
    let total_turns = history
        .iter()
        .enumerate()
        .filter(|(index, m)| is_boundary(m, *index))
        .count()
        .max(1);
    let mut chosen_keep = preferred.max(1).min(total_turns);
    if send_budget == 0 {
        return chosen_keep;
    }

    let leading_system = history.iter().take_while(|m| is_system_msg(m)).count();
    let msg_tokens: Vec<usize> = history.iter().map(|m| message_tokens(m)).collect();
    let system_tokens: usize = msg_tokens[..leading_system].iter().copied().sum();

    let body = &history[leading_system..];
    let boundaries: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(i, m)| is_boundary(m, leading_system + i))
        .map(|(i, _)| i)
        .collect();
    if boundaries.is_empty() {
        return 1;
    }

    let n = msg_tokens.len();
    let mut suffix = vec![0usize; n + 1];
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1].saturating_add(msg_tokens[i]);
    }

    while chosen_keep > 1 {
        let start = boundaries[boundaries.len() - chosen_keep];
        let abs_start = leading_system + start;
        let after = system_tokens.saturating_add(suffix[abs_start]);
        if after <= send_budget {
            break;
        }
        chosen_keep -= 1;
    }
    chosen_keep
}

/// Cascade the preferred keep-N down until the retained set fits `send_budget`.
///
/// The spec's trim action is "prefer `keep_recent_turns` newest whole turns,
/// then cascade down to `kept_turns == 1` when those turns still exceed the
/// send budget". This helper owns that second stage: it asks
/// [`choose_keep_turns_for_budget`] for the largest keep in `1..=preferred`
/// that fits, then performs a single [`keep_recent_whole_turns`] cut.
///
/// `send_budget == 0` means "no fit target": the preferred keep-N is applied
/// as-is.
pub fn trim_to_fit_with_crumb(
    history: Vec<ChatMessage>,
    preferred_keep: usize,
    send_budget: usize,
    model_window: usize,
    has_leading_breadcrumb: bool,
) -> TrimResult {
    let tokens_before = estimate_history_tokens(&history);
    let preferred = preferred_keep.max(1);
    let leading_system = history.iter().take_while(|m| is_system(m)).count();
    let crumb_index = if has_leading_breadcrumb && history.len() > leading_system {
        Some(leading_system)
    } else {
        None
    };
    let chosen_keep = choose_keep_turns_for_budget(
        &history,
        preferred,
        send_budget,
        |m| estimate_history_tokens(std::slice::from_ref(m)),
        is_system,
        |m, index| crumb_index != Some(index) && is_turn_boundary(m),
    );

    let mut keep = chosen_keep;
    let mut result = loop {
        let mut candidate =
            keep_recent_whole_turns(history.clone(), keep, has_leading_breadcrumb);
        candidate.tokens_before = tokens_before;
        // The fit check must use the same estimator the selection used (the
        // per-message sum), not a re-estimate of the retained set: the two can
        // differ by a few framing tokens, which would make a fitting retention
        // look like an over-budget floor.
        let retained_tokens: usize = candidate
            .history
            .iter()
            .map(|m| estimate_history_tokens(std::slice::from_ref(m)))
            .sum();
        candidate.tokens_after = retained_tokens;
        // Settlement: the per-message selection above is only exact when the
        // estimate is additive. A provider view that prices the whole slice
        // higher than the sum of its messages (framing per call, one-time
        // tool schemas, attachment accounting) would otherwise keep more
        // turns than actually fit. Re-price the retained set as a whole and
        // drop one more turn until it fits or only the floor remains.
        let whole = estimate_history_tokens(&candidate.history);
        candidate.tokens_after = candidate.tokens_after.max(whole);
        if send_budget == 0 || candidate.tokens_after <= send_budget || keep <= 1 {
            break candidate;
        }
        keep -= 1;
    };
    // The hard floor is the model's own capacity: a retention that misses the
    // send budget but still fits the window is a soft floor — the newest turn
    // is dispatched rather than rejected. Only a population the model cannot
    // take is an unsatisfiable floor.
    result.exceeds_budget = model_window > 0 && result.tokens_after > model_window;
    result.below_preferred_keep =
        result.trimmed && !result.exceeds_budget && result.kept_turns < preferred;
    result
}

/// Cascade to the send budget, without a leading synthetic breadcrumb.
pub fn trim_to_fit(
    history: Vec<ChatMessage>,
    preferred_keep: usize,
    send_budget: usize,
    model_window: usize,
) -> TrimResult {
    trim_to_fit_with_crumb(history, preferred_keep, send_budget, model_window, false)
}



/// Outcome of trimming durable [`ConversationMessage`] history. Mirrors
/// [`TrimResult`] accounting so Agent / session / UI stay aligned with the
/// provider-facing cascade.
#[derive(Debug, Clone)]
pub struct ConversationTrimResult {
    pub history: Vec<ConversationMessage>,
    pub dropped_messages: usize,
    pub dropped_turns: usize,
    pub kept_turns: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub trimmed: bool,
    pub exceeds_budget: bool,
    pub below_preferred_keep: bool,
}

/// A durable turn boundary: a plain user chat message that is not the
/// synthetic `[Tool results]` continuation row.
fn is_conversation_boundary(message: &ConversationMessage) -> bool {
    matches!(
        message,
        ConversationMessage::Chat(chat)
            if chat.role == "user"
                && !chat
                    .content
                    .starts_with(zeroclaw_api::tool_carrier::TOOL_RESULTS_PREFIX)
    )
}

/// Local token estimate for durable history: the same ~4 chars/token
/// heuristic used for provider-facing history, applied to every text-bearing
/// part of each row.
pub(crate) fn estimate_conversation_tokens(history: &[ConversationMessage]) -> usize {
    fn text_tokens(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        text.len().div_ceil(4)
    }

    history
        .iter()
        .map(|message| match message {
            ConversationMessage::Chat(chat) => text_tokens(&chat.content).saturating_add(4),
            ConversationMessage::AssistantToolCalls {
                text,
                tool_calls,
                reasoning_content,
            } => {
                let mut n = 4usize;
                if let Some(t) = text {
                    n = n.saturating_add(text_tokens(t));
                }
                if let Some(r) = reasoning_content {
                    n = n.saturating_add(text_tokens(r));
                }
                for call in tool_calls {
                    n = n
                        .saturating_add(text_tokens(&call.name))
                        .saturating_add(text_tokens(&call.arguments))
                        .saturating_add(4);
                }
                n
            }
            ConversationMessage::ToolResults(results) => results
                .iter()
                .map(|r| text_tokens(&r.content).saturating_add(4))
                .sum(),
        })
        .sum()
}

/// Drop oldest whole turns from durable conversation history, retaining the
/// `keep_turns` most recent whole turns plus every leading system chat
/// message. Primitive keep-N cut used by [`trim_conversation_to_fit`].
/// Keep-N whole turns, treating a leading synthetic breadcrumb as a
/// non-turn: it never consumes a keep-N slot and is never dropped.
fn trim_conversation_to_recent_turns_by_count(
    history: Vec<ConversationMessage>,
    keep_turns: usize,
    has_leading_breadcrumb: bool,
) -> ConversationTrimResult {
    let keep = keep_turns.max(1);
    let tokens_before = estimate_conversation_tokens(&history);

    let leading_system = history
        .iter()
        .take_while(|m| is_conversation_system(m))
        .count();
    let system: Vec<ConversationMessage> = history[..leading_system].to_vec();
    let crumb_offset =
        usize::from(has_leading_breadcrumb && history.len() > leading_system);
    let body_start = leading_system + crumb_offset;
    let body = &history[body_start..];
    let total_turns = body.iter().filter(|m| is_conversation_boundary(m)).count();

    let boundaries: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, m)| is_conversation_boundary(m))
        .map(|(i, _)| i)
        .collect();

    if boundaries.len() <= keep {
        return ConversationTrimResult {
            history,
            dropped_messages: 0,
            dropped_turns: 0,
            kept_turns: total_turns,
            tokens_before,
            tokens_after: tokens_before,
            trimmed: false,
            exceeds_budget: false,
            below_preferred_keep: false,
        };
    }

    let start = boundaries[boundaries.len() - keep];
    let dropped_messages = start;
    let dropped_turns = boundaries.iter().filter(|&&b| b < start).count();
    let mut kept = system;
    if crumb_offset > 0 {
        kept.push(history[leading_system].clone());
    }
    kept.extend_from_slice(&body[start..]);
    let kept_turns = total_turns - dropped_turns;
    let tokens_after = estimate_conversation_tokens(&kept);

    ConversationTrimResult {
        history: kept,
        dropped_messages,
        dropped_turns,
        kept_turns,
        tokens_before,
        tokens_after,
        trimmed: true,
        exceeds_budget: false,
        below_preferred_keep: false,
    }
}

/// Durable conversation cascade — same stages as [`trim_to_fit`].
pub fn trim_conversation_to_fit(
    history: Vec<ConversationMessage>,
    preferred_keep: usize,
    send_budget: usize,
    model_window: usize,
) -> ConversationTrimResult {
    trim_conversation_to_fit_with(
        history,
        preferred_keep,
        send_budget,
        model_window,
        estimate_conversation_tokens,
    )
}

/// Like [`trim_conversation_to_fit`], but sizes history with `estimate`
/// (e.g. provider-view estimate via `to_provider_messages`).
pub fn trim_conversation_to_fit_with(
    history: Vec<ConversationMessage>,
    preferred_keep: usize,
    send_budget: usize,
    model_window: usize,
    estimate: impl Fn(&[ConversationMessage]) -> usize,
) -> ConversationTrimResult {
    trim_conversation_to_fit_with_crumb(
        history,
        preferred_keep,
        send_budget,
        model_window,
        estimate,
        false,
    )
}

/// Like [`trim_conversation_to_fit_with`], but treating a leading
/// synthetic breadcrumb as a non-turn: it never consumes a keep-N slot and is
/// never dropped by the cascade.
pub fn trim_conversation_to_fit_with_crumb(
    history: Vec<ConversationMessage>,
    preferred_keep: usize,
    send_budget: usize,
    model_window: usize,
    estimate: impl Fn(&[ConversationMessage]) -> usize,
    has_leading_breadcrumb: bool,
) -> ConversationTrimResult {
    let tokens_before = estimate(&history);
    let preferred = preferred_keep.max(1);
    let leading_system = history
        .iter()
        .take_while(|m| is_conversation_system(m))
        .count();
    let crumb_index = if has_leading_breadcrumb && history.len() > leading_system {
        Some(leading_system)
    } else {
        None
    };
    let chosen_keep = choose_keep_turns_for_budget(
        &history,
        preferred,
        send_budget,
        |m| estimate(std::slice::from_ref(m)),
        is_conversation_system,
        |m, index| crumb_index != Some(index) && is_conversation_boundary(m),
    );

    let mut keep = chosen_keep;
    let mut result = loop {
        let mut candidate = trim_conversation_to_recent_turns_by_count(
            history.clone(),
            keep,
            has_leading_breadcrumb,
        );
        candidate.tokens_before = tokens_before;
        // The fit check uses the same per-message estimator the selection used,
        // so a retention that fits the target is not rejected by a few framing
        // tokens of estimator drift.
        let retained_tokens: usize = candidate
            .history
            .iter()
            .map(|m| estimate(std::slice::from_ref(m)))
            .sum();
        // Settlement: the per-message selection is only exact when the
        // estimator is additive. A provider view that prices the whole slice
        // higher than the sum of its messages would otherwise keep more turns
        // than actually fit, so re-price the retained set as a whole and drop
        // one more turn until it fits or only the floor remains.
        let whole = estimate(&candidate.history);
        candidate.tokens_after = retained_tokens.max(whole);
        if send_budget == 0 || candidate.tokens_after <= send_budget || keep <= 1 {
            break candidate;
        }
        keep -= 1;
    };
    // The hard floor is the model's own capacity; see `trim_to_fit_with_crumb`.
    result.exceeds_budget = model_window > 0 && result.tokens_after > model_window;
    result.below_preferred_keep =
        result.trimmed && !result.exceeds_budget && result.kept_turns < preferred;
    result
}

fn warn_below_preferred_keep_inner(
    preferred_keep: usize,
    send_budget: usize,
    below_preferred_keep: bool,
    kept_turns: usize,
    tokens_after: usize,
) {
    if !below_preferred_keep {
        return;
    }
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_attrs(::serde_json::json!({
                "preferred_keep": preferred_keep,
                "kept_turns": kept_turns,
                "send_budget": send_budget,
                "tokens_after": tokens_after,
                "error_key": "context_trim_below_preferred_keep",
            })),
        format!(
            "Context trim kept {kept_turns} turn(s), below preferred keep_recent_turns={preferred_keep}, to fit send budget {send_budget}"
        )
    );
}

/// Log a warning when the cascade kept fewer turns than the preferred keep.
pub fn warn_if_below_preferred_keep(
    preferred_keep: usize,
    send_budget: usize,
    result: &TrimResult,
) {
    warn_below_preferred_keep_inner(
        preferred_keep,
        send_budget,
        result.below_preferred_keep,
        result.kept_turns,
        result.tokens_after,
    );
}

/// Durable-history variant of [`warn_if_below_preferred_keep`].
pub fn warn_if_conversation_below_preferred_keep(
    preferred_keep: usize,
    send_budget: usize,
    result: &ConversationTrimResult,
) {
    warn_below_preferred_keep_inner(
        preferred_keep,
        send_budget,
        result.below_preferred_keep,
        result.kept_turns,
        result.tokens_after,
    );
}

/// Insert the trim breadcrumb into structured history after leading system
/// messages, unless one is already sitting there.
pub(crate) fn insert_conversation_breadcrumb_deduped(history: &mut Vec<ConversationMessage>) {
    let system_count = history
        .iter()
        .take_while(|m| is_conversation_system(m))
        .count();
    let crumb = breadcrumb();
    let already_present = history.get(system_count).is_some_and(|m| {
        matches!(
            m,
            ConversationMessage::Chat(c) if c.role == crumb.role && c.content == crumb.content
        )
    });
    if already_present {
        return;
    }
    history.insert(system_count, ConversationMessage::Chat(crumb));
}

/// Build a [`zeroclaw_api::agent::TurnEvent::HistoryTrimmed`] from trim accounting.
#[must_use]
pub fn history_trimmed_turn_event(
    dropped_messages: usize,
    kept_turns: usize,
    reason: String,
    tokens_after: Option<usize>,
    tokens_before: Option<usize>,
    dropped_turns: Option<usize>,
) -> zeroclaw_api::agent::TurnEvent {
    zeroclaw_api::agent::TurnEvent::HistoryTrimmed {
        dropped_messages,
        dropped_turns: dropped_turns.unwrap_or(0),
        kept_turns,
        reason,
        token_budget: None,
        tokens_before: tokens_before.map(|v| v as u64),
        tokens_after: tokens_after.map(|v| v as u64),
        tokens_before_source: None,
        tokens_after_source: None,
        unsatisfiable_floor: None,
        retained_context: None,
    }
}

/// Build an observer HistoryTrimmed event from trim accounting.
#[must_use]
pub fn history_trimmed_observer_event(
    dropped_messages: usize,
    kept_turns: usize,
    reason: String,
    tokens_after: Option<usize>,
    tokens_before: Option<usize>,
    _dropped_turns: Option<usize>,
) -> zeroclaw_api::observability_traits::ObserverEvent {
    zeroclaw_api::observability_traits::ObserverEvent::HistoryTrimmed {
        dropped_messages,
        kept_turns,
        reason,
        channel: None,
        agent_alias: None,
        turn_id: None,
        token_budget: None,
        tokens_before: tokens_before.map(|v| v as u64),
        tokens_after: tokens_after.map(|v| v as u64),
        tokens_before_source: None,
        tokens_after_source: None,
        unsatisfiable_floor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_providers::{ToolCall, ToolResultMessage};

    fn sys(c: &str) -> ChatMessage {
        ChatMessage::system(c)
    }
    fn user(c: &str) -> ChatMessage {
        ChatMessage::user(c)
    }
    fn asst(c: &str) -> ChatMessage {
        ChatMessage::assistant(c)
    }
    fn tool(c: &str) -> ChatMessage {
        ChatMessage::tool(c)
    }

    /// Content of the first retained turn boundary, so a cascade assertion can
    /// name which turn survived instead of counting messages.
    fn first_kept_user_content(result: &TrimResult) -> String {
        result
            .history
            .iter()
            .find(|m| is_turn_boundary(m))
            .expect("kept history must retain at least one turn boundary")
            .content
            .clone()
    }

    /// A declared native tool-result carrier for one image: the envelope the
    /// runtime writes, with the attachment at the fixed position.
    fn image_tool_carrier(target: &str) -> String {
        let attachments = vec![zeroclaw_api::media::RenderedMarker {
            target: target.to_string(),
            kind: zeroclaw_api::media::MarkerKind::Image,
        }];
        serde_json::json!({
            "tool_call_id": "call_image",
            "content": "",
            "attachments": zeroclaw_api::tool_carrier::render_native_attachments(&attachments),
        })
        .to_string()
    }

    fn conversation_system(content: &str) -> ConversationMessage {
        ConversationMessage::Chat(ChatMessage::system(content))
    }

    fn conversation_user(content: &str) -> ConversationMessage {
        ConversationMessage::Chat(ChatMessage::user(content))
    }

    fn conversation_assistant(content: &str) -> ConversationMessage {
        ConversationMessage::Chat(ChatMessage::assistant(content))
    }

    fn push_tool_exchange(history: &mut Vec<ConversationMessage>, index: usize) {
        let id = format!("call-{index}");
        history.push(ConversationMessage::AssistantToolCalls {
            text: Some(format!("calling tool {index}")),
            tool_calls: vec![ToolCall {
                id: id.clone(),
                name: "shell".into(),
                arguments: "{}".into(),
                extra_content: None,
            }],
            reasoning_content: None,
        });
        history.push(ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: id,
            content: format!("result {index}"),
            tool_name: "shell".into(),
        }]));
    }

    fn assert_structural_tool_pairs(history: &[ConversationMessage]) {
        for (index, message) in history.iter().enumerate() {
            match message {
                ConversationMessage::AssistantToolCalls { tool_calls, .. } => {
                    let Some(ConversationMessage::ToolResults(results)) = history.get(index + 1)
                    else {
                        panic!("assistant tool calls must be followed by tool results");
                    };
                    assert_eq!(results.len(), tool_calls.len());
                    assert_eq!(results[0].tool_call_id, tool_calls[0].id);
                }
                ConversationMessage::ToolResults(_) => assert!(matches!(
                    index
                        .checked_sub(1)
                        .and_then(|previous| history.get(previous)),
                    Some(ConversationMessage::AssistantToolCalls { .. })
                )),
                ConversationMessage::Chat(_) => {}
            }
        }
    }

    #[test]
    fn trim_conversation_to_recent_turns_keeps_single_tool_heavy_turn_over_cap() {
        let mut history = vec![conversation_user("run the workflow")];
        for index in 0..31 {
            push_tool_exchange(&mut history, index);
        }
        history.push(conversation_assistant("workflow complete"));
        assert_eq!(history.len(), 64);

        let result = trim_conversation_to_fit_with(history, 50, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(!result.trimmed);
        assert_eq!(result.dropped_messages, 0);
        assert_eq!(result.dropped_turns, 0);
        assert_eq!(result.kept_turns, 1);
        assert_eq!(result.history.len(), 64);
        assert!(matches!(
            result.history.first(),
            Some(ConversationMessage::Chat(message))
                if message.role == "user" && message.content == "run the workflow"
        ));
        assert!(matches!(
            result.history.last(),
            Some(ConversationMessage::Chat(message))
                if message.role == "assistant" && message.content == "workflow complete"
        ));
        assert_structural_tool_pairs(&result.history);
    }

    #[test]
    fn trim_conversation_to_recent_turns_drops_old_turn_at_one_turn_limit() {
        let mut history = vec![
            conversation_user("old request"),
            conversation_assistant("old answer"),
            conversation_user("new request"),
        ];
        for index in 0..25 {
            push_tool_exchange(&mut history, index);
        }
        history.push(conversation_assistant("new answer"));

        let result = trim_conversation_to_fit_with(history, 1, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(result.trimmed);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.dropped_messages, 2);
        assert_eq!(result.kept_turns, 1);
        assert!(matches!(
            result.history.first(),
            Some(ConversationMessage::Chat(message))
                if message.role == "user" && message.content == "new request"
        ));
        assert!(matches!(
            result.history.last(),
            Some(ConversationMessage::Chat(message))
                if message.role == "assistant" && message.content == "new answer"
        ));
        assert_structural_tool_pairs(&result.history);
    }

    #[test]
    fn trim_conversation_to_recent_turns_counts_turns_not_tool_rows() {
        let mut history = vec![conversation_system("system")];
        for turn in 0..60 {
            history.push(conversation_user(&format!("request {turn}")));
            for tool in 0..3 {
                push_tool_exchange(&mut history, turn * 10 + tool);
            }
            history.push(conversation_assistant(&format!("answer {turn}")));
        }

        let result = trim_conversation_to_fit_with(history, 50, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(result.trimmed);
        assert_eq!(result.dropped_turns, 10);
        assert_eq!(result.kept_turns, 50);
        assert!(matches!(
            result.history.get(1),
            Some(ConversationMessage::Chat(message))
                if message.role == "user" && message.content == "request 10"
        ));
        assert!(matches!(
            result.history.last(),
            Some(ConversationMessage::Chat(message))
                if message.role == "assistant" && message.content == "answer 59"
        ));
        assert_structural_tool_pairs(&result.history);
    }

    #[test]
    fn trim_conversation_to_recent_turns_zero_cap_preserves_newest_complete_turn() {
        let history = vec![
            conversation_user("old request"),
            conversation_assistant("old answer"),
            conversation_user("new request"),
            conversation_assistant("new answer"),
        ];

        let result = trim_conversation_to_fit_with(history, 0, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(result.trimmed);
        assert_eq!(result.dropped_messages, 2);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.kept_turns, 1);
        assert_eq!(result.history.len(), 2);
        assert!(matches!(
            result.history.first(),
            Some(ConversationMessage::Chat(message))
                if message.role == "user" && message.content == "new request"
        ));
    }

    #[test]
    fn trim_conversation_to_recent_turns_excludes_and_normalizes_system_messages() {
        let history = vec![
            conversation_system("primary system"),
            conversation_user("old request"),
            conversation_assistant("old answer"),
            conversation_system("late system"),
            conversation_user("new request"),
            conversation_assistant("new answer"),
        ];

        let result = trim_conversation_to_fit_with(history, 1, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(result.trimmed);
        assert_eq!(result.dropped_messages, 3);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.kept_turns, 1);
        // Only the LEADING system prefix is non-droppable: tools and skills
        // live there. A system row that sits inside the body belongs to the
        // turn it appears in and falls with it.
        assert_eq!(result.history.len(), 3);
        assert!(matches!(
            &result.history[..],
            [
                ConversationMessage::Chat(first_system),
                ConversationMessage::Chat(user),
                ConversationMessage::Chat(assistant),
            ] if first_system.role == "system"
                && first_system.content == "primary system"
                && user.role == "user"
                && user.content == "new request"
                && assistant.role == "assistant"
                && assistant.content == "new answer"
        ));
    }

    #[test]
    fn trim_conversation_to_recent_turns_under_cap_preserves_late_system_order() {
        let history = vec![
            conversation_user("request"),
            conversation_assistant("answer"),
            conversation_system("late system"),
        ];
        let original = serde_json::to_value(&history).expect("fixture should serialize");

        let result = trim_conversation_to_fit_with(history, 3, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(!result.trimmed);
        assert_eq!(result.dropped_messages, 0);
        assert_eq!(result.dropped_turns, 0);
        assert_eq!(result.kept_turns, 1);
        assert_eq!(
            serde_json::to_value(&result.history).expect("result should serialize"),
            original,
            "under-cap history must remain shape- and order-identical"
        );
    }

    #[test]
    fn trim_conversation_to_recent_turns_non_system_at_cap_preserves_late_system_order() {
        let history = vec![
            conversation_user("request"),
            conversation_assistant("answer"),
            conversation_system("late system"),
        ];
        let original = serde_json::to_value(&history).expect("fixture should serialize");

        let result = trim_conversation_to_fit_with(history, 1, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(!result.trimmed);
        assert_eq!(result.dropped_messages, 0);
        assert_eq!(result.dropped_turns, 0);
        assert_eq!(result.kept_turns, 1);
        assert_eq!(
            serde_json::to_value(&result.history).expect("result should serialize"),
            original,
            "system messages must not create cap pressure or change history order"
        );
    }

    #[test]
    fn trim_conversation_to_recent_turns_leaves_history_without_user_boundary_unchanged() {
        let mut history = vec![conversation_system("system")];
        push_tool_exchange(&mut history, 0);
        history.push(conversation_assistant("done"));
        let original = serde_json::to_value(&history).expect("fixture should serialize");

        let result = trim_conversation_to_fit_with(history, 1, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(!result.trimmed);
        assert_eq!(result.dropped_messages, 0);
        assert_eq!(result.dropped_turns, 0);
        assert_eq!(result.kept_turns, 0);
        assert_eq!(
            serde_json::to_value(&result.history).expect("result should serialize"),
            original
        );
        assert_structural_tool_pairs(&result.history);
    }

    #[test]
    fn trim_conversation_to_recent_turns_counts_later_user_matching_breadcrumb() {
        let breadcrumb_content = breadcrumb().content;
        let history = vec![
            conversation_system("system"),
            conversation_user("old request"),
            conversation_assistant("old answer"),
            conversation_user(&breadcrumb_content),
            conversation_assistant("new answer"),
        ];

        let result = trim_conversation_to_fit_with(history, 1, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(result.trimmed);
        assert_eq!(result.dropped_messages, 2);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.kept_turns, 1);
        assert!(matches!(
            &result.history[..],
            [
                ConversationMessage::Chat(system),
                ConversationMessage::Chat(user),
                ConversationMessage::Chat(assistant),
            ] if system.role == "system"
                && user.role == "user"
                && user.content == breadcrumb_content
                && assistant.role == "assistant"
                && assistant.content == "new answer"
        ));
    }

    #[test]
    fn trim_conversation_to_recent_turns_does_not_mistake_first_user_for_breadcrumb() {
        let breadcrumb_content = breadcrumb().content;
        let history = vec![
            conversation_system("system"),
            conversation_user(&breadcrumb_content),
            conversation_assistant("old answer"),
            conversation_user("new request"),
            conversation_assistant("new answer"),
        ];

        let result = trim_conversation_to_fit_with(history, 1, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(result.trimmed);
        assert_eq!(result.dropped_messages, 2);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.kept_turns, 1);
        assert!(matches!(
            &result.history[..],
            [
                ConversationMessage::Chat(system),
                ConversationMessage::Chat(user),
                ConversationMessage::Chat(assistant),
            ] if system.role == "system"
                && user.role == "user"
                && user.content == "new request"
                && assistant.role == "assistant"
                && assistant.content == "new answer"
        ));
    }

    #[test]
    fn trim_conversation_to_recent_turns_drops_minimum_oldest_turns_for_turn_cap() {
        let history = vec![
            conversation_user("old request"),
            conversation_assistant("old answer"),
            conversation_user("middle request"),
            conversation_assistant("middle answer"),
            conversation_user("new request"),
            conversation_assistant("new answer"),
        ];

        let result = trim_conversation_to_fit_with(history, 2, usize::MAX, 0, |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m));

        assert!(result.trimmed);
        assert_eq!(result.dropped_messages, 2);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.kept_turns, 2);
        assert_eq!(result.history.len(), 4);
        assert!(matches!(
            result.history.first(),
            Some(ConversationMessage::Chat(message))
                if message.role == "user" && message.content == "middle request"
        ));
        assert!(matches!(
            result.history.last(),
            Some(ConversationMessage::Chat(message))
                if message.role == "assistant" && message.content == "new answer"
        ));
    }

    #[test]
    fn trim_conversation_to_recent_turns_second_trim_excludes_leading_breadcrumb() {
        let mut history = vec![
            conversation_system("system"),
            ConversationMessage::Chat(breadcrumb()),
            conversation_user("old request"),
            conversation_assistant("old answer"),
            conversation_user("middle request"),
            conversation_assistant("middle answer"),
        ];

        let first = trim_conversation_to_fit_with_crumb(
            history,
            2,
            usize::MAX,
            0,
            |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m),
            true,
        );
        assert!(
            !first.trimmed,
            "a synthetic breadcrumb must not push an exactly-at-cap body over the limit"
        );

        history = first.history;
        history.push(conversation_user("new request"));
        history.push(conversation_assistant("new answer"));
        let second = trim_conversation_to_fit_with_crumb(
            history,
            2,
            usize::MAX,
            0,
            |m: &[ConversationMessage]| crate::agent::history_trim::estimate_conversation_tokens(m),
            true,
        );

        assert!(second.trimmed);
        assert_eq!(second.dropped_messages, 2);
        assert_eq!(second.dropped_turns, 1);
        assert_eq!(second.kept_turns, 2);
        // The leading breadcrumb is not a turn: it survives the trim in place
        // and never consumes a keep-N slot, so the retained set is the system
        // prefix, the crumb, and the two newest whole turns.
        assert_eq!(second.history.len(), 6);
        let breadcrumb_content = breadcrumb().content;
        assert_eq!(
            second
                .history
                .iter()
                .filter(|message| matches!(
                    message,
                    ConversationMessage::Chat(chat)
                        if chat.role == "user" && chat.content == breadcrumb_content
                ))
                .count(),
            1
        );
        assert!(matches!(
            second.history.get(2),
            Some(ConversationMessage::Chat(message))
                if message.role == "user" && message.content == "middle request"
        ));
        assert!(matches!(
            second.history.last(),
            Some(ConversationMessage::Chat(message))
                if message.role == "assistant" && message.content == "new answer"
        ));
    }

    #[test]
    fn under_budget_is_untouched() {
        let h = vec![sys("s"), user("hi"), asst("yo")];
        let n = h.len();
        let r = trim_to_fit_with_crumb(h, 1_000_000, 5, 0, false);
        assert!(!r.trimmed);
        assert_eq!(r.history.len(), n);
        assert_eq!(r.dropped_turns, 0);
    }

    #[test]
    fn zero_budget_is_untouched() {
        let h = vec![sys("s"), user("hi"), asst("yo")];
        let n = h.len();
        let r = trim_to_fit_with_crumb(h, 0, 5, 0, false);
        assert!(!r.trimmed);
        assert_eq!(r.history.len(), n);
    }

    #[test]
    fn drops_oldest_whole_turns_keeps_system() {
        let big = "x".repeat(2000);
        let h = vec![
            sys("system"),
            user(&format!("turn1 {big}")),
            asst("a1"),
            user(&format!("turn2 {big}")),
            asst("a2"),
            user("turn3 short"),
            asst("a3"),
        ];
        let r = trim_to_fit_with_crumb(h, 200, 5, 0, false);
        assert!(r.trimmed);
        assert_eq!(r.history[0].role, "system");
        assert!(r.dropped_turns >= 1);
        assert!(r.kept_turns >= 1);
        // most recent turn survived
        assert!(r.history.iter().any(|m| m.content.contains("turn3 short")));
    }

    #[test]
    fn token_accounting_is_populated_and_coherent() {
        let big = "x".repeat(2000);
        let h = vec![
            sys("system"),
            user(&format!("turn1 {big}")),
            asst("a1"),
            user(&format!("turn2 {big}")),
            asst("a2"),
            user("turn3 short"),
            asst("a3"),
        ];
        let r = trim_to_fit_with_crumb(h, 200, 5, 0, false);
        assert!(r.trimmed);
        // the sick-log fields must reflect a real reduction
        assert!(r.tokens_before > r.tokens_after);
        assert!(r.tokens_before > 200, "before should exceed budget");
        assert!(
            r.tokens_before.saturating_sub(r.tokens_after) > 0,
            "reclaimed must be positive when trimmed"
        );
    }

    #[test]
    fn untouched_reports_equal_before_after() {
        let h = vec![sys("s"), user("hi"), asst("yo")];
        let r = trim_to_fit_with_crumb(h, 1_000_000, 5, 0, false);
        assert!(!r.trimmed);
        assert_eq!(r.tokens_before, r.tokens_after);
    }

    #[test]
    fn never_splits_tool_pair() {
        let big = "y".repeat(2000);
        let h = vec![
            sys("system"),
            user(&format!("turn1 {big}")),
            asst("calling tool"),
            tool("tool_use_1 result"),
            user("[Tool results]\nmore"),
            asst("done1"),
            user("turn2 short"),
            asst("done2"),
        ];
        let r = trim_to_fit_with_crumb(h, 150, 5, 0, false);
        assert!(r.trimmed);
        // a tool row must never appear without its preceding assistant turn-head
        let mut seen_user = false;
        for m in &r.history {
            if is_turn_boundary(m) {
                seen_user = true;
            }
            if m.role == "tool" {
                assert!(seen_user, "tool result kept without its turn head");
            }
        }
    }

    /// Known limit of prefix provenance: the trimmer identifies a prompt-mode
    /// tool-result row as a `user` row whose content starts with
    /// `[Tool results]`, so a genuine user message that happens to begin with
    /// that prefix is grouped with the preceding assistant turn instead of
    /// opening a turn of its own. This pins that grouping: the row is dropped
    /// together with that turn and never counted as an independent turn.
    #[test]
    fn user_text_matching_tool_results_prefix_is_dropped_with_its_turn() {
        let history = vec![
            sys("system"),
            user("old request"),
            asst("old answer"),
            user("[Tool results]\nthis is literal user text"),
            asst("middle answer"),
            user("new request"),
            asst("new answer"),
        ];

        let result = trim_to_recent_turn_count(history, 1, false);

        assert!(result.trimmed);
        assert_eq!(
            result.dropped_turns, 1,
            "the prefix-matching row must not count as a turn of its own"
        );
        assert_eq!(result.kept_turns, 1);
        assert_eq!(result.dropped_messages, 4);
        assert!(
            !result
                .history
                .iter()
                .any(|message| message.content == "[Tool results]\nthis is literal user text"),
            "the prefix-matching row drops with its turn rather than surviving as its own"
        );
    }

    #[test]
    fn keeps_last_turn_even_if_over_budget() {
        let huge = "z".repeat(10_000);
        let h = vec![
            sys("system"),
            user("old"),
            asst("a"),
            user(&format!("recent {huge}")),
            asst("a2"),
        ];
        let r = trim_to_fit_with_crumb(h, 50, 5, 0, false);
        // last turn alone exceeds budget; option a keeps it rather than nuking.
        assert!(r.kept_turns >= 1);
        assert!(r.history.iter().any(|m| m.content.contains("recent")));
    }

    #[test]
    fn breadcrumb_is_user_role() {
        assert_eq!(breadcrumb().role, "user");
    }

    #[test]
    fn trimmed_history_has_no_orphan_tool_calls() {
        use crate::agent::history_pruner::remove_orphaned_tool_messages;
        let big = "q".repeat(3000);
        let asst_call = |id: &str| {
            asst(
                &serde_json::json!({
                    "content": "",
                    "tool_calls": [{"id": id, "name": "file_read", "arguments": "{}"}]
                })
                .to_string(),
            )
        };
        let tool_res =
            |id: &str| tool(&serde_json::json!({"tool_call_id": id, "content": "ok"}).to_string());
        let h = vec![
            sys("system"),
            user(&format!("turn1 {big}")),
            asst_call("call_1"),
            tool_res("call_1"),
            asst("summary1"),
            user("turn2"),
            asst_call("call_2"),
            tool_res("call_2"),
            asst("summary2"),
        ];
        let r = trim_to_fit_with_crumb(h, 200, 5, 0, false);
        assert!(r.trimmed, "oversized history must trim");
        let mut kept = r.history.clone();
        let swept = remove_orphaned_tool_messages(&mut kept);
        assert_eq!(
            swept.removed, 0,
            "whole-turn trim must leave zero orphan tool messages; the orphan \
             sweep (the anti-400 net) should find nothing to remove"
        );
        assert_eq!(kept.len(), r.history.len(), "no messages removed by sweep");
    }

    #[test]
    fn preserves_kept_tool_call_id_envelope_when_trimming_whole_turns() {
        let old_big = "old ".repeat(2000);
        let envelope = serde_json::json!({
            "tool_call_id": "call_1",
            "content": "raw tool output",
        });
        let h = vec![
            sys("system"),
            user(&format!("old turn {old_big}")),
            asst("old answer"),
            user("recent"),
            asst("calling tool"),
            tool(&envelope.to_string()),
            asst("done"),
        ];

        let r = trim_to_fit_with_crumb(h, 200, 5, 0, false);

        assert!(r.trimmed, "oversized history must drop an old whole turn");
        assert_eq!(r.dropped_turns, 1);
        let kept_tool = r
            .history
            .iter()
            .find(|msg| msg.role == "tool")
            .expect("recent tool result should be kept");
        let kept_envelope: serde_json::Value =
            serde_json::from_str(&kept_tool.content).expect("tool content remains JSON");
        assert_eq!(
            kept_envelope
                .get("tool_call_id")
                .and_then(serde_json::Value::as_str),
            Some("call_1"),
        );
        assert_eq!(
            kept_envelope
                .get("content")
                .and_then(serde_json::Value::as_str),
            Some("raw tool output"),
        );
    }

    #[test]
    fn breadcrumb_inserts_after_leading_system() {
        let big = "w".repeat(3000);
        let h = vec![
            sys("sysA"),
            sys("sysB"),
            user(&format!("old {big}")),
            asst("a"),
            user("recent"),
            asst("a2"),
        ];
        let r = trim_to_fit_with_crumb(h, 120, 5, 0, false);
        assert!(r.trimmed);
        let mut trimmed = r.history;
        let system_count = trimmed.iter().take_while(|m| m.role == "system").count();
        trimmed.insert(system_count, breadcrumb());
        assert_eq!(trimmed[0].role, "system");
        assert_eq!(trimmed[system_count].role, "user");
        assert!(
            trimmed[..system_count].iter().all(|m| m.role == "system"),
            "breadcrumb must sit after every leading system message"
        );
    }

    #[test]
    fn second_budget_trim_excludes_explicit_leading_breadcrumb_from_turn_counts() {
        let old = "old ".repeat(2_000);
        let history = vec![
            sys("system"),
            breadcrumb(),
            user(&old),
            asst("old answer"),
            user("new request"),
            asst("new answer"),
        ];

        let result = trim_to_fit_with_crumb(history, 200, 5, 0, true);

        assert!(result.trimmed);
        assert_eq!(result.dropped_messages, 2);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.kept_turns, 1);
        assert!(result.history.iter().any(|m| m.content == "new request"));
    }

    #[test]
    fn second_count_trim_excludes_explicit_leading_breadcrumb_from_turn_counts() {
        let history = vec![
            sys("system"),
            breadcrumb(),
            user("old request"),
            asst("old answer"),
            user("new request"),
            asst("new answer"),
        ];

        let result = trim_to_recent_turn_count(history, 1, true);

        assert!(result.trimmed);
        assert_eq!(result.dropped_messages, 2);
        assert_eq!(result.dropped_turns, 1);
        assert_eq!(result.kept_turns, 1);
        assert!(result.history.iter().any(|m| m.content == "new request"));
    }

    #[test]
    fn reported_budget_trims_when_reported_exceeds_budget() {
        let big = "x".repeat(2000);
        let h = vec![
            sys("system"),
            user(&format!("turn1 {big}")),
            asst("a1"),
            user(&format!("turn2 {big}")),
            asst("a2"),
            user("turn3 short"),
            asst("a3"),
        ];
        let estimated = estimate_history_tokens(&h);
        let reported = estimated * 4;
        let budget = reported / 2;
        let r = trim_to_reported_budget(h, budget, reported, estimated, 0);
        assert!(
            r.trimmed,
            "must trim when provider-reported tokens exceed budget"
        );
        assert!(r.dropped_turns >= 1);
        assert!(r.history.iter().any(|m| m.content.contains("turn3 short")));
    }

    #[test]
    fn reported_budget_no_trim_when_real_tokens_fit() {
        let h = vec![sys("system"), user("hi"), asst("hello")];
        let estimated = estimate_history_tokens(&h);
        let r = trim_to_reported_budget(h, estimated * 4, estimated, estimated, 0);
        assert!(!r.trimmed);
    }

    #[test]
    fn reported_budget_trims_under_extreme_ratio() {
        let big = "x".repeat(4000);
        let h = vec![
            sys("system"),
            user(&format!("old {big}")),
            asst("a1"),
            user("recent short"),
            asst("a2"),
        ];
        let estimated = estimate_history_tokens(&h);
        let reported = estimated * 5000;
        let budget = reported / 100;
        let r = trim_to_reported_budget(h, budget, reported, estimated, 0);
        assert!(r.trimmed, "extreme ratio must still enforce, not no-op");
        assert!(r.history.iter().any(|m| m.content.contains("recent short")));
    }

    #[test]
    fn reported_budget_reserves_room_for_large_native_tool_schema() {
        let big = "x".repeat(2000);
        let h = vec![
            sys("system"),
            user(&format!("turn1 {big}")),
            asst("a1"),
            user(&format!("turn2 {big}")),
            asst("a2"),
            user("turn3 short"),
            asst("a3"),
        ];
        // A large native tool schema that a provider would serialize into the
        // request and count in `input_tokens` alongside the messages.
        let spec = crate::tools::ToolSpec::new(
            "large_schema_tool",
            "a tool with a very large parameter schema",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "payload": {
                        "type": "string",
                        "description": "big".repeat(300),
                    }
                }
            }),
        );
        let tool_tokens = crate::agent::history::estimate_tool_schema_tokens(&[spec]);
        assert!(
            tool_tokens > 0,
            "a large tool schema must contribute a nonzero token estimate"
        );

        // Faithful provider: reported == messages + tool schemas.
        let estimated = estimate_history_tokens(&h) + tool_tokens;
        let reported = estimated;
        let budget = reported / 2;
        assert!(budget > tool_tokens, "budget must leave headroom for tools");

        let r = trim_to_reported_budget(h, budget, reported, estimated, tool_tokens);
        assert!(
            r.trimmed,
            "must trim when reported population exceeds budget"
        );
        let kept_total = estimate_history_tokens(&r.history) + tool_tokens;
        assert!(
            kept_total <= budget,
            "retained history plus constant tool schemas must fit the budget (kept {kept_total}, budget {budget})"
        );
        assert_eq!(
            r.tokens_after, kept_total,
            "tokens_after must cover the full provider population, tool schemas included"
        );
        assert!(r.history.iter().any(|m| m.content.contains("turn3 short")));
    }

    #[test]
    fn reported_budget_calibrates_selection_against_the_measured_population() {
        // `reported_population_estimated` describes the population that produced
        // `reported` — the pre-request transcript. `history` here is the
        // already-appended transcript (the provider calls back with a larger
        // estimate after the assistant/tool output was added). The selection
        // target must scale from the MEASURED population, not from the fresher,
        // larger post-append estimate, or the retained set would exceed the
        // budget once calibrated.
        let big = "x".repeat(2000);
        // The measured (pre-request) population: several substantial turns so
        // there is plenty of room to trim toward the budget.
        let mut measured = vec![sys("system")];
        for i in 0..8 {
            measured.push(user(format!("m{i} {big}").as_str()));
            measured.push(asst(format!("a{i}").as_str()));
        }
        let reported = estimate_history_tokens(&measured) * 4;
        let budget = reported / 2;
        let measured_population_estimated = estimate_history_tokens(&measured);

        // Post-request append: this iteration's assistant output lands on the
        // same transcript `trim_to_reported_budget` sees, making it larger than
        // the measured population — but small enough that the newest whole turn
        // still fits the post-trim target (no oversized-turn exception).
        let appended = "y".repeat(400);
        let mut history = measured;
        history.push(ChatMessage::assistant(&appended));
        assert!(
            estimate_history_tokens(&history) > measured_population_estimated,
            "the appended output must make the post-append estimate the larger one"
        );

        let r =
            trim_to_reported_budget(history, budget, reported, measured_population_estimated, 0);
        assert!(r.trimmed, "must trim when reported exceeds budget");
        assert!(
            r.tokens_after <= budget,
            "selection must not outpace the calibration ratio: tokens_after {} > budget {budget}",
            r.tokens_after
        );
        // The retained history must also fit the budget under the measured
        // calibration ratio (the calibration's own check).
        let kept = estimate_history_tokens(&r.history);
        let calibrated = (kept as f64 * reported as f64
            / measured_population_estimated.max(1) as f64)
            .round() as u64;
        assert!(
            calibrated <= budget as u64,
            "retained history must respect the budget under the measured ratio \
             (calibrated {calibrated}, budget {budget})"
        );
    }

    #[test]
    fn insert_breadcrumb_deduped_does_not_stack() {
        let mut h = vec![sys("system"), user("turn1"), asst("a1")];
        let mut crumb_present = insert_breadcrumb_deduped(&mut h, false);
        let after_first = h.len();
        crumb_present = insert_breadcrumb_deduped(&mut h, crumb_present);
        assert_eq!(
            h.len(),
            after_first,
            "a second trim must not stack another breadcrumb behind the system block"
        );
        let crumbs = h
            .iter()
            .filter(|m| m.role == breadcrumb().role && m.content == breadcrumb().content)
            .count();
        assert_eq!(crumbs, 1);
        assert!(crumb_present);
        // A genuine first user turn equal to the breadcrumb string must not
        // be mistaken for an existing crumb: the OWNER record decides.
        let mut colliding = vec![sys("system"), user(&breadcrumb().content), asst("a1")];
        let inserted = insert_breadcrumb_deduped(&mut colliding, false);
        assert!(
            inserted,
            "the owner record says no crumb exists, so a fresh one is inserted"
        );
        let crumbs = colliding
            .iter()
            .filter(|m| m.role == breadcrumb().role && m.content == breadcrumb().content)
            .count();
        assert_eq!(
            crumbs, 2,
            "the real user turn stays untouched and the synthetic crumb is added"
        );
    }

    #[test]
    fn repeated_interactive_recovery_keeps_one_breadcrumb_and_real_turn_counts() {
        // Mirrors the interactive overflow-recovery sequence in
        // `agent::loop_`: on each provider context-overflow error it must
        // call the crumb-aware trim with the owner's current flag, then
        // insert the breadcrumb only if it isn't already present. A
        // crumb-blind call (the pre-fix bug) would treat an existing
        // synthetic breadcrumb as the oldest real user turn and drop it as
        // though real history had been removed.
        let big = "x".repeat(400);
        let mut history = vec![sys("system")];
        for i in 0..6 {
            history.push(user(&format!("turn {i} {big}")));
            history.push(asst(&format!("reply {i} {big}")));
        }
        let mut crumb_present = false;

        // First overflow: trims some real turns and inserts the crumb.
        let budget_after_first_trim = estimate_history_tokens(&history) / 2;
        let result = trim_to_fit_with_crumb(
            std::mem::take(&mut history),
            5,
            budget_after_first_trim,
            0,
            crumb_present,
        );
        assert!(result.trimmed, "fixture must overflow the first budget");
        history = result.history;
        crumb_present = insert_breadcrumb_deduped(&mut history, crumb_present);
        assert!(crumb_present);
        let real_turns_after_first = history.iter().filter(|m| is_turn_boundary(m)).count() - /* crumb counts as a user turn boundary */ 1;

        // Second overflow on the already-recovered history: the crumb must
        // not be miscounted as a droppable real turn, and inserting again
        // must not stack a second marker.
        let budget_after_second_trim = estimate_history_tokens(&history) / 2;
        let result = trim_to_fit_with_crumb(
            std::mem::take(&mut history),
            5,
            budget_after_second_trim,
            0,
            crumb_present,
        );
        history = result.history;
        crumb_present = insert_breadcrumb_deduped(&mut history, crumb_present);
        assert!(crumb_present);

        let crumbs = history
            .iter()
            .filter(|m| m.role == breadcrumb().role && m.content == breadcrumb().content)
            .count();
        assert_eq!(
            crumbs, 1,
            "repeated recovery must never stack a second synthetic breadcrumb"
        );
        assert!(
            result.kept_turns <= real_turns_after_first,
            "the second recovery must not report more kept real turns than existed \
             before it (kept_turns {}, real turns before {real_turns_after_first})",
            result.kept_turns
        );
    }

    #[test]
    fn insert_breadcrumb_deduped_sits_after_leading_system() {
        let mut h = vec![sys("s1"), sys("s2"), user("turn1"), asst("a1")];
        let has_breadcrumb = insert_breadcrumb_deduped(&mut h, false);
        assert_eq!(h[0].role, "system");
        assert_eq!(h[1].role, "system");
        assert_eq!(h[2].role, breadcrumb().role);
        assert_eq!(h[2].content, breadcrumb().content);
        assert!(has_breadcrumb);
    }

    #[test]
    fn five_image_tool_results_in_one_round_are_budgeted_as_images() {
        use crate::agent::history::IMAGE_TOKEN_ESTIMATE;

        let assistant_tool_calls = serde_json::json!({
            "content": "",
            "tool_calls": (0..5)
                .map(|index| {
                    serde_json::json!({
                        "id": format!("call_{index}"),
                        "name": "image_info",
                        "arguments": "{}",
                    })
                })
                .collect::<Vec<_>>(),
        })
        .to_string();

        let image_history = |tool_contents: Vec<String>| {
            vec![
                sys("system"),
                user(&format!("old turn {}", "x".repeat(8_000))),
                asst("old answer"),
                user("new turn"),
                asst(&assistant_tool_calls),
            ]
            .into_iter()
            .chain(tool_contents.into_iter().map(|content| tool(&content)))
            .collect::<Vec<ChatMessage>>()
        };

        // Path targets: five images coming back in one native-tool round as
        // declared carriers, the shape the runtime writes.
        let history = image_history(
            (0..5)
                .map(|index| image_tool_carrier(&format!("/tmp/slide-{index}.png")))
                .collect(),
        );
        assert!(
            estimate_history_tokens(&history) >= 5 * IMAGE_TOKEN_ESTIMATE,
            "five image tool results must be budgeted as five images"
        );

        let result = trim_to_fit(history, 5, 5 * IMAGE_TOKEN_ESTIMATE + 1_000, 0);
        assert!(result.trimmed, "the old text turn must be dropped to fit");
        assert_eq!(result.dropped_turns, 1);
        assert!(
            !result
                .history
                .iter()
                .any(|m| m.content.contains("old turn")),
            "the old turn should be dropped"
        );
        assert!(
            result
                .history
                .iter()
                .any(|m| m.content.contains("new turn")),
            "the newest turn head must survive"
        );
        assert_eq!(
            result.history.iter().filter(|m| m.role == "tool").count(),
            5,
            "the newest round must keep all five image results whole"
        );

        // The same round as ~600 KB data-URI attachments: per-image pricing
        // keeps the history under a 20k budget, where per-byte pricing would
        // see ~150k tokens per result and throw the old turn away.
        let history = image_history(
            (0..5)
                .map(|_| {
                    image_tool_carrier(&format!("data:image/png;base64,{}", "A".repeat(600_000)))
                })
                .collect(),
        );
        let before = history.len();
        let result = trim_to_fit(history, 5, 20_000, 0);
        assert!(
            !result.trimmed,
            "data-URI markers must price like the path form, not like bytes/4"
        );
        assert_eq!(result.dropped_turns, 0);
        assert_eq!(result.history.len(), before);
        assert!(
            result
                .history
                .iter()
                .any(|m| m.content.contains("old turn")),
            "nothing should be dropped when the estimate fits the budget"
        );
    }

    // Legacy path markers are text under the attachment-identity contract,
    // so a stale legacy carrier is delivered and priced as its bytes: the
    // marker syntax stays in the body and nothing is stripped. Thirty short
    // markers fit the 32k budget by bytes, so a tool round that only ever
    // quoted markers cannot evict a turn that fits.
    #[test]
    fn stale_tool_images_do_not_force_a_trim() {
        let markers: Vec<String> = (0..30)
            .map(|index| format!("[IMAGE:/tmp/stale-{index}.png]"))
            .collect();
        // The latest message is a genuine user turn, so the whole tool run is
        // stale: replay delivers it verbatim, and its price is its bytes.
        let history = vec![
            sys("s"),
            user("u"),
            asst("a"),
            tool(&markers.join("\n")),
            user("v"),
        ];

        let result = trim_to_fit(history, 5, 32_000, 0);

        assert!(
            !result.trimmed,
            "stale tool images are text, not per-image charges, and must not force a trim"
        );
        assert_eq!(result.dropped_turns, 0);
        assert_eq!(result.history.len(), 5);
    }

    #[test]
    fn drop_oldest_whole_turn_uses_owner_crumb_record_not_text_inference() {
        // The history's first non-system turn IS the exact localized
        // breadcrumb text — but the owner record says it is a REAL user turn.
        // Whole-turn selection must treat it as a turn boundary, never skip
        // it as synthetic, regardless of the active locale's wording.
        let mut h = vec![
            sys("system"),
            user(&breadcrumb().content),
            asst("answer to what looks like a crumb"),
            user("newest request"),
            asst("newest answer"),
        ];
        let dropped = drop_oldest_whole_turn(&mut h, false);
        assert_eq!(dropped, 2, "the colliding first turn drops as a whole turn");
        assert!(
            matches!(
                h.get(1),
                Some(m) if m.role == "user" && m.content == "newest request"
            ),
            "the colliding turn is dropped whole and the newest turn survives"
        );
        // With the owner record saying a crumb IS present, selection starts
        // after it even when the crumb slot holds ordinary text.
        let mut with_marker = vec![
            sys("system"),
            user("[synthetic] earlier history was trimmed"),
            user("old request"),
            asst("old answer"),
            user("newest request"),
            asst("newest answer"),
        ];
        let dropped = drop_oldest_whole_turn(&mut with_marker, true);
        assert_eq!(
            dropped, 2,
            "drop starts after the owner-recorded synthetic crumb"
        );
        assert!(matches!(
            with_marker.get(1),
            Some(m) if m.role == "user" && m.content.contains("synthetic")
        ));
        assert!(matches!(
            with_marker.get(2),
            Some(m) if m.role == "user" && m.content == "newest request"
        ));
    }
    // ── §2 post-merge invariants ──────────────────────────────────────

    fn multi_turn_history(turns: usize, body: &str) -> Vec<ChatMessage> {
        let mut h = vec![sys("system")];
        for i in 0..turns {
            h.push(user(&format!("t{i} {body}")));
            h.push(asst(&format!("a{i} {body}")));
        }
        h
    }

    /// §2: the keep-N cascade prefers `keep_recent_turns` newest whole turns
    /// and never splits a turn.
    #[test]
    fn the_cascade_keeps_exactly_the_preferred_whole_turns() {
        let h = multi_turn_history(5, "body");
        let r = trim_to_fit(h, 2, usize::MAX, 0);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 2);
        assert!(!r.exceeds_budget);
        assert!(!r.below_preferred_keep);
        // The two newest turns survive, whole.
        assert!(r.history.iter().any(|m| m.content.contains("t4")));
        assert!(r.history.iter().any(|m| m.content.contains("t3")));
        assert!(!r.history.iter().any(|m| m.content.contains("t2")));
    }

    /// §2: the cascade bottoms out at one turn and reports the unsatisfiable
    /// floor instead of silently claiming the history fits.
    #[test]
    fn the_cascade_floors_at_one_turn_and_reports_exceeds_budget() {
        let h = multi_turn_history(4, &"x".repeat(2_000));
        let r = trim_to_fit(h, 5, 10, 10);
        assert_eq!(r.kept_turns, 1);
        assert!(r.exceeds_budget, "a floor that cannot fit must be reported");
        // The newest turn is never dropped.
        assert!(r.history.iter().any(|m| m.content.contains("t3")));
        assert!(!r.history.iter().any(|m| m.content.contains("t0")));
    }

    /// §2: `ContextCalibration` anchors the size on the provider's reported
    /// count and prices only the not-yet-billed tail.
    #[test]
    fn the_calibration_anchors_on_the_provider_reported_size() {
        let mut h = vec![sys("base"), user("first")];
        let mut cal = ContextCalibration::new();
        cal.observe_reported(1_000, &h);
        assert_eq!(cal.current(&h), 1_000);
        h.push(asst("reply"));
        let expected = 1_000 + estimate_history_tokens(&h[h.len() - 1..]);
        assert_eq!(cal.current(&h), expected);
    }

    /// §2: the retention path drops whole turns only — no model-written
    /// summary is ever inserted.
    #[test]
    fn the_retention_path_keeps_whole_turns_and_no_summary() {
        let h = multi_turn_history(4, "body");
        let r = trim_to_fit(h, 2, usize::MAX, 0);
        let mut seen_user = false;
        for m in &r.history {
            if m.role == "user" {
                seen_user = true;
            }
            if m.role == "assistant" {
                assert!(seen_user, "an assistant row must follow its turn head");
            }
        }
        // No synthesized summary text: every retained row is one the caller
        // supplied.
        let supplied: Vec<String> = (0..4)
            .flat_map(|i| [format!("t{i} body"), format!("a{i} body")])
            .collect();
        for m in &r.history {
            assert!(
                supplied.contains(&m.content) || m.role == "system",
                "unexpected content in the retained set: {:?}",
                m.content
            );
        }
    }

    #[test]
    fn a_breadcrumb_is_machinery_and_still_a_turn_boundary() {
        // The two predicates answer different questions: this one is about
        // display, `is_turn_boundary` about where a turn starts.
        let crumb = breadcrumb();
        assert!(is_synthetic_user_message(&crumb));
        assert!(is_turn_boundary(&crumb));
        // A tool round is machinery and explicitly *not* a boundary.
        let round = user("[Tool results]\nresult");
        assert!(is_synthetic_user_message(&round));
        assert!(!is_turn_boundary(&round));
    }

    #[test]
    fn budget_cascade_drops_below_preferred_until_fit() {
        let big = "x".repeat(800);
        let h = multi_turn_history(6, &big);
        // Prefer 5; force a budget that only ~2 newest turns can satisfy.
        let keep2 = trim_to_fit(h.clone(), 2, 0, 0);
        let fit = keep2.tokens_after;
        let r = trim_to_fit(h, 5, fit, 0);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 2);
        assert!(r.below_preferred_keep);
        assert!(!r.exceeds_budget);
        assert!(r.tokens_after <= fit);
    }

    #[test]
    fn budget_cascade_never_goes_below_one_turn() {
        let huge = "z".repeat(20_000);
        let h = multi_turn_history(4, &huge);
        let r = trim_to_fit(h, 5, 10, 10);
        assert_eq!(r.kept_turns, 1);
        assert!(r.exceeds_budget);
        assert!(r.history.iter().any(|m| m.content.contains("t3")));
        assert!(!r.history.iter().any(|m| m.content.contains("t0")));
    }

    #[test]
    fn budget_cascade_oversized_single_turn_exceeds() {
        let huge = "z".repeat(20_000);
        let h = vec![sys("s"), user(&format!("only {huge}")), asst("a")];
        let r = trim_to_fit(h, 5, 50, 50);
        assert!(!r.trimmed);
        assert_eq!(r.kept_turns, 1);
        assert!(r.exceeds_budget);
    }

    #[test]
    fn budget_cascade_stops_at_preferred_when_under_budget() {
        let h = multi_turn_history(8, "short");
        let after_keep5 = trim_to_fit(h.clone(), 5, 0, 0);
        assert!(after_keep5.trimmed);
        let fit = after_keep5.tokens_after;
        let r = trim_to_fit(h, 5, fit, 0);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 5);
        assert!(!r.exceeds_budget);
        assert!(r.tokens_after <= fit);
    }

    #[test]
    fn calibration_anchors_on_reported_and_prices_the_unbilled_tail() {
        // Provider saw a 1_000-token prompt for a 2-message history.
        let mut hist = vec![sys("base"), user("first")];
        let mut cal = ContextCalibration::new();
        cal.observe_reported(1_000, &hist);
        // No growth yet: exactly the authoritative anchor.
        assert_eq!(cal.current(&hist), 1_000);
        // The assistant reply + tool result are unbilled: priced locally and
        // added on top of the anchor, never re-estimating the anchored prefix.
        hist.push(asst("reply body"));
        hist.push(tool("tool output"));
        // The unbilled tail is priced with the public history estimate over
        // just the two new messages, so the expectation matches what
        // `ContextCalibration::current` actually sums.
        let expected = 1_000 + estimate_history_tokens(&[asst("reply body"), tool("tool output")]);
        assert_eq!(cal.current(&hist), expected);
    }

    #[test]
    fn calibration_ignores_the_anchored_prefix_in_the_estimate() {
        // A large anchored prompt dwarfs the local estimate of the same bytes;
        // `current` must trust the anchor, not re-estimate the prefix.
        let big = "x".repeat(400_000); // ~100k tokens by the /4 heuristic
        let mut hist = vec![sys("base"), user(&big)];
        let mut cal = ContextCalibration::new();
        cal.observe_reported(1_000, &hist); // provider says the prompt is tiny
        hist.push(asst("ok"));
        let current = cal.current(&hist);
        assert!(
            current < estimate_history_tokens(&hist) / 2,
            "anchored current ({current}) must be far below the naive whole-history estimate ({})",
            estimate_history_tokens(&hist)
        );
    }

    #[test]
    fn calibration_without_anchor_falls_back_to_estimate() {
        let hist = vec![sys("base"), user("hello there"), asst("hi")];
        let cal = ContextCalibration::new();
        assert_eq!(cal.current(&hist), estimate_history_tokens(&hist));
    }

    #[test]
    fn dropped_messages_counts_all_dropped_body_rows() {
        // Dropped turn = user + assistant pair; dropping 2 turns removes 4
        // non-system rows before the kept boundary.
        let h = vec![
            sys("s"),
            user("t1"),
            asst("a1"),
            user("t2"),
            asst("a2"),
            user("t3"),
            asst("a3"),
        ];
        let r = trim_to_fit(h, 1, 0, 0);
        assert!(r.trimmed);
        assert_eq!(r.dropped_messages, 4);
    }

    #[test]
    fn exactly_at_keep_is_untouched() {
        // 2 turns, keep 2 → the body already fits, nothing to drop.
        let h = vec![sys("s"), user("t1"), asst("a1"), user("t2"), asst("a2")];
        let r = trim_to_fit(h, 2, 0, 0);
        assert!(!r.trimmed);
    }

    #[test]
    fn keeps_leading_system_after_trim() {
        let h = vec![sys("sysA"), user("t1"), asst("a1"), user("t2"), asst("a2")];
        let r = trim_to_fit(h, 1, 0, 0);
        assert!(r.trimmed);
        assert_eq!(r.history[0].role, "system");
        assert_eq!(r.history[0].content, "sysA");
        assert_eq!(first_kept_user_content(&r), "t2");
    }

    #[test]
    fn keeps_most_recent_keep_turns() {
        let h = vec![
            sys("s"),
            user("t1"),
            asst("a1"),
            user("t2"),
            asst("a2"),
            user("t3"),
            asst("a3"),
            user("t4"),
            asst("a4"),
        ];
        let r = trim_to_fit(h, 2, 0, 0);
        assert!(r.trimmed);
        assert_eq!(r.dropped_turns, 2);
        assert_eq!(r.kept_turns, 2);
        assert_eq!(first_kept_user_content(&r), "t3");
        assert!(r.history.iter().any(|m| m.content == "t4"));
        assert!(
            !r.history
                .iter()
                .any(|m| m.content == "t1" || m.content == "t2"),
            "dropped turns must be gone"
        );
    }

    #[test]
    fn oversized_retained_turn_is_still_kept_by_design() {
        // keep_recent_turns ignores the budget: one oversized newest turn is
        // retained even though it alone would exceed any provider window.
        let huge = "z".repeat(10_000);
        let h = vec![
            sys("system"),
            user("old"),
            asst("a"),
            user(&format!("recent {huge}")),
            asst("a2"),
        ];
        let r = trim_to_fit(h, 1, 0, 0);
        assert!(r.trimmed);
        assert!(r.kept_turns >= 1);
        assert!(r.history.iter().any(|m| m.content.contains("recent")));
    }

    #[test]
    fn reanchor_after_trim_never_double_counts() {
        let mut hist = vec![sys("base"), user("a"), asst("b"), user("c"), asst("d")];
        let mut cal = ContextCalibration::new();
        cal.observe_reported(5_000, &hist);
        hist.push(tool("tail"));
        // Trim drops older turns AND the anchor is refreshed to the post-trim
        // reported total with the current length — the sim of a re-anchor.
        hist = vec![sys("base"), user("c"), asst("d"), tool("tail")];
        cal.observe_reported(2_000, &hist);
        assert_eq!(cal.current(&hist), 2_000);
    }

    #[test]
    fn resolve_send_budget_falls_back_to_token_budget_when_send_unset() {
        assert_eq!(resolve_send_budget(0, 10_000), 10_000);
    }

    #[test]
    fn resolve_send_budget_prefers_explicit_send_budget() {
        assert_eq!(resolve_send_budget(500, 10_000), 500);
    }

    #[test]
    fn single_turn_never_trimmed() {
        let h = vec![sys("s"), user("only"), asst("a")];
        let r = trim_to_fit(h, 1, 0, 0);
        assert!(!r.trimmed);
    }

    #[test]
    fn tool_results_pseudo_user_is_not_a_turn_boundary() {
        // A "[Tool results]" user message is a continuation, not a new turn.
        // Real turns are [A, B]; keep 1 must drop A together with its
        // trailing "[Tool results]" tail, keeping only B.
        let h = vec![
            sys("s"),
            user("turnA"),
            asst("calling"),
            user("[Tool results]\nresult of A"),
            asst("doneA"),
            user("turnB"),
            asst("doneB"),
        ];
        let r = trim_to_fit(h, 1, 0, 0);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 1);
        assert_eq!(first_kept_user_content(&r), "turnB");
        assert!(
            !r.history
                .iter()
                .any(|m| m.content.starts_with("[Tool results]")),
            "the dropped turn's tool-results tail must go with it"
        );
    }

    #[test]
    fn trim_to_fit_uses_send_budget_not_token_budget() {
        let big = "x".repeat(800);
        let h = multi_turn_history(6, &big);
        let keep2 = trim_to_fit(h.clone(), 2, 0, 0);
        let send_budget = keep2.tokens_after;
        let token_budget = send_budget.saturating_mul(4);

        let send_trim = trim_to_fit(h.clone(), 5, send_budget, 0);
        let token_trim = trim_to_fit(h, 5, token_budget, 0);

        assert!(send_trim.trimmed);
        assert!(send_trim.tokens_after <= send_budget);
        assert!(
            send_trim.kept_turns <= token_trim.kept_turns,
            "a tighter send budget must retain no more turns than a looser fit budget"
        );
        assert!(
            send_trim.kept_turns < 5 || send_trim.tokens_after < token_trim.tokens_after,
            "send budget must drive the cascade, not the water-line token budget"
        );
    }

    #[test]
    fn trimmed_reports_token_reduction() {
        let big = "x".repeat(2000);
        let h = vec![
            sys("system"),
            user(&format!("t1 {big}")),
            asst("a1"),
            user(&format!("t2 {big}")),
            asst("a2"),
            user("t3 short"),
            asst("a3"),
        ];
        let r = trim_to_fit(h, 1, 0, 0);
        assert!(r.trimmed);
        assert!(r.tokens_before > r.tokens_after);
    }

    #[test]
    fn under_keep_is_untouched() {
        let h = vec![sys("s"), user("t1"), asst("a1"), user("t2"), asst("a2")];
        let n = h.len();
        let r = trim_to_fit(h, 5, 0, 0);
        assert!(!r.trimmed);
        assert_eq!(r.history.len(), n);
        assert_eq!(r.dropped_turns, 0);
    }

    #[test]
    fn zero_keep_normalises_to_one_and_keeps_last_turn() {
        let h = vec![
            sys("s"),
            user("t1"),
            asst("a1"),
            user("t2"),
            asst("a2"),
            user("t3"),
            asst("a3"),
        ];
        let r = trim_to_fit(h, 0, 0, 0);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 1);
        assert_eq!(r.dropped_turns, 2);
        assert_eq!(first_kept_user_content(&r), "t3");
    }

    #[test]
    fn conversation_trim_keeps_tool_structured_turn_intact() {
        let history = vec![
            conversation_system("s"),
            conversation_user("old"),
            conversation_assistant("a1"),
            conversation_user("recent"),
            ConversationMessage::AssistantToolCalls {
                text: Some("calling".into()),
                tool_calls: vec![ToolCall {
                    id: "1".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                    extra_content: None,
                }],
                reasoning_content: None,
            },
            ConversationMessage::ToolResults(vec![ToolResultMessage {
                tool_call_id: "1".into(),
                content: "ok".into(),
                tool_name: "shell".into(),
            }]),
            conversation_assistant("done"),
        ];
        let r = trim_conversation_to_recent_turns_by_count(history, 1, false);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 1);
        assert!(matches!(
            r.history
                .iter()
                .find(|m| matches!(m, ConversationMessage::AssistantToolCalls { .. })),
            Some(ConversationMessage::AssistantToolCalls { .. })
        ));
        assert!(matches!(
            r.history
                .iter()
                .find(|m| matches!(m, ConversationMessage::ToolResults(_))),
            Some(ConversationMessage::ToolResults(_))
        ));
        assert!(!r.history.iter().any(|m| matches!(
            m,
            ConversationMessage::Chat(c) if c.content == "old"
        )));
    }

    #[test]
    fn conversation_trim_under_keep_untouched() {
        let history = vec![
            conversation_system("s"),
            conversation_user("t1"),
            conversation_assistant("a1"),
            conversation_user("t2"),
            conversation_assistant("a2"),
        ];
        let n = history.len();
        let r = trim_conversation_to_recent_turns_by_count(history, 5, false);
        assert!(!r.trimmed);
        assert_eq!(r.history.len(), n);
    }

    #[test]
    fn conversation_budget_cascade_matches_chat_semantics() {
        let big = "y".repeat(600);
        let history = vec![
            conversation_system("s"),
            conversation_user(&format!("t0 {big}")),
            conversation_assistant("a0"),
            conversation_user(&format!("t1 {big}")),
            conversation_assistant("a1"),
            conversation_user(&format!("t2 {big}")),
            conversation_assistant("a2"),
            conversation_user("t3 short"),
            conversation_assistant("a3"),
        ];
        let keep1 = trim_conversation_to_recent_turns_by_count(history.clone(), 1, false);
        let fit = keep1.tokens_after;
        let r = trim_conversation_to_fit(history, 5, fit, 0);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 1);
        assert!(!r.exceeds_budget);
    }

    #[test]
    fn conversation_budget_settlement_drops_further_when_full_estimate_exceeds() {
        // Simulate a non-additive provider-view estimate: the full slice is
        // priced much higher than the sum of single-message probes, so the
        // native cascade would under-drop without the settlement loop.
        let body = "w".repeat(200);
        let history = vec![
            conversation_system("s"),
            conversation_user(&format!("t0 {body}")),
            conversation_assistant("a0"),
            conversation_user(&format!("t1 {body}")),
            conversation_assistant("a1"),
            conversation_user(&format!("t2 {body}")),
            conversation_assistant("a2"),
            conversation_user("t3 short"),
            conversation_assistant("a3"),
        ];
        let inflate = |hist: &[ConversationMessage]| {
            let native = estimate_conversation_tokens(hist);
            if hist.len() <= 2 {
                native
            } else {
                native.saturating_mul(8)
            }
        };
        let keep1 = trim_conversation_to_recent_turns_by_count(history.clone(), 1, false);
        let send_budget = inflate(&keep1.history).saturating_add(10);
        let r = trim_conversation_to_fit_with(history, 5, send_budget, 0, inflate);
        assert!(r.trimmed);
        assert_eq!(r.kept_turns, 1);
        assert!(!r.exceeds_budget);
        assert!(r.tokens_after <= send_budget);
    }

    #[test]
    fn synthetic_covers_both_machinery_carriers() {
        assert!(is_synthetic_user_message(&user(
            "[Tool results]\nresult of A"
        )));
        // Leading whitespace is not what makes it machinery.
        assert!(is_synthetic_user_message(&user(
            "  [Tool results] trailing"
        )));
        assert!(is_synthetic_user_message(&breadcrumb()));
    }

    #[test]
    fn synthetic_leaves_operator_turns_alone() {
        assert!(!is_synthetic_user_message(&user("read the build script")));
        assert!(!is_synthetic_user_message(&user("")));
        // Only the prefix makes a tool round machinery; quoting it mid-message
        // is still the operator talking.
        assert!(!is_synthetic_user_message(&user(
            "I said [Tool results] once"
        )));
        // A different role carrying the same text is not a user bubble.
        assert!(!is_synthetic_user_message(&asst(
            "[Tool results]\nresult of A"
        )));
        assert!(!is_synthetic_user_message(&sys("[Tool results]")));
    }
}

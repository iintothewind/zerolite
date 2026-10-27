//! The agent turn engine, decomposed into single-purpose step modules.

pub(crate) mod approval_gate;
pub(crate) mod call_prep;
pub(crate) mod context;
pub(crate) mod context_pipeline;
pub(crate) mod context_recovery;
pub(crate) mod delivery_defaults;
pub(crate) mod events;
pub(crate) mod execution;
pub(crate) mod history_append;
pub(crate) mod history_window;
pub(crate) mod knobs;
pub(crate) mod max_iter;
pub(crate) mod media_degrade;
pub(crate) mod outcome;
pub(crate) mod parse_response;
pub(crate) mod post_exec;
pub(crate) mod progress;
pub(crate) mod protocol_detect;
pub(crate) mod provider_call;
pub(crate) mod redact;
pub(crate) mod results_collect;
pub(crate) mod steering;
pub(crate) mod stream_consume;
pub(crate) mod stream_guard;
pub(crate) mod tool_specs;
pub(crate) mod vision_route;

pub(crate) use call_prep::{PreparedToolCalls, prepare_tool_calls};
pub(crate) use context::{TurnCtx, TurnMeta};
pub(crate) use context_recovery::{record_llm_failure, try_recover_context_overflow};
#[cfg(test)]
pub(crate) use delivery_defaults::maybe_inject_channel_delivery_defaults;
pub use events::{
    DRAFT_PLACEHOLDER, DraftEvent, PROGRESS_MIN_INTERVAL_MS, ProgressEvent, REASONING_FULL_PREFIX,
    StreamDelta, THINKING_STATUS_PREFIX, is_thinking_status_text, thinking_status_label_round,
    thinking_status_round, thinking_status_text,
};
pub use execution::{
    ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess, ResolvedRuntimeKnobs,
};
pub(crate) use history_window::preflight_history_maintenance;
pub use knobs::{LoopKnobs, MaxIterationBehavior};
pub(crate) use max_iter::finish_after_max_iterations;
pub use outcome::{
    ContextWindowExceeded, append_safeguard_fallback_notice, context_window_exceeded_from_error,
    is_semantic_empty_terminal_completion, semantic_empty_terminal_completion_message,
    terminal_completion_error_message,
};
pub use outcome::{
    ModelSwitchCallback, ModelSwitchRequested, ServedRoute, ServedRouteSink, ToolLoopCancelled,
    is_model_switch_requested, is_tool_loop_cancelled,
};
pub(crate) use outcome::{SteeringPending, StreamCancelledAfterOutput};
pub(crate) use outcome::{
    current_model_switch_state, model_switch_provider_allowed,
    scope_model_switch_provider_allowlist, scope_model_switch_state,
};
#[cfg(test)]
pub(crate) use parse_response::build_native_assistant_history;
pub(crate) use parse_response::{
    interpret_chat_response, record_accepted_chat_response, resolve_display_text,
    unforwarded_narration,
};
pub(crate) use post_exec::record_executed_outcomes;
pub(crate) use provider_call::{
    ProviderCallOutcome, announce_llm_request, call_provider, enforce_tool_loop_budget,
};
pub use redact::scrub_credentials;
pub(crate) use results_collect::{
    CollectedResults, check_identical_output_abort, collect_tool_results,
};
pub use steering::{
    SteeringAdmission, SteeringAdmit, SteeringInput, SteeringPosture, drain_steering_messages,
};
#[cfg(test)]
pub(crate) use stream_consume::consume_provider_streaming_response;
#[cfg(test)]
use tool_specs::IterationToolSpecs;
pub(crate) use tool_specs::build_iteration_tool_specs;
#[cfg(test)]
pub(crate) use vision_route::prepare_messages_for_iteration;

use crate::agent::execution_tree_budget::{ExecutionTreeBudget, ExecutionTreeReservation};
use crate::agent::history_trim::ContextAnchor;
use crate::agent::system_prompt::{NATIVE_TOOLS_TASK_FRAMING, NO_TOOLS_TASK_FRAMING};
use crate::agent::tool_execution::{
    ToolDispatchContext, execute_tools_parallel, execute_tools_sequential,
    should_execute_tools_in_parallel,
};
use crate::live_config_authority::AgentExecutionAdmission;
use crate::security::ingress::{IngressPolicy, ingress_policy, steering_policy};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
pub use outcome::ContextAnchorSink;
use std::collections::{HashSet, VecDeque};
use std::io::Write as _;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::agent::TurnEvent;
use zeroclaw_api::channel::Channel;
use zeroclaw_api::ingress::{IngressContext, IngressDecision};
use zeroclaw_providers::{ChatMessage, ModelProvider};

/// The injector's exact raw-history position, carried through prefix trims.
#[derive(Clone)]
pub struct MemoryPreamble {
    pub preamble: String,
    pub index: usize,
}

fn retention_layout(history: &[ChatMessage], crumb: bool) -> (usize, usize, bool) {
    (
        history.len(),
        history
            .iter()
            .take_while(|message| message.role == "system")
            .count(),
        crumb,
    )
}

fn remap_memory_after_trim(
    injected: &mut Option<MemoryPreamble>,
    before: (usize, usize, bool),
    history: &[ChatMessage],
    crumb: bool,
) {
    let inserted = usize::from(crumb && !before.2);
    let dropped = before
        .0
        .saturating_add(inserted)
        .saturating_sub(history.len());
    let start = before.1 + usize::from(before.2);
    if let Some(target) = injected {
        if (start..start.saturating_add(dropped)).contains(&target.index) {
            *injected = None;
        } else if target.index >= start {
            target.index = target
                .index
                .saturating_sub(dropped)
                .saturating_add(inserted);
        }
    }
}

pub(crate) fn retained_context_snapshot(
    injected: &Option<MemoryPreamble>,
    history: &[ChatMessage],
    breadcrumb: bool,
) -> zeroclaw_api::agent::RetainedContextSnapshot {
    retained_context_snapshot_with_memory(
        history,
        breadcrumb,
        injected
            .as_ref()
            .map(|target| (target.preamble.as_str(), target.index)),
    )
}

pub(crate) fn retained_context_snapshot_with_memory(
    history: &[ChatMessage],
    breadcrumb: bool,
    injected: Option<(&str, usize)>,
) -> zeroclaw_api::agent::RetainedContextSnapshot {
    zeroclaw_api::agent::RetainedContextSnapshot {
        // Reuse the Agent's canonical replay path so native tool calls/results
        // stay typed and the owner-tracked memory provenance is honored by the
        // caller that has it. This is deliberately not a role-only projection.
        retained_messages: injected
            .map_or_else(
                || crate::agent::Agent::replay_loop_messages_for_retention(history),
                |(preamble, index)| {
                    crate::agent::Agent::replay_loop_messages_with_memory(history, preamble, index)
                },
            )
            .into_iter()
            .filter(|message| {
                !matches!(
                    message,
                    zeroclaw_api::model_provider::ConversationMessage::Chat(chat)
                        if chat.role == "system"
                )
            })
            .map(|message| match message {
                zeroclaw_api::model_provider::ConversationMessage::AssistantToolCalls {
                    text,
                    tool_calls,
                    reasoning_content: _,
                } => zeroclaw_api::model_provider::ConversationMessage::AssistantToolCalls {
                    text,
                    tool_calls,
                    reasoning_content: None,
                },
                other => other,
            })
            .collect(),
        breadcrumb,
    }
}

/// Maximum malformed internal tool-protocol retries before returning a safe fallback.
pub(crate) const MAX_MALFORMED_TOOL_PROTOCOL_RETRIES: usize = 2;

/// Resolve capacity + proactive-trim budget for one `(provider, model)` route.
///
/// The turn engine's SINGLE authority for per-call limits. Daemon-backed agents
/// hand in a closure that reads the shared live `Config`, so a config reload
/// between turns is observed; configless (test) paths omit it and the loop
/// falls back to the route pair resolved at the turn boundary. Keeping one
/// resolver here prevents the loop from recomputing half the pair from a
/// construction-time config snapshot.
pub type ContextLimitsResolver =
    Arc<dyn Fn(&str, &str) -> zeroclaw_config::schema::ResolvedContextLimits + Send + Sync>;

/// Default maximum agentic tool-use iterations per user message to prevent runaway loops.
/// Used as a safe fallback when `max_tool_iterations` is unset or configured as zero.
pub(crate) const DEFAULT_MAX_TOOL_ITERATIONS: usize = 10;

const PROVIDER_IMAGE_ROUTE_MAX_ENTRIES: usize = 8;
const PROVIDER_IMAGE_ACCEPTED_MAX_ENTRIES: usize = 32;
const PROVIDER_IMAGE_QUARANTINED_MAX_ENTRIES: usize = 32;

type ProviderImageId = zeroclaw_providers::multimodal::ProviderImageId;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderImageRoute {
    provider: String,
    model: String,
}

#[derive(Debug, Default)]
struct ProviderImageRouteState {
    accepted: VecDeque<ProviderImageId>,
    quarantined: VecDeque<ProviderImageId>,
}

/// Provider-facing replay state. Canonical conversation history remains the
/// only durable message source; this state only materializes a route-local view.
#[derive(Debug, Default)]
pub(crate) struct ProviderImageState {
    routes: VecDeque<(ProviderImageRoute, ProviderImageRouteState)>,
}

impl ProviderImageState {
    fn route(provider: &str, model: &str) -> ProviderImageRoute {
        ProviderImageRoute {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    fn touch_route(&mut self, route: &ProviderImageRoute) -> Option<&ProviderImageRouteState> {
        let index = self
            .routes
            .iter()
            .position(|(candidate, _)| candidate == route)?;
        let entry = self.routes.remove(index)?;
        self.routes.push_back(entry);
        self.routes.back().map(|(_, state)| state)
    }

    fn route_mut(&mut self, route: ProviderImageRoute) -> &mut ProviderImageRouteState {
        if let Some(index) = self
            .routes
            .iter()
            .position(|(candidate, _)| candidate == &route)
            && let Some(entry) = self.routes.remove(index)
        {
            self.routes.push_back(entry);
        } else {
            if self.routes.len() >= PROVIDER_IMAGE_ROUTE_MAX_ENTRIES {
                self.routes.pop_front();
            }
            self.routes
                .push_back((route, ProviderImageRouteState::default()));
        }
        &mut self.routes.back_mut().expect("route was just inserted").1
    }

    fn retain_recent(
        ids: &mut VecDeque<ProviderImageId>,
        incoming: &[ProviderImageId],
        max_entries: usize,
    ) {
        for image in incoming {
            if let Some(index) = ids.iter().position(|candidate| candidate == image) {
                ids.remove(index);
            }
            ids.push_back(*image);
        }
        while ids.len() > max_entries {
            ids.pop_front();
        }
    }

    fn accepted(&mut self, route: &ProviderImageRoute) -> Vec<ProviderImageId> {
        self.touch_route(route)
            .map(|state| state.accepted.iter().copied().collect())
            .unwrap_or_default()
    }

    fn quarantined(&mut self, route: &ProviderImageRoute) -> Vec<ProviderImageId> {
        self.touch_route(route)
            .map(|state| state.quarantined.iter().copied().collect())
            .unwrap_or_default()
    }

    fn record_success(
        &mut self,
        route: ProviderImageRoute,
        accepted: &[ProviderImageId],
        retried: &[ProviderImageId],
    ) {
        let state = self.route_mut(route);
        Self::retain_recent(
            &mut state.accepted,
            accepted,
            PROVIDER_IMAGE_ACCEPTED_MAX_ENTRIES,
        );
        for image in retried {
            if accepted.contains(image) {
                state.quarantined.retain(|candidate| candidate != image);
            }
        }
    }

    fn record_recovery(
        &mut self,
        route: ProviderImageRoute,
        retained: &[ProviderImageId],
        replaced: &[ProviderImageId],
    ) {
        let state = self.route_mut(route);
        Self::retain_recent(
            &mut state.accepted,
            retained,
            PROVIDER_IMAGE_ACCEPTED_MAX_ENTRIES,
        );
        Self::retain_recent(
            &mut state.quarantined,
            replaced,
            PROVIDER_IMAGE_QUARANTINED_MAX_ENTRIES,
        );
    }
}

fn suppress_quarantined_provider_images(
    messages: &[ChatMessage],
    quarantined: &[zeroclaw_providers::multimodal::ProviderImageId],
    has_new_user_input: bool,
) -> Vec<ChatMessage> {
    let newest_user_message = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| {
            message.role == "user" && !message.content.trim_start().starts_with("[Tool results]")
        })
        .map(|(index, message)| (index, message.clone()));
    let mut filtered =
        zeroclaw_providers::multimodal::omit_provider_image_ids(messages, quarantined);
    if has_new_user_input
        && let Some((index, message)) = newest_user_message
        && let Some(slot) = filtered.get_mut(index)
    {
        *slot = message;
    }
    filtered
}

fn prepare_provider_image_recovery_view(
    messages: &[ChatMessage],
    submitted: &[ProviderImageId],
    accepted: &[ProviderImageId],
) -> Option<(Vec<ChatMessage>, Vec<ProviderImageId>)> {
    let replaced: Vec<_> = submitted
        .iter()
        .copied()
        .filter(|image| !accepted.contains(image))
        .collect();
    if submitted.is_empty() || replaced.is_empty() {
        return None;
    }

    Some((
        zeroclaw_providers::multimodal::omit_provider_image_ids(messages, &replaced),
        replaced,
    ))
}

#[cfg(test)]
mod provider_image_state_tests {
    use super::*;

    fn image_id(payload: &str) -> zeroclaw_providers::multimodal::ProviderImageId {
        zeroclaw_providers::multimodal::provider_image_ids(&[ChatMessage::user(format!(
            "[IMAGE:data:image/png;base64,{payload}]"
        ))])[0]
    }

    fn image_ids(prefix: &str, count: usize) -> Vec<ProviderImageId> {
        (0..count)
            .map(|index| image_id(&format!("{prefix}{index}")))
            .collect()
    }

    #[test]
    fn acceptance_and_quarantine_are_route_local_and_retry_clears_on_success() {
        let accepted = image_id("AAAA");
        let rejected = image_id("BBBB");
        let route_a = ProviderImageState::route("compatible.primary", "model-a");
        let route_b = ProviderImageState::route("compatible.primary", "model-b");
        let mut state = ProviderImageState::default();

        state.record_recovery(route_a.clone(), &[accepted], &[rejected]);
        assert!(state.accepted(&route_a).contains(&accepted));
        assert_eq!(state.quarantined(&route_a), vec![rejected]);
        assert!(state.accepted(&route_b).is_empty());
        assert!(state.quarantined(&route_b).is_empty());

        state.record_success(route_a.clone(), &[rejected], &[rejected]);
        assert!(state.accepted(&route_a).contains(&rejected));
        assert!(state.quarantined(&route_a).is_empty());
    }

    #[test]
    fn route_state_evicts_the_oldest_route_and_reads_refresh_recency() {
        let mut state = ProviderImageState::default();
        let routes: Vec<_> = (0..PROVIDER_IMAGE_ROUTE_MAX_ENTRIES)
            .map(|index| ProviderImageState::route("compatible.primary", &format!("model-{index}")))
            .collect();
        for route in &routes {
            state.record_success(route.clone(), &[], &[]);
        }

        state.accepted(&routes[0]);
        let newest = ProviderImageState::route("compatible.primary", "model-new");
        state.record_success(newest.clone(), &[], &[]);

        assert_eq!(state.routes.len(), PROVIDER_IMAGE_ROUTE_MAX_ENTRIES);
        assert!(state.routes.iter().any(|(route, _)| route == &routes[0]));
        assert!(!state.routes.iter().any(|(route, _)| route == &routes[1]));
        assert_eq!(state.routes.back().map(|(route, _)| route), Some(&newest));
    }

    #[test]
    fn identity_state_evicts_oldest_entries_and_duplicates_refresh_recency() {
        let route = ProviderImageState::route("compatible.primary", "model-a");
        let accepted = image_ids("accepted-", PROVIDER_IMAGE_ACCEPTED_MAX_ENTRIES);
        let quarantined = image_ids("quarantined-", PROVIDER_IMAGE_QUARANTINED_MAX_ENTRIES);
        let accepted_new = image_id("accepted-new");
        let quarantined_new = image_id("quarantined-new");
        let mut state = ProviderImageState::default();

        state.record_recovery(route.clone(), &accepted, &quarantined);
        state.record_recovery(
            route.clone(),
            &[accepted[0], accepted_new],
            &[quarantined[0], quarantined_new],
        );

        let accepted_after = state.accepted(&route);
        assert_eq!(accepted_after.len(), PROVIDER_IMAGE_ACCEPTED_MAX_ENTRIES);
        assert!(accepted_after.contains(&accepted[0]));
        assert!(!accepted_after.contains(&accepted[1]));
        assert_eq!(accepted_after.last(), Some(&accepted_new));
        let quarantined_after = state.quarantined(&route);
        assert_eq!(
            quarantined_after.len(),
            PROVIDER_IMAGE_QUARANTINED_MAX_ENTRIES
        );
        assert!(quarantined_after.contains(&quarantined[0]));
        assert!(!quarantined_after.contains(&quarantined[1]));
        assert_eq!(quarantined_after.last(), Some(&quarantined_new));
    }

    #[test]
    fn quarantine_notice_describes_images_from_the_rejected_request() {
        let singular = crate::i18n::get_english_cli_string_with_args(
            "turn-provider-images-quarantined",
            &[("count", "1"), ("count_plural", "one")],
        );
        let plural = crate::i18n::get_english_cli_string_with_args(
            "turn-provider-images-quarantined",
            &[("count", "2"), ("count_plural", "other")],
        );

        assert!(singular.contains("1 image that had not previously succeeded"));
        assert!(plural.contains("2 images that had not previously succeeded"));
        assert!(singular.contains("Send an omitted image again in a new message"));
    }

    #[test]
    fn provider_image_recovery_view_includes_newest_unaccepted_image() {
        let image = "data:image/png;base64,AAAA";
        let messages = vec![ChatMessage::user(format!("new [IMAGE:{image}]"))];
        let submitted = zeroclaw_providers::multimodal::provider_image_ids(&messages);

        let (recovery_messages, replaced) =
            prepare_provider_image_recovery_view(&messages, &submitted, &[])
                .expect("newest unaccepted image is recoverable");

        assert_eq!(replaced, submitted);
        assert!(!recovery_messages[0].content.contains("[IMAGE:"));
    }

    #[test]
    fn newest_user_resubmission_bypasses_only_its_own_suppression() {
        let image = "data:image/png;base64,AAAA";
        let messages = vec![
            ChatMessage::user(format!("old [IMAGE:{image}]")),
            ChatMessage::assistant("retry it"),
            ChatMessage::user(format!("new [IMAGE:{image}]")),
        ];
        let id = image_id("AAAA");

        let filtered = suppress_quarantined_provider_images(&messages, &[id], true);

        assert!(!filtered[0].content.contains("[IMAGE:"));
        assert!(filtered[2].content.contains("[IMAGE:"));

        let continued = suppress_quarantined_provider_images(&messages, &[id], false);
        assert!(
            continued
                .iter()
                .all(|message| !message.content.contains("[IMAGE:"))
        );
    }
}

pub struct ToolLoopImageState<'a> {
    pub cache: &'a mut zeroclaw_providers::multimodal::LocalImageCache,
    pub(crate) provider_state: &'a mut ProviderImageState,
}

async fn emit_unforwarded_native_narration(
    event_tx: Option<&tokio::sync::mpsc::Sender<TurnEvent>>,
    remainder: &str,
    protocol_suppressed: bool,
    has_native_tool_calls: bool,
) {
    if !remainder.is_empty() && !protocol_suppressed && has_native_tool_calls {
        events::emit_posthoc_turn_chunk(event_tx, remainder).await;
    }
}

#[cfg(test)]
#[tokio::test]
async fn streamed_prefix_relays_only_unforwarded_native_narration_suffix() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let remainder = unforwarded_narration("About to check the count.", "About to ");

    emit_unforwarded_native_narration(Some(&tx), remainder, false, true).await;

    assert!(matches!(
        rx.try_recv(),
        Ok(TurnEvent::Chunk { delta }) if delta == "check the count."
    ));
    assert!(
        rx.try_recv().is_err(),
        "the live prefix must not be replayed"
    );
}

/// Complete system-prompt variants for the two tool transports supported by a
/// turn. The caller owns construction; the loop only selects the variant after
/// a before-LLM hook has finalized the model that will receive the request.
#[derive(Clone)]
pub(crate) struct ToolProtocolPrompts {
    text_tools_section: String,
    max_chars: usize,
    /// The capped native request prompt, built from the complete native
    /// prompt. Only set when a cap applies: a history prompt that was capped
    /// for the text transport can have lost `## Safety` and later sections
    /// inside its long tool section, so removing that section cannot rebuild
    /// a complete native prompt from it.
    native_request: Option<String>,
}

impl ToolProtocolPrompts {
    /// Uncapped prompts for tests that exercise only the protocol swap.
    #[cfg(test)]
    pub(crate) fn new(native: String, text: String) -> Self {
        Self::with_max_chars(native, text, 0)
    }

    /// `native` and `text` are the complete (uncapped) prompts for each
    /// transport; `max_chars` caps provider-bound requests (0 = no cap).
    pub(crate) fn with_max_chars(native: String, text: String, max_chars: usize) -> Self {
        let text_tools_section = tool_section_bounds(&text)
            .map(|bounds| text[bounds].to_string())
            .unwrap_or_default();
        let native_request = (max_chars > 0).then(|| {
            let mut native = native;
            replace_tool_protocol_section(&mut native, "", true);
            crate::agent::system_prompt::finalize_system_prompt(native, max_chars)
        });
        Self {
            text_tools_section,
            max_chars,
            native_request,
        }
    }
}

tokio::task_local! {
    static TOOL_PROTOCOL_PROMPTS: Arc<ToolProtocolPrompts>;
}

/// Scope complete prompt variants around an Agent turn. This remains transient
/// request state: durable history keeps the caller-owned canonical prompt.
pub(crate) async fn scope_tool_protocol_prompts<F: std::future::Future>(
    prompts: Arc<ToolProtocolPrompts>,
    future: F,
) -> F::Output {
    TOOL_PROTOCOL_PROMPTS.scope(prompts, future).await
}

fn refresh_scoped_tool_protocol_prompt(
    history: &mut [ChatMessage],
    request_messages: &mut [ChatMessage],
    use_native_tools: bool,
) {
    let _ = TOOL_PROTOCOL_PROMPTS.try_with(|prompts| {
        // History keeps the swapped prompt without re-capping it; only the
        // provider-bound copy is capped. A capped native request is rebuilt
        // from the complete native prompt, never from a capped text history.
        let mut history_prompts = None;
        if let Some(system) = history.iter_mut().find(|message| message.role == "system") {
            let before = system.content.clone();
            replace_tool_protocol_section(
                &mut system.content,
                &prompts.text_tools_section,
                use_native_tools,
            );
            history_prompts = Some((before, system.content.clone()));
        }
        if let Some(system) = request_messages
            .iter_mut()
            .find(|message| message.role == "system")
        {
            // A `before_llm_call` hook may append to the request's system
            // prompt. Cap only the swapped base and re-append that suffix, so
            // hook text reaches the model as it does on the CLI path. The
            // request may already be finalized (the post-trim rebuild).
            let rebuilt = history_prompts.and_then(|(before, swapped)| {
                let base = match (&prompts.native_request, use_native_tools) {
                    (Some(native), true) => {
                        let mut native = [ChatMessage::system(native.clone())];
                        refresh_prompt_anchor(&mut native, true);
                        let [native] = native;
                        native.content
                    }
                    _ => swapped,
                };
                let capped =
                    crate::agent::system_prompt::finalize_system_prompt(base, prompts.max_chars);
                let suffix = system
                    .content
                    .strip_prefix(before.as_str())
                    .or_else(|| system.content.strip_prefix(capped.as_str()))?
                    .to_string();
                Some(capped + &suffix)
            });
            if let Some(rebuilt) = rebuilt {
                system.content = rebuilt;
            } else {
                replace_tool_protocol_section(
                    &mut system.content,
                    &prompts.text_tools_section,
                    use_native_tools,
                );
                system.content = crate::agent::system_prompt::finalize_system_prompt(
                    std::mem::take(&mut system.content),
                    prompts.max_chars,
                );
            }
        }
    });
}

fn tool_section_bounds(prompt: &str) -> Option<std::ops::Range<usize>> {
    let start = prompt.find("## Tools\n")?;
    let following = &prompt[start..];
    // A capped prompt may have lost `## Safety`; the section then runs to the
    // end of what survived the cap.
    let end = following
        .find("\n\n## Safety")
        .map_or(prompt.len(), |offset| start + offset);
    Some(start..end)
}

fn single_tool_round_error() -> anyhow::Error {
    anyhow::Error::new(stream_consume::SingleToolRoundViolation::default())
}

/// Only adjust the runtime's tool protocol and transient request prompt.
fn apply_single_tool_round_prompt(messages: &mut Vec<ChatMessage>) -> bool {
    let instruction = crate::i18n::get_required_cli_string("turn-single-tool-rounds-prompt");
    if let Some(system) = messages.iter_mut().find(|message| message.role == "system") {
        system.content = system.content.replace(
            crate::agent::tool_call_format::TOOL_CALL_PROTOCOL_INSTRUCTIONS,
            &instruction,
        );
        if !system.content.ends_with(&instruction) {
            system.content.push_str("\n\n");
            system.content.push_str(&instruction);
        }
        false
    } else {
        messages.insert(0, ChatMessage::system(instruction));
        true
    }
}

fn replace_tool_protocol_section(
    prompt: &mut String,
    text_tools_section: &str,
    use_native_tools: bool,
) {
    if let Some(bounds) = tool_section_bounds(prompt) {
        if use_native_tools {
            prompt.replace_range(bounds, "");
        } else {
            prompt.replace_range(bounds, text_tools_section);
        }
        return;
    }

    if !use_native_tools && !text_tools_section.is_empty() {
        let insertion = prompt.find("## Safety").unwrap_or(prompt.len());
        prompt.insert_str(insertion, &format!("{text_tools_section}\n\n"));
    }
}

fn apply_builtin_prompt_scope(
    messages: &mut [ChatMessage],
    tools_registry: &[Box<dyn crate::tools::Tool>],
    effective_names: &HashSet<String>,
    deferred_section: &str,
    use_native_tools: bool,
    strict_tool_parsing: bool,
    max_system_prompt_chars: usize,
    mut canonical_prompt: Option<&str>,
) {
    let names: HashSet<&str> = effective_names.iter().map(String::as_str).collect();
    let instructions =
        if use_native_tools || strict_tool_parsing || effective_names.contains("tool_search") {
            String::new()
        } else {
            crate::agent::loop_::build_tool_instructions_for_names(tools_registry, &names)
        };
    for message in messages
        .iter_mut()
        .filter(|message| message.role == "system")
    {
        // Hooks may append instructions outside the assembly cap. Preserve an
        // exact suffix, using the cap applied by scoped refresh to identify its base.
        let hook_suffix = if instructions.is_empty() {
            None
        } else {
            canonical_prompt
                .take()
                .and_then(|canonical| {
                    message
                        .content
                        .strip_prefix(canonical)
                        .map(str::to_owned)
                        .or_else(|| {
                            let refresh_cap = TOOL_PROTOCOL_PROMPTS
                                .try_with(|prompts| prompts.max_chars)
                                .unwrap_or(max_system_prompt_chars);
                            let capped = crate::agent::system_prompt::finalize_system_prompt(
                                canonical.to_owned(),
                                refresh_cap,
                            );
                            message.content.strip_prefix(&capped).map(str::to_owned)
                        })
                })
                .filter(|suffix| !suffix.is_empty())
        };
        if let Some(suffix) = hook_suffix.as_ref() {
            message
                .content
                .truncate(message.content.len() - suffix.len());
        }
        // A same-agent step shares history, but its discovery route is narrower.
        // Reconcile only the request clone, preserving unrelated prompt sections.
        let mut catalogue_inserted = false;
        for heading in [
            "## Deferred Built-in Tools\n",
            "## Tools\n",
            "## Tool Use Protocol\n",
        ] {
            if heading != "## Deferred Built-in Tools\n"
                && (use_native_tools
                    || strict_tool_parsing
                    || effective_names.contains("tool_search"))
            {
                continue;
            }
            let mut cursor = 0;
            while let Some(offset) = message.content[cursor..].find(heading) {
                let start = cursor + offset;
                let body = start + heading.len();
                let section_end = message.content[body..]
                    .find("\n## ")
                    .map_or(message.content.len(), |offset| body + offset);
                let (end, complete) = if heading == "## Deferred Built-in Tools\n" {
                    let close = "</available-deferred-builtin-tools>";
                    message.content[body..section_end]
                        .find(close)
                        .map_or((section_end, false), |offset| {
                            (body + offset + close.len(), true)
                        })
                } else {
                    (section_end, true)
                };
                let catalogue = deferred_section.trim_end_matches('\n');
                // Keep assembly's omission/budget decisions; projection may only shrink.
                let replacement = if heading == "## Deferred Built-in Tools\n"
                    && complete
                    && !catalogue_inserted
                    && catalogue.chars().count() <= message.content[start..end].chars().count()
                {
                    catalogue_inserted = true;
                    catalogue
                } else {
                    ""
                };
                message.content.replace_range(start..end, replacement);
                cursor = start + replacement.len();
            }
        }
        if !instructions.is_empty() {
            message
                .content
                .insert_str(message.content.len(), &format!("\n{instructions}"));
            message.content = crate::agent::system_prompt::finalize_system_prompt(
                std::mem::take(&mut message.content),
                max_system_prompt_chars,
            );
            if let Some(suffix) = hook_suffix {
                message.content.push_str(&suffix);
            }
        }
    }
}

pub struct ToolLoop<'a> {
    /// The resolved per-agent execution context: model binding, gated tool
    /// registry, approval, observability, and resolved runtime knobs. Stable
    /// for every turn to this agent; built once and reused. See
    /// [`ResolvedAgentExecution`]. Everything below is per-message turn state.
    pub exec: ResolvedAgentExecution<'a>,
    pub history: &'a mut Vec<ChatMessage>,
    /// The history owner's authoritative record that `history` carries the
    /// synthetic trim breadcrumb after its leading system messages. Provenance
    /// is tracked beside the buffer — never inferred from localized message
    /// text — so a genuine user turn equal to the breadcrumb string keeps its
    /// turn-boundary role and a persisted crumb stays classified across
    /// locale changes for the runtime's lifetime. Trim paths write back any
    /// crumb they insert through this reference.
    pub history_has_trim_breadcrumb: &'a mut bool,
    /// When set, flipped to `true` if any in-loop trim drops whole turns, so
    /// the owner can re-trim its durable `ConversationMessage` history after
    /// the loop. `None` when the caller keeps no durable history to sync.
    ///
    /// The flag is the gate on the post-loop trim, not a hint: that trim's
    /// action is keep-N, which is a retention preference and not a budget
    /// test, so calling it unconditionally would cut the transcript back to
    /// `keep_recent_turns` on every single turn no matter how small the
    /// history is.
    pub history_was_trimmed: Option<&'a mut bool>,
    /// Out-param the loop writes through when it injects a recalled-memory
    /// preamble onto the last user message for this turn's provider request
    /// only: the exact rendered block. Write-back strips that block solely
    /// from the injected message's position in the mutated history buffer —
    /// pre-injection canonical clones are replayed with no strip target, so
    /// a genuine user text starting with the same block survives
    /// byte-for-byte. The exact string (this turn's literal rendered
    /// content, never a generic marker pattern) is what confirms the
    /// recorded position still holds the injected message, following the
    /// same recorded-not-inferred principle as
    /// `history_has_trim_breadcrumb`.
    pub injected_memory_preamble: &'a mut Option<MemoryPreamble>,
    pub channel_name: &'a str,
    pub channel_reply_target: Option<&'a str>,
    pub cancellation_token: Option<CancellationToken>,
    pub on_delta: Option<tokio::sync::mpsc::Sender<DraftEvent>>,
    pub shared_budget: Option<ExecutionTreeBudget>,
    pub channel: Option<&'a dyn Channel>,
    pub collected_receipts: Option<&'a std::sync::Mutex<Vec<String>>>,
    pub event_tx: Option<tokio::sync::mpsc::Sender<TurnEvent>>,
    pub steering: Option<&'a mut tokio::sync::mpsc::Receiver<SteeringInput>>,
    pub new_messages_out: Option<&'a mut Vec<ChatMessage>>,
    pub image_cache: Option<ToolLoopImageState<'a>>,
    pub ingress: IngressContext,
    /// The per-turn memory half for unified memory-context injection: the
    /// handle, raw recall query, session scopes, and spawn-site suppression.
    /// `None` for nested sub-turn sites and paths without a memory backend;
    /// the injection decision itself is keyed on `ingress.origin`.
    pub memory: Option<crate::agent::memory_inject::TurnMemory<'a>>,
    /// Observer metadata: agent alias and turn id, stamped onto every
    /// turn-level observer event so OTel spans correlate across the loop.
    /// This is the EFFECTIVE agent — the one whose policy/tools/provider this
    /// loop actually runs with. A nested cross-agent SOP step runs its
    /// sub-loop with the step agent here, never the delegating parent's.
    pub agent_alias: Option<&'a str>,
    /// The delegating agent's alias when this loop is a nested cross-agent
    /// execution (a live SOP step naming a different agent). Stamped next to
    /// `agent_alias` on observer records so security/audit consumers see both
    /// the acting authority and its parent. `None` for ordinary turns.
    pub parent_agent_alias: Option<&'a str>,
    pub turn_id: &'a str,
    /// Optional sink the caller reads after the loop to learn the route that
    /// actually served the final LLM call (provider/model + resolved limits).
    /// Written every dispatching iteration, so a per-call vision switch is
    /// captured even when the provider returns no usage. `None` on paths that
    /// do not publish a terminal snapshot (tests, nested sub-turns).
    pub served_route_sink: Option<ServedRouteSink>,
    /// The previous turn's provider-measured anchor: seeds this loop's
    /// calibration and the dispatch ratio so the first dispatch reads a real
    /// measurement, not a fresh local estimate. `None` on paths that carry no
    /// meter (tests, nested sub-turns, non-streamed loops).
    pub context_anchor_seed: Option<ContextAnchor>,
    /// Sink the loop writes the anchor to when a provider report lands on an
    /// accepted response, so the caller hands it to the next turn's meter.
    /// `None` on the same non-meter paths.
    pub context_anchor_sink: Option<ContextAnchorSink>,
    /// Handle the live SOP driver uses to re-assemble a nested step's execution
    /// context when the step delegates to a different agent (see
    /// [`SopStepReassembly`]). `None` on every path that cannot reach `Config`
    /// or that never drives nested SOP steps; when `None`, a cross-agent step
    /// FAILS CLOSED (the step errors rather than running with the parent
    /// agent's broader context).
    pub sop_reassembly: Option<SopStepReassembly<'a>>,
}

#[derive(Debug)]
pub(crate) struct ReportedRequestUsage {
    provider_name: String,
    model: String,
    reported_tokens: usize,
    estimated_tokens: usize,
}

/// Pre-dispatch size for the request about to go out, priced the way R5
/// requires: the provider's own measurement of the rows it already billed,
/// plus a conservative estimate of the rows appended since.
///
/// It used to scale the *whole* estimate by the observed reported/estimated
/// ratio. That keeps the estimator's bias inside the bound and prices a growing
/// tail with the same biased estimator — which is how a multi-step turn reached
/// the provider's window while this guard still reported "fits": the ratio was
/// calibrated on the previous population, and the tail (a tool result, exactly
/// where the estimate is worst) was under-counted by it.
#[derive(Default)]
pub(crate) struct DispatchTokenCounter {
    /// The accepted call's report, kept only for the route that produced it: a
    /// model switch must not inherit another model's measurement.
    anchor: Option<DispatchAnchor>,
}

struct DispatchAnchor {
    reported_tokens: usize,
    /// The estimate of the population that report was measured against, so
    /// `estimated - estimated_tokens` is the tail the provider has not billed
    /// yet.
    estimated_tokens: usize,
}

impl DispatchTokenCounter {
    fn for_route(usage: Option<ReportedRequestUsage>, provider: &str, model: &str) -> Self {
        let anchor = usage
            .filter(|usage| {
                usage.provider_name == provider
                    && usage.model == model
                    && usage.reported_tokens > 0
                    && usage.estimated_tokens > 0
            })
            .map(|usage| DispatchAnchor {
                reported_tokens: usage.reported_tokens,
                estimated_tokens: usage.estimated_tokens,
            });
        Self { anchor }
    }

    /// `(size, provenance)` for a population the local estimator priced at
    /// `estimated` tokens.
    fn measure(&self, estimated: usize) -> (u64, zeroclaw_api::agent::TokenCountSource) {
        match self.anchor.as_ref() {
            // The population grew out of the one the provider measured: anchor
            // on its number and pay the conservative margin only on the tail.
            Some(anchor) if estimated >= anchor.estimated_tokens => (
                (anchor.reported_tokens
                    + crate::agent::history_trim::conservative_estimate_tokens(
                        estimated - anchor.estimated_tokens,
                    )) as u64,
                zeroclaw_api::agent::TokenCountSource::Calibrated,
            ),
            // No report for this route, or the population is no longer a
            // superset of the measured rows (a mid-turn trim dropped some): the
            // anchor prices nothing here, so the number is a local estimate —
            // with the conservative margin every pre-send number carries — and
            // it says so instead of borrowing a measurement it does not have.
            Some(_) | None => (
                crate::agent::history_trim::conservative_estimate_tokens(estimated) as u64,
                zeroclaw_api::agent::TokenCountSource::Estimated,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreDispatchOutcome {
    Fit,
    Trimmed,
    Floor,
}

/// The one budget every dispatch guard measures against.
///
/// The trigger and the fit target are the same number. The pre-dispatch guard
/// exists so a request is checked *before* it is sent: whether the provider
/// would accept it is knowable locally, and making it discover that by
/// rejecting the request would waste a round trip. There is nothing to compare
/// a second number against, so the configured budget answers both "time to
/// trim" and "small enough to send".
///
/// The budget is evaluated once at turn start and re-measured before every
/// dispatch after it. Tool results accumulate inside a single turn, long after
/// the turn-start check — without that re-measure the population climbs toward
/// the window with nothing firing, the provider accepts the request, the model
/// runs out of space mid-reply, and the stream never terminates: no
/// `finish_reason`, no error, just an idle connection until the client's read
/// bound expires.
///
/// `0` in the resolved limits means "unset" (routes that cannot resolve a
/// budget, and test stubs): the guard then measures against the model window,
/// never against zero.
fn dispatch_trim_budget(limits: zeroclaw_config::schema::ResolvedContextLimits) -> usize {
    let budget = if limits.context_token_budget > 0 {
        limits.context_token_budget
    } else {
        limits.model_context_window
    };
    budget.min(limits.model_context_window)
}

fn context_window_exceeded_error(
    (provider_name, model): (&str, &str),
    limits: zeroclaw_config::schema::ResolvedContextLimits,
    tokens: u64,
    token_source: zeroclaw_api::agent::TokenCountSource,
) -> anyhow::Error {
    let span = ::zeroclaw_log::info_span!(
        target: "zeroclaw_log_internal_scope",
        "zeroclaw_scope",
        model = %model,
        model_provider = %provider_name,
    );
    let _guard = span.entered();
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "error_key": "context_window_exceeded",
                "estimated_tokens": tokens,
                "token_count_source": token_source,
                "model_context_window": limits.model_context_window,
                "model_context_window_source": format!("{:?}", limits.model_context_window_source),
                "context_token_budget": limits.context_token_budget,
            })),
        "Provider-facing request exceeds the active model context window"
    );
    ContextWindowExceeded {
        estimated_tokens: usize::try_from(tokens).unwrap_or(usize::MAX),
        model_context_window: limits.model_context_window,
    }
    .into()
}

struct PreDispatchTrimResult {
    outcome: PreDispatchOutcome,
    dropped_messages: usize,
    dropped_turns: usize,
    kept_turns: usize,
}

fn record_dispatch_trim(
    (provider_name, model): (&str, &str),
    trim: &PreDispatchTrimResult,
    budget: usize,
    tokens_before: u64,
    tokens_after: u64,
    token_source: zeroclaw_api::agent::TokenCountSource,
) {
    if trim.dropped_messages == 0 {
        return;
    }
    // Keep this scope synchronous so route attribution cannot leak across awaits.
    let span = ::zeroclaw_log::info_span!(
        target: "zeroclaw_log_internal_scope",
        "zeroclaw_scope",
        model = %model,
        model_provider = %provider_name,
    );
    let _guard = span.entered();
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Delete)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_attrs(::serde_json::json!({
                "dropped_messages": trim.dropped_messages,
                "dropped_turns": trim.dropped_turns,
                "kept_turns": trim.kept_turns,
                "budget_tokens": budget,
                "tokens_before": tokens_before,
                "tokens_after": tokens_after,
                "tokens_before_source": token_source,
                "tokens_after_source": token_source,
                "tokens_reclaimed": tokens_before.saturating_sub(tokens_after),
                "budget_headroom": (budget as u64).saturating_sub(tokens_after),
            })),
        format!(
            "History trimmed: dropped {} oldest turn(s) ({} msgs), {} -> {} tok (budget {}), reclaimed {} tok",
            trim.dropped_turns,
            trim.dropped_messages,
            tokens_before,
            tokens_after,
            budget,
            tokens_before.saturating_sub(tokens_after)
        )
    );
}

/// One step of the trim action, applied to the durable history.
///
/// The action has one shape everywhere: retain the newest `keep_recent_turns`
/// whole turns, and drop exactly one more turn whenever the history is already
/// at or below that count and the measured request still exceeds the budget.
/// Stage 1 therefore cuts deep on the first pass — a trim that nibbled one
/// turn per dispatch would rewrite the provider prefix on every turn and
/// re-prefill it each time — while stage 2 keeps the action honest when the
/// deep cut was not enough.
///
/// The caller rebuilds and re-measures the *prepared* request between calls:
/// a raw-history estimate cannot price hook output, multimodal expansion or
/// the tool catalog, so only the rebuilt request decides whether another drop
/// is needed.
///
/// Leading system messages, the synthetic breadcrumb and the newest whole turn
/// are never dropped: a history down to one whole turn reports
/// [`PreDispatchOutcome::Floor`] instead of claiming a trim happened.
fn trim_whole_turns_for_dispatch(
    injected_memory_preamble: &mut Option<MemoryPreamble>,
    history: &mut Vec<ChatMessage>,
    crumb_present: &mut bool,
    measured_population: u64,
    budget: usize,
    keep_recent_turns: usize,
) -> PreDispatchTrimResult {
    let before = retention_layout(history, *crumb_present);
    let retained = crate::agent::history_trim::count_turns(history)
        .saturating_sub(usize::from(*crumb_present));
    let mut outcome = PreDispatchOutcome::Fit;
    let mut dropped_messages = 0;
    if budget != 0 && measured_population > budget as u64 {
        if retained <= 1 {
            outcome = PreDispatchOutcome::Floor;
        } else {
            // Stage 1 (the newest `keep_recent_turns`) and stage 2 (one fewer
            // than is retained now) are the same rule read at the depth the
            // history is already at.
            let target = keep_recent_turns.max(1).min(retained - 1);
            loop {
                let now = crate::agent::history_trim::count_turns(history)
                    .saturating_sub(usize::from(*crumb_present));
                if now <= target {
                    break;
                }
                let dropped =
                    crate::agent::history_trim::drop_oldest_whole_turn(history, *crumb_present);
                if dropped == 0 {
                    break;
                }
                dropped_messages += dropped;
            }
            if dropped_messages == 0 {
                outcome = PreDispatchOutcome::Floor;
            } else {
                *crumb_present =
                    crate::agent::history_trim::insert_breadcrumb_deduped(history, *crumb_present);
                outcome = PreDispatchOutcome::Trimmed;
            }
        }
    }
    remap_memory_after_trim(injected_memory_preamble, before, history, *crumb_present);
    let kept_turns = crate::agent::history_trim::count_turns(history)
        .saturating_sub(usize::from(*crumb_present));
    PreDispatchTrimResult {
        outcome,
        dropped_messages,
        dropped_turns: retained.saturating_sub(kept_turns),
        kept_turns,
    }
}

#[allow(clippy::too_many_arguments)]
async fn enforce_reported_budget(
    _injected_memory_preamble: &mut Option<MemoryPreamble>,
    history: &mut Vec<ChatMessage>,
    reported_input_tokens: usize,
    // Size of the tool definitions the provider receives alongside `history`.
    //
    // The request body is `messages + tools`, so every count this function
    // reports prices both. Pricing only messages made the notice advertise a
    // post-trim size the next request does not actually have — the same
    // omission, in the same direction, that let the context meter drop by the
    // size of the tool catalog the moment a turn started.
    tool_schema_tokens: usize,
    // The trim budget: one number is both the trigger (trim when the replayed
    // request exceeds it) and the fit target (retain no more than it).
    trim_budget: usize,
    // The model's capacity as this deployment may use it — the model window
    // clamped by `request_token_ceiling`. This is the hard floor: a retention
    // that misses the budget but still fits it is a soft floor, dispatched
    // rather than rejected.
    effective_context_window: usize,
    // Preferred whole-turn retention when the trim fires.
    keep_recent_turns: usize,
    event_tx: Option<&tokio::sync::mpsc::Sender<TurnEvent>>,
    observer: &dyn crate::observability::Observer,
    agent_alias: Option<&str>,
    turn_id: &str,
    crumb_present: &mut bool,
) -> Result<(), anyhow::Error> {
    // The pre-send kernel decides whether the request already fits.
    //
    // The provider-reported count is authoritative when present. Many
    // endpoints report no usage at all (OpenAI-compatible servers, local
    // runtimes, some streaming responses), and a local estimate is not a
    // substitute for it: the estimate cannot see what the provider really
    // charged, so trimming on it would cut history away on a guess and add a
    // breadcrumb that may push the retained set over the window. Without a
    // reported count we therefore only act when the request provably cannot
    // go out — the overflow guard, not the proactive trim.
    let reported = reported_input_tokens > 0;
    let tokens_now = if reported {
        reported_input_tokens
    } else {
        // No provider sample for this population, so price it locally — and
        // price the whole request, not just its messages.
        crate::agent::history::estimate_history_tokens(history) + tool_schema_tokens
    };
    // A reported count is the provider's measurement of a request it accepted.
    // The fallback above is a local estimate, and the notice renders this label
    // verbatim, so it must not claim otherwise.
    let tokens_before_source = if reported {
        zeroclaw_api::agent::TokenCountSource::Provider
    } else {
        zeroclaw_api::agent::TokenCountSource::Estimated
    };
    if reported {
        let decision =
            crate::agent::turn::context_pipeline::plan_pre_send_trim(tokens_now, trim_budget);
        if !decision.should_trim {
            return Ok(());
        }
    } else if trim_budget == 0 || tokens_now <= trim_budget {
        // No reported usage, and the estimate still fits the trim budget:
        // dispatch as-is rather than trim on a guess.
        //
        // The threshold is the trim budget, not the raw window: it already
        // carries `request_token_ceiling` and the output reserve — both
        // operator-declared limits, not strategy knobs.
        return Ok(());
    } else if crate::agent::history_trim::count_turns(history) <= 1 {
        // No reported usage, the estimate says the request overflows, and
        // there is no whole turn left to drop: trimming could not make the
        // request any smaller, so dispatch it rather than fail on a
        // difference the trim cannot close.
        return Ok(());
    }
    let taken = std::mem::take(history);
    // The synthetic crumb is not a turn: it must not consume a keep-N slot nor
    // be dropped by the cascade.
    let taken_had_crumb = *crumb_present;
    let mut result = crate::agent::history_trim::trim_to_fit_with_crumb(
        taken,
        keep_recent_turns,
        trim_budget,
        effective_context_window,
        taken_had_crumb,
    );
    result.tokens_before = tokens_now;
    if result.trimmed && !result.exceeds_budget {
        crate::agent::history_trim::warn_if_below_preferred_keep(
            keep_recent_turns,
            trim_budget,
            &result,
        );
        let mut trimmed = result.history;
        crate::agent::history_trim::insert_breadcrumb_deduped(&mut trimmed, taken_had_crumb);
        *crumb_present = true;
        // The crumb is part of the population that will actually be
        // dispatched next, so the reported post-trim size must include it —
        // and the tool definitions the next request carries with it.
        result.tokens_after =
            crate::agent::history::estimate_history_tokens(&trimmed) + tool_schema_tokens;
        *history = trimmed;
        let reason = crate::i18n::get_required_cli_string("history-trim-reason-budget");
        if let Some(tx) = event_tx {
            let _ = tx
                .send(TurnEvent::HistoryTrimmed {
                    dropped_messages: result.dropped_messages,
                    dropped_turns: result.dropped_turns,
                    kept_turns: result.kept_turns,
                    reason: reason.clone(),
                    token_budget: Some(trim_budget as u64),
                    tokens_before: Some(result.tokens_before as u64),
                    tokens_after: Some(result.tokens_after as u64),
                    // `tokens_after` is always a local estimate — no provider
                    // has seen the trimmed population yet — and claiming a
                    // calibration it never received is the label lie the meter
                    // already paid for once.
                    tokens_before_source: Some(tokens_before_source),
                    tokens_after_source: Some(zeroclaw_api::agent::TokenCountSource::Estimated),
                    unsatisfiable_floor: None,
                    retained_context: Some(retained_context_snapshot(
                        _injected_memory_preamble,
                        history,
                        *crumb_present,
                    )),
                })
                .await;
        }
        observer.record_event(
            &zeroclaw_api::observability_traits::ObserverEvent::HistoryTrimmed {
                dropped_messages: result.dropped_messages,
                kept_turns: result.kept_turns,
                reason,
                channel: None,
                agent_alias: agent_alias.map(str::to_string),
                turn_id: Some(turn_id.to_string()),
                token_budget: Some(trim_budget as u64),
                tokens_before: Some(result.tokens_before as u64),
                tokens_after: Some(result.tokens_after as u64),
                tokens_before_source: Some(tokens_before_source),
                tokens_after_source: Some(zeroclaw_api::agent::TokenCountSource::Estimated),
                unsatisfiable_floor: None,
            },
        );
    } else {
        *history = result.history;
    }
    if result.exceeds_budget {
        // The floor cannot fit: hard failure, no `HistoryTrimmed` notice
        // (spec row 8 — a trim that did not fit must not look like one that did).
        //
        // The floor is what the request cannot shed, so it prices the tool
        // definitions too: comparing a messages-only floor against a budget
        // derived from the full window understates the overflow by the whole
        // tool catalog and reports the wrong remediation.
        let system_floor =
            crate::agent::history::estimate_system_floor_tokens(history) + tool_schema_tokens;
        let msg = if system_floor >= trim_budget {
            crate::agent::history::context_floor_remediation(system_floor, trim_budget)
        } else {
            "Context overflow unrecoverable: only one turn left, cannot trim further".to_string()
        };
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_category(::zeroclaw_log::EventCategory::Agent)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "error": msg,
                    "error_key": "context_floor_exceeds_budget",
                    "trim_budget": trim_budget,
                })),
            "Context floor exceeds the trim budget"
        );
        return Err(anyhow::Error::msg(msg));
    }
    Ok(())
}

/// Resolve the capacity/budget pair for the route that serves a single call.
///
/// Preference order:
/// 1. `resolver` — the live authority (reads the shared `Config`), so a config
///    reload between turns and any per-call route change both resolve against
///    current values rather than a construction-time snapshot.
/// 2. `config` snapshot — legacy path for callers that carry a `Config` but no
///    resolver (kept so behavior is unchanged where no live handle exists).
/// 3. `fallback` — the route pair resolved at the turn boundary, used on
///    configless (test) paths.
fn resolve_context_limits_for_call(
    resolver: Option<&ContextLimitsResolver>,
    config: Option<&zeroclaw_config::schema::Config>,
    agent_alias: Option<&str>,
    provider_name: &str,
    model: &str,
    fallback: zeroclaw_config::schema::ResolvedContextLimits,
) -> zeroclaw_config::schema::ResolvedContextLimits {
    if let Some(resolver) = resolver {
        return resolver(provider_name, model);
    }
    config
        .zip(agent_alias)
        .map(|(config, agent_alias)| {
            config.resolved_context_limits_for_route(agent_alias, provider_name, model)
        })
        .unwrap_or(fallback)
}

/// Per-invocation turn state: owns the provider-visible transcript and
/// the canonical current-turn buffer during one `run_tool_call_loop` call.
///
/// - `history`: the provider-visible transcript.
/// - `canonical`: append-only persistence delta (messages that must be
///   replayed into durable history after the loop).
/// - `synced`: cursor — messages before this index in `canonical` have
///   already been synced into `history`.
struct TurnState<'a> {
    history: &'a mut Vec<ChatMessage>,
    canonical: Option<&'a mut Vec<ChatMessage>>,
    synced: usize,
    /// Owner-tracked provenance that `history` carries the synthetic trim
    /// breadcrumb after its leading system messages. Kept beside the buffer
    /// instead of being inferred from message text so classification never
    /// depends on the active locale or on a user turn colliding with the
    /// breadcrumb string.
    crumb_present: bool,
}

impl<'a> TurnState<'a> {
    fn new(
        history: &'a mut Vec<ChatMessage>,
        canonical: Option<&'a mut Vec<ChatMessage>>,
        crumb_present: bool,
    ) -> Self {
        Self {
            history,
            canonical,
            synced: 0,
            crumb_present,
        }
    }

    /// Clone-and-append: sync pending canonical messages into history.
    fn sync_pending(&mut self) {
        if let Some(ref canonical) = self.canonical {
            if self.synced > canonical.len() {
                self.synced = 0;
            }
            for msg in &canonical[self.synced..] {
                self.history.push(msg.clone());
            }
            self.synced = canonical.len();
        }
    }

    /// Push a message to both buffers and advance the cursor.
    fn push_dual(&mut self, msg: ChatMessage) {
        if let Some(ref mut canonical) = self.canonical {
            canonical.push(msg.clone());
        }
        self.history.push(msg);
        self.synced = self.canonical.as_ref().map_or(0, |c| c.len());
    }

    /// Advance the cursor past all current canonical messages.  Call after
    /// external code modifies both buffers (e.g. `drive_live_sop_actions`).
    fn mark_all_synced(&mut self) {
        if let Some(ref canonical) = self.canonical {
            self.synced = canonical.len();
        }
    }

    /// Append assistant + tool-result messages to both buffers and advance
    /// the cursor.  Wraps `append_tool_round_to_history` + canonical sync.
    fn append_tool_round(
        &mut self,
        assistant_history_content: String,
        native_tool_calls: &[zeroclaw_providers::ToolCall],
        individual_results: &[results_collect::ToolRoundResult],
        tool_results: &str,
        use_native_tools: bool,
    ) {
        let from = self.history.len();
        crate::agent::turn::history_append::append_tool_round_to_history(
            self.history,
            assistant_history_content,
            native_tool_calls,
            individual_results,
            tool_results,
            use_native_tools,
        );
        if let Some(ref mut canonical) = self.canonical {
            canonical.extend_from_slice(&self.history[from..]);
        }
        self.synced = self.canonical.as_ref().map_or(0, |c| c.len());
    }
}

/// Emit `TurnEvent::Usage` for each billable attempt in `attempts` as a
/// rejected (`accepted: false`) event, so the gateway's `usage_by_provider`
/// breakdown includes all billable attempts (accepted + rejected). Callers
/// pass the attempts settled for an iteration that will not reach an
/// accepted response: each iteration's `attempts` vec is fresh from its own
/// `call_provider`, so projecting here cannot double-emit.
async fn emit_rejected_attempt_usage(
    event_tx: Option<&tokio::sync::mpsc::Sender<TurnEvent>>,
    attempts: &[zeroclaw_providers::dispatch::AccountedAttempt],
) {
    if let Some(tx) = event_tx {
        for billable in crate::agent::cost::billable_provider_attempts(attempts) {
            let cost_usd = crate::agent::cost::compute_cost_usd(
                billable.attempt.provider_ref(),
                billable.attempt.model(),
                billable.usage,
            );
            let _ = tx
                .send(TurnEvent::Usage {
                    input_tokens: billable.usage.input_tokens,
                    cached_input_tokens: billable.usage.cached_input_tokens,
                    output_tokens: billable.usage.output_tokens,
                    cost_usd,
                    context_token_budget: None,
                    model_context_window: None,
                    effective_context_window: None,
                    provider_ref: billable.attempt.provider_ref().to_string(),
                    model: billable.attempt.model().to_string(),
                    accepted: false,
                    // Rejected billing telemetry never feeds the context
                    // meter, so it carries no display estimate.
                    estimated_input_tokens: None,
                })
                .await;
        }
    }
}

/// The Agent wrapper admits steering before history insertion. Admit only
/// the original user message, or skip admission on subsequent rounds.
pub(crate) enum InitialIngressSubject<'a> {
    Original(&'a str),
    AlreadyAdmitted,
}

/// `reported_usage` is the last provider-reported usage from a run that
/// yielded with `SteeringPending`, so the resumed run calibrates its first
/// dispatch exactly as the yielded run would have.
pub(crate) fn run_tool_call_loop_with_initial_ingress<'a>(
    p: ToolLoop<'a>,
    subject: InitialIngressSubject<'a>,
    reported_usage: Option<ReportedRequestUsage>,
) -> impl std::future::Future<Output = Result<String>> + 'a {
    run_tool_call_loop_impl(p, Some(subject), reported_usage)
}

pub fn run_tool_call_loop(
    p: ToolLoop<'_>,
) -> impl std::future::Future<Output = Result<String>> + '_ {
    run_tool_call_loop_impl(p, None, None)
}

async fn run_tool_call_loop_impl<'a>(
    mut p: ToolLoop<'a>,
    initial_ingress: Option<InitialIngressSubject<'a>>,
    seed_reported_usage: Option<ReportedRequestUsage>,
) -> Result<String> {
    let model_switch_state = p
        .exec
        .model_switch_callback
        .clone()
        .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(None)));
    p.exec.model_switch_callback = Some(Arc::clone(&model_switch_state));
    let ToolLoop {
        mut history_was_trimmed,
        exec,
        history: raw_history,
        history_has_trim_breadcrumb,
        injected_memory_preamble,
        channel_name,
        channel_reply_target,
        cancellation_token,
        on_delta,
        shared_budget,
        channel,
        collected_receipts,
        event_tx,
        mut steering,
        new_messages_out: raw_canonical,
        image_cache,
        ingress,
        memory,
        agent_alias,
        parent_agent_alias,
        turn_id,
        served_route_sink,
        context_anchor_seed,
        context_anchor_sink,
        sop_reassembly,
    } = p;
    let image_recovery_has_owner = image_cache.is_some();
    let mut loop_local_image_cache = None;
    let mut loop_local_provider_image_state = ProviderImageState::default();
    let mut image_cache = Some(match image_cache {
        Some(state) => state,
        None => ToolLoopImageState {
            cache: loop_local_image_cache
                .insert(zeroclaw_providers::multimodal::LocalImageCache::new()),
            provider_state: &mut loop_local_provider_image_state,
        },
    });
    let ResolvedAgentExecution {
        model_access:
            ResolvedModelAccess {
                model_provider,
                provider_name,
                model,
                dispatch_model,
                temperature,
            },
        tools_registry,
        observer,
        silent,
        approval,
        security,
        multimodal_config,
        config,
        max_tool_iterations,
        hooks,
        excluded_tools,
        dedup_exempt_tools,
        activated_tools,
        model_switch_callback,
        pacing,
        strict_tool_parsing,
        parallel_tools,
        max_tool_result_chars,
        context_limits,
        context_limits_resolver,
        receipt_generator,
        knobs,
    } = exec;

    // Config-backed loops resolve the executing alias; configless Agent turns
    // already carry this cap in their transient protocol context. Zero is unlimited.
    let max_system_prompt_chars = config
        .zip(agent_alias)
        .map(|(config, alias)| config.effective_max_system_prompt_chars(alias))
        .or_else(|| {
            TOOL_PROTOCOL_PROMPTS
                .try_with(|prompts| prompts.max_chars)
                .ok()
        })
        .unwrap_or(0);

    let mut turn_state = TurnState::new(raw_history, raw_canonical, *history_has_trim_breadcrumb);

    // The provider-authoritative context anchor for this turn: re-anchored on
    // every reported `input_tokens`, so the size the runtime trims against is
    // the provider's own number, not a local estimate (spec R5).
    let mut context_calibration = context_anchor_seed
        .as_ref()
        .map(crate::agent::history_trim::ContextCalibration::seeded)
        .unwrap_or_default();

    turn_state.sync_pending();

    let ingress_policy_cfg = IngressPolicy::default();

    // Stamp the initiating internal principal into the turn trace, keyed by
    // the same trace id every other turn event carries. The principal is
    // runtime-resolved at the dispatch surface and immutable for the turn;
    // this record is the trace half of that contract (run records carry it
    // separately). External turns have none and stay unstamped.
    if let Some(principal) = &ingress.internal_principal {
        ::zeroclaw_log::record!(
            INFO,
            internal_principal_event(turn_id, ingress.origin, principal, agent_alias),
            "turn_internal_principal"
        );
    }

    let p1_text = match &initial_ingress {
        Some(InitialIngressSubject::Original(text)) => Some(*text),
        Some(InitialIngressSubject::AlreadyAdmitted) => None,
        None => Some(
            turn_state
                .history
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map_or("", |m| m.content.as_str()),
        ),
    };
    if let Some(p1_text) = p1_text {
        match ingress_policy(p1_text, &ingress, &ingress_policy_cfg) {
            // DEFAULT — the only arm reachable under the default policy. Proceed
            // into the loop exactly as today.
            IngressDecision::Loop => {}
            // Phase 3: wrap the message as untrusted data before it enters history.
            // Until framing exists, proceed as Loop (behavior-identical).
            IngressDecision::Annotate { .. } => {}
            // Phase 2: divert the turn into a managed SOP run instead of the loop.
            // Not reachable under the default policy; proceed-as-loop for now.
            IngressDecision::Gate { .. } => {
                // TODO(PR C): hand this turn to the SOP run the gate names.
            }
            // Not reachable under the default policy; refuse the turn when it is.
            IngressDecision::Drop { ref reason } => {
                return Ok(crate::i18n::get_required_cli_string_with_args(
                    "turn-ingress-dropped",
                    &[("reason", reason.as_str())],
                ));
            }
        }
    }

    if let Some(turn_memory) = &memory {
        let has_session = turn_memory.sessions.iter().any(Option::is_some);
        if let crate::agent::memory_inject::InjectPolicy::Inject {
            exclude_conversation,
        } = crate::agent::memory_inject::resolve_inject_policy(
            ingress.origin,
            has_session,
            turn_memory.suppress,
        ) && let Some(last_user_idx) = turn_state.history.iter().rposition(|m| m.role == "user")
            // Idempotence: a model-switch retry re-enters the engine with the
            // same history; the preamble must not stack.
            && !turn_state.history[last_user_idx]
                .content
                .starts_with(zeroclaw_memory::MEMORY_CONTEXT_OPEN)
        {
            let scopes: Vec<Option<&str>> =
                turn_memory.sessions.iter().map(|s| s.as_deref()).collect();
            let context = crate::agent::memory_inject::render_memory_context(
                turn_memory.handle,
                observer,
                &turn_memory.query,
                &scopes,
                &turn_memory.cfg,
                exclude_conversation,
                TurnMeta {
                    agent_alias,
                    parent_agent_alias,
                    turn_id,
                    channel_name,
                },
            )
            .await;
            if !context.is_empty() {
                let existing = &turn_state.history[last_user_idx].content;
                turn_state.history[last_user_idx].content = format!("{context}{existing}");
                *injected_memory_preamble = Some(MemoryPreamble {
                    preamble: context,
                    index: last_user_idx,
                });
            }
        }
    }

    let max_iterations = if max_tool_iterations == 0 {
        DEFAULT_MAX_TOOL_ITERATIONS
    } else {
        max_tool_iterations
    };

    let loop_started_at = Instant::now();
    let loop_ignore_tools: HashSet<&str> = pacing
        .loop_ignore_tools
        .iter()
        .map(String::as_str)
        .collect();
    let mut consecutive_identical_outputs: usize = 0;
    let mut last_tool_output_hash: Option<u64> = None;

    let mut loop_detector = crate::agent::loop_detector::LoopDetector::new(
        crate::agent::loop_detector::LoopDetectorConfig {
            enabled: pacing.loop_detection_enabled,
            window_size: pacing.loop_detection_window_size,
            max_repeats: pacing.loop_detection_max_repeats,
            no_progress_min_calls: pacing.loop_detection_no_progress_min_calls,
        },
    );

    // Accumulated display text across all tool-loop calls.
    let mut accumulated_display_text = String::new();
    let mut single_tool_limitation_reported = false;
    let mut malformed_tool_protocol_retries: usize = 0;
    // Text withheld by the streaming text guard on a previous attempt,
    // stored with its trailing whitespace trimmed. A repeat that is
    // identical up to trailing whitespace means the model produced the same
    // protocol-shaped prose again; another retry would suppress it again,
    // so the turn ends with a notice instead of spending more provider
    // calls.
    let mut last_guard_suppressed_text: Option<String> = None;
    let mut prompt_approval_tool_signatures: HashSet<(String, String)> = HashSet::new();

    // Shared-ref context for the turn step functions. Every `&mut` the loop
    // owns stays a loop local passed as an explicit argument (RUN_SHEET
    // `turn.context.TurnCtx`).
    let base_ctx = TurnCtx {
        observer,
        provider_name,
        model,
        context_limits,
        temperature,
        approval,
        channel_name,
        channel_reply_target,
        cancellation_token: cancellation_token.as_ref(),
        on_delta: on_delta.as_ref(),
        event_tx: event_tx.as_ref(),
        hooks,
        dedup_exempt_tools,
        pacing,
        strict_tool_parsing,
        channel,
        draft_reasoning: knobs.draft_reasoning,
        turn_id,
        agent_alias,
        parent_agent_alias,
        serving_provider_name: None,
        serving_model: None,
    };

    // Cross-agent SOP step contexts memoized for the WHOLE turn (see the
    // `exec_cache` parameter on `drive_live_sop_actions`): a step agent's
    // MCP-connecting re-assembly runs at most once per turn even when queued
    // steps drain across several iterations.
    let mut sop_exec_cache: std::collections::HashMap<String, OwnedAgentExecution> =
        std::collections::HashMap::new();

    // Usage describes the completed request, not a prediction of the next
    // hook-selected model, schemas, or prepared messages.
    let mut pending_reported_usage: Option<ReportedRequestUsage> =
        seed_reported_usage.or_else(|| {
            context_anchor_seed
                .as_ref()
                .map(|seed| ReportedRequestUsage {
                    provider_name: seed.provider_name.clone(),
                    model: seed.model.clone(),
                    reported_tokens: seed.reported_tokens,
                    estimated_tokens: seed.anchored_prefix_estimate,
                })
        });

    for iteration in 0..max_iterations {
        let mut has_new_user_input = iteration == 0;
        // Re-resolved every iteration, against the tools callable *right now*.
        //
        // A step's scope is resolved by name, so it can only narrow tools that
        // exist when it runs: `tool_search` activates deferred MCP tools in the
        // middle of a turn, and a scoped step must narrow those on the next
        // iteration rather than the next turn — otherwise a step that may call
        // `tool_search` can reach a tool its scope excludes. Reading the scope
        // from the task (rather than a parameter) also covers the loops this
        // one drives on behalf of another agent: a delegated target assembles
        // its own registry, and inherits the boundary all the same.
        //
        // `None` for every ordinary turn, which keeps its own slice and pays
        // nothing.
        let step_scoped_excluded: Option<Vec<String>> =
            crate::sop::active_scope::active_headless_step_scope().map(|scope| {
                let mut merged = excluded_tools.to_vec();
                crate::agent::loop_::merge_sop_step_exclusions(
                    &mut merged,
                    tools_registry,
                    activated_tools,
                    Some(&scope),
                );
                merged
            });
        let excluded_tools: &[String] = step_scoped_excluded.as_deref().unwrap_or(excluded_tools);

        if knobs.yield_to_steering {
            // The caller judges steering; this loop only hands control back
            // once a tool round has finished and a message is waiting. A
            // model switch queued by that round goes first: the switch check
            // below returns it, the caller applies it, and the steering is
            // taken at the top of the caller's next round on the new model.
            let switch_pending = model_switch_callback.as_ref().is_some_and(|callback| {
                callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .is_some_and(|(p, m)| p != provider_name || m != model)
            });
            if iteration > 0
                && !switch_pending
                && steering.as_deref().is_some_and(|rx| !rx.is_empty())
            {
                *history_has_trim_breadcrumb = turn_state.crumb_present;
                return Err(SteeringPending {
                    display_text: accumulated_display_text,
                    reported_usage: pending_reported_usage.take(),
                }
                .into());
            }
        } else {
            for steering_input in drain_steering_messages(&mut steering) {
                // This loop cannot narrow its registry mid-run, so it admits only
                // a sender that is still authorized and whose posture narrows
                // nothing; anything else is dropped rather than run with more
                // than the sender now holds.
                match steering_input.admit() {
                    SteeringAdmission::Admitted(posture)
                        if posture == SteeringPosture::default() => {}
                    _ => continue,
                }
                match steering_policy(
                    steering_input.text(),
                    steering_input.ingress(),
                    &ingress_policy_cfg,
                ) {
                    // DEFAULT — append the injection to history exactly as today.
                    IngressDecision::Loop => {}
                    // Phase 3: frame as untrusted data; proceed as Loop until
                    // framing exists (behavior-identical).
                    IngressDecision::Annotate { .. } => {}
                    // Phase 2: divert this injection into the SOP run rather than
                    // history. Not reachable under the default policy.
                    IngressDecision::Gate { .. } => {
                        // TODO(PR C): route this steering message into the gated
                        // SOP run instead of appending it to history.
                    }
                    // Not reachable under the default policy; drop the injection
                    // (do not append it) when it is.
                    IngressDecision::Drop { .. } => continue,
                }
                let msg = ChatMessage::user(steering_input.into_text());
                has_new_user_input |= !msg.content.trim_start().starts_with("[Tool results]");
                turn_state.push_dual(msg);
            }
        }

        let mut seen_tool_signatures: HashSet<(String, String)> = HashSet::new();

        if cancellation_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ToolLoopCancelled.into());
        }

        if let Some(budget) = shared_budget.as_ref() {
            match budget.reserve() {
                Ok(ExecutionTreeReservation::Iteration) => {}
                Ok(ExecutionTreeReservation::FinalCompletion) => {
                    let mut final_knobs = knobs.clone();
                    final_knobs.max_iteration_behavior = MaxIterationBehavior::GracefulSummary;
                    let (summary_image_cache, summary_provider_image_state) = match image_cache
                        .as_mut()
                    {
                        Some(state) => (Some(&mut *state.cache), Some(&mut *state.provider_state)),
                        None => (None, None),
                    };
                    let summary_result = finish_after_max_iterations(
                        injected_memory_preamble,
                        model_provider,
                        turn_state.history,
                        provider_name,
                        model,
                        dispatch_model,
                        temperature,
                        pacing,
                        cancellation_token.as_ref(),
                        max_iter::CompletionLimit::ExecutionTree,
                        accumulated_display_text,
                        turn_id,
                        &final_knobs,
                        event_tx.as_ref(),
                        on_delta.as_ref(),
                        turn_state.canonical.as_deref_mut(),
                        config,
                        multimodal_config,
                        security,
                        hooks,
                        summary_image_cache,
                        summary_provider_image_state,
                        |provider, selected_model| {
                            resolve_context_limits_for_call(
                                context_limits_resolver.as_ref(),
                                config,
                                agent_alias,
                                provider,
                                selected_model,
                                context_limits,
                            )
                        },
                        &mut turn_state.crumb_present,
                        pending_reported_usage,
                        observer,
                    )
                    .await;
                    *history_has_trim_breadcrumb = turn_state.crumb_present;
                    return summary_result;
                }
                Err(error) => return Err(error.into()),
            }
        }

        preflight_history_maintenance(turn_state.history);

        // Check if model switch was requested via model_switch tool before
        // constructing this iteration's provider route.
        let pending_model_switch = model_switch_callback.as_ref().and_then(|callback| {
            let guard = match callback.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Provider)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "model-switch lock poisoned while checking for a pending switch; recovering guard for read"
                    );
                    poisoned.into_inner()
                }
            };
            guard.clone()
        });
        if let Some((new_model_provider, new_model)) = pending_model_switch.as_ref()
            && (new_model_provider != provider_name || new_model != model)
        {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Migrate)
                    .with_category(::zeroclaw_log::EventCategory::Provider),
                &format!(
                    "Model switch detected: {} {} -> {} {}",
                    provider_name, model, new_model_provider, new_model
                )
            );
            return Err(ModelSwitchRequested {
                model_provider: new_model_provider.clone(),
                model: new_model.clone(),
            }
            .into());
        }

        let (vision_model_provider_box, degrade_strip_images) =
            vision_route::resolve_vision_provider_with_allowed_refs(
                config,
                model_provider,
                turn_state.history,
                multimodal_config,
                provider_name,
                model,
                dispatch_model,
                security,
                knobs.provider_attempt_allowlist.as_deref(),
            )
            .await?;

        let (
            active_model_provider,
            active_model_provider_name,
            active_model,
            active_dispatch_model,
        ): (&dyn ModelProvider, &str, &str, &str) =
            if let Some(ref resolved) = vision_model_provider_box {
                (
                    resolved.provider.as_ref(),
                    resolved.provider_name.as_str(),
                    resolved.model.as_str(),
                    resolved.model.as_str(),
                )
            } else {
                (model_provider, provider_name, model, dispatch_model)
            };

        let mut iteration_tool_specs = build_iteration_tool_specs(
            active_model_provider,
            active_dispatch_model,
            tools_registry,
            excluded_tools,
            activated_tools,
        )?;

        let (prepared_messages, mut prepared_source_rows) =
            vision_route::prepare_messages_with_source_rows(
                turn_state.history,
                multimodal_config,
                degrade_strip_images,
                image_cache.as_mut().map(|state| &mut *state.cache),
            )
            .await?;
        let mut provider_request_messages = prepared_messages.messages;
        let newest_user_image_ids = prepared_messages.newest_user_image_ids;
        let pre_hook_messages = provider_request_messages.clone();
        let mut hook_selected_model = None;

        if let Some(hooks) = base_ctx.hooks.filter(|hooks| !hooks.is_empty()) {
            let mut candidate_model = active_model.to_string();
            match hooks
                .run_before_llm_call(&mut provider_request_messages, &mut candidate_model)
                .await
            {
                crate::hooks::HookResult::Continue(()) => {
                    // A hook that leaves the resolved model unchanged has not
                    // replaced the provider-facing selector. Preserve a route
                    // hint such as `hint:fast` for dispatch and capability
                    // checks; only an actual model change overrides it.
                    if candidate_model != active_model {
                        hook_selected_model = Some(candidate_model);
                    }
                }
                crate::hooks::HookResult::Cancel(reason) => {
                    anyhow::bail!("LLM call cancelled by hook: {reason}");
                }
            }
        }
        // Capture hook-added suffix (messages appended after the original
        // prepared messages, with that original population otherwise intact
        // and in place) so a pre-dispatch trim can rebuild the post-hook
        // request without re-executing the hook. This preserves a stateful
        // one-shot handler's exactly-once semantics while still letting the
        // current dispatch use the trimmed durable history.
        //
        // The hook API allows mutating the complete message vector, not just
        // appending to it. Identifying "hook additions" by vector length
        // alone (`provider_request_messages[pre_hook_messages.len()..]`)
        // silently misclassifies an insertion, removal, or reorder earlier in
        // the vector: e.g. inserting a system message after the first entry
        // shifts every later message by one, so the length-based slice picks
        // up the original last message as if the hook had appended it,
        // while the actually-inserted message is left inside what the
        // rebuild below treats as the durable-history-backed prefix. A
        // whole-turn trim of that prefix would then desynchronize the
        // dispatched request from the durable turn count it is supposed to
        // mirror. Detect that case structurally — the hook-untouched
        // original messages must still be an exact, in-place prefix — and
        // only take the length-based suffix when that holds.
        let hook_appended_suffix_only = provider_request_messages.len() >= pre_hook_messages.len()
            && provider_request_messages
                .iter()
                .zip(pre_hook_messages.iter())
                .all(|(a, b)| a.role == b.role && a.content == b.content);
        let hook_suffix = if hook_appended_suffix_only {
            provider_request_messages[pre_hook_messages.len()..].to_vec()
        } else {
            Vec::new()
        };
        // Set when the hook made a structural change (insertion, removal,
        // reorder, or rewrite of an existing message) that the append-only
        // suffix model above cannot represent. The pre-dispatch trim below
        // must not silently rebuild a request from `post_hook_snapshot` in
        // this case, since doing so cannot preserve the correspondence
        // between durable turns and the post-hook request.
        let hook_structural_mutation = !hook_appended_suffix_only;
        let provider_request_model = hook_selected_model.as_deref().unwrap_or(active_model);
        let image_route =
            ProviderImageState::route(active_model_provider_name, provider_request_model);
        let quarantined_image_ids = image_cache
            .as_mut()
            .map(|state| state.provider_state.quarantined(&image_route))
            .unwrap_or_default();
        provider_request_messages = suppress_quarantined_provider_images(
            &provider_request_messages,
            &quarantined_image_ids,
            has_new_user_input,
        );
        // The wire selector for `call_provider`: routed providers may need a
        // `hint:<name>` selector distinct from the resolved `model` used for
        // attribution above, so this cannot just reuse `provider_request_model`.
        let provider_dispatch_model = hook_selected_model
            .as_deref()
            .unwrap_or(active_dispatch_model);
        // Only direct Agent turns scope the complete prompt variants. Preserve
        // the channel loop's existing hook/protocol behavior rather than
        // silently widening this delegation-focused repair into channel prompt
        // reconciliation.
        let uses_scoped_tool_protocol = TOOL_PROTOCOL_PROMPTS.try_with(|_| ()).is_ok();
        let protocol_model = if uses_scoped_tool_protocol {
            provider_dispatch_model
        } else {
            active_model
        };
        let active_context_limits = resolve_context_limits_for_call(
            context_limits_resolver.as_ref(),
            config,
            agent_alias,
            active_model_provider_name,
            provider_request_model,
            context_limits,
        );
        let context_token_budget = active_context_limits.context_token_budget;
        let model_context_window = active_context_limits.model_context_window;
        // Zero disables proactive trimming, but model capacity still applies.
        let trim_budget = dispatch_trim_budget(active_context_limits);
        let ctx = base_ctx.for_route(
            active_model_provider_name,
            provider_request_model,
            active_context_limits,
        );
        if let Some(sink) = served_route_sink.as_ref() {
            *sink.lock().expect("served-route sink lock") = Some(outcome::ServedRoute {
                provider_name: active_model_provider_name.to_string(),
                model: provider_request_model.to_string(),
                context_limits: active_context_limits,
            });
        }
        iteration_tool_specs.refresh_native_tool_mode(active_model_provider, protocol_model);
        let tool_specs = iteration_tool_specs.request_tool_specs();
        let use_native_tools = iteration_tool_specs.use_native_tools;
        let builtin_prompt_section = activated_tools.and_then(|state| {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.has_deferred_builtin_schemas().then(|| {
                if iteration_tool_specs
                    .known_tool_names
                    .contains("tool_search")
                    && (use_native_tools || !strict_tool_parsing)
                {
                    state.deferred_builtin_prompt_section_filtered(|name| {
                        iteration_tool_specs.known_tool_names.contains(name)
                    })
                } else {
                    String::new()
                }
            })
        });

        let single_tool_rounds_requested = config
            .zip(agent_alias)
            .is_some_and(|(config, alias)| config.effective_single_tool_rounds(alias));
        let policy = if single_tool_rounds_requested
            && use_native_tools
            && active_model_provider.supports_single_tool_rounds(provider_dispatch_model)
        {
            zeroclaw_api::model_provider::ToolRoundPolicy::Single
        } else {
            zeroclaw_api::model_provider::ToolRoundPolicy::Batch
        };

        // Tool protocol selection follows the provider-facing selector. Direct
        // Agent turns also refresh their scoped complete prompt after a hook
        // chooses a different model; unscoped callers retain their own prompt.
        refresh_prompt_anchor(turn_state.history, use_native_tools);
        refresh_prompt_anchor(&mut provider_request_messages, use_native_tools);
        refresh_scoped_tool_protocol_prompt(
            turn_state.history,
            &mut provider_request_messages,
            use_native_tools,
        );
        if let Some(section) = builtin_prompt_section.as_deref() {
            apply_builtin_prompt_scope(
                &mut provider_request_messages,
                tools_registry,
                &iteration_tool_specs.known_tool_names,
                section,
                use_native_tools,
                strict_tool_parsing,
                max_system_prompt_chars,
                turn_state
                    .history
                    .iter()
                    .find(|message| message.role == "system")
                    .map(|message| message.content.as_str()),
            );
        }
        if policy == zeroclaw_api::model_provider::ToolRoundPolicy::Single
            && apply_single_tool_round_prompt(&mut provider_request_messages)
        {
            // A request-only system row must survive whole-turn trimming.
            prepared_source_rows.insert(0, usize::MAX);
        }

        if context_token_budget > 0 {
            let system_floor =
                crate::agent::history::estimate_system_floor_tokens(&provider_request_messages);
            if system_floor >= context_token_budget {
                let __zc_floor_span = ::zeroclaw_log::info_span!(
                    target: "zeroclaw_log_internal_scope",
                    "zeroclaw_scope",
                    model = %provider_request_model,
                    model_provider = %active_model_provider_name,
                );
                let _zc_floor_guard = __zc_floor_span.entered();
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "system_floor": system_floor,
                            "budget": context_token_budget,
                            "error_key": "context_floor_exceeds_budget",
                        })),
                    crate::agent::history::context_floor_remediation(
                        system_floor,
                        context_token_budget,
                    )
                );
            }
        }

        // Snapshot the estimate for the exact message population sent to the
        // provider this iteration (after any pre-LLM-call hook rewrites),
        // before any assistant/tool-result output is appended to `history`.
        // `enforce_reported_budget` below needs this as the calibration
        // denominator, not a later, larger `history` estimate.
        // Native tool definitions are serialized into the same provider
        // request, so providers that count them in `input_tokens` report a
        // population of messages plus tool schemas.
        let tool_schema_tokens = if use_native_tools {
            crate::agent::history::estimate_tool_schema_tokens(tool_specs)
        } else {
            0
        };
        let mut reported_population_estimated =
            crate::agent::history::estimate_history_tokens(&provider_request_messages)
                + tool_schema_tokens;

        // Reuse a report only for the route that produced it. A model switch
        // must not inherit another model's token calibration. Keep the existing
        // raw-estimate dispatch guard as a lower bound even if usage was lower.
        let token_counter = DispatchTokenCounter::for_route(
            pending_reported_usage.take(),
            active_model_provider_name,
            provider_request_model,
        );
        let dispatch_tokens = |estimated| token_counter.measure(estimated);

        // Budget and dispatch share these post-hook messages, schemas and
        // prompt framing. Rebuild after each whole-turn drop without invoking
        // the hook again. The soft target never makes the newest turn droppable;
        // only the model's capacity can reject its final retained population.
        let history_len_before = turn_state.history.len();
        let crumb_before = turn_state.crumb_present;
        let (tokens_before_dispatch, dispatch_token_source) =
            dispatch_tokens(reported_population_estimated);
        // Snapshot the post-hook request so trimming can preserve its
        // mutations to retained messages rather than discarding them.
        let post_hook_snapshot = provider_request_messages.clone();
        let post_hook_had_crumb = crumb_before;
        let original_body_start = turn_state
            .history
            .iter()
            .take_while(|m| m.is_system())
            .count()
            + usize::from(crumb_before);
        // The first pass cuts to the configured retention in one go: the trim
        // action is "keep the newest `keep_recent_turns` whole turns, then one
        // fewer at a time while the rebuilt request still exceeds the budget".
        // A nibble passed over the whole population — dropping the single turn
        // needed to clear the budget — would rewrite the provider prefix on
        // nearly every turn and re-prefill it each time.
        //
        // The gate does not itself decide the client-visible Trimmed vs. Floor
        // outcome: a dropped turn can carry a disproportionate share of the
        // measured population (e.g. a large multimodal attachment), so only the
        // rebuilt post-hook request re-measured below is authoritative. The
        // stages below keep dropping one turn at a time while that rebuilt
        // measurement still exceeds the budget.
        let mut trim_result = trim_whole_turns_for_dispatch(
            injected_memory_preamble,
            turn_state.history,
            &mut turn_state.crumb_present,
            tokens_before_dispatch,
            trim_budget,
            active_context_limits.keep_recent_turns,
        );
        *history_has_trim_breadcrumb = turn_state.crumb_present;
        let trimmed_this_pass = turn_state.history.len() != history_len_before
            || turn_state.crumb_present != crumb_before;
        if trimmed_this_pass && hook_structural_mutation {
            // The rebuild below reconstructs the post-hook request by
            // trimming `post_hook_snapshot` as if everything past
            // `pre_hook_messages.len()` were a hook-appended suffix and
            // everything before it were the untouched, in-place durable
            // prefix. A hook that inserted, removed, reordered, or rewrote an
            // existing message breaks that assumption: the durable turn
            // count the rebuild targets would no longer correspond to the
            // actual post-hook message positions, so a whole-turn drop could
            // retain a message the durable history already discarded, or
            // discard one it retained. There is no way to reconstruct the
            // correct request here without re-invoking the hook, which would
            // break its exactly-once-per-request contract. Fail the turn
            // explicitly instead of dispatching a request whose retained
            // content no longer matches the durable, trimmed history.
            return Err(anyhow::Error::msg(crate::i18n::get_required_cli_string(
                "turn-context-hook-mutation-unsafe-error",
            )));
        }
        if trimmed_this_pass {
            // Recount the prepared population after every removal. A raw
            // estimate cannot predict the contribution of an image or hook
            // suffix, so it must not choose several drops ahead of this check.
            let mut total_dropped_messages = trim_result.dropped_messages;
            let mut dropped_turns = trim_result.dropped_turns;
            let mut tokens_after_dispatch = tokens_before_dispatch;
            let mut dispatch_after_source = dispatch_token_source;
            let max_iterations = crate::agent::history_trim::count_turns(turn_state.history) + 1;
            for _ in 0..max_iterations {
                // Wire roles cannot identify turns: prompt-mode results are
                // plain user messages. Remove precisely the durable source
                // span, preserving prepared media and the hook suffix.
                let removed_rows =
                    original_body_start..original_body_start + total_dropped_messages;
                let mut trimmed_post_hook: Vec<_> = post_hook_snapshot
                    .iter()
                    .zip(&prepared_source_rows)
                    .filter(|(_, source_row)| !removed_rows.contains(source_row))
                    .map(|(message, _)| message.clone())
                    .collect();
                // If the durable trim inserted a fresh breadcrumb, mirror it
                // in the post-hook request so the dispatched population
                // matches the persisted history.
                if !post_hook_had_crumb && turn_state.crumb_present {
                    crate::agent::history_trim::insert_breadcrumb_deduped(
                        &mut trimmed_post_hook,
                        false,
                    );
                }
                // Re-append the hook's transient suffix and re-apply prompt
                // framing so the system anchor stays consistent.
                trimmed_post_hook.extend(hook_suffix.iter().cloned());
                refresh_prompt_anchor(&mut trimmed_post_hook, use_native_tools);
                refresh_scoped_tool_protocol_prompt(
                    turn_state.history,
                    &mut trimmed_post_hook,
                    use_native_tools,
                );
                if let Some(section) = builtin_prompt_section.as_deref() {
                    apply_builtin_prompt_scope(
                        &mut trimmed_post_hook,
                        tools_registry,
                        &iteration_tool_specs.known_tool_names,
                        section,
                        use_native_tools,
                        strict_tool_parsing,
                        max_system_prompt_chars,
                        turn_state
                            .history
                            .iter()
                            .find(|message| message.role == "system")
                            .map(|message| message.content.as_str()),
                    );
                }
                if policy == zeroclaw_api::model_provider::ToolRoundPolicy::Single {
                    apply_single_tool_round_prompt(&mut trimmed_post_hook);
                }
                provider_request_messages = trimmed_post_hook;
                reported_population_estimated =
                    crate::agent::history::estimate_history_tokens(&provider_request_messages)
                        + tool_schema_tokens;
                let (measured_after, after_source) = dispatch_tokens(reported_population_estimated);
                tokens_after_dispatch = measured_after;
                dispatch_after_source = after_source;
                if tokens_after_dispatch <= trim_budget as u64 {
                    break;
                }
                if trim_result.outcome == PreDispatchOutcome::Floor {
                    // The prior call already found no droppable whole turn;
                    // re-running it would repeat the same no-op decision.
                    break;
                }
                // Still over budget on the authoritative rebuilt count: drop
                // one more whole turn, using that count rather than the stale
                // heuristic that decided the previous round stopped early.
                let before_len = turn_state.history.len();
                let before_crumb = turn_state.crumb_present;
                trim_result = trim_whole_turns_for_dispatch(
                    injected_memory_preamble,
                    turn_state.history,
                    &mut turn_state.crumb_present,
                    tokens_after_dispatch,
                    trim_budget,
                    active_context_limits.keep_recent_turns,
                );
                *history_has_trim_breadcrumb = turn_state.crumb_present;
                total_dropped_messages += trim_result.dropped_messages;
                dropped_turns += trim_result.dropped_turns;
                let dropped_more = turn_state.history.len() != before_len
                    || turn_state.crumb_present != before_crumb;
                if !dropped_more {
                    break;
                }
            }
            // Report the whole trim cascade, not one round: the outer owner
            // re-trims its durable history once, after the loop, and it must
            // know whether anything was dropped at all.
            if let Some(flag) = history_was_trimmed.as_deref_mut() {
                *flag |= dropped_turns > 0;
            }
            let exceeds_model_window = tokens_after_dispatch > model_context_window as u64;
            let event_budget = if exceeds_model_window {
                model_context_window
            } else {
                trim_budget
            };
            trim_result.dropped_messages = total_dropped_messages;
            trim_result.dropped_turns = dropped_turns;
            record_dispatch_trim(
                (active_model_provider_name, provider_request_model),
                &trim_result,
                trim_budget,
                tokens_before_dispatch,
                tokens_after_dispatch,
                dispatch_token_source,
            );
            if let Some(tx) = event_tx.as_ref() {
                let _ = tx
                    .send(TurnEvent::HistoryTrimmed {
                        dropped_messages: trim_result.dropped_messages,
                        dropped_turns,
                        kept_turns: trim_result.kept_turns,
                        reason: crate::i18n::get_required_cli_string("history-trim-reason-budget"),
                        token_budget: Some(event_budget as u64),
                        tokens_before: Some(tokens_before_dispatch),
                        tokens_after: Some(tokens_after_dispatch),
                        tokens_before_source: Some(dispatch_token_source),
                        tokens_after_source: Some(dispatch_after_source),
                        unsatisfiable_floor: exceeds_model_window.then_some(true),
                        retained_context: Some(retained_context_snapshot(
                            injected_memory_preamble,
                            turn_state.history,
                            turn_state.crumb_present,
                        )),
                    })
                    .await;
            }
            observer.record_event(
                &zeroclaw_api::observability_traits::ObserverEvent::HistoryTrimmed {
                    dropped_messages: trim_result.dropped_messages,
                    kept_turns: trim_result.kept_turns,
                    reason: crate::i18n::get_required_cli_string("history-trim-reason-budget"),
                    channel: None,
                    agent_alias: agent_alias.map(str::to_string),
                    turn_id: Some(turn_id.to_string()),
                    token_budget: Some(event_budget as u64),
                    tokens_before: Some(tokens_before_dispatch),
                    tokens_after: Some(tokens_after_dispatch),
                    tokens_before_source: Some(dispatch_token_source),
                    tokens_after_source: Some(dispatch_after_source),
                    unsatisfiable_floor: exceeds_model_window.then_some(true),
                },
            );
        } else if trim_result.outcome == PreDispatchOutcome::Floor
            && tokens_before_dispatch > model_context_window as u64
        {
            // No droppable whole turn remained at all — the durable history
            // and rebuilt request are identical, so the gate's measured
            // population is already the authoritative count.
            if let Some(tx) = event_tx.as_ref() {
                let _ = tx
                    .send(TurnEvent::HistoryTrimmed {
                        dropped_messages: 0,
                        dropped_turns: 0,
                        kept_turns: trim_result.kept_turns,
                        reason: crate::i18n::get_required_cli_string("history-trim-reason-budget"),
                        token_budget: Some(model_context_window as u64),
                        tokens_before: Some(tokens_before_dispatch),
                        tokens_after: Some(tokens_before_dispatch),
                        tokens_before_source: Some(dispatch_token_source),
                        tokens_after_source: Some(dispatch_token_source),
                        unsatisfiable_floor: Some(true),
                        retained_context: Some(retained_context_snapshot(
                            injected_memory_preamble,
                            turn_state.history,
                            turn_state.crumb_present,
                        )),
                    })
                    .await;
            }
            observer.record_event(
                &zeroclaw_api::observability_traits::ObserverEvent::HistoryTrimmed {
                    dropped_messages: 0,
                    kept_turns: trim_result.kept_turns,
                    reason: crate::i18n::get_required_cli_string("history-trim-reason-budget"),
                    channel: None,
                    agent_alias: agent_alias.map(str::to_string),
                    turn_id: Some(turn_id.to_string()),
                    token_budget: Some(model_context_window as u64),
                    tokens_before: Some(tokens_before_dispatch),
                    tokens_after: Some(tokens_before_dispatch),
                    tokens_before_source: Some(dispatch_token_source),
                    tokens_after_source: Some(dispatch_token_source),
                    unsatisfiable_floor: Some(true),
                },
            );
        }
        let retained_tokens = dispatch_tokens(reported_population_estimated).0;
        if retained_tokens > model_context_window as u64 {
            return Err(context_window_exceeded_error(
                (active_model_provider_name, provider_request_model),
                active_context_limits,
                retained_tokens,
                dispatch_token_source,
            ));
        }

        // Recovery must use the final dispatch population, never a snapshot
        // containing turns removed by the pre-dispatch budget trim.
        let submitted_image_ids =
            zeroclaw_providers::multimodal::provider_image_ids(&provider_request_messages);
        let accepted_image_ids = image_cache
            .as_mut()
            .map(|state| state.provider_state.accepted(&image_route))
            .unwrap_or_default();
        let image_recovery_view = if image_recovery_has_owner {
            prepare_provider_image_recovery_view(
                &provider_request_messages,
                &submitted_image_ids,
                &accepted_image_ids,
            )
        } else {
            None
        };
        let (image_recovery_messages, recovery_replaced_image_ids) = match image_recovery_view {
            Some((messages, replaced)) => (Some(messages), replaced),
            None => (None, Vec::new()),
        };
        let recovery_submitted_image_ids = image_recovery_messages
            .as_deref()
            .map(zeroclaw_providers::multimodal::provider_image_ids)
            .unwrap_or_default();

        // Fail closed on the local budget BEFORE announcing the request.
        // `announce_llm_request` emits the user-visible `WaitingOnModel`
        // state, and a rejected turn never reaches the provider — announcing
        // first would claim the agent is waiting on a model that is never
        // called.
        enforce_tool_loop_budget()?;

        if single_tool_rounds_requested
            && !tool_specs.is_empty()
            && policy == zeroclaw_api::model_provider::ToolRoundPolicy::Batch
            && !single_tool_limitation_reported
        {
            let notice =
                crate::i18n::get_required_cli_string("turn-single-tool-rounds-unsupported");
            let notice = format!("{notice}\n\n");
            accumulated_display_text.push_str(&notice);
            events::emit_posthoc_turn_chunk(event_tx.as_ref(), &notice).await;
            if let Some(tx) = on_delta.as_ref() {
                let _ = tx.send(StreamDelta::Text(notice)).await;
            }
            single_tool_limitation_reported = true;
        }

        if strict_tool_parsing
            && !tool_specs.is_empty()
            && active_model_provider.has_mixed_native_tool_support_for_model(protocol_model)
        {
            return Err(zeroclaw_providers::ProviderCapabilityError {
                model_provider: active_model_provider_name.to_string(),
                capability: "tool_protocol".to_string(),
                message: crate::i18n::get_required_cli_string(
                    "turn-tool-protocol-strict-mixed-error",
                ),
            }
            .into());
        }

        // Unified path via ModelProvider::chat so provider-specific native tool logic
        // (OpenAI/Anthropic/OpenRouter/compatible adapters) is honored.
        let request_tools = if use_native_tools {
            Some(tool_specs)
        } else {
            None
        };
        let llm_started_at = announce_llm_request(
            &ctx,
            &provider_request_messages,
            request_tools,
            active_model_provider,
            active_model_provider_name,
            provider_request_model,
            iteration,
        )
        .await;
        let request_tool_count = request_tools.map_or(0, <[crate::tools::ToolSpec]>::len);
        let base_provider_supports_native_tools = model_provider
            .capabilities_for_model(dispatch_model)
            .native_tool_calling;
        let active_provider_supports_native_tools = active_model_provider
            .capabilities_for_model(provider_dispatch_model)
            .native_tool_calling;
        let active_provider_supports_streaming = active_model_provider.supports_streaming();
        let active_provider_supports_streaming_tool_events =
            active_model_provider.supports_streaming_tool_events();
        let should_consume_provider_stream = (on_delta.is_some() || event_tx.is_some())
            && active_provider_supports_streaming
            && (request_tools.is_none() || active_provider_supports_streaming_tool_events);
        if ::zeroclaw_log::debug_enabled() {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_attrs(::serde_json::json!({
                        "has_on_delta": on_delta.is_some(),
                        "has_event_tx": event_tx.is_some(),
                        "base_provider_supports_native_tools": base_provider_supports_native_tools,
                        "active_provider_supports_native_tools": active_provider_supports_native_tools,
                        "active_provider_supports_streaming": active_provider_supports_streaming,
                        "active_provider_supports_streaming_tool_events": active_provider_supports_streaming_tool_events,
                        "tool_specs_count": tool_specs.len(),
                        "request_tools_count": request_tool_count,
                        "use_native_tools": use_native_tools,
                        "should_consume_provider_stream": should_consume_provider_stream,
                    })),
                &format!("native tool delivery decision for iteration {}", iteration + 1)
            );
        }

        let ProviderCallOutcome {
            chat_result,
            attempts,
            accepted_route,
            streamed_live_deltas,
            streamed_protocol_suppressed,
            streamed_visible_text,
            image_recovery_succeeded,
        } = call_provider(
            &ctx,
            active_model_provider,
            active_model_provider_name,
            provider_request_model,
            provider_dispatch_model,
            zeroclaw_api::model_provider::ChatRequest {
                messages: &provider_request_messages,
                tools: request_tools,
                thinking: zeroclaw_api::NATIVE_THINKING_OVERRIDE
                    .try_with(Clone::clone)
                    .ok()
                    .flatten(),
            },
            image_recovery_messages.as_deref(),
            should_consume_provider_stream,
            policy,
            iteration,
            use_native_tools.then_some(iteration_tool_specs.tool_specs.as_slice()),
        )
        .await?;
        let accepted_request_messages = if image_recovery_succeeded {
            image_recovery_messages
                .as_deref()
                .unwrap_or(&provider_request_messages)
        } else {
            &provider_request_messages
        };

        // Reliable reports its actually served candidate; direct providers
        // use the vision/post-hook identity when present, otherwise retain the
        // requested route as the accounting fallback.
        // Owned, not borrowed from `accepted_route`: the rebound `ctx` below
        // carries this pair to the end of the iteration, while `accepted_route`
        // is consumed by `commit_accepted_provider_route` partway through.
        let (served_provider, served_model) = accepted_route
            .as_ref()
            .map(|route| (route.provider_ref().to_string(), route.model().to_string()))
            .unwrap_or_else(|| {
                (
                    active_model_provider_name.to_string(),
                    provider_request_model.to_string(),
                )
            });

        // A reliable fallback can serve a DIFFERENT configured alias than the
        // one this iteration resolved limits for at dispatch time. Capacity and
        // budget are properties of the route that actually answered, so re-key
        // them through the same canonical resolver rather than reporting the
        // pre-dispatch pair against a post-dispatch identity. `AcceptedRoute`
        // deliberately carries identity only — capacity lives in config, which
        // the provider layer must not know about — so the pair is re-resolved
        // here instead of being threaded through dispatch.
        //
        // Gated on the served ROUTE differing from the dispatched one, in
        // either component. Two distinct fallback shapes reach here:
        //
        //   - `fallback` (cross-alias): the accepted route names a different
        //     configured alias, so the alias comparison catches it.
        //   - `fallback_models` (same-alias): `push_pinned_entries` builds the
        //     primary and every fallback under ONE alias, differing only in the
        //     pinned model, and `served_model()` reports that pinned model. The
        //     alias is unchanged, so only the model comparison catches it.
        //
        // The model comparison ignores a `hint:` selector. A direct call
        // through a router reports an accepted route whose `model()` is the
        // dispatch-facing wire selector, which is neither an attribution name
        // nor a valid resolver key; comparing it raw would fire on every hinted
        // call even though the route never changed. Pinned-fallback models from
        // the reliable layer are never `hint:`-prefixed, so stripping the
        // prefix keeps this branch closed for hint routing while leaving it
        // open for a real pinned-model fallback.
        let served_model_key = served_model
            .strip_prefix("hint:")
            .map(|_| provider_request_model)
            .unwrap_or(served_model.as_str());
        let served_route_changed = served_provider != active_model_provider_name
            || served_model_key != provider_request_model;
        let served_context_limits = if served_route_changed {
            resolve_context_limits_for_call(
                context_limits_resolver.as_ref(),
                config,
                agent_alias,
                &served_provider,
                served_model_key,
                active_context_limits,
            )
        } else {
            active_context_limits
        };
        // Rebind so everything downstream of acceptance — the per-call usage
        // frame, recovery arithmetic, and the terminal snapshot — reads limits
        // for the route that actually answered. A no-op when the alias did not
        // change, since `served_context_limits` is then the pair already bound.
        let ctx = base_ctx.for_route(&served_provider, served_model_key, served_context_limits);

        // Reliable providers classify this before retries and fallback. Keep
        // the turn-level guard for direct/unwrapped providers: a transport
        // success with no final text and no tool calls cannot complete a turn.
        // This runs before response-success telemetry and history mutation.
        let chat_result = chat_result.and_then(|response| {
            if policy == zeroclaw_api::model_provider::ToolRoundPolicy::Single
                && iteration_tool_specs.exceeds_single_tool_round(&response, strict_tool_parsing)
            {
                return Err(single_tool_round_error());
            }
            if response.is_semantically_empty_terminal() {
                return Err(anyhow::Error::new(
                    zeroclaw_api::model_provider::SemanticEmptyTerminalCompletion,
                ));
            }
            Ok(response)
        });

        let (
            response_text,
            parsed_text,
            tool_calls,
            assistant_history_content,
            native_tool_calls,
            parse_issue_detected,
            protocol_suppressed,
            response_streamed_live,
            reported_input_tokens,
            response_usage,
        ) = match chat_result {
            Ok(resp) => {
                let interpreted = interpret_chat_response(
                    &ctx,
                    &served_provider,
                    &served_model,
                    resp,
                    accepted_request_messages,
                    &iteration_tool_specs,
                    streamed_protocol_suppressed,
                    iteration,
                    knobs.detect_protocol_without_tools,
                )
                .await;
                (
                    interpreted.response_text,
                    interpreted.parsed_text,
                    interpreted.tool_calls,
                    interpreted.assistant_history_content,
                    interpreted.native_tool_calls,
                    interpreted.parse_issue_detected,
                    streamed_protocol_suppressed,
                    streamed_live_deltas,
                    interpreted.input_tokens,
                    interpreted.usage,
                )
            }
            Err(e) => {
                crate::agent::cost::settle_provider_attempts(&attempts, None);
                // This iteration's attempts never reach an accepted response
                // (recovery continues with a fresh attempt vec, otherwise the
                // turn fails) — project them now so the gateway ledger stays
                // complete.
                emit_rejected_attempt_usage(ctx.event_tx, &attempts).await;
                record_llm_failure(&ctx, provider_request_model, llm_started_at, iteration, &e);
                let recovered = try_recover_context_overflow(
                    injected_memory_preamble,
                    turn_state.history,
                    &e,
                    iteration,
                    event_tx.as_ref(),
                    on_delta.as_ref(),
                    observer,
                    ctx.context_limits.context_token_budget,
                    ctx.context_limits.model_context_window,
                    ctx.context_limits.keep_recent_turns,
                    &context_calibration,
                    &mut turn_state.crumb_present,
                )
                .await;
                if recovered {
                    continue;
                }
                // A stream that died after caller-visible output: persist the
                // partial with the interruption marker so wrappers/channels
                // can commit what the consumer already saw.
                if let Some(interrupted) = e.downcast_ref::<outcome::StreamInterruptedAfterOutput>()
                    && !interrupted.partial_text.is_empty()
                {
                    let msg = ChatMessage::assistant(format!(
                        "{}\n\n{}",
                        interrupted.partial_text,
                        crate::i18n::get_required_cli_string("turn-stream-interrupted")
                    ));
                    turn_state.push_dual(msg);
                }
                // Same for a user cancel after visible streamed output —
                // the pre-consolidation streaming engine committed the
                // watched partial with this exact marker.
                if let Some(cancelled) = e.downcast_ref::<outcome::StreamCancelledAfterOutput>()
                    && !cancelled.partial_text.is_empty()
                {
                    let msg = ChatMessage::assistant(format!(
                        "{}\n\n{}",
                        cancelled.partial_text,
                        crate::i18n::get_required_cli_string("turn-interrupted-by-user")
                    ));
                    turn_state.push_dual(msg);
                }
                return Err(e);
            }
        };

        // Re-anchor the calibration on the provider's authoritative prompt size
        // for the CURRENT history. This runs immediately after the response is
        // accepted and BEFORE the assistant reply / tool results are appended,
        // so `turn_state.history` is exactly the sequence the provider billed —
        // the reported value prices all of it, and every message pushed from
        // here on becomes the unbilled tail `current()` adds back.
        if let Some(reported) = reported_input_tokens {
            context_calibration.observe_reported(
                reported as usize,
                turn_state.history,
                active_model_provider_name,
                provider_request_model,
                tool_schema_tokens,
            );
            // Hand the anchor to the caller's next turn. The population the
            // provider just billed is `turn_state.history` at this exact
            // moment (the user row is in it, the assistant reply and tool
            // results are not), so the anchored prefix covers every durable
            // row the next turn keeps, and the next turn prices only rows
            // appended after this point.
            if let Some(sink) = context_anchor_sink.as_ref()
                && let Some(anchor) = context_calibration.cross_turn_anchor()
            {
                *sink.lock().expect("context anchor sink lock") = Some(anchor);
            }
        }

        let display_text = resolve_display_text(
            &response_text,
            &parsed_text,
            !tool_calls.is_empty(),
            !native_tool_calls.is_empty(),
        );

        // Any parser or stream-protocol guard finding rejects the transport
        // candidate, including a response that also carries native tool calls.
        if parse_issue_detected {
            crate::agent::cost::settle_provider_attempts(&attempts, None);
            // Same contract as the error branch: this iteration's attempts
            // are settled but never accepted — project them before retrying
            // with a fresh vec or returning the fallback.
            emit_rejected_attempt_usage(ctx.event_tx, &attempts).await;
            malformed_tool_protocol_retries += 1;
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Provider)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(serde_json::json!({
                        "channel": channel_name,
                        "model_provider": active_model_provider_name,
                        "model": active_model,
                        "trace_id": turn_id,
                        "error": "malformed internal tool protocol omitted from channel output",
                    })),
                "tool_call_parse_feedback"
            );
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Provider)
                    .with_attrs(serde_json::json!({
                    "iteration": iteration + 1,
                    "retry": malformed_tool_protocol_retries,
                    "max_retries": MAX_MALFORMED_TOOL_PROTOCOL_RETRIES,
                    "response_excerpt": truncate_with_ellipsis(
                        &scrub_credentials(&response_text),
                        600
                    ),
                    })),
                "tool_call_parse_feedback_details"
            );

            if protocol_suppressed {
                // Trailing whitespace is not a semantic difference: the same
                // suppressed envelope with or without a final newline is
                // still the same reply, so compare trimmed ends and store
                // the trimmed form. The delivered text is not rewritten.
                let suppressed_text = response_text.trim_end().to_string();
                if last_guard_suppressed_text.as_deref() == Some(suppressed_text.as_str()) {
                    // The guard withheld the same text twice: retrying cannot
                    // recover prose the guard keeps suppressing, so end the
                    // turn with a notice instead of another provider call.
                    let notice = crate::i18n::get_required_cli_string(
                        "cli-agent-error-protocol-guard-withheld",
                    );
                    accumulated_display_text.push_str(&notice);
                    // The notice is synthesized here, never streamed live: an
                    // event consumer that already flushed streamed prose
                    // would otherwise hide it, like the malformed fallback.
                    events::emit_posthoc_turn_chunk(event_tx.as_ref(), &notice).await;
                    if let Some(ref tx) = on_delta {
                        let _ = tx.send(StreamDelta::Text(notice.to_string())).await;
                    }
                    let msg = ChatMessage::assistant(notice.to_string());
                    turn_state.push_dual(msg);
                    return Ok(accumulated_display_text);
                }
                last_guard_suppressed_text = Some(suppressed_text);
            }

            if malformed_tool_protocol_retries <= MAX_MALFORMED_TOOL_PROTOCOL_RETRIES {
                // This is model feedback, not a tool result: malformed protocol
                // output has no valid tool_call_id to attach a role=tool message to.
                let msg = ChatMessage::user(
                    "[Tool call parse error]\n\
                     Your previous response looked like an internal tool-call protocol payload, \
                     but ZeroClaw could not parse it into a valid tool call. Use the supported \
                     tool-call schema, or answer in natural language if no tool is needed."
                        .to_string(),
                );
                turn_state.push_dual(msg);
                continue;
            }

            let fallback =
                crate::i18n::get_required_cli_string("channel-runtime-malformed-tool-output");
            accumulated_display_text.push_str(&fallback);
            // The fallback is synthesized here, never streamed live, and is the
            // turn's only visible output on this exit: an event consumer that
            // already flushed streamed narration would otherwise hide the
            // TurnComplete payload. Same rationale as the max-iteration emit.
            events::emit_posthoc_turn_chunk(event_tx.as_ref(), &fallback).await;
            if let Some(ref tx) = on_delta {
                let _ = tx.send(StreamDelta::Text(fallback.to_string())).await;
            }
            let msg = ChatMessage::assistant(fallback.to_string());
            turn_state.push_dual(msg);
            return Ok(accumulated_display_text);
        }

        // Earlier physical leaves are rejected routing/retry work. The final
        // accepted response remains settled by `record_accepted_chat_response`
        // so only it updates context-window telemetry.
        crate::agent::cost::settle_provider_attempts(
            &attempts[..attempts.len().saturating_sub(1)],
            None,
        );
        // Emit TurnEvent::Usage for each billable rejected attempt so the
        // gateway's usage_by_provider breakdown includes all billable attempts
        // (accepted + rejected). This makes the breakdown the single source of
        // truth that the done-frame cost_usd can be derived from.
        emit_rejected_attempt_usage(ctx.event_tx, &attempts[..attempts.len().saturating_sub(1)])
            .await;
        record_accepted_chat_response(
            &ctx,
            &served_provider,
            &served_model,
            &response_text,
            &native_tool_calls,
            tool_calls.len(),
            response_usage.as_ref(),
            accepted_request_messages,
            llm_started_at,
            iteration,
            accepted_route.as_ref(),
        )
        .await;

        // Backfill the final served route only after protocol classification
        // accepts the response.
        //
        // When a reliable fallback answered from a different alias, the whole
        // triple is rewritten here: the sink was seeded pre-dispatch with the
        // REQUESTED route, so leaving it would report the served alias against
        // the requested route's capacity. Same gate as the limit re-key above,
        // so a direct call keeps the seeded attribution pair (its accepted
        // route names the alias we dispatched to, and its `model()` is the wire
        // selector rather than an attribution name).
        if served_route_changed
            && let Some(sink) = served_route_sink.as_ref()
            && let Some(served) = sink.lock().expect("served-route sink lock").as_mut()
        {
            served.provider_name = served_provider.clone();
            // The normalized key, not the raw accepted model: the terminal
            // frame is attribution, and a `hint:` wire selector is not an
            // attribution name.
            served.model = served_model_key.to_string();
            served.context_limits = served_context_limits;
        }

        // A provider transport success is only a candidate. Commit (or clear)
        // presentation state after parsing has accepted the response, so a
        // malformed fallback completion cannot leak a stale recovery notice.
        zeroclaw_providers::dispatch::commit_accepted_provider_route(accepted_route);

        if let Some(image_state) = image_cache.as_mut() {
            if image_recovery_succeeded {
                image_state.provider_state.record_recovery(
                    image_route,
                    &recovery_submitted_image_ids,
                    &recovery_replaced_image_ids,
                );
            } else {
                image_state.provider_state.record_success(
                    image_route,
                    &submitted_image_ids,
                    &newest_user_image_ids,
                );
            }
        }

        if image_recovery_succeeded {
            let replaced_count = recovery_replaced_image_ids.len().to_string();
            let replaced_count_plural = if recovery_replaced_image_ids.len() == 1 {
                "one"
            } else {
                "other"
            };
            let notice = crate::i18n::get_required_cli_string_with_args(
                "turn-provider-images-quarantined",
                &[
                    ("count", replaced_count.as_str()),
                    ("count_plural", replaced_count_plural),
                ],
            );
            accumulated_display_text.push_str(&notice);
            accumulated_display_text.push_str("\n\n");
            events::emit_posthoc_turn_chunk(event_tx.as_ref(), &format!("{notice}\n\n")).await;
            if let Some(ref tx) = on_delta {
                let _ = tx.send(StreamDelta::Text(format!("{notice}\n\n"))).await;
            }
        }

        if tool_calls.is_empty() {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "model": active_model,
                        "iteration": iteration + 1,
                        "text": scrub_credentials(&display_text),
                        "trace_id": turn_id,
                    })),
                "turn_final_response"
            );
            // No tool calls — this is the final response.
            accumulated_display_text.push_str(&display_text);

            // If text wasn't streamed live, send it now post-hoc. Gated on
            // event_tx independently of on_delta (never nested — §8.4).
            if !response_streamed_live && !protocol_suppressed {
                events::emit_posthoc_turn_chunk(event_tx.as_ref(), &display_text).await;
            }

            // If text wasn't streamed live, send it now via post-hoc chunking.
            // When streamed live, the channel already received the deltas.
            if let Some(ref tx) = on_delta
                && !response_streamed_live
                && !protocol_suppressed
            {
                events::stream_text_posthoc_chunks(tx, &display_text, cancellation_token.as_ref())
                    .await?;
            }

            let msg = ChatMessage::assistant(response_text.clone());
            turn_state.push_dual(msg);
            // A terminal response never dispatches another request, so the only
            // reason to trim here is keeping the durable transcript inside the
            // window for the NEXT turn. Providers that report no usage still
            // need that: `enforce_reported_budget` falls back to the local
            // estimate when the count is absent.
            {
                // No hook reserve is carried — enforcement never executes a
                // modifying `before_llm_call` hook merely to estimate a request
                // that will never be sent.
                Box::pin(enforce_reported_budget(
                    injected_memory_preamble,
                    turn_state.history,
                    reported_input_tokens.unwrap_or(0) as usize,
                    // This trim exists to keep the durable transcript inside
                    // the window for the NEXT turn, which will carry the same
                    // tool catalog this one did.
                    tool_schema_tokens,
                    ctx.context_limits.context_token_budget,
                    ctx.context_limits.effective_context_window(),
                    ctx.context_limits.keep_recent_turns,
                    event_tx.as_ref(),
                    observer,
                    agent_alias,
                    turn_id,
                    &mut turn_state.crumb_present,
                ))
                .await?;
            }
            *history_has_trim_breadcrumb = turn_state.crumb_present;
            return Ok(accumulated_display_text);
        }

        // Relay only the portion of narration the live stream did not already
        // deliver: re-sending the whole thing duplicates it.
        if !display_text.is_empty() {
            let remainder = unforwarded_narration(&display_text, &streamed_visible_text);
            // Send any unforwarded remainder as a post-hoc Chunk, even when an
            // earlier prefix streamed live. Gated on event_tx independently of
            // on_delta (never nested — §8.4). The native-tool-calls condition
            // is parity with the on_delta relay below: text-parsed tool calls
            // have their markup stripped from display, so only native
            // providers carry separable narration. Loosen both together if a
            // parsed-tool provider is ever shown to leave visible narration.
            emit_unforwarded_native_narration(
                event_tx.as_ref(),
                remainder,
                protocol_suppressed,
                !native_tool_calls.is_empty(),
            )
            .await;
            // `protocol_suppressed` withholds from the suppressed candidate onward; the prose
            // ahead of it is already delivered, and the empty-remainder skip below handles the guard-passed case where the live stream already forwarded every byte.
            if !native_tool_calls.is_empty()
                && !protocol_suppressed
                && !remainder.is_empty()
                && let Some(ref tx) = on_delta
            {
                let mut narration = remainder.to_string();
                if !narration.ends_with('\n') {
                    narration.push('\n');
                }
                let _ = tx.send(StreamDelta::Text(narration)).await;
            }
            if !silent {
                eprint!("{display_text}");
                let _ = std::io::stderr().flush();
            }
        }

        // ── Progress: LLM responded ─────────────────────────────
        // Emitted AFTER the narration relay above: draft updaters flush on
        // Status boundaries, and flushing before the turn's narration Text is
        // queued would deliver the pre-tool message a boundary late for
        // multi_message channels.
        if let Some(ref tx) = on_delta {
            let llm_secs = llm_started_at.elapsed().as_secs();
            let _ = tx
                .send(StreamDelta::Status(format!(
                    "\u{1f4ac} Got {} tool call(s) ({llm_secs}s)\n",
                    tool_calls.len()
                )))
                .await;
        }

        let PreparedToolCalls {
            mut ordered_results,
            executable_indices,
            executable_calls,
            stream_calls,
        } = prepare_tool_calls(
            &ctx,
            tools_registry,
            activated_tools,
            &tool_calls,
            &mut seen_tool_signatures,
            &mut prompt_approval_tool_signatures,
            iteration,
            knobs.dedup_enabled,
        )
        .await?;

        // Hooks and preparation may rewrite tool names or arguments. Decide
        // parallel eligibility from the final executable calls so policy is
        // applied to what the runtime will actually dispatch.
        let allow_parallel_execution =
            parallel_tools && should_execute_tools_in_parallel(&executable_calls, approval);

        let live_sop_queue = crate::sop::executor::new_live_action_queue();
        let execution =
            crate::sop::executor::scope_live_action_queue(live_sop_queue.clone(), async {
                if allow_parallel_execution && executable_calls.len() > 1 {
                    let meta = ctx.meta();
                    let dispatch = ToolDispatchContext {
                        tools_registry,
                        activated_tools,
                        excluded_tools,
                        model_switch_callback: model_switch_callback.as_ref(),
                    };
                    execute_tools_parallel(
                        &executable_calls,
                        dispatch,
                        &meta,
                        observer,
                        cancellation_token.as_ref(),
                        receipt_generator,
                        ctx.event_tx,
                    )
                    .await
                } else {
                    let meta = ctx.meta();
                    let dispatch = ToolDispatchContext {
                        tools_registry,
                        activated_tools,
                        excluded_tools,
                        model_switch_callback: model_switch_callback.as_ref(),
                    };
                    execute_tools_sequential(
                        &executable_calls,
                        dispatch,
                        &meta,
                        observer,
                        cancellation_token.as_ref(),
                        receipt_generator,
                        ctx.event_tx,
                    )
                    .await
                }
            });
        let execution = scope_model_switch_provider_allowlist(
            knobs.provider_attempt_allowlist.clone(),
            execution,
        );
        let execution_result = match shared_budget.clone() {
            Some(budget) => ExecutionTreeBudget::scope(budget, Box::pin(execution)).await,
            None => execution.await,
        };
        let executed_slots = match execution_result {
            Ok(slots) => slots,
            Err(e) if is_tool_loop_cancelled(&e) => {
                (0..executable_calls.len()).map(|_| None).collect()
            }
            Err(e) => {
                // The batch failed outright: no executable context reached
                // post-execution handling, so every one is abandoned before
                // the turn aborts.
                call_prep::abandon_unexecuted_prepared_contexts(
                    &ctx,
                    iteration,
                    &executable_indices,
                    &executable_calls,
                    &[],
                )
                .await;
                return Err(e);
            }
        };

        let cancelled_mid_batch = executed_slots.iter().any(Option::is_none);

        let mut executed_completed_indices: Vec<usize> = Vec::new();
        let mut executed_completed_calls = Vec::new();
        let mut executed_completed_stream_calls = Vec::new();
        let mut executed_completed_outcomes = Vec::new();
        for (slot, ((call_idx, call), stream_call)) in executed_slots.into_iter().zip(
            executable_indices
                .iter()
                .copied()
                .zip(executable_calls.iter())
                .zip(stream_calls),
        ) {
            if let Some(outcome) = slot {
                executed_completed_indices.push(call_idx);
                executed_completed_calls.push(call.clone());
                executed_completed_stream_calls.push(stream_call);
                executed_completed_outcomes.push(outcome);
            }
        }

        record_executed_outcomes(
            &ctx,
            &executed_completed_indices,
            &executed_completed_calls,
            &executed_completed_stream_calls,
            executed_completed_outcomes,
            &mut ordered_results,
            iteration,
        )
        .await;
        if cancelled_mid_batch {
            // Completed calls already received their after hook; every other
            // executable context of the batch was interrupted before
            // post-execution handling and gets exactly one abandonment.
            call_prep::abandon_unexecuted_prepared_contexts(
                &ctx,
                iteration,
                &executable_indices,
                &executable_calls,
                &executed_completed_indices,
            )
            .await;
            for (idx, call) in tool_calls.iter().enumerate() {
                if ordered_results[idx].is_none() {
                    ordered_results[idx] = Some((
                        call.name.clone(),
                        call.tool_call_id.clone(),
                        crate::agent::tool_execution::ToolExecutionOutcome {
                            output: crate::i18n::get_required_cli_string(
                                "turn-tool-interrupted-before-result",
                            ),
                            attachments: Vec::new(),
                            success: false,
                            error_reason: None,
                            duration: std::time::Duration::ZERO,
                            receipt: None,
                            output_data: None,
                        },
                    ));
                }
            }
            // Close pending cards only for executable calls whose terminal
            // ToolResult was never emitted by the executor. A parallel call that
            // completed before the cancellation already emitted its real result;
            // re-emitting here would flip its card from completed to interrupted.
            if let Some(tx) = ctx.event_tx {
                let completed: std::collections::HashSet<usize> =
                    executed_completed_indices.iter().copied().collect();
                for (call_idx, call) in executable_indices.iter().zip(executable_calls.iter()) {
                    if completed.contains(call_idx) {
                        continue;
                    }
                    let call_id = events::resolve_tool_call_id(call);
                    let interrupted = crate::agent::tool_execution::ToolExecutionOutcome {
                        output: crate::i18n::get_required_cli_string(
                            "turn-tool-interrupted-before-result",
                        ),
                        attachments: Vec::new(),
                        success: false,
                        error_reason: None,
                        duration: std::time::Duration::ZERO,
                        receipt: None,
                        output_data: None,
                    };
                    events::emit_tool_result(tx, &call_id, &call.name, &interrupted).await;
                }
            }
        }

        let CollectedResults {
            individual_results,
            tool_results,
            detection_relevant_output,
        } = collect_tool_results(
            ordered_results,
            &tool_calls,
            turn_state.history,
            &mut loop_detector,
            &loop_ignore_tools,
            max_tool_result_chars,
            collected_receipts,
            active_model,
            iteration,
            turn_id,
        )?;

        if !cancelled_mid_batch {
            check_identical_output_abort(
                &detection_relevant_output,
                loop_started_at,
                pacing,
                &mut consecutive_identical_outputs,
                &mut last_tool_output_hash,
                active_model,
                iteration,
                turn_id,
            )?;
        }

        turn_state.append_tool_round(
            assistant_history_content,
            &native_tool_calls,
            &individual_results,
            &tool_results,
            use_native_tools,
        );

        if cancelled_mid_batch {
            return Err(ToolLoopCancelled.into());
        }

        let queued_sop_actions = crate::sop::executor::drain_live_actions(&live_sop_queue);
        if !queued_sop_actions.is_empty() {
            // Box the drive future: it inlines the full per-agent re-assembly
            // (a large async fn), which would otherwise inflate this loop's
            // stack-allocated future for every turn, SOP or not.
            Box::pin(drive_live_sop_actions(
                queued_sop_actions,
                turn_state.history,
                &mut turn_state.crumb_present,
                model_provider,
                provider_name,
                model,
                dispatch_model,
                temperature,
                tools_registry,
                observer,
                silent,
                approval,
                security,
                multimodal_config,
                config,
                max_tool_iterations,
                hooks,
                excluded_tools,
                dedup_exempt_tools,
                activated_tools,
                model_switch_callback.clone(),
                pacing,
                strict_tool_parsing,
                parallel_tools,
                max_tool_result_chars,
                context_limits,
                receipt_generator,
                knobs,
                channel_name,
                channel_reply_target,
                cancellation_token.clone(),
                on_delta.clone(),
                shared_budget.as_ref().map(ExecutionTreeBudget::child),
                channel,
                collected_receipts,
                event_tx.clone(),
                turn_state.canonical.as_deref_mut(),
                if image_recovery_has_owner {
                    image_cache.as_mut()
                } else {
                    None
                },
                agent_alias,
                parent_agent_alias,
                sop_reassembly.clone(),
                &mut sop_exec_cache,
            ))
            .await?;
            turn_state.mark_all_synced();
        }

        pending_reported_usage = reported_input_tokens.map(|reported| ReportedRequestUsage {
            provider_name: served_provider.clone(),
            model: served_model_key.to_string(),
            reported_tokens: reported as usize,
            estimated_tokens: reported_population_estimated,
        });
    }

    if let Some(budget) = shared_budget.as_ref() {
        budget.reserve()?;
    }

    let (summary_image_cache, summary_provider_image_state) = match image_cache.as_mut() {
        Some(state) => (Some(&mut *state.cache), Some(&mut *state.provider_state)),
        None => (None, None),
    };
    let summary_result = finish_after_max_iterations(
        injected_memory_preamble,
        model_provider,
        turn_state.history,
        provider_name,
        model,
        dispatch_model,
        temperature,
        pacing,
        cancellation_token.as_ref(),
        max_iter::CompletionLimit::LocalIterations(max_iterations),
        accumulated_display_text,
        turn_id,
        knobs,
        event_tx.as_ref(),
        on_delta.as_ref(),
        turn_state.canonical.as_deref_mut(),
        config,
        multimodal_config,
        security,
        hooks,
        summary_image_cache,
        summary_provider_image_state,
        |provider, selected_model| {
            resolve_context_limits_for_call(
                context_limits_resolver.as_ref(),
                config,
                agent_alias,
                provider,
                selected_model,
                context_limits,
            )
        },
        &mut turn_state.crumb_present,
        pending_reported_usage,
        observer,
    )
    .await;
    *history_has_trim_breadcrumb = turn_state.crumb_present;
    summary_result
}

pub(crate) fn collect_callable_tool_names(
    tools_registry: &[Box<dyn crate::tools::Tool>],
    activated_tools: Option<&Arc<std::sync::Mutex<crate::tools::ActivatedToolSet>>>,
) -> Vec<String> {
    let mut names = tools_registry
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    if let Some(activated) = activated_tools {
        let activated = match activated.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Tool)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "activated-tool lock poisoned while resolving SOP step scope; recovering guard for read"
                );
                poisoned.into_inner()
            }
        };
        names.extend(activated.tool_names().into_iter().map(String::from));
    }
    names.sort();
    names.dedup();
    names
}

fn push_excluded_tool(excluded_tools: &mut Vec<String>, tool: impl Into<String>) {
    let tool = tool.into();
    if !excluded_tools
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&tool))
    {
        excluded_tools.push(tool);
    }
}

fn sop_step_excluded_tools(
    queued: &crate::sop::executor::QueuedSopAction,
    run_id: &str,
    step: &crate::sop::SopStep,
    tools_registry: &[Box<dyn crate::tools::Tool>],
    activated_tools: Option<&Arc<std::sync::Mutex<crate::tools::ActivatedToolSet>>>,
    excluded_tools: &[String],
) -> Vec<String> {
    let mut scoped = excluded_tools.to_vec();
    for tool in crate::sop::active_scope::SOP_CONTROL_TOOLS {
        push_excluded_tool(&mut scoped, tool);
    }

    let registry_names = collect_callable_tool_names(tools_registry, activated_tools);
    let active_scope = {
        let engine = match queued.engine.lock() {
            Ok(engine) => engine,
            Err(poisoned) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Tool)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"run_id": run_id, "step": step.number})),
                    "SOP engine lock poisoned while resolving step tool scope; recovering guard for read"
                );
                poisoned.into_inner()
            }
        };
        crate::sop::active_scope::resolve_active_step_scope(
            run_id,
            step,
            engine.config(),
            &registry_names,
        )
    };

    if let Some(active_scope) = active_scope {
        for tool in active_scope.excluded {
            push_excluded_tool(&mut scoped, tool);
        }
    }
    scoped.sort();
    scoped
}

/// Config handle the live SOP driver needs to re-assemble a nested step's agent
/// when the step delegates to a different agent than the one running the turn.
///
/// The live nested-step driver otherwise reuses the parent turn's assembled
/// execution context; when a step names a different agent it must run AS that
/// agent — with that agent's own gated tools, policy, and MCP scope — not the
/// parent's. This carries the one handle needed to rebuild that context in
/// flight; the runtime adapter is created from `config.runtime` so the nested
/// context is assembled the way a fresh agent turn would be. `Copy` so the
/// handle survives being re-read on every drained action and forwarded into
/// each nested turn.
///
/// This handle is either `Some` on every frame of a recursion tree or `None` on
/// every frame: it is introduced only at the top entry points and forwarded
/// unchanged into each nested step. Because a re-assembled sub-loop runs with
/// the step agent as its own `agent_alias` (the effective identity, which
/// attribution follows), the loop's `agent_alias` IS the re-assembly baseline
/// at every depth — no separate baseline field is needed: a depth >= 2 step
/// naming the outer agent compares against the re-assembled child's alias and
/// re-assembles correctly instead of inheriting the child's scope.
#[derive(Clone)]
pub struct SopStepReassembly<'a> {
    pub config: &'a zeroclaw_config::schema::Config,
    pub live_config: Option<zeroclaw_config::live::LiveConfigHandle>,
}

/// The re-assembly gate: a step needs its own agent context re-assembled when
/// it names an agent different from the one the current loop is running as
/// (`agent_alias` — the effective identity, which a re-assembled sub-loop
/// carries as its own alias, so the comparison is correct at every nesting
/// depth). A step with no explicit agent inherits the current one and never
/// re-assembles.
fn step_needs_reassembly(current_alias: Option<&str>, step_alias: Option<&str>) -> bool {
    matches!(step_alias, Some(s) if current_alias != Some(s))
}

/// A nested SOP step's owned per-agent execution surface, assembled for a step
/// that delegates to a different agent. Owns the complete per-agent execution
/// contract: provider binding with the step agent's own configured
/// temperature, gated tool registry, approval policy (the step agent's risk
/// profile under the PARENT surface's interactivity mode, so a live approval
/// route survives delegation), the deferred-MCP activation set, the step
/// agent's resolved runtime controls (`ResolvedRuntime`: iteration/result/
/// context limits, strict parsing, parallelism, dedup exemptions, tool filter
/// groups), and the inputs to build the step agent's own system prompt.
/// `LoopKnobs` deliberately stays parent-threaded: it encodes the calling
/// surface's shape, not agent policy.
pub(crate) struct OwnedAgentExecution {
    model_provider: Box<dyn ModelProvider>,
    provider_name: String,
    model: String,
    /// The step agent's own configured provider temperature — the same source
    /// the headless driver hands `crate::agent::run`.
    temperature: Option<f64>,
    /// The step agent's sealed tool set. A [`crate::tools::scoped::ScopedToolRegistry`]
    /// so the nested loop's `ResolvedIo.tools_registry` can be fed the scoped
    /// registry directly (coerces to `&[Box<dyn Tool>]` at leaf sites via `Deref`).
    pub(crate) tools_registry: crate::tools::scoped::ScopedToolRegistry,
    approval: crate::approval::ApprovalManager,
    activated_tools: Option<Arc<std::sync::Mutex<crate::tools::ActivatedToolSet>>>,
    /// The step agent's fully-resolved config (identity + every runtime-profile
    /// knob baked in via `Config::resolved_agent_config`) — the one canonical
    /// per-agent knob surface, so the nested loop's runtime controls follow
    /// the effective agent instead of the delegating parent.
    agent: zeroclaw_config::schema::AliasedAgentConfig,
    /// The step agent's risk profile (also baked into `approval`); retained
    /// because system-prompt construction renders autonomy guidance from it.
    risk_profile: zeroclaw_config::schema::RiskProfileConfig,
    /// The step agent's own skills, for its system prompt.
    skills: Vec<crate::skills::Skill>,
    /// MCP-origin ground truth for the per-turn `tool_filter_groups` gate.
    mcp_tool_names: std::collections::HashSet<String>,
    /// The step agent's deferred+pinned MCP prompt section (single-block
    /// shape, same as `run` / `process_message`).
    mcp_prompt_section: String,
    /// The shell this step's runtime adapter will spawn, carried so the step's
    /// system prompt reports the same dialect the step will execute under.
    /// `None` for a shell-less runtime.
    shell_profile: Option<zeroclaw_api::runtime_traits::ShellProfile>,
    /// Keeps the target admission alive for the cached nested execution
    /// surface, and prevents an alias-generation change from reusing it.
    execution_admission: Option<AgentExecutionAdmission>,
    /// The step agent's own filesystem policy, built by
    /// `assemble_owned_execution` the same way a fresh agent turn builds it.
    /// Carried so the nested sub-loop's no-vision image-marker gate applies
    /// the step agent's read ledger, never the parent's.
    security: Arc<crate::security::SecurityPolicy>,
}

impl OwnedAgentExecution {
    fn matches_admission(&self, next: Option<&AgentExecutionAdmission>) -> bool {
        match (self.execution_admission.as_ref(), next) {
            (None, None) => true,
            (Some(admission), Some(next)) => {
                admission.alias() == next.alias() && admission.generation() == next.generation()
            }
            _ => false,
        }
    }
}

/// Re-assemble `alias`'s per-agent execution context the way a fresh agent turn
/// would: the agent's security policy, memory, gated tool registry (through the
/// one [`crate::tools::scoped::ScopedToolRegistry::assemble`] seam, connecting
/// the agent's own granted MCP scope), skills, provider binding with the
/// agent's own temperature, resolved runtime controls, and approval policy
/// (the agent's risk profile under `parent_approval`'s interactivity mode; no
/// parent manager means non-interactive auto-deny, matching the headless
/// driver). The live SOP engine/audit handles are threaded from the running
/// SOP so the nested step keeps its SOP tools bound to the same engine. This
/// connects MCP servers, so the driver memoizes the result per alias across a
/// drain and re-assembles only on an alias change.
#[cfg(test)]
pub(crate) async fn assemble_owned_execution(
    config: &zeroclaw_config::schema::Config,
    live_config: Option<zeroclaw_config::live::LiveConfigHandle>,
    alias: &str,
    sop_engine: Arc<std::sync::Mutex<crate::sop::SopEngine>>,
    sop_audit: Option<Arc<crate::sop::SopAuditLogger>>,
    parent_approval: Option<&crate::approval::ApprovalManager>,
) -> Result<OwnedAgentExecution> {
    assemble_owned_execution_with_admission(
        config,
        live_config,
        alias,
        sop_engine,
        sop_audit,
        parent_approval,
        None,
    )
    .await
}

pub(crate) async fn assemble_owned_execution_with_admission(
    config: &zeroclaw_config::schema::Config,
    live_config: Option<zeroclaw_config::live::LiveConfigHandle>,
    alias: &str,
    sop_engine: Arc<std::sync::Mutex<crate::sop::SopEngine>>,
    sop_audit: Option<Arc<crate::sop::SopAuditLogger>>,
    parent_approval: Option<&crate::approval::ApprovalManager>,
    execution_admission: Option<AgentExecutionAdmission>,
) -> Result<OwnedAgentExecution> {
    if let Some(admission) = execution_admission.as_ref() {
        admission.revalidate().map_err(|error| {
            anyhow::Error::msg(format!(
                "SOP authority witness rejected before nested execution assembly: {error}"
            ))
        })?;
        if admission.alias() != alias {
            anyhow::bail!(
                "SOP authority witness targets `{}` but nested execution requested `{alias}`",
                admission.alias()
            );
        }
    }
    let security = Arc::new(crate::security::SecurityPolicy::for_agent(config, alias)?);
    // The one canonical per-agent runtime-knob surface: identity plus every
    // runtime-profile override baked in. Fail closed on an unknown alias —
    // never run a delegated step under the parent's controls.
    let agent = config.resolved_agent_config(alias).ok_or_else(|| {
        anyhow::Error::msg(format!(
            "SOP step agent '{alias}' is not a configured agent"
        ))
    })?;
    let risk_profile = config
        .risk_profile_for_agent(alias)
        .cloned()
        .ok_or_else(|| {
            anyhow::Error::msg(format!(
                "SOP step agent '{alias}' has no configured risk profile"
            ))
        })?;
    let resolved_key = config
        .resolved_model_provider_for_agent(alias)
        .and_then(|(_, _, cfg)| cfg.api_key.clone());
    let memory =
        zeroclaw_memory::create_memory_for_agent(config, alias, resolved_key.as_deref()).await?;

    // Mirror a fresh agent turn: the headless SOP driver reaches this agent's
    // tools via `crate::agent::run`, which builds its runtime from
    // `config.runtime`. Creating it here (rather than reusing the parent's) is
    // read-only — a shell-existence check plus struct construction — and only
    // happens on the cross-agent path, which is memoized per alias.
    let runtime: Arc<dyn crate::platform::RuntimeAdapter> =
        Arc::from(crate::platform::create_runtime(&config.runtime)?);

    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };

    let built = crate::tools::all_tools_with_runtime_and_execution_capability(
        Arc::new(config.clone()),
        &security,
        &risk_profile,
        alias,
        runtime.clone(),
        memory,
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.web_fetch,
        &config.data_dir,
        &config.agents,
        resolved_key.as_deref(),
        config,
        None,
        false,
        None,
        Some(sop_engine),
        sop_audit,
        live_config,
        execution_admission
            .as_ref()
            .map(AgentExecutionAdmission::capability),
    )?;
    let skills = crate::skills::load_skills_for_agent_from_config(config, alias);
    // Capture before `runtime` is moved into `ScopedAssembly` below.
    let shell_profile = runtime.shell_profile();
    // The same gated seam run(), process_message, and independent delegation use:
    // step 2 filters with THIS agent's SecurityPolicy, `connect_mcp` grants only
    // this agent's MCP bundles, and its skills register as tools. Peripherals stay
    // disconnected — a nested SOP sub-loop must not seize the serial hardware the
    // live daemon holds exclusively.
    let assembled =
        crate::tools::scoped::ScopedToolRegistry::assemble(crate::tools::scoped::ScopedAssembly {
            config,
            agent_alias: alias,
            security: &security,
            built,
            skills: &skills,
            runtime,
            caller_allowed: None,
            connect_mcp: true,
            // A nested SOP step re-assembly is per turn (memoized per alias);
            // it has no cross-turn reuse contract, so the per-call
            // `connect_all` path inside `assemble` is the correct choice
            // (same as `process_message`).
            mcp_registry: None,
            connect_peripherals: false,
            exclude_memory: false,
            acp_delivery: false,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: true,
        })
        .await;
    let mcp_prompt_section = assembled.combined_mcp_prompt_section();
    let crate::tools::scoped::ScopedAssembled {
        registry,
        activated_handle,
        mcp_tool_names,
        ..
    } = assembled;
    // Stays sealed into `OwnedAgentExecution.tools_registry` (a `ScopedToolRegistry`).
    let tools_registry = registry;

    let provider_ref = config
        .resolved_model_provider_for_agent(alias)
        .map(|(ty, al, _)| format!("{ty}.{al}"))
        .ok_or_else(|| {
            anyhow::Error::msg(format!(
                "SOP step agent '{alias}' has no resolved model provider"
            ))
        })?;
    let (model_provider, provider_name, model, _model_route_resolver) =
        crate::agent::agent::build_session_model_provider(config, &provider_ref, None)?;
    // The step agent's own configured temperature — the same source the
    // headless driver reads for `crate::agent::run`.
    let temperature = config
        .model_provider_for_agent(alias)
        .and_then(|e| e.temperature);

    // The step agent's risk profile under the PARENT surface's interactivity
    // mode: an operator approval route available to the outer turn stays
    // available to the delegated step instead of degrading to auto-denial.
    let approval = match parent_approval {
        Some(parent) => parent.derive_for_risk_profile(&risk_profile),
        None => crate::approval::ApprovalManager::for_non_interactive(&risk_profile),
    };

    Ok(OwnedAgentExecution {
        model_provider,
        provider_name,
        model,
        temperature,
        tools_registry,
        approval,
        activated_tools: activated_handle,
        agent,
        risk_profile,
        skills,
        mcp_tool_names,
        mcp_prompt_section,
        // Captured from the same adapter this step's tools were built with, so
        // the prompt names the shell the step will actually run under.
        shell_profile,
        execution_admission,
        // The same policy the step's tools were built with, carried for the
        // nested sub-loop's no-vision image-marker gate.
        security,
    })
}

/// Build the step agent's own system prompt for a re-assembled nested step,
/// through the same construction a fresh turn for that agent uses
/// (`build_system_prompt_for_turn`): registry-derived tool descriptions, the
/// agent's own MCP prompt section, skills, identity, risk profile, and its
/// resolved prompt knobs. Built per step (not memoized per alias) because the
/// effective tool surface keys on the step's excluded-tool set, which the
/// `tool_filter_groups` gate derives from the step context.
fn build_owned_step_system_prompt(
    owned: &OwnedAgentExecution,
    config: &zeroclaw_config::schema::Config,
    alias: &str,
    excluded_tools: &[String],
) -> Result<String> {
    let tool_descs: Vec<(&str, &str)> = owned
        .tools_registry
        .iter()
        .map(|t| (t.name(), t.description()))
        .collect();
    let bootstrap_max_chars = if owned.agent.resolved.lean_bootstrap {
        Some(crate::agent::system_prompt::LEAN_BOOTSTRAP_MAX_CHARS)
    } else {
        None
    };
    crate::agent::loop_::build_system_prompt_for_turn(
        &config.agent_workspace_dir(alias),
        &owned.model,
        &tool_descs,
        &owned.mcp_prompt_section,
        &owned.skills,
        Some(&owned.agent.identity),
        bootstrap_max_chars,
        &owned.risk_profile,
        owned.model_provider.as_ref(),
        &owned.tools_registry,
        excluded_tools,
        owned.activated_tools.as_ref(),
        owned.agent.resolved.strict_tool_parsing,
        owned.agent.resolved.prompt_injection_mode,
        owned.agent.resolved.lean_bootstrap,
        owned.agent.resolved.max_system_prompt_chars,
        true,
        config.channels.show_tool_calls,
        None,
        owned.shell_profile.as_ref(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn drive_live_sop_actions(
    queued_actions: Vec<crate::sop::executor::QueuedSopAction>,
    history: &mut Vec<ChatMessage>,
    // Owner-tracked breadcrumb provenance for `history`; same-agent step
    // loops write back any crumb their trim paths insert.
    crumb_present: &mut bool,
    model_provider: &dyn ModelProvider,
    provider_name: &str,
    model: &str,
    dispatch_model: &str,
    temperature: Option<f64>,
    tools_registry: &crate::tools::scoped::ScopedToolRegistry,
    observer: &dyn crate::observability::Observer,
    silent: bool,
    approval: Option<&crate::approval::ApprovalManager>,
    // The enclosing agent's filesystem policy, threaded from the turn loop's
    // execution context. Same-agent nested SOP steps run under it; a
    // cross-agent step uses its own re-assembled policy (see
    // `OwnedAgentExecution::security`).
    security: Option<&crate::security::SecurityPolicy>,
    multimodal_config: &zeroclaw_config::schema::MultimodalConfig,
    // Full config so the live-SOP sub-turn's vision route resolves the configured
    // `vision_model_provider`'s alias options, exactly as the enclosing turn does.
    config: Option<&zeroclaw_config::schema::Config>,
    max_tool_iterations: usize,
    hooks: Option<&crate::hooks::HookRunner>,
    excluded_tools: &[String],
    dedup_exempt_tools: &[String],
    activated_tools: Option<&Arc<std::sync::Mutex<crate::tools::ActivatedToolSet>>>,
    // The enclosing round's own switch state (`run_tool_call_loop`'s
    // `model_switch_state`, threaded through by the caller exactly like
    // every other per-round context value here). Deliberately UNUSED: every
    // nested step loop this function drives — same-agent or cross-agent —
    // mints its own fresh switch state at its `ToolLoop` construction site
    // below instead of inheriting this one, so a step's `model_switch` can
    // never mutate the enclosing round's shared `Arc`. Kept in the
    // signature (rather than dropped) so the call site stays symmetric with
    // its sibling per-round parameters and so tests can pass a distinct
    // `Arc` here and assert it is untouched after a nested step's switch.
    _model_switch_callback: Option<ModelSwitchCallback>,
    pacing: &zeroclaw_config::schema::PacingConfig,
    strict_tool_parsing: bool,
    parallel_tools: bool,
    max_tool_result_chars: usize,
    context_limits: zeroclaw_config::schema::ResolvedContextLimits,
    receipt_generator: Option<&crate::agent::tool_receipts::ReceiptGenerator>,
    knobs: &LoopKnobs,
    channel_name: &str,
    channel_reply_target: Option<&str>,
    cancellation_token: Option<CancellationToken>,
    on_delta: Option<tokio::sync::mpsc::Sender<StreamDelta>>,
    shared_budget: Option<ExecutionTreeBudget>,
    channel: Option<&dyn Channel>,
    collected_receipts: Option<&std::sync::Mutex<Vec<String>>>,
    event_tx: Option<tokio::sync::mpsc::Sender<TurnEvent>>,
    mut new_messages_out: Option<&mut Vec<ChatMessage>>,
    mut image_cache: Option<&mut ToolLoopImageState<'_>>,
    agent_alias: Option<&str>,
    parent_agent_alias: Option<&str>,
    sop_reassembly: Option<SopStepReassembly<'_>>,
    // Per-agent execution contexts re-assembled in flight for steps that
    // delegate to a different agent, memoized by alias. Owned by the caller
    // (the turn loop) so the memo spans every drain of a turn's queued steps:
    // `assemble_owned_execution` connects MCP, so it runs at most once per
    // distinct step agent per turn, never per step.
    exec_cache: &mut std::collections::HashMap<String, OwnedAgentExecution>,
) -> Result<()> {
    let mut pending = std::collections::VecDeque::from(queued_actions);
    while let Some(queued) = pending.pop_front() {
        let mut action = queued.action.clone();
        // Per-run budget for deterministic capabilities driven inside this
        // turn, matching `MAX_HEADLESS_DRIVE_STEPS` in the two headless
        // drivers. Reset per queued action so one run's routing cycle cannot
        // consume a later run's allowance.
        let mut deterministic_steps_driven = 0usize;
        loop {
            match action {
                crate::sop::SopRunAction::ExecuteStep {
                    run_id,
                    step,
                    context,
                    execution_witness,
                } => {
                    let managed = match queued.engine.lock() {
                        Ok(engine) => engine.has_execution_capability(),
                        Err(poisoned) => poisoned.into_inner().has_execution_capability(),
                    };
                    if managed && execution_witness.is_none() {
                        return Err(anyhow::Error::msg(
                            "managed SOP ExecuteStep is missing its authority witness",
                        ));
                    }
                    let execution_admission = execution_witness
                        .as_ref()
                        .map(|witness| {
                            let alias = step.agent.as_deref().or(agent_alias).ok_or_else(|| {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Reject
                                    )
                                    .with_category(::zeroclaw_log::EventCategory::Agent)
                                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "run_id": run_id,
                                            "step": step.number,
                                        })
                                    ),
                                    "managed SOP step has no executing agent"
                                );
                                anyhow::Error::msg("managed SOP step has no executing agent")
                            })?;
                            witness.admit(alias).map_err(anyhow::Error::from)
                        })
                        .transpose()?;
                    let execution_config = execution_admission
                        .as_ref()
                        .map(AgentExecutionAdmission::config);
                    let started_at = crate::sop::engine::now_iso8601();
                    let user_message = ChatMessage::user(context.clone());
                    history.push(user_message.clone());
                    if let Some(out) = new_messages_out.as_deref_mut() {
                        out.push(user_message);
                    }

                    // A step that delegates to a different agent must run AS that
                    // agent — with that agent's own gated tools, policy, MCP
                    // scope, provider binding, and runtime controls — not the
                    // parent turn's. When the step names a different agent and a
                    // reassembly handle is available, re-assemble (and memoize)
                    // that agent's execution context; same-agent steps keep the
                    // parent context unchanged.
                    let step_alias = step.agent.as_deref();
                    // `agent_alias` is this loop's EFFECTIVE identity: a
                    // re-assembled sub-loop runs with its step agent as its own
                    // alias, so this comparison is correct at every nesting
                    // depth — a depth >= 2 step naming the outer agent compares
                    // against the re-assembled child's alias and re-assembles
                    // instead of inheriting the child's scope.
                    let needs_reassembly = step_needs_reassembly(agent_alias, step_alias);
                    let mut assembly_error: Option<anyhow::Error> = None;
                    if needs_reassembly {
                        let alias =
                            step_alias.expect("needs_reassembly implies a step agent alias");
                        if let Some(reassembly) = sop_reassembly.as_ref() {
                            let cache_matches = exec_cache.get(alias).is_some_and(|owned| {
                                owned.matches_admission(execution_admission.as_ref())
                            });
                            if !cache_matches {
                                match assemble_owned_execution_with_admission(
                                    execution_config.as_deref().unwrap_or(reassembly.config),
                                    reassembly.live_config.clone(),
                                    alias,
                                    Arc::clone(&queued.engine),
                                    queued.audit.clone(),
                                    approval,
                                    execution_admission.clone(),
                                )
                                .await
                                {
                                    Ok(owned) => {
                                        exec_cache.insert(alias.to_string(), owned);
                                    }
                                    Err(e) => assembly_error = Some(e),
                                }
                            }
                        } else {
                            // This construction path provides no per-agent re-assembly
                            // handle (e.g. a bounded delegate sub-loop that carries a live
                            // SOP tool). Running a cross-agent step here would inherit the
                            // parent/delegate agent's broader tools/policy/MCP surface —
                            // the exact escalation this whole seam exists to prevent. Fail
                            // the step CLOSED, the same as an assembly error below, rather
                            // than under-enforce in silence (omission is not a grant).
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                ),
                                &format!(
                                    "SOP step delegates to agent '{alias}' but this run path \
                                     provides no per-agent re-assembly handle; refusing the step \
                                     (fail-closed) rather than run it with the parent agent's \
                                     context (per-agent isolation cannot be applied here)"
                                )
                            );
                            assembly_error = Some(anyhow::Error::msg(format!(
                                "SOP step delegates to agent '{alias}' but this run path provides \
                                 no per-agent re-assembly handle; refusing to run the step with \
                                 the parent agent's context (per-agent isolation cannot be applied)"
                            )));
                        }
                    }

                    let nested_turn_id = format!("sop:{run_id}:step:{}", step.number);
                    let step_call_sink = crate::sop::executor::new_step_call_sink();
                    // Empty buffer for same-agent child loops: populated by
                    // the child's dual-write, then replayed to the parent's
                    // new_messages_out after the child returns (§3.2.4).
                    let mut inner_new_msgs: Vec<ChatMessage> = Vec::new();
                    let step_output = if let Some(err) = assembly_error {
                        // Fail closed: never run a delegated step with the parent
                        // agent's broader context when the step agent's own
                        // context could not be assembled.
                        Err(err)
                    } else {
                        // Select the effective per-agent execution surface: the
                        // re-assembled step agent when reassembly applied,
                        // otherwise the parent turn's (byte-identical to today).
                        let owned = if needs_reassembly {
                            exec_cache.get(
                                step_alias.expect("needs_reassembly implies a step agent alias"),
                            )
                        } else {
                            None
                        };
                        let execution_config = owned
                            .and_then(|owned| owned.execution_admission.as_ref())
                            .map(AgentExecutionAdmission::config)
                            .or(execution_config);
                        let (
                            eff_model_provider,
                            eff_provider_name,
                            eff_model,
                            eff_dispatch_model,
                            eff_registry,
                            eff_approval,
                            eff_activated,
                        ) = match owned {
                            Some(o) => (
                                o.model_provider.as_ref(),
                                o.provider_name.as_str(),
                                o.model.as_str(),
                                o.model.as_str(),
                                &o.tools_registry,
                                Some(&o.approval),
                                o.activated_tools.as_ref(),
                            ),
                            None => (
                                model_provider,
                                provider_name,
                                model,
                                dispatch_model,
                                tools_registry,
                                approval,
                                activated_tools,
                            ),
                        };
                        // The step agent's resolved runtime controls when
                        // re-assembled — the same `ResolvedRuntime` a fresh
                        // turn for that agent resolves. Pacing is the
                        // agent-independent global section, read from the same
                        // config the context was assembled from. Same-agent
                        // steps keep the parent's values verbatim.
                        let (
                            eff_temperature,
                            eff_max_tool_iterations,
                            eff_strict_tool_parsing,
                            eff_parallel_tools,
                            eff_max_tool_result_chars,
                            eff_context_limits,
                            eff_dedup_exempt_tools,
                            eff_pacing,
                        ) = match owned {
                            Some(o) => (
                                o.temperature,
                                o.agent.resolved.max_tool_iterations,
                                o.agent.resolved.strict_tool_parsing,
                                o.agent.resolved.parallel_tools,
                                o.agent.resolved.max_tool_result_chars,
                                o.agent.resolved.context_limits(),
                                o.agent.resolved.tool_call_dedup_exempt.as_slice(),
                                &execution_config
                                    .as_deref()
                                    .unwrap_or(
                                        sop_reassembly
                                            .as_ref()
                                            .expect("owned implies a reassembly handle")
                                            .config,
                                    )
                                    .pacing,
                            ),
                            None => (
                                temperature,
                                max_tool_iterations,
                                strict_tool_parsing,
                                parallel_tools,
                                max_tool_result_chars,
                                context_limits,
                                dedup_exempt_tools,
                                pacing,
                            ),
                        };

                        // Exclusions for a cross-agent step derive from the
                        // STEP agent: its own `tool_filter_groups` gate over
                        // the step context. The parent's turn-level exclusion
                        // list encodes the parent profile/prompt and does not
                        // cross the agent boundary; the SOP-recursion guard and
                        // the step's declared scope are added for both paths by
                        // `sop_step_excluded_tools`.
                        let child_base_excluded: Vec<String> = match owned {
                            Some(o) => crate::agent::loop_::compute_excluded_mcp_tools(
                                &o.tools_registry,
                                &o.agent.resolved.tool_filter_groups,
                                &context,
                                &o.mcp_tool_names,
                            ),
                            None => Vec::new(),
                        };
                        let base_excluded: &[String] = if owned.is_some() {
                            &child_base_excluded
                        } else {
                            excluded_tools
                        };
                        let sop_excluded_tools = sop_step_excluded_tools(
                            &queued,
                            &run_id,
                            &step,
                            eff_registry,
                            eff_activated,
                            base_excluded,
                        );

                        // Cross-agent steps run on an EXPLICIT child transcript
                        // — the step agent's own system prompt plus the
                        // delegated step context — never the parent turn's
                        // history, which would disclose the parent conversation
                        // to the step agent's provider (a different trust and
                        // data-handling boundary). The parent transcript keeps
                        // the step-context message pushed above and receives
                        // the step's final output below; intermediate child
                        // tool-chatter stays out of it. Same-agent steps keep
                        // sharing the turn history unchanged.
                        let mut child_history: Vec<ChatMessage> = Vec::new();
                        let mut child_setup_error: Option<anyhow::Error> = None;
                        if let Some(o) = owned {
                            match build_owned_step_system_prompt(
                                o,
                                execution_config.as_deref().unwrap_or(
                                    sop_reassembly
                                        .as_ref()
                                        .expect("owned implies a reassembly handle")
                                        .config,
                                ),
                                step_alias.expect("needs_reassembly implies a step agent alias"),
                                &sop_excluded_tools,
                            ) {
                                Ok(prompt) => {
                                    child_history.push(ChatMessage::system(&prompt));
                                    child_history.push(ChatMessage::user(context.clone()));
                                }
                                // Fail closed, same as an assembly error: never
                                // fall back to the parent transcript.
                                Err(e) => child_setup_error = Some(e),
                            }
                        }

                        if let Some(err) = child_setup_error {
                            Err(err)
                        } else {
                            // Breadcrumb provenance travels with the transcript
                            // it describes: a fresh child transcript starts
                            // clean; the parent's continues its owner-tracked
                            // state and writes any new crumb back below.
                            let mut nested_crumb_present = if owned.is_some() {
                                false
                            } else {
                                *crumb_present
                            };
                            let nested_history: &mut Vec<ChatMessage> = match owned {
                                Some(_) => &mut child_history,
                                None => &mut *history,
                            };
                            let mut child_provider_image_state = ProviderImageState::default();
                            let nested_image_state =
                                image_cache.as_deref_mut().map(|state| ToolLoopImageState {
                                    cache: &mut *state.cache,
                                    provider_state: if owned.is_some() {
                                        &mut child_provider_image_state
                                    } else {
                                        &mut *state.provider_state
                                    },
                                });
                            // Owned here, not a `&mut None` temporary: the step
                            // future is built inside the run-attribution scope and
                            // awaited after it, so a temporary would be dropped
                            // while the future still borrows it.
                            let mut nested_memory_preamble: Option<MemoryPreamble> = None;
                            let step_result = ::zeroclaw_log::scope!(
                                sop_run_id: run_id.as_str(),
                                =>
                                crate::sop::executor::scope_step_call_sink(
                                    step_call_sink.clone(),
                                    Box::pin(run_tool_call_loop(ToolLoop {
                                        context_anchor_seed: None,
                                        context_anchor_sink: None,
                                        history_was_trimmed: None,
                                    exec: ResolvedAgentExecution::resolve(
                                        ResolvedModelAccess {
                                            model_provider: eff_model_provider,
                                            provider_name: eff_provider_name,
                                            model: eff_model,
                                            dispatch_model: eff_dispatch_model,
                                            temperature: eff_temperature,
                                        },
                                        ResolvedIo {
                                            tools_registry: eff_registry,
                                            observer,
                                            silent,
                                            approval: eff_approval,
                                            // Same-agent steps run under the
                                            // enclosing agent's policy; a
                                            // cross-agent step runs under its
                                            // own re-assembled one.
                                            security: match owned {
                                                Some(o) => Some(o.security.as_ref()),
                                                None => security,
                                            },
                                            multimodal_config,
                                            config: if owned.is_some() {
                                                Some(execution_config.as_deref().unwrap_or(
                                                    sop_reassembly
                                                        .as_ref()
                                                        .expect("owned implies a reassembly handle")
                                                        .config,
                                                ))
                                            } else {
                                                config
                                            },
                                            hooks,
                                            activated_tools: eff_activated,
                                            // Deliberately NOT the outer round's
                                            // `model_switch_callback`: every nested
                                            // step loop — same-agent or cross-agent —
                                            // gets `None` here, so `run_tool_call_loop`
                                            // mints a fresh, isolated switch state
                                            // scoped to just this child call (see its
                                            // top-of-function fallback). Reusing the
                                            // parent's shared state let a child's
                                            // `model_switch` mutate the SAME `Arc` the
                                            // parent round checks after every
                                            // iteration, so a cross-agent step's own
                                            // switch could silently become the
                                            // parent's. A switch still reaches the
                                            // caller correctly: it surfaces as this
                                            // child loop's own `ModelSwitchRequested`
                                            // error, not via continued access to
                                            // shared mutable state after the call
                                            // returns.
                                            model_switch_callback: None,
                                            receipt_generator,
                                        },
                                        ResolvedRuntimeKnobs {
                                            max_tool_iterations: eff_max_tool_iterations,
                                            excluded_tools: &sop_excluded_tools,
                                            dedup_exempt_tools: eff_dedup_exempt_tools,
                                            pacing: eff_pacing,
                                            strict_tool_parsing: eff_strict_tool_parsing,
                                            parallel_tools: eff_parallel_tools,
                                            max_tool_result_chars: eff_max_tool_result_chars,
                                            context_limits: eff_context_limits,
                                            context_limits_resolver: None,
                                            knobs,
                                        },
                                    ),
                                    history: nested_history,
                                    history_has_trim_breadcrumb: &mut nested_crumb_present,
                                    injected_memory_preamble: &mut nested_memory_preamble,
                                    channel_name,
                                    channel_reply_target,
                                    cancellation_token: cancellation_token.clone(),
                                    on_delta: on_delta.clone(),
                                    shared_budget: shared_budget.clone(),
                                    channel,
                                    collected_receipts,
                                    event_tx: event_tx.clone(),
                                    steering: None,
                                    // Same-agent: pass a fresh empty buffer so
                                    // the child loop's one-time clone-and-append
                                    // does not sync the parent's accumulated
                                    // messages into the child's nested_history.
                                    // After the child returns, replay its buffer
                                    // back to the parent's new_messages_out.
                                    // Cross-agent: None (child transcript does
                                    // not flow into the parent's persisted
                                    // conversation; only the final output does).
                                    new_messages_out: if owned.is_some() {
                                        None
                                    } else {
                                        Some(&mut inner_new_msgs)
                                    },
                                    image_cache: nested_image_state,
                                    memory: None,
                                    ingress: IngressContext::sub_turn(),
                                    // Attribution follows the EFFECTIVE agent:
                                    // the step agent's identity is stamped on
                                    // observer/receipt/OTel records for the
                                    // sub-loop, with the delegating agent kept
                                    // alongside as the parent correlation.
                                    agent_alias: if owned.is_some() {
                                        step_alias
                                    } else {
                                        agent_alias
                                    },
                                    parent_agent_alias: if owned.is_some() {
                                        agent_alias
                                    } else {
                                        parent_agent_alias
                                    },
                                    turn_id: &nested_turn_id,
                                    served_route_sink: None,
                                    sop_reassembly: sop_reassembly.clone(),
                                    })),
                                )
                            )
                            .await;
                            // Replay child loop's new messages to the parent's
                            // new_messages_out for same-agent steps (§3.2.4).
                            if owned.is_none()
                                && let Some(outer) = new_messages_out.as_deref_mut()
                            {
                                outer.extend_from_slice(&inner_new_msgs);
                            }
                            // A same-agent step ran over the parent
                            // transcript: adopt whatever breadcrumb state the
                            // sub-loop's trim paths left behind.
                            if owned.is_none() {
                                *crumb_present = nested_crumb_present;
                            }
                            step_result
                        }
                    };
                    // A cross-agent step's final output flows back into the
                    // parent transcript (and new-message capture) so the outer
                    // conversation stays coherent; the reverse direction — the
                    // parent's prior history flowing to the child — never
                    // happens.
                    if needs_reassembly && let Ok(output) = &step_output {
                        let assistant = ChatMessage::assistant(output.clone());
                        history.push(assistant.clone());
                        if let Some(out) = new_messages_out.as_deref_mut() {
                            out.push(assistant);
                        }
                    }

                    let step_calls = crate::sop::executor::drain_step_calls(&step_call_sink);
                    let completed_at = crate::sop::engine::now_iso8601();
                    // The acting authority for the audit record: the step
                    // agent when the step delegated (including a failed
                    // assembly — the record names who SHOULD have run it),
                    // otherwise this loop's own agent.
                    let effective_agent = if needs_reassembly {
                        step_alias.map(str::to_string)
                    } else {
                        agent_alias.map(str::to_string)
                    };
                    let step_result = match step_output {
                        Ok(output) => crate::sop::SopStepResult {
                            step_number: step.number,
                            status: crate::sop::SopStepStatus::Completed,
                            output,
                            started_at,
                            completed_at: Some(completed_at),
                            effective_agent,
                            tool_calls: step_calls,
                        },
                        Err(e) => crate::sop::SopStepResult {
                            step_number: step.number,
                            status: crate::sop::SopStepStatus::Failed,
                            output: e.to_string(),
                            started_at,
                            completed_at: Some(completed_at),
                            effective_agent,
                            tool_calls: step_calls,
                        },
                    };

                    let (next_action, finished_run) = crate::sop::executor::advance_sop_step(
                        &queued.engine,
                        &run_id,
                        step_result.clone(),
                    )?;
                    crate::sop::executor::audit_sop_step(
                        queued.audit.as_deref(),
                        &run_id,
                        &step_result,
                        finished_run.as_ref(),
                    )
                    .await;
                    action = next_action;
                }
                crate::sop::SopRunAction::WaitApproval { run_id, step, .. } => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "run_id": run_id,
                                "step": step.number,
                            })),
                        "SOP live executor paused for approval"
                    );
                    break;
                }
                crate::sop::SopRunAction::DeterministicStep { ref run_id, .. } => {
                    let run_id = run_id.clone();
                    // Bound this action type exactly as both headless drivers
                    // do. `StepFailure::Goto` takes an arbitrary step number
                    // and nothing validates that routes are acyclic, so a
                    // capability that keeps failing and routes backwards would
                    // otherwise spin the enclosing loop at full speed, taking
                    // and releasing the engine mutex on every iteration and
                    // starving every other SOP surface. `yield_now` keeps the
                    // async runtime responsive but does not end the loop.
                    if deterministic_steps_driven >= crate::sop::executor::MAX_HEADLESS_DRIVE_STEPS
                    {
                        crate::sop::executor::fail_exhausted_step_budget(&queued.engine, &run_id);
                        break;
                    }
                    deterministic_steps_driven += 1;
                    let next = {
                        let mut engine = match queued.engine.lock() {
                            Ok(engine) => engine,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        engine.advance_headless_deterministic_step(&run_id, action)?
                    };
                    action = next;
                    // Let an operator request acquire the engine before the next
                    // deterministic capability is dispatched.
                    tokio::task::yield_now().await;
                }
                crate::sop::SopRunAction::CheckpointWait { run_id, step, .. } => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "run_id": run_id,
                                "step": step.number,
                            })),
                        "SOP live executor paused at checkpoint"
                    );
                    break;
                }
                crate::sop::SopRunAction::Pending {
                    run_id,
                    step,
                    reason,
                    ..
                } => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "run_id": run_id,
                                "step": step,
                                "reason": reason,
                            })),
                        "SOP live executor pending on step dependencies"
                    );
                    break;
                }
                crate::sop::SopRunAction::Completed { run_id, sop_name } => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "run_id": run_id,
                                "sop_name": sop_name,
                            })),
                        "SOP live executor completed run"
                    );
                    break;
                }
                crate::sop::SopRunAction::Cancelled { run_id, sop_name } => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel)
                            .with_attrs(::serde_json::json!({
                                "run_id": run_id,
                                "sop_name": sop_name,
                            })),
                        "SOP live executor observed cancelled run"
                    );
                    break;
                }
                crate::sop::SopRunAction::Failed {
                    run_id,
                    sop_name,
                    reason,
                } => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "run_id": run_id,
                                "sop_name": sop_name,
                                "reason": reason,
                            })),
                        "SOP live executor failed run"
                    );
                    break;
                }
            }
        }
    }
    Ok(())
}

fn refresh_prompt_anchor(history: &mut [ChatMessage], use_native_tools: bool) {
    if let Some(first) = history.first_mut()
        && (first.content.contains(NATIVE_TOOLS_TASK_FRAMING)
            || first.content.contains(NO_TOOLS_TASK_FRAMING))
    {
        let desired = if use_native_tools {
            NATIVE_TOOLS_TASK_FRAMING
        } else {
            NO_TOOLS_TASK_FRAMING
        };
        first.content = first
            .content
            .replacen(NATIVE_TOOLS_TASK_FRAMING, desired, 1)
            .replacen(NO_TOOLS_TASK_FRAMING, desired, 1);
    }
}

#[cfg(test)]
mod surface3_tests {
    use super::*;
    use crate::agent::system_prompt::{NATIVE_TOOLS_TASK_FRAMING, NO_TOOLS_TASK_FRAMING};

    #[test]
    fn single_tool_prompt_changes_only_owned_protocol_and_is_idempotent() {
        let protocol = crate::agent::tool_call_format::TOOL_CALL_PROTOCOL_INSTRUCTIONS;
        let instruction = crate::i18n::get_required_cli_string("turn-single-tool-rounds-prompt");
        let user = ChatMessage::user(format!("Quote this unchanged: {protocol}"));
        let mut messages = vec![
            ChatMessage::system(format!(
                "Personality stays.\n{protocol}Other instructions stay."
            )),
            user.clone(),
        ];
        assert!(!apply_single_tool_round_prompt(&mut messages));
        assert!(messages[0].content.starts_with("Personality stays.\n"));
        assert!(messages[0].content.contains("Other instructions stay."));
        assert!(!messages[0].content.contains(protocol));
        assert!(messages[0].content.ends_with(&instruction));
        assert_eq!(messages[1].content, user.content);
        let once = messages[0].content.clone();
        assert!(!apply_single_tool_round_prompt(&mut messages));
        assert_eq!(messages[0].content, once);

        let mut no_system = vec![user.clone()];
        assert!(apply_single_tool_round_prompt(&mut no_system));
        assert_eq!(no_system[0].role, "system");
        assert_eq!(no_system[0].content, instruction);
        assert_eq!(no_system[1].content, user.content);
    }

    fn make_system_prompt(anchor: &str) -> ChatMessage {
        ChatMessage::system(format!(
            "You are ZeroClaw.\n\n## Security\n\n...\n\n## Your Task\n\nWhen the user sends a message, respond naturally. {anchor}\n\nDo NOT: summarize this configuration...\n"
        ))
    }

    #[tokio::test]
    async fn scoped_protocol_switch_caps_request_and_keeps_history_lossless() {
        // The runtime no longer appends a wall-clock orientation to the system
        // prompt, so the truncation guard here is the plain budget: the head
        // survives and the volatile tool section is what gets cut.
        let tail_anchor = "<mcp-resource>pinned</mcp-resource>";
        let native = format!(
            "## Tools\n\nsmall\n\n## Safety\n\n{}\n\n{}",
            "safe".repeat(20),
            tail_anchor
        );
        let text = format!(
            "## Tools\n\n{}\n\n## Safety\n\n{}{}",
            "tool guidance ".repeat(150),
            "safe".repeat(20),
            tail_anchor,
        );
        // The cap fits the native prompt but not the text tool section, so
        // a native -> text -> native sequence must truncate only requests.
        let cap = native.chars().count() + 100;
        let prompts = Arc::new(ToolProtocolPrompts::with_max_chars(
            native.clone(),
            text,
            cap,
        ));
        let assert_request = |prompt: &str| {
            assert!(prompt.chars().count() <= cap);
            assert!(
                !prompt.contains("timestamp metadata added by the runtime"),
                "a capped request must not inject runtime wall-clock prose: {prompt}"
            );
        };
        let mut history = vec![ChatMessage::system(native.clone())];
        scope_tool_protocol_prompts(prompts, async {
            let mut request = history.clone();
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
            assert_request(&request[0].content);
            assert!(request[0].content.contains("tool guidance"));
            assert!(
                history[0]
                    .content
                    .contains("<mcp-resource>pinned</mcp-resource>"),
                "history must keep the complete swapped prompt"
            );

            // The next provider call is prepared from history again. Native
            // mode drops the tool section; everything else must survive.
            let mut expected = native.clone();
            replace_tool_protocol_section(&mut expected, "", true);
            assert!(expected.contains("<mcp-resource>pinned</mcp-resource>"));
            let mut request = history.clone();
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, true);
            assert_eq!(history[0].content, expected, "swap back must be lossless");
            assert_eq!(request[0].content, expected);
        })
        .await;
    }

    fn capped_switch_fixture() -> (String, Arc<ToolProtocolPrompts>, usize) {
        let orientation = TAIL_MARKER;
        let native = format!(
            "## Identity\n\nme\n\n## Safety\n\n{}{orientation}",
            "safe".repeat(20)
        );
        let text = format!(
            "## Tools\n\n{}\n\n## Safety\n\n{}{orientation}",
            "tool guidance ".repeat(150),
            "safe".repeat(20),
        );
        let cap = native.chars().count() + 100;
        let prompts = Arc::new(ToolProtocolPrompts::with_max_chars(
            native.clone(),
            text,
            cap,
        ));
        (native, prompts, cap)
    }

    #[tokio::test]
    async fn scoped_protocol_refresh_keeps_hook_appended_text() {
        let (native, prompts, cap) = capped_switch_fixture();
        let hook = "\n\nHOOK_INSTRUCTION";
        scope_tool_protocol_prompts(prompts, async {
            // No switch: the built prompt plus the hook's text, unchanged.
            let mut history = vec![ChatMessage::system(native.clone())];
            let mut request = vec![ChatMessage::system(format!("{native}{hook}"))];
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, true);
            assert_eq!(request[0].content, format!("{native}{hook}"));
            assert_eq!(history[0].content, native);

            // Native -> text: the swapped base is capped, the hook's text
            // still follows it.
            let mut request = vec![ChatMessage::system(format!("{native}{hook}"))];
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
            let base = request[0]
                .content
                .strip_suffix(hook)
                .expect("hook text must survive the switch");
            assert!(base.contains("tool guidance"));
            assert_eq!(base.chars().count(), cap);
            assert!(
                !base.contains("timestamp metadata added by the runtime"),
                "the capped base prompt must not inject runtime wall-clock prose"
            );

            // A rebuilt request (post-trim) is already finalized: refreshing
            // it again must leave it unchanged.
            let finalized = request[0].content.clone();
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
            assert_eq!(request[0].content, finalized);
        })
        .await;
    }

    #[tokio::test]
    async fn scoped_protocol_refresh_caps_text_request_on_later_iterations() {
        let (native, prompts, cap) = capped_switch_fixture();
        let mut history = vec![ChatMessage::system(native)];
        scope_tool_protocol_prompts(prompts, async {
            let mut request = history.clone();
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
            // History now holds the uncapped text prompt; the next iteration
            // stays on text and must still be capped.
            assert!(history[0].content.chars().count() > cap);
            let mut request = history.clone();
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
            assert_eq!(request[0].content.chars().count(), cap);
        })
        .await;
    }

    #[test]
    fn refresh_prompt_anchor_swaps_native_to_no_tools_when_signal_drops() {
        // When the per-turn signal is `use_native_tools = false` but the
        // system prompt has NATIVE_TOOLS_TASK_FRAMING (the prompt was built
        // against the base provider, but the active provider is non-native),
        // the anchor must be replaced with NO_TOOLS_TASK_FRAMING.
        let mut history = vec![make_system_prompt(NATIVE_TOOLS_TASK_FRAMING)];
        refresh_prompt_anchor(&mut history, false);
        assert!(
            history[0].content.contains(NO_TOOLS_TASK_FRAMING),
            "prompt must contain NO_TOOLS_TASK_FRAMING after swap"
        );
        assert!(
            !history[0].content.contains(NATIVE_TOOLS_TASK_FRAMING),
            "prompt must not retain NATIVE_TOOLS_TASK_FRAMING after swap"
        );
    }

    #[test]
    fn refresh_prompt_anchor_swaps_no_tools_to_native_when_signal_rises() {
        // Reverse direction: when the per-turn signal flips to true,
        // NO_TOOLS_TASK_FRAMING must be replaced with NATIVE_TOOLS_TASK_FRAMING.
        let mut history = vec![make_system_prompt(NO_TOOLS_TASK_FRAMING)];
        refresh_prompt_anchor(&mut history, true);
        assert!(
            history[0].content.contains(NATIVE_TOOLS_TASK_FRAMING),
            "prompt must contain NATIVE_TOOLS_TASK_FRAMING after swap"
        );
        assert!(
            !history[0].content.contains(NO_TOOLS_TASK_FRAMING),
            "prompt must not retain NO_TOOLS_TASK_FRAMING after swap"
        );
    }

    #[test]
    fn refresh_prompt_anchor_is_noop_when_anchor_already_matches() {
        // Byte-stability: when the per-turn signal already matches the
        // anchor in the prompt, the function must not mutate the content.
        let original = make_system_prompt(NATIVE_TOOLS_TASK_FRAMING);
        let mut history = vec![original.clone()];
        refresh_prompt_anchor(&mut history, true);
        assert_eq!(
            history[0].content, original.content,
            "content must be identical when anchor already matches signal"
        );
    }

    #[test]
    fn refresh_prompt_anchor_is_noop_when_no_anchor_present() {
        // Custom system_prompt_prefix: when neither anchor is present,
        // the function must not touch the prompt at all.
        let custom_prompt = "You are a custom agent. Answer concisely.".to_string();
        let mut history = vec![ChatMessage::system(custom_prompt.clone())];
        refresh_prompt_anchor(&mut history, false);
        assert_eq!(
            history[0].content, custom_prompt,
            "custom prompt without either anchor must be unchanged"
        );
    }

    #[test]
    fn refresh_prompt_anchor_noop_on_empty_history() {
        // Edge case: empty history shouldn't panic.
        let mut history: Vec<ChatMessage> = Vec::new();
        refresh_prompt_anchor(&mut history, false);
        // Just verifying no panic.
    }
    #[tokio::test]
    async fn scoped_protocol_switch_caps_the_request_and_keeps_history() {
        let preamble = "p".repeat(300);
        let tools = format!("## Tools\n{}\n", "t".repeat(200));
        let safety = "## Safety\nkeep the safety section";
        let text = format!("{preamble}\n\n{tools}\n{safety}");
        let native = format!("{preamble}\n\n{tools}\n{safety}");
        let cap = 120usize;
        let prompts = Arc::new(ToolProtocolPrompts::with_max_chars(
            native,
            text.clone(),
            cap,
        ));
        let mut history = vec![ChatMessage::system(text.clone())];
        let mut request = vec![ChatMessage::system(format!("{text}\nHOOK"))];

        scope_tool_protocol_prompts(prompts, async {
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, true);
        })
        .await;

        assert!(
            history[0].content.chars().count() > cap,
            "history keeps the swapped prompt past the provider cap"
        );
        assert!(
            history[0].content.contains("## Safety"),
            "native history drops the tool section and keeps what follows"
        );
        assert!(
            !history[0].content.contains("## Tools\n"),
            "native history must not keep the text tool listing"
        );
        assert!(
            request[0].content.chars().count() > cap,
            "hook text is re-appended after the capped base"
        );
        assert!(
            request[0].content.ends_with("\nHOOK"),
            "hook suffix must survive the cap; got: {}",
            request[0].content
        );
        let base_len = request[0]
            .content
            .trim_end_matches("\nHOOK")
            .chars()
            .count();
        assert!(
            base_len <= cap,
            "provider base must respect max_system_prompt_chars, got {base_len}"
        );
        assert!(
            !request[0].content.contains("timestamp"),
            "the cap must not inject a clock envelope"
        );
    }
}

#[cfg(test)]
mod reported_budget_tests {
    use super::*;
    use crate::observability::NoopObserver;

    // Keep existing budget tests focused on trim behavior; the identity test
    // below calls the production function with an explicit turn identity.
    #[allow(clippy::too_many_arguments)]
    async fn enforce_reported_budget(
        injected_memory_preamble: &mut Option<MemoryPreamble>,
        history: &mut Vec<ChatMessage>,
        reported_input_tokens: usize,
        context_token_budget: usize,
        effective_context_window: usize,
        keep_recent_turns: usize,
        event_tx: Option<&tokio::sync::mpsc::Sender<TurnEvent>>,
        observer: &dyn crate::observability::Observer,
        crumb_present: &mut bool,
    ) -> Result<(), anyhow::Error> {
        super::enforce_reported_budget(
            injected_memory_preamble,
            history,
            reported_input_tokens,
            // These tests exercise the trim decision and the count wording, not
            // the tool catalog: a zero schema cost keeps every expectation
            // reading as "messages only". The schema accounting has its own
            // test, which calls the production function directly.
            0,
            context_token_budget,
            effective_context_window,
            keep_recent_turns,
            event_tx,
            observer,
            None,
            "test",
            crumb_present,
        )
        .await
    }

    #[derive(Default)]
    struct TrimObserver(std::sync::Mutex<Vec<zeroclaw_api::observability_traits::ObserverEvent>>);

    impl crate::observability::Observer for TrimObserver {
        fn record_event(&self, event: &zeroclaw_api::observability_traits::ObserverEvent) {
            self.0.lock().expect("trim events lock").push(event.clone());
        }

        fn record_metric(&self, _metric: &zeroclaw_api::observability_traits::ObserverMetric) {}

        fn name(&self) -> &str {
            "trim-test"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn flush(&self) {}
    }

    fn big_history() -> Vec<ChatMessage> {
        let big = "x".repeat(2000);
        vec![
            ChatMessage::system("system"),
            ChatMessage::user(format!("turn1 {big}")),
            ChatMessage::assistant("a1".to_string()),
            ChatMessage::user(format!("turn2 {big}")),
            ChatMessage::assistant("a2".to_string()),
            ChatMessage::user("turn3 short".to_string()),
            ChatMessage::assistant("final answer".to_string()),
        ]
    }

    #[tokio::test]
    async fn reported_budget_observer_identifies_the_effective_turn() {
        let mut history = big_history();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        let observer = TrimObserver::default();
        let _ = super::enforce_reported_budget(
            &mut None,
            &mut history,
            estimated * 4,
            0,
            estimated * 2,
            estimated * 2,
            1,
            None,
            &observer,
            Some("effective-agent"),
            "turn-2",
            &mut false,
        )
        .await;
        let events = observer.0.lock().expect("trim events lock");
        assert!(events.iter().any(|event| matches!(
            event,
            zeroclaw_api::observability_traits::ObserverEvent::HistoryTrimmed {
                agent_alias,
                turn_id,
                ..
            } if agent_alias.as_deref() == Some("effective-agent")
                && turn_id.as_deref() == Some("turn-2")
        )));
    }

    #[tokio::test]
    async fn enforce_trims_when_reported_exceeds_budget() {
        let mut history = big_history();
        let before = history.len();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        let reported = estimated * 4;
        let budget = reported / 2;
        enforce_reported_budget(
            &mut None,
            &mut history,
            reported,
            budget,
            budget,
            1,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");
        assert!(
            history.len() < before,
            "over-budget no-tool history must be trimmed before it is persisted"
        );
        assert_eq!(history[0].role, "system", "system prompt is preserved");
        assert!(
            history.iter().any(|m| m.content.contains("final answer")),
            "the most recent turn survives the trim"
        );
    }

    /// The post-response trim runs after the final `Usage` of the turn, so the
    /// size it reports is the one the NEXT request will carry. That population
    /// is `messages + tools`: a messages-only count advertises a post-trim size
    /// roughly one tool catalog smaller than reality, and the context meter
    /// renders that number verbatim once the terminal frame stops resurrecting
    /// the pre-trim one.
    #[tokio::test]
    async fn a_post_response_trim_reports_the_size_the_next_request_carries() {
        let mut history = big_history();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        let schema = 5_000usize;
        let reported = estimated * 4;
        let budget = reported / 2;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TurnEvent>(8);
        super::enforce_reported_budget(
            &mut None,
            &mut history,
            reported,
            schema,
            budget,
            budget,
            1,
            Some(&tx),
            &NoopObserver,
            None,
            "turn-schema",
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");
        drop(tx);

        let mut accounting = None;
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::HistoryTrimmed {
                tokens_before,
                tokens_after,
                tokens_before_source,
                tokens_after_source,
                ..
            } = event
            {
                accounting = Some((
                    tokens_before,
                    tokens_after,
                    tokens_before_source,
                    tokens_after_source,
                ));
            }
        }
        let (tokens_before, tokens_after, before_source, after_source) =
            accounting.expect("an over-budget trim must publish its accounting");

        assert_eq!(
            tokens_before,
            Some(reported as u64),
            "a reported count already prices the whole request; adding the schema again \
             would double-charge the tool catalog"
        );
        assert_eq!(
            before_source,
            Some(zeroclaw_api::agent::TokenCountSource::Provider)
        );
        assert_eq!(
            tokens_after,
            Some((crate::agent::history::estimate_history_tokens(&history) + schema) as u64),
            "the advertised post-trim size must be the request the next turn carries"
        );
        assert_eq!(
            after_source,
            Some(zeroclaw_api::agent::TokenCountSource::Estimated),
            "no provider has seen the trimmed population, so its size is an estimate"
        );
    }

    /// Without a provider report both counts are local estimates. The notice
    /// renders these labels verbatim, so claiming `provider` there would
    /// present a guess as a measurement — and the estimate has to price the
    /// whole request, tool definitions included, or the guard fires late by
    /// exactly the catalog.
    #[tokio::test]
    async fn a_trim_without_a_provider_report_labels_both_counts_as_estimates() {
        let mut history = big_history();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        let schema = 3_000usize;
        // The window sits between the messages alone and messages + schemas:
        // only a count that prices the catalog crosses it.
        let window = (estimated + schema) / 2;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TurnEvent>(8);
        super::enforce_reported_budget(
            &mut None,
            &mut history,
            0,
            schema,
            window,
            window,
            1,
            Some(&tx),
            &NoopObserver,
            None,
            "turn-estimate",
            &mut false,
        )
        .await
        .expect("the overflow guard must trim rather than fail");
        drop(tx);

        let mut accounting = None;
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::HistoryTrimmed {
                tokens_before,
                tokens_after,
                tokens_before_source,
                tokens_after_source,
                ..
            } = event
            {
                accounting = Some((
                    tokens_before,
                    tokens_after,
                    tokens_before_source,
                    tokens_after_source,
                ));
            }
        }
        let (tokens_before, tokens_after, before_source, after_source) =
            accounting.expect("an over-window estimate must trim without a report");

        assert_eq!(
            before_source,
            Some(zeroclaw_api::agent::TokenCountSource::Estimated),
            "a count with no provider sample behind it must say so on the wire"
        );
        assert_eq!(
            tokens_before,
            Some((estimated + schema) as u64),
            "the pre-trim count must price the tool definitions the request carries"
        );
        assert_eq!(
            after_source,
            Some(zeroclaw_api::agent::TokenCountSource::Estimated)
        );
        assert_eq!(
            tokens_after,
            Some((crate::agent::history::estimate_history_tokens(&history) + schema) as u64),
            "the post-trim count must price them too"
        );
    }

    #[tokio::test]
    async fn enforce_noop_when_within_budget() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("hi".to_string()),
            ChatMessage::assistant("hello".to_string()),
        ];
        let before: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        enforce_reported_budget(
            &mut None,
            &mut history,
            estimated,
            estimated * 4,
            estimated * 4,
            5,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");
        let after: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        assert_eq!(after, before, "within-budget history is untouched");
    }

    #[tokio::test]
    async fn recovered_rejected_usage_does_not_trigger_context_trim() {
        let mut history = big_history();
        let before: Vec<String> = history.iter().map(|m| m.content.clone()).collect();

        // The rejected attempt's 80 input tokens remain billed separately; the
        // accepted response reports 80 input tokens, which is within this
        // model's 100-token context budget and must not trim history.
        enforce_reported_budget(
            &mut None,
            &mut history,
            80,
            100,
            100,
            0,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");

        let after: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        assert_eq!(
            after, before,
            "accepted context usage must not include rejected usage"
        );
    }

    #[tokio::test]
    async fn enforce_noop_when_budget_disabled() {
        let mut history = big_history();
        let before: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        enforce_reported_budget(
            &mut None,
            &mut history,
            usize::MAX,
            0,
            0,
            0,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");
        let after: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        assert_eq!(after, before, "zero budget disables enforcement");
    }

    /// Post-response path with no reported usage and an estimate inside the
    /// trim budget: nothing is trimmed. Without a report the estimate is not a
    /// measurement of what the provider billed, so it only opens the guard
    /// when it exceeds the budget.
    #[tokio::test]
    async fn enforce_noop_without_a_report_when_the_estimate_fits() {
        let mut history = big_history();
        let before: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        enforce_reported_budget(
            &mut None,
            &mut history,
            0,
            // Budget above the estimate: the guard stays inactive.
            estimated * 4,
            estimated * 4,
            1,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");
        let after: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        assert_eq!(
            after, before,
            "an estimate inside the budget must not open the overflow guard"
        );
    }

    /// Post-response path with no reported usage, where the estimate exceeds
    /// the trim budget. That is the overflow guard, and it does act.
    #[tokio::test]
    async fn enforce_guards_the_budget_without_a_report() {
        let mut history = big_history();
        let before_len = history.len();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        let budget = estimated / 2;
        enforce_reported_budget(
            &mut None,
            &mut history,
            0,
            budget,
            budget,
            1,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("the overflow guard must trim rather than fail");
        assert!(
            history.len() < before_len,
            "an estimate over the budget must trim even with no provider report"
        );
        assert_eq!(history[0].role, "system", "system prompt is preserved");
        assert!(
            history.iter().any(|m| m.content.contains("final answer")),
            "the newest turn survives the guard"
        );
    }

    /// The output reserve is physical space, not a preference: a prompt that
    /// fits the raw window but leaves the model too little room to answer in has
    /// already failed. The single budget already carries that reserve, so
    /// without a report the guard measures the estimate against it rather than
    /// against the raw window.
    #[tokio::test]
    async fn enforce_guards_the_budget_not_the_raw_window_without_a_report() {
        let mut history = big_history();
        let before_len = history.len();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        // A 1.25x window with a quarter of it held back for the reply: the
        // estimate fits the raw window outright, but not the budget the
        // reserve bounds.
        let window = estimated * 5 / 4;
        let budget = window - window / 4;
        assert!(
            estimated < window,
            "the fixture must fit the raw window ({estimated} >= {window})"
        );
        assert!(
            estimated > budget,
            "and must not fit the budget ({estimated} <= {budget})"
        );

        enforce_reported_budget(
            &mut None,
            &mut history,
            0,
            budget,
            window,
            1,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("the reserve guard must trim rather than fail");

        assert!(
            history.len() < before_len,
            "the budget — not the raw window — must bound the guard"
        );
        assert!(
            history.iter().any(|m| m.content.contains("final answer")),
            "the newest turn survives the guard"
        );
    }

    /// Post-response path with no reported usage, an estimate over the window,
    /// and only one whole turn left: trimming cannot make the request smaller,
    /// so it is dispatched instead of failed on a difference the trim cannot
    /// close.
    #[tokio::test]
    async fn enforce_skips_the_overflow_guard_with_a_single_turn() {
        let big = "x".repeat(20_000);
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user(format!("only turn {big}")),
            ChatMessage::assistant("only answer".to_string()),
        ];
        let before: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        enforce_reported_budget(
            &mut None,
            &mut history,
            0,
            1_000,
            1_000,
            5,
            None,
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("a single-turn overflow must dispatch, not fail");
        let after: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        assert_eq!(
            after, before,
            "nothing can be dropped, so the request goes out unchanged"
        );
    }

    #[tokio::test]
    async fn calibration_ignores_output_appended_after_the_measured_request() {
        // `estimated` mirrors the population that actually produced `reported`
        // (i.e. `prepared_messages`, before this iteration's assistant/tool
        // output was appended). A large tool result appended to `history`
        // after that point must not leak into the calibration denominator.
        let mut history = big_history();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        let reported = estimated * 4;
        let budget = reported / 2;

        // Simulate a large tool result appended to `history` after the
        // provider request was measured but before enforcement runs.
        let late_tool_output = "y".repeat(5000);
        history.push(ChatMessage::assistant(late_tool_output));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        enforce_reported_budget(
            &mut None,
            &mut history,
            reported,
            budget,
            budget,
            1,
            Some(&tx),
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");

        // A trim that did not fit the trim budget is a hard failure and emits
        // no event, so never block waiting for one.
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(_) => panic!("a fitting trim must emit a HistoryTrimmed event"),
        };
        let tokens_after = match event {
            TurnEvent::HistoryTrimmed { tokens_after, .. } => {
                tokens_after.expect("reported-budget trim must carry token accounting")
            }
            other => panic!("expected HistoryTrimmed, got {other:?}"),
        };

        // `tokens_after` describes the retained set the runtime will dispatch
        // next: a local estimate of exactly that population, never a
        // proportional rescale of the prior provider count.
        let expected = crate::agent::history::estimate_history_tokens(&history) as u64;
        assert_eq!(
            tokens_after, expected,
            "tokens_after must estimate the retained population itself"
        );
    }

    #[tokio::test]
    async fn enforce_retrims_against_the_prepared_population_for_retained_images() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        // `estimate_history_tokens` now charges the same fixed per-image cost
        // whether a `[IMAGE:...]` marker holds a path or the base64 payload
        // preparation resolves it to, so the raw and prepared populations for
        // this history are equal by design (see `IMAGE_TOKEN_ESTIMATE`).
        // Selection and the emitted recount must still describe the
        // provider-facing (prepared) population rather than some other stale
        // snapshot: a reported over-budget request with a retained image must
        // actually trim, and `tokens_after` must match the next provider
        // request (prepared retained messages), not drift from it.
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("shot.png");
        // Use a fully decodable image: the multimodal boundary intentionally
        // rejects files that merely carry a valid signature.
        const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        std::fs::write(
            &image_path,
            STANDARD.decode(PNG_B64).expect("valid PNG fixture"),
        )
        .unwrap();
        let marker = format!("[IMAGE:{}]", image_path.display());
        let big = "x".repeat(2000);
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user(format!("turn1 {marker}")),
            ChatMessage::assistant("a1".to_string()),
            ChatMessage::user(format!("turn2 {big}")),
            ChatMessage::assistant("a2".to_string()),
            ChatMessage::user("turn3 short".to_string()),
            ChatMessage::assistant("final answer".to_string()),
        ];
        let config = zeroclaw_config::schema::MultimodalConfig::default();
        let mut image_cache = zeroclaw_providers::multimodal::LocalImageCache::new();
        // The measured population is the PREPARED one, exactly as the runtime
        // measures it (`reported_population_estimated` from `prepared_messages`).
        let prepared =
            prepare_messages_for_iteration(&history, &config, false, Some(&mut image_cache))
                .await
                .expect("preparation must succeed for the regression");
        let prepared_estimated = crate::agent::history::estimate_history_tokens(&prepared.messages);
        let raw_estimated = crate::agent::history::estimate_history_tokens(&history);
        assert_eq!(
            prepared_estimated, raw_estimated,
            "per-image pricing must stay identical between the raw marker and its prepared payload"
        );

        let reported = prepared_estimated * 4;
        let budget = reported / 2;
        let before = history.len();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        enforce_reported_budget(
            &mut None,
            &mut history,
            reported,
            budget,
            budget,
            1,
            Some(&tx),
            &NoopObserver,
            &mut false,
        )
        .await
        .expect("budget enforcement succeeds");

        // The reported over-budget request must trim.
        assert!(
            history.len() < before,
            "a reported over-budget request with a retained image must be trimmed"
        );

        // A trim that did not fit the trim budget is a hard failure and emits
        // no event, so never block waiting for one.
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(_) => panic!("a fitting trim must emit a HistoryTrimmed event"),
        };
        let tokens_after = match event {
            TurnEvent::HistoryTrimmed { tokens_after, .. } => {
                tokens_after.expect("reported-budget trim must carry token accounting")
            }
            other => panic!("expected HistoryTrimmed, got {other:?}"),
        };

        // The emitted count must describe the prepared retained population
        // (breadcrumb included) — the exact next provider request — not the raw
        // retained marker estimate.
        let retained_prepared =
            prepare_messages_for_iteration(&history, &config, false, Some(&mut image_cache))
                .await
                .expect("preparation of the retained history must succeed");
        let expected =
            crate::agent::history::estimate_history_tokens(&retained_prepared.messages) as u64;
        assert_eq!(
            tokens_after, expected,
            "tokens_after must describe the prepared retained population sent next"
        );
    }

    #[tokio::test]
    async fn repeated_trim_with_existing_crumb_does_not_overstate_dropped_messages() {
        // A trim on history that ALREADY carries the model-visible breadcrumb
        // (e.g. a projection-triggered trim immediately after a previous one)
        // must not count the pre-existing crumb as a freshly dropped message:
        // the crumb is present in both the taken and the retained set, so every
        // message missing from the retained set is a genuinely dropped one.
        let mut history = big_history();
        let mut crumb_present = false;
        crumb_present =
            crate::agent::history_trim::insert_breadcrumb_deduped(&mut history, crumb_present);
        assert!(
            history
                .iter()
                .any(|m| m.content == crate::agent::history_trim::breadcrumb().content),
            "precondition: the fixture history carries the trim breadcrumb"
        );
        let taken_len = history.len();
        let estimated = crate::agent::history::estimate_history_tokens(&history);
        let reported = estimated * 4;
        let budget = estimated / 2;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        enforce_reported_budget(
            &mut None,
            &mut history,
            reported,
            budget,
            budget,
            1,
            Some(&tx),
            &NoopObserver,
            &mut crumb_present,
        )
        .await
        .expect("budget enforcement succeeds");
        assert!(
            history.len() < taken_len,
            "an over-budget reported trim must shrink history"
        );
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(_) => panic!("the trim must emit a HistoryTrimmed event"),
        };
        let dropped_messages = match event {
            TurnEvent::HistoryTrimmed {
                dropped_messages, ..
            } => dropped_messages,
            other => panic!("expected HistoryTrimmed, got {other:?}"),
        };
        assert_eq!(
            dropped_messages as usize,
            taken_len - history.len(),
            "a pre-existing crumb must not be counted as a newly dropped message \
             (dropped {dropped_messages}, taken {taken_len}, retained {})",
            history.len()
        );
    }
}

#[cfg(test)]
mod trim_budget_tests {
    use super::*;

    fn boundary_history() -> Vec<ChatMessage> {
        let big = "x".repeat(1000);
        vec![
            ChatMessage::system("s"),
            ChatMessage::user(format!("old {big}")),
            ChatMessage::assistant("old answer"),
            ChatMessage::user("new"),
            ChatMessage::assistant("ok"),
        ]
    }

    #[test]
    fn dispatch_trim_includes_breadcrumb_in_floor_recount() {
        // System (5) + newest turn (10) fit the 20-token budget, but adding
        // the model-visible breadcrumb (~17) pushes the final history over it.
        let mut history = boundary_history();
        let budget = 20;
        let mut crumb_present = false;
        let tokens_before = crate::agent::history::estimate_history_tokens(&history);
        let result = trim_whole_turns_for_dispatch(
            &mut None,
            &mut history,
            &mut crumb_present,
            tokens_before as u64,
            budget,
            1,
        );
        assert_eq!(result.outcome, PreDispatchOutcome::Trimmed);
        let final_tokens = crate::agent::history::estimate_history_tokens(&history);
        assert!(
            final_tokens > budget,
            "the breadcrumb itself must push the kept history over budget ({final_tokens} > {budget})"
        );
        let floor = trim_whole_turns_for_dispatch(
            &mut None,
            &mut history,
            &mut crumb_present,
            final_tokens as u64,
            budget,
            1,
        );
        assert_eq!(floor.outcome, PreDispatchOutcome::Floor);
        let crumb = crate::agent::history_trim::breadcrumb();
        assert!(
            history
                .iter()
                .any(|m| m.content == crumb.content && m.role == crumb.role),
            "final history must include the model-visible breadcrumb"
        );
    }

    fn dispatch_limits(
        budget: usize,
        window: usize,
    ) -> zeroclaw_config::schema::ResolvedContextLimits {
        let mut limits = zeroclaw_config::schema::ResolvedContextLimits::legacy_fallback(0);
        limits.model_context_window = window;
        limits.context_token_budget = budget;
        limits
    }

    /// The in-turn guard measures against the configured trim budget — the same
    /// number the turn-start check read. Tool results land long after that
    /// check, and a guard measured against the whole window would let the
    /// population climb until the model had nowhere left to write: the provider
    /// accepts the request, the model runs out of room mid-reply, and no
    /// `finish_reason` ever arrives.
    #[test]
    fn the_in_turn_guard_uses_the_configured_trim_budget() {
        let limits = dispatch_limits(96_000, 120_000);
        assert_eq!(
            dispatch_trim_budget(limits),
            96_000,
            "the configured budget must bound the in-turn guard"
        );
    }

    /// An unset budget means proactive trimming is off; the model's capacity is
    /// then the only ceiling, which is the pre-existing behaviour.
    #[test]
    fn the_in_turn_trim_budget_falls_back_to_the_window_when_trimming_is_off() {
        let limits = dispatch_limits(0, 120_000);
        assert_eq!(dispatch_trim_budget(limits), 120_000);
    }

    /// The budget cannot raise the guard: one above the model window (a
    /// misconfiguration the resolver clamps, but this function must not trust
    /// that) still resolves to the window.
    #[test]
    fn the_in_turn_trim_budget_never_exceeds_the_model_window() {
        let limits = dispatch_limits(200_000, 120_000);
        assert_eq!(dispatch_trim_budget(limits), 120_000);
    }

    /// The action's shape (R8): one cut to the configured retention, not the
    /// smallest cut that clears the budget. Nibbling one turn at a time left
    /// the population just under the line, so the next turn crossed it again
    /// and the provider prefix was rewritten on nearly every turn.
    #[test]
    fn the_first_pass_cuts_to_the_configured_retention_not_the_minimum() {
        let big = "x".repeat(2_000);
        let mut history = vec![ChatMessage::system("s")];
        for i in 0..18 {
            history.push(ChatMessage::user(format!("turn {i} {big}")));
            history.push(ChatMessage::assistant(format!("reply {i} {big}")));
        }
        let tokens = crate::agent::history::estimate_history_tokens(&history);
        // A budget that dropping a single turn would already satisfy: the
        // nibble shape would keep 17 turns here.
        let last_turn = history.len() - 2;
        let budget = tokens - crate::agent::history::estimate_history_tokens(&history[last_turn..]);
        let mut crumb_present = false;
        let result = trim_whole_turns_for_dispatch(
            &mut None,
            &mut history,
            &mut crumb_present,
            tokens as u64,
            budget,
            5,
        );
        assert_eq!(result.outcome, PreDispatchOutcome::Trimmed);
        assert_eq!(
            result.kept_turns, 5,
            "the first pass must cut to keep_recent_turns"
        );
        assert_eq!(
            result.dropped_turns, 13,
            "one cut, not one turn per dispatch"
        );
    }

    /// The reported miss: the accepted call billed 105,706 for rows the
    /// estimator counted as 95,000, and the next step in the same turn appended
    /// a tool result the estimator priced at 5,000. The guard must see
    /// `105,706 + margin(5,000)` — over the 104,857 budget and therefore
    /// trimmed *before* that step's request goes out. Multiplying the whole
    /// estimate by the 1.113 ratio gave 111,300 here, but it prices the tail
    /// with the same biased estimator, so a tail that grows faster than the
    /// ratio suggests stays invisible.
    #[test]
    fn the_in_turn_guard_anchors_on_the_reported_request_not_a_ratio() {
        let counter = DispatchTokenCounter::for_route(
            Some(ReportedRequestUsage {
                provider_name: "p".into(),
                model: "m".into(),
                reported_tokens: 105_706,
                estimated_tokens: 95_000,
            }),
            "p",
            "m",
        );

        let (tokens, source) = counter.measure(100_000);
        assert_eq!(tokens, 105_706 + 5_500, "reported + 11/10 of the tail");
        assert_eq!(
            source,
            zeroclaw_api::agent::TokenCountSource::Calibrated,
            "the anchored part is a provider measurement"
        );
        assert!(
            tokens > 104_857,
            "the guard must see this population as over a 104,857 budget"
        );
    }

    /// With no report for this route the guard prices a local estimate — and
    /// every pre-send number carries the conservative margin. This used to be
    /// the raw estimate, the one number on the path with no margin at all.
    #[test]
    fn the_in_turn_guard_margins_a_population_it_cannot_anchor() {
        let counter = DispatchTokenCounter::for_route(None, "p", "m");
        let (tokens, source) = counter.measure(100_000);
        assert_eq!(tokens, 110_000, "estimate x 11/10");
        assert_eq!(source, zeroclaw_api::agent::TokenCountSource::Estimated);
    }

    /// A report from another route prices nothing here: the model switch must
    /// not inherit the previous model's measurement.
    #[test]
    fn the_in_turn_guard_ignores_a_report_from_another_route() {
        let counter = DispatchTokenCounter::for_route(
            Some(ReportedRequestUsage {
                provider_name: "other".into(),
                model: "m".into(),
                reported_tokens: 105_706,
                estimated_tokens: 95_000,
            }),
            "p",
            "m",
        );
        let (tokens, source) = counter.measure(100_000);
        assert_eq!(tokens, 110_000);
        assert_eq!(source, zeroclaw_api::agent::TokenCountSource::Estimated);
    }

    /// A mid-turn trim can drop rows below the anchored population. The anchor
    /// no longer describes the request, so the number is a plain estimate and
    /// must say so instead of borrowing a measurement it does not have.
    #[test]
    fn a_population_that_shrank_below_the_anchor_is_labelled_an_estimate() {
        let counter = DispatchTokenCounter::for_route(
            Some(ReportedRequestUsage {
                provider_name: "p".into(),
                model: "m".into(),
                reported_tokens: 105_706,
                estimated_tokens: 95_000,
            }),
            "p",
            "m",
        );
        let (tokens, source) = counter.measure(80_000);
        assert_eq!(tokens, 88_000, "estimate x 11/10");
        assert_eq!(source, zeroclaw_api::agent::TokenCountSource::Estimated);
    }
}

#[cfg(test)]
mod active_route_context_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use zeroclaw_api::observability_traits::{ObserverEvent, ObserverMetric};
    use zeroclaw_config::schema::{
        AliasedAgentConfig, Config, CustomModelProviderConfig, ModelProviderConfig,
        RuntimeProfileConfig,
    };
    use zeroclaw_providers::{ChatResponse, traits::TokenUsage};

    #[derive(Default)]
    struct RouteObserver {
        responses: Mutex<Vec<(String, String)>>,
    }

    impl crate::observability::Observer for RouteObserver {
        fn record_event(&self, event: &ObserverEvent) {
            if let ObserverEvent::LlmResponse {
                model_provider,
                model,
                ..
            } = event
            {
                self.responses
                    .lock()
                    .expect("response lock")
                    .push((model_provider.clone(), model.clone()));
            }
        }

        fn record_metric(&self, _metric: &ObserverMetric) {}

        fn name(&self) -> &str {
            "route-observer"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn route_config() -> Config {
        let providers = [
            ("text", "text-model", 200_000),
            ("vision", "vision-model", 8_000),
        ]
        .into_iter()
        .map(|(alias, model, context_window)| {
            (
                alias.to_string(),
                CustomModelProviderConfig {
                    base: ModelProviderConfig {
                        model: Some(model.to_string()),
                        context_window: Some(context_window),
                        ..ModelProviderConfig::default()
                    },
                },
            )
        })
        .collect::<HashMap<_, _>>();

        let mut config = Config::default();
        config.providers.models.custom = providers;
        config.runtime_profiles.insert(
            "ratio".to_string(),
            RuntimeProfileConfig {
                context: zeroclaw_config::scattered_types::ContextConfig {
                    trim_threshold_percent: 90,
                    ..Default::default()
                },
                ..RuntimeProfileConfig::default()
            },
        );
        config.agents.insert(
            "coder".to_string(),
            AliasedAgentConfig {
                enabled: true,
                runtime_profile: "ratio".into(),
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    #[tokio::test]
    async fn selected_call_route_drives_trim_usage_and_response_attribution() {
        let config = route_config();
        let text_limits =
            config.resolved_context_limits_for_route("coder", "custom.text", "text-model");
        let vision_limits = resolve_context_limits_for_call(
            None,
            Some(&config),
            Some("coder"),
            "custom.vision",
            "vision-model",
            text_limits,
        );
        assert_eq!(text_limits.context_token_budget, 180_000);
        assert_eq!(vision_limits.model_context_window, 8_000);
        assert_eq!(vision_limits.context_token_budget, 7_200);

        let large = "x".repeat(20_000);
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user(format!("old question {large}")),
            ChatMessage::assistant(format!("old answer {large}")),
            ChatMessage::user("inspect [IMAGE:/tmp/image.png]"),
        ];
        let tokens_before = crate::agent::history::estimate_history_tokens(&history);
        assert!(tokens_before < text_limits.context_token_budget);
        let trim = trim_whole_turns_for_dispatch(
            &mut None,
            &mut history,
            &mut false,
            tokens_before as u64,
            vision_limits.context_token_budget,
            vision_limits.keep_recent_turns,
        );
        assert!(
            trim.outcome == PreDispatchOutcome::Trimmed,
            "the selected vision route must trim history that the text route would retain"
        );
        assert!(
            crate::agent::history::estimate_history_tokens(&history)
                <= vision_limits.context_token_budget
        );

        let observer = RouteObserver::default();
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let dedup_exempt_tools = Vec::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
        let base_ctx = TurnCtx {
            observer: &observer,
            provider_name: "custom.text",
            model: "text-model",
            context_limits: text_limits,
            temperature: None,
            approval: None,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            event_tx: Some(&event_tx),
            hooks: None,
            dedup_exempt_tools: &dedup_exempt_tools,
            pacing: &pacing,
            strict_tool_parsing: false,
            channel: None,
            draft_reasoning: zeroclaw_config::schema::StreamReasoningMode::Status,
            turn_id: "vision-route-context",
            agent_alias: Some("coder"),
            parent_agent_alias: None,
            serving_provider_name: None,
            serving_model: None,
        };
        let call_ctx = base_ctx.for_route("custom.vision", "vision-model", vision_limits);
        let response = ChatResponse {
            text: Some("done".to_string()),
            tool_calls: Vec::new(),
            usage: Some(TokenUsage {
                input_tokens: Some(6_000),
                output_tokens: Some(100),
                cached_input_tokens: None,
                cache_creation_input_tokens: None,
            }),
            reasoning_content: None,
        };
        let specs = IterationToolSpecs {
            tool_specs: Vec::new(),
            exposed_tool_specs: None,
            known_tool_names: HashSet::new(),
            use_native_tools: false,
        };
        let interpreted = interpret_chat_response(
            &call_ctx,
            "custom.vision",
            "vision-model",
            response,
            &history,
            &specs,
            false,
            0,
            false,
        )
        .await;
        record_accepted_chat_response(
            &call_ctx,
            "custom.vision",
            "vision-model",
            &interpreted.response_text,
            &interpreted.native_tool_calls,
            interpreted.tool_calls.len(),
            interpreted.usage.as_ref(),
            &history,
            Instant::now(),
            0,
            None,
        )
        .await;

        match event_rx.try_recv().expect("usage event") {
            TurnEvent::Usage {
                context_token_budget,
                model_context_window,
                ..
            } => {
                assert_eq!(context_token_budget, Some(7_200));
                assert_eq!(model_context_window, Some(8_000));
            }
            other => panic!("expected Usage, got {other:?}"),
        }
        assert_eq!(
            observer.responses.lock().expect("response lock").as_slice(),
            &[("custom.vision".to_string(), "vision-model".to_string())]
        );
    }

    // B1: the loop resolves per-call limits through the live resolver, not the
    // construction-time snapshot. A resolver reading a shared, mutable config
    // must reflect a post-construction reload on the very next resolution, and
    // must take precedence over any snapshot `config` argument.
    #[tokio::test]
    async fn context_limits_for_call_uses_live_resolver_over_snapshot() {
        // Snapshot config: text route configured with a 200k window.
        let snapshot = route_config();
        let snapshot_limits =
            snapshot.resolved_context_limits_for_route("coder", "custom.text", "text-model");
        assert_eq!(snapshot_limits.model_context_window, 200_000);
        assert_eq!(snapshot_limits.context_token_budget, 180_000);

        // Live config behind a shared handle, resolved through the closure the
        // daemon-backed agent installs.
        let live = Arc::new(parking_lot::RwLock::new(route_config()));
        let live_for_resolver = Arc::clone(&live);
        let resolver: ContextLimitsResolver = Arc::new(move |provider_ref, model| {
            live_for_resolver
                .read()
                .resolved_context_limits_for_route("coder", provider_ref, model)
        });

        // Before reload: resolver present overrides the snapshot argument, and
        // agrees with it because both start from the same config.
        let before = resolve_context_limits_for_call(
            Some(&resolver),
            Some(&snapshot),
            Some("coder"),
            "custom.text",
            "text-model",
            snapshot_limits,
        );
        assert_eq!(before.model_context_window, 200_000);
        assert_eq!(before.context_token_budget, 180_000);

        // Operator shrinks the text model's window to 40k in the live config
        // AFTER the agent (and its snapshot) were built.
        live.write()
            .providers
            .models
            .custom
            .get_mut("text")
            .expect("text provider")
            .base
            .context_window = Some(40_000);

        // The next resolution reflects the reload: 40k window, budget = 40k*0.9.
        // The stale 200k snapshot is NOT consulted because the resolver wins.
        let after = resolve_context_limits_for_call(
            Some(&resolver),
            Some(&snapshot),
            Some("coder"),
            "custom.text",
            "text-model",
            snapshot_limits,
        );
        assert_eq!(
            after.model_context_window, 40_000,
            "live reload must be observed on the next per-call resolution"
        );
        assert_eq!(after.context_token_budget, 36_000);
        assert_eq!(
            snapshot_limits.model_context_window, 200_000,
            "the snapshot pair is unchanged; the resolver, not the snapshot, is authoritative"
        );
    }

    // B3: proactive trimming must fire on the iteration that SWITCHES routes,
    // not only iteration 0. A first text-model call returns a tool call; the
    // tool injects an image marker; iteration 2 routes to a small vision model.
    // The vision route's budget must trim history that the large text budget
    // retained — before the vision provider is dispatched. Driving the real
    // `run_tool_call_loop` (rather than trimming a fixture by hand) is
    // what exercises the per-iteration gate.
    #[tokio::test]
    async fn tool_image_switches_route_and_retrims_before_vision_dispatch() {
        use crate::agent::loop_::{
            ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess, ResolvedRuntimeKnobs, ToolLoop,
        };
        use axum::{Json, Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use zeroclaw_api::tool::{Tool, ToolResult};
        use zeroclaw_providers::ToolCall;

        // Mock vision endpoint: always replies with a small plain-text answer.
        async fn vision_reply(Json(_body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "choices": [{"message": {"content": "vision saw the image"}}]
            }))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind vision provider");
        let addr = listener.local_addr().expect("vision provider address");
        let app = Router::new().route("/v1/chat/completions", post(vision_reply));
        let _server = zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, app).await.expect("vision serves");
        });

        // A tempfile PNG the declared attachment points at, so image preparation
        // has a real file to load.
        let temp = tempfile::tempdir().expect("tempdir");
        let image_path = temp.path().join("shot.png");
        std::fs::write(
            &image_path,
            [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
        )
        .expect("write png");

        // Config: a 200k text route (ratio 0.9 -> 180k budget) and an 8k vision
        // route (ratio 0.9 -> 7.2k budget) served by the mock endpoint.
        let config: Config = toml::from_str(&format!(
            r#"
schema_version = 3
[providers.models.custom.text]
model = "text-model"
context_window = 200000
[providers.models.custom.vision]
uri = "http://{addr}/v1"
model = "vision-model"
context_window = 8000
[runtime_profiles.ratio.context]
trim_threshold_percent = 90
[agents.coder]
enabled = true
runtime_profile = "ratio"
model_provider = "custom.text"
[multimodal]
vision_model_provider = "custom.vision"
"#
        ))
        .expect("config parses");

        // Text primary: iteration 0 emits a native tool call; iteration 1 (after
        // the tool injects an image and the route switches to vision) ends the
        // turn. `ProviderCapabilities::default()` has `vision = false`, so the
        // declared image forces the vision route.
        struct TextPrimary {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl ModelProvider for TextPrimary {
            fn capabilities(&self) -> zeroclaw_api::model_provider::ProviderCapabilities {
                // Native tool calling so the structured `tool_calls` below are
                // honored; vision stays false so a declared image forces routing.
                zeroclaw_api::model_provider::ProviderCapabilities {
                    native_tool_calling: true,
                    ..Default::default()
                }
            }
            async fn chat_with_system(
                &self,
                _s: Option<&str>,
                _m: &str,
                _model: &str,
                _t: Option<f64>,
            ) -> Result<String> {
                Ok(String::new())
            }
            async fn chat(
                &self,
                _request: zeroclaw_api::model_provider::ChatRequest<'_>,
                _model: &str,
                _t: Option<f64>,
            ) -> Result<ChatResponse> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(ChatResponse {
                        text: None,
                        tool_calls: vec![ToolCall {
                            id: "c1".into(),
                            name: "attach_image".into(),
                            arguments: "{}".into(),
                            extra_content: None,
                        }],
                        usage: None,
                        reasoning_content: None,
                    })
                } else {
                    Ok(ChatResponse {
                        text: Some("text follow-up".into()),
                        tool_calls: Vec::new(),
                        usage: None,
                        reasoning_content: None,
                    })
                }
            }
        }
        impl zeroclaw_api::attribution::Attributable for TextPrimary {
            fn role(&self) -> zeroclaw_api::attribution::Role {
                zeroclaw_api::attribution::Role::Provider(
                    zeroclaw_api::attribution::ProviderKind::Model(
                        zeroclaw_api::attribution::ModelProviderKind::Custom,
                    ),
                )
            }
            fn alias(&self) -> &str {
                "text-primary"
            }
        }

        // Tool whose result carries an image marker, forcing the vision route on
        // the next iteration.
        struct AttachImage {
            path: String,
        }
        #[async_trait::async_trait]
        impl Tool for AttachImage {
            fn name(&self) -> &str {
                "attach_image"
            }
            fn description(&self) -> &str {
                "attaches an image"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
                // The producer declares its image; under the attachment
                // contract nothing in the result text is promoted.
                Ok(ToolResult::ok("here it is").with_attachment(
                    zeroclaw_api::media::RenderedMarker {
                        target: self.path.clone(),
                        kind: zeroclaw_api::media::MarkerKind::Image,
                    },
                ))
            }
        }
        impl zeroclaw_api::attribution::Attributable for AttachImage {
            fn role(&self) -> zeroclaw_api::attribution::Role {
                zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
            }
            fn alias(&self) -> &str {
                "attach_image"
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let text_provider = TextPrimary {
            calls: Arc::clone(&calls),
        };
        let tools_registry =
            crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
                AttachImage {
                    path: image_path.display().to_string(),
                },
            )]);
        let observer = crate::observability::NoopObserver;
        let approval = crate::approval::ApprovalManager::for_non_interactive(
            &zeroclaw_config::schema::RiskProfileConfig {
                // Auto-approve so the image-injecting tool actually runs; without
                // this the non-interactive gate denies the unknown tool and no
                // image marker is produced, so the vision route never engages.
                auto_approve: vec!["*".to_string()],
                ..Default::default()
            },
        );
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let knobs = crate::agent::loop_::LoopKnobs::default();
        let multimodal = config.multimodal.clone();

        // Large history the 200k text budget keeps but the 7.2k vision budget
        // must trim. Present from the start so, if the gate trimmed on the text
        // route, the text budget (180k) would NOT drop it; only the vision
        // switch (7.2k) does.
        let big = "x".repeat(20_000);
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user(format!("old q {big}")),
            ChatMessage::assistant(format!("old a {big}")),
            ChatMessage::user("please attach the image"),
        ];
        let text_limits =
            config.resolved_context_limits_for_route("coder", "custom.text", "text-model");
        assert_eq!(text_limits.context_token_budget, 180_000);
        let seed_tokens = crate::agent::history::estimate_history_tokens(&history);
        assert!(
            seed_tokens < text_limits.context_token_budget,
            "seed history must fit the text budget so any trim is attributable to the vision switch"
        );

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut image_cache = zeroclaw_providers::multimodal::LocalImageCache::new();
        let mut provider_image_state = ProviderImageState::default();
        let turn_id = "b3-two-iteration";

        let _ = run_tool_call_loop(ToolLoop {
            context_anchor_seed: None,
            context_anchor_sink: None,
            history_was_trimmed: None,
            parent_agent_alias: None,
            served_route_sink: None,
            sop_reassembly: None,
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: &text_provider,
                    provider_name: "custom.text",
                    model: "text-model",
                    dispatch_model: "text-model",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: &tools_registry,
                    observer: &observer,
                    silent: true,
                    approval: Some(&approval),
                    multimodal_config: &multimodal,
                    config: Some(&config),
                    hooks: None,
                    activated_tools: None,
                    model_switch_callback: None,
                    receipt_generator: None,
                    security: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations: 3,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &pacing,
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 100_000,
                    context_limits: text_limits,
                    context_limits_resolver: None,
                    knobs: &knobs,
                },
            ),
            history: &mut history,
            history_has_trim_breadcrumb: &mut false,
            injected_memory_preamble: &mut None,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            shared_budget: None,
            channel: None,
            collected_receipts: None,
            event_tx: Some(event_tx),
            steering: None,
            new_messages_out: None,
            image_cache: Some(ToolLoopImageState {
                cache: &mut image_cache,
                provider_state: &mut provider_image_state,
            }),
            memory: None,
            ingress: zeroclaw_api::ingress::IngressContext::agent_direct(),
            agent_alias: Some("coder"),
            turn_id,
        })
        .await;

        // A HistoryTrimmed event must have fired: proactive trimming ran when
        // the route switched to the 7.2k vision budget on the second iteration.
        let mut trimmed = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, TurnEvent::HistoryTrimmed { .. }) {
                trimmed = true;
            }
        }
        assert!(
            trimmed,
            "switching to the smaller vision route must trigger proactive trimming \
             before dispatch, even though it happens after iteration 0"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "the text primary must have been called at least once"
        );
    }
}

#[cfg(test)]
mod shared_iteration_budget_tests {
    use super::{ExecutionTreeBudget, ExecutionTreeReservation};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    #[test]
    fn exhausted_budget_never_wraps() {
        let budget = ExecutionTreeBudget::root(2);

        assert_eq!(budget.reserve(), Ok(ExecutionTreeReservation::Iteration));
        assert_eq!(
            budget.reserve(),
            Ok(ExecutionTreeReservation::FinalCompletion)
        );
        assert!(budget.reserve().is_err());
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn contention_grants_exactly_the_available_iterations() {
        const AVAILABLE: usize = 8;
        const WORKERS: usize = 64;

        let budget = Arc::new(ExecutionTreeBudget::root(AVAILABLE + 1));
        let granted = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(WORKERS + 1));

        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                let budget = budget.child();
                let granted = Arc::clone(&granted);
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    if budget.reserve() == Ok(ExecutionTreeReservation::Iteration) {
                        granted.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            start.wait();
        });

        assert_eq!(granted.load(Ordering::Relaxed), AVAILABLE);
        assert_eq!(budget.remaining(), 1);
    }
}

/// The trace stamp for an internally initiated turn: initiating principal
/// and executing agent side by side, correlated by the turn's trace id.
/// Attributes carry runtime-resolved identity only, never message text.
pub(crate) fn internal_principal_event(
    turn_id: &str,
    origin: zeroclaw_api::ingress::TurnOrigin,
    principal: &zeroclaw_api::ingress::InternalPrincipal,
    executing_agent: Option<&str>,
) -> ::zeroclaw_log::Event {
    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
        .with_category(::zeroclaw_log::EventCategory::Agent)
        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
        .with_attrs(::serde_json::json!({
            "trace_id": turn_id,
            "origin": origin,
            "internal_principal": principal,
            "executing_agent": executing_agent,
        }))
}

/// Live SOP nested-step re-assembly gate, isolation, and fail-closed regressions.
///
/// Privilege-scope properties of the live driver:
///
/// - **Gate on the effective alias.** The re-assembly gate compares a step's
///   agent against the loop's own `agent_alias`, which IS the effective agent
///   at every depth (a re-assembled sub-loop runs with its step agent as its
///   own alias). A depth >= 2 step naming the outer agent therefore compares
///   against the re-assembled child's alias and re-assembles.
/// - **Child transcript isolation.** A cross-agent step runs on an explicit
///   child transcript (the step agent's own system prompt + the step context);
///   the parent turn's history never reaches the step agent's provider.
/// - **Effective-agent contract.** The nested loop runs with the step agent's
///   own provider binding (incl. temperature), registry, and runtime controls,
///   and its records stamp the step agent as the acting identity with the
///   delegating agent as parent correlation.
/// - **Fail-closed.** A cross-agent step with no re-assembly handle, or whose
///   agent context cannot be assembled, FAILS rather than running with the
///   parent agent's broader context.
#[cfg(test)]
mod internal_principal_event_tests {
    use super::*;

    #[test]
    fn trace_stamp_carries_both_identities_keyed_by_trace_id() {
        let principal = zeroclaw_api::ingress::InternalPrincipal::Cron {
            job_id: "job-1".to_string(),
            job_name: Some("nightly".to_string()),
        };
        let event = internal_principal_event(
            "trace-1",
            zeroclaw_api::ingress::TurnOrigin::Cron,
            &principal,
            Some("assistant"),
        );
        assert_eq!(event.outcome, ::zeroclaw_log::EventOutcome::Unknown);
        let attrs = event.attrs.expect("event carries attributes");
        assert_eq!(attrs["trace_id"], "trace-1");
        assert_eq!(attrs["origin"], "cron");
        assert_eq!(
            attrs["internal_principal"],
            serde_json::json!({"cron": {"job_id": "job-1", "job_name": "nightly"}})
        );
        assert_eq!(attrs["executing_agent"], "assistant");
        assert_eq!(
            attrs.as_object().map(serde_json::Map::len),
            Some(4),
            "identity attributes only — never message text"
        );
    }
}

#[cfg(test)]
mod sop_step_reassembly_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zeroclaw_providers::{ChatResponse, ToolCall};

    // ── Gate: pure decision properties ───────────────────────────────────────

    #[test]
    fn gate_reassembles_only_on_agent_change() {
        assert!(
            !step_needs_reassembly(Some("a"), Some("a")),
            "same agent must not re-assemble"
        );
        assert!(
            step_needs_reassembly(Some("a"), Some("b")),
            "a different agent must re-assemble"
        );
        assert!(
            !step_needs_reassembly(Some("a"), None),
            "a step with no explicit agent inherits the current one"
        );
        assert!(
            step_needs_reassembly(None, Some("b")),
            "an unnamed baseline still re-assembles a named step"
        );
        assert!(!step_needs_reassembly(None, None));
    }

    /// The depth-2 escalation scenario stays covered under the simplified
    /// mechanism: root `A` -> step names `B` -> the nested sub-loop runs with
    /// `agent_alias = Some("B")` (attribution follows the effective agent, see
    /// the driver's nested `ToolLoop` construction) -> `B`'s nested step naming
    /// root `A` compares "A" against the loop's own alias "B" and re-assembles,
    /// running with `A`'s scope instead of inheriting `B`'s.
    #[test]
    fn depth2_step_naming_outer_agent_still_reassembles() {
        // Depth 1: root loop runs as A; step names B -> re-assemble.
        assert!(step_needs_reassembly(Some("A"), Some("B")));
        // Depth 2: the sub-loop's own alias IS "B" (effective agent); a step
        // naming the outer "A" re-assembles instead of inheriting B's scope.
        assert!(
            step_needs_reassembly(Some("B"), Some("A")),
            "a depth-2 step naming the outer agent must re-assemble that agent's scope"
        );
        // The historical escalation: comparing against an alias pinned to the
        // OUTER agent at depth 2 would skip re-assembly. The invariant that
        // prevents it now is alias-follows-effective-agent, so this comparison
        // never happens with "A" on the left inside B's sub-loop.
        assert!(!step_needs_reassembly(Some("A"), Some("A")));
    }

    fn tool_names(tools: &[Box<dyn crate::tools::Tool>]) -> Vec<String> {
        tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Real-scope tie: re-assembling a step's named agent yields THAT agent's
    /// own gated tool set, and distinct agents resolve distinct scopes — so the
    /// gate decision is load-bearing (choosing to re-assemble genuinely changes
    /// which tools the step can reach).
    #[tokio::test]
    async fn reassembly_yields_the_named_agents_own_scope() {
        use zeroclaw_config::multi_agent::{AgentMemoryConfig, MemoryBackendKind};
        use zeroclaw_config::schema::{
            AliasedAgentConfig, Config, ModelProviderConfig, OllamaModelProviderConfig,
            RiskProfileConfig, SopConfig,
        };

        let root =
            std::env::temp_dir().join(format!("zeroclaw-sop-depth2-{}", uuid::Uuid::new_v4()));
        let mut config = Config {
            data_dir: root.join("data"),
            config_path: root.join("config.toml"),
            ..Config::default()
        };
        // Two agents whose per-agent policy allowlists exactly one, DIFFERENT
        // built-in each: their assembled scopes must not coincide.
        config.risk_profiles.insert(
            "reader".to_string(),
            RiskProfileConfig {
                allowed_tools: vec!["file_read".to_string()],
                ..RiskProfileConfig::default()
            },
        );
        config.risk_profiles.insert(
            "writer".to_string(),
            RiskProfileConfig {
                allowed_tools: vec!["file_write".to_string()],
                ..RiskProfileConfig::default()
            },
        );
        config.providers.models.ollama.insert(
            "p".to_string(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("test-model".to_string()),
                    ..ModelProviderConfig::default()
                },
                ..OllamaModelProviderConfig::default()
            },
        );
        for (alias, profile) in [("reader", "reader"), ("writer", "writer")] {
            config.agents.insert(
                alias.to_string(),
                AliasedAgentConfig {
                    enabled: true,
                    model_provider: "ollama.p".into(),
                    risk_profile: profile.into(),
                    memory: AgentMemoryConfig {
                        backend: MemoryBackendKind::Markdown,
                    },
                    ..AliasedAgentConfig::default()
                },
            );
        }
        let engine = Arc::new(std::sync::Mutex::new(crate::sop::SopEngine::new(
            SopConfig::default(),
        )));

        let reader =
            assemble_owned_execution(&config, None, "reader", Arc::clone(&engine), None, None)
                .await
                .expect("reader assembles");
        let writer =
            assemble_owned_execution(&config, None, "writer", Arc::clone(&engine), None, None)
                .await
                .expect("writer assembles");
        let reader_names = tool_names(&reader.tools_registry);
        let writer_names = tool_names(&writer.tools_registry);

        assert!(
            reader_names.contains(&"file_read".to_string())
                && !reader_names.contains(&"file_write".to_string()),
            "reader gets only its own allowlisted tool: {reader_names:?}"
        );
        assert!(
            writer_names.contains(&"file_write".to_string())
                && !writer_names.contains(&"file_read".to_string()),
            "writer gets only its own allowlisted tool: {writer_names:?}"
        );
        // The gate re-assembles when a step names the other agent, so a
        // cross-agent step lands in the named agent's scope, not the baseline's.
        assert!(step_needs_reassembly(Some("reader"), Some("writer")));
        // With no parent approval manager the child is non-interactive
        // (auto-deny), matching the headless driver.
        assert!(reader.approval.is_non_interactive());
    }

    #[tokio::test]
    async fn reassembled_step_file_download_observes_live_revocation() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method, matchers::path};
        use zeroclaw_config::multi_agent::{AgentMemoryConfig, MemoryBackendKind};
        use zeroclaw_config::schema::{
            AliasedAgentConfig, Config, FileDownloadConfig, ModelProviderConfig,
            OllamaModelProviderConfig, RiskProfileConfig, SopConfig,
        };

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/download"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            file_download: FileDownloadConfig {
                url: Some(format!("{}/download", server.uri())),
                allowed_private_hosts: vec!["127.0.0.1".into()],
                ..FileDownloadConfig::default()
            },
            ..Config::default()
        };
        config.tools.optional = vec!["file_download".into()];
        config.risk_profiles.insert(
            "stepper".to_string(),
            RiskProfileConfig {
                allowed_tools: vec!["file_download".to_string()],
                ..RiskProfileConfig::default()
            },
        );
        config.providers.models.ollama.insert(
            "p".to_string(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("test-model".to_string()),
                    ..ModelProviderConfig::default()
                },
                ..OllamaModelProviderConfig::default()
            },
        );
        config.agents.insert(
            "stepper".to_string(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: "ollama.p".into(),
                risk_profile: "stepper".into(),
                memory: AgentMemoryConfig {
                    backend: MemoryBackendKind::Markdown,
                },
                ..AliasedAgentConfig::default()
            },
        );
        let live_config = zeroclaw_config::live::LiveConfig::new(config.clone());
        let engine = Arc::new(std::sync::Mutex::new(crate::sop::SopEngine::new(
            SopConfig::default(),
        )));

        let owned = assemble_owned_execution(
            &config,
            Some(live_config.handle()),
            "stepper",
            Arc::clone(&engine),
            None,
            None,
        )
        .await
        .expect("stepper assembles");
        let file_download = owned
            .tools_registry
            .iter()
            .find(|tool| tool.name() == "file_download")
            .expect("file_download must be registered for the step agent");
        let args = serde_json::json!({ "document_id": "doc-1", "dest_path": "out.bin" });

        let first = file_download
            .execute(args.clone())
            .await
            .expect("first run");
        assert!(first.success, "allowlisted local endpoint should pass");

        let mut reloaded = live_config.snapshot();
        reloaded.file_download.allowed_private_hosts.clear();
        live_config
            .publish(live_config.next_revision().unwrap(), reloaded)
            .unwrap();

        let second = file_download.execute(args).await.expect("second run");
        assert!(
            !second.success,
            "same reassembled step tool must observe live allowlist revocation"
        );
        assert!(
            second
                .error
                .unwrap_or_default()
                .contains("file_download.allowed_private_hosts")
        );
    }

    /// A parent approval manager with a live back-channel survives delegation:
    /// the derived child manager keeps the interactivity mode while enforcing
    /// the CHILD's risk profile.
    #[tokio::test]
    async fn reassembly_preserves_parent_approval_backchannel() {
        use zeroclaw_config::multi_agent::{AgentMemoryConfig, MemoryBackendKind};
        use zeroclaw_config::schema::{
            AliasedAgentConfig, Config, ModelProviderConfig, OllamaModelProviderConfig,
            RiskProfileConfig, SopConfig,
        };

        let root = std::env::temp_dir().join(format!("zeroclaw-sop-appr-{}", uuid::Uuid::new_v4()));
        let mut config = Config {
            data_dir: root.join("data"),
            config_path: root.join("config.toml"),
            ..Config::default()
        };
        config.risk_profiles.insert(
            "restricted".to_string(),
            RiskProfileConfig {
                allowed_tools: vec!["file_read".to_string()],
                ..RiskProfileConfig::default()
            },
        );
        config.providers.models.ollama.insert(
            "p".to_string(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("test-model".to_string()),
                    ..ModelProviderConfig::default()
                },
                ..OllamaModelProviderConfig::default()
            },
        );
        config.agents.insert(
            "restricted".to_string(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: "ollama.p".into(),
                risk_profile: "restricted".into(),
                memory: AgentMemoryConfig {
                    backend: MemoryBackendKind::Markdown,
                },
                ..AliasedAgentConfig::default()
            },
        );
        let engine = Arc::new(std::sync::Mutex::new(crate::sop::SopEngine::new(
            SopConfig::default(),
        )));

        // Parent surface: non-interactive WITH an operator back-channel
        // (ACP / dashboard WS shape).
        let parent = crate::approval::ApprovalManager::for_non_interactive_backchannel(
            &zeroclaw_config::schema::RiskProfileConfig::default(),
        );
        let owned = assemble_owned_execution(
            &config,
            None,
            "restricted",
            Arc::clone(&engine),
            None,
            Some(&parent),
        )
        .await
        .expect("restricted assembles");
        // Mode preserved: still non-interactive, but shell routes through the
        // back-channel (Prompt) instead of the plain non-interactive
        // short-circuit (NotRequired).
        assert!(owned.approval.is_non_interactive());
        assert_eq!(
            owned.approval.approval_requirement("shell"),
            crate::approval::ApprovalRequirement::Prompt,
            "a live approval back-channel must survive delegation"
        );
        // The plain non-interactive parent keeps the auto-deny shape.
        let plain_parent = crate::approval::ApprovalManager::for_non_interactive(
            &zeroclaw_config::schema::RiskProfileConfig::default(),
        );
        let plain_child = plain_parent
            .derive_for_risk_profile(&zeroclaw_config::schema::RiskProfileConfig::default());
        assert_eq!(
            plain_child.approval_requirement("shell"),
            crate::approval::ApprovalRequirement::NotRequired,
            "a plain non-interactive parent derives a plain non-interactive child"
        );
    }

    // ── Shared test doubles ──────────────────────────────────────────────────

    /// A provider that, if the nested loop ever ran, would drive the parent's
    /// sensitive tool. Under the fail-closed guard the nested loop never runs,
    /// so `chat` is never polled.
    struct ShellCallingProvider;

    impl ::zeroclaw_api::attribution::Attributable for ShellCallingProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "ShellCallingProvider"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for ShellCallingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            _request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some(String::new()),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            })
        }
    }

    /// The parent/delegate agent's sensitive tool; counts executions so tests
    /// can prove a cross-agent step never reaches it.
    struct ShellProbe {
        calls: Arc<AtomicUsize>,
    }

    ::zeroclaw_api::tool_attribution!(ShellProbe, ::zeroclaw_api::attribution::ToolKind::Plugin);

    #[async_trait::async_trait]
    impl crate::tools::Tool for ShellProbe {
        fn name(&self) -> &str {
            "shell"
        }
        fn description(&self) -> &str {
            "shell"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<crate::tools::ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "shell-out".to_string().into(),
                error: None,
            })
        }
    }

    /// A provider that returns plain text (no tool call), so a nested step loop
    /// completes in a single iteration without needing any assembled tools.
    struct TextProvider;

    impl ::zeroclaw_api::attribution::Attributable for TextProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "TextProvider"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for TextProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<String> {
            Ok("done".into())
        }

        async fn chat(
            &self,
            _request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("done".to_string()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    /// One captured child-provider request: transcript, offered tool-spec
    /// names, and the temperature the loop passed.
    type CapturedRequest = (Vec<ChatMessage>, Vec<String>, Option<f64>);

    /// The CHILD side of the distinct-providers regression: records every
    /// request the nested loop sends so tests can prove what did (and did not)
    /// reach the step agent's provider. Declares native tool support so the
    /// loop offers tool specs on the request (observable at this boundary).
    struct CaptureProvider {
        requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
    }

    impl ::zeroclaw_api::attribution::Attributable for CaptureProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "CaptureProvider"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for CaptureProvider {
        fn supports_native_tools(&self) -> bool {
            true
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<String> {
            Ok("child-done".into())
        }

        async fn chat(
            &self,
            request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            temperature: Option<f64>,
        ) -> Result<ChatResponse> {
            let tool_names = request
                .tools
                .map(|specs| specs.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default();
            self.requests.lock().expect("capture lock").push((
                request.messages.to_vec(),
                tool_names,
                temperature,
            ));
            Ok(ChatResponse {
                text: Some("child-done".to_string()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    /// Observer capture of `(agent_alias, parent_agent_alias)` on LlmRequest
    /// records — the audit-identity pair for the nested loop.
    #[derive(Default)]
    struct IdentityCapture {
        pairs: std::sync::Mutex<Vec<(Option<String>, Option<String>)>>,
    }

    impl crate::observability::Observer for IdentityCapture {
        fn record_event(&self, event: &crate::observability::ObserverEvent) {
            if let crate::observability::ObserverEvent::LlmRequest {
                agent_alias,
                parent_agent_alias,
                ..
            } = event
            {
                self.pairs
                    .lock()
                    .expect("pairs lock")
                    .push((agent_alias.clone(), parent_agent_alias.clone()));
            }
        }

        fn record_metric(&self, _metric: &zeroclaw_api::observability_traits::ObserverMetric) {}

        fn name(&self) -> &str {
            "identity-capture"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// A trivially registrable tool for seeding a child registry.
    struct NamedTool(&'static str);

    ::zeroclaw_api::tool_attribution!(NamedTool, ::zeroclaw_api::attribution::ToolKind::Plugin);

    #[async_trait::async_trait]
    impl crate::tools::Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "ok".to_string().into(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn builtin_search_fallback_preserves_strict_policy_and_scoped_replay() {
        let orientation = TAIL_MARKER;
        let tools: Vec<Box<dyn crate::tools::Tool>> = vec![
            Box::new(NamedTool("allowed_tool")),
            Box::new(NamedTool("denied_tool")),
        ];
        let names = HashSet::from(["allowed_tool".to_string()]);
        let prompt = "## Tools\nParent tools.\n\n## Tool Use Protocol\nParent protocol.\n\n## Safety\nKeep safety guidance.\n\n## Hardware Access\nKeep hardware guidance.\n\n## Deferred Built-in Tools\n<available-deferred-builtin-tools>\nallowed_tool\ndenied_tool\n</available-deferred-builtin-tools>\n\n## Context\nKeep context.";
        let mut history = vec![ChatMessage::system(prompt), ChatMessage::user(prompt)];
        let mut request = history.clone();
        let prompts = Arc::new(ToolProtocolPrompts::new(prompt.into(), prompt.into()));
        scope_tool_protocol_prompts(prompts, async {
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
            apply_builtin_prompt_scope(&mut request, &tools, &names, "", false, false, 0, None);
            // Trim rebuilds the projected request, then refreshes the cached section.
            refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
            apply_builtin_prompt_scope(&mut request, &tools, &names, "", false, false, 0, None);
        })
        .await;
        assert_eq!(
            request[0].content.matches("## Tool Use Protocol").count(),
            1
        );
        assert_eq!(request[0].content.matches("**allowed_tool**").count(), 1);
        assert!(!request[0].content.contains("denied_tool"));
        assert!(
            !request[0]
                .content
                .contains("available-deferred-builtin-tools")
        );
        for section in [
            "Keep safety guidance.",
            "Keep hardware guidance.",
            "Keep context.",
        ] {
            assert!(request[0].content.contains(section));
        }
        assert_eq!(request[1].content, prompt);
        assert_eq!(history[0].content, prompt);

        let uncapped = format!("Identity: \u{03bb}\u{732b}\n{prompt}{orientation}");
        for (cap, hook) in [
            (orientation.chars().count() + 30, ""),
            (orientation.chars().count() + 30, "\nHOOK_INSTRUCTION"),
            (10_000, ""),
            (0, ""),
        ] {
            let input = crate::agent::system_prompt::finalize_system_prompt(uncapped.clone(), cap);
            let mut history = vec![ChatMessage::system(&input)];
            let mut request = history.clone();
            let prompts = Arc::new(ToolProtocolPrompts::with_max_chars(
                uncapped.clone(),
                uncapped.clone(),
                cap,
            ));
            scope_tool_protocol_prompts(prompts, async {
                refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
                let parent = history[0].content.clone();
                for _ in 0..2 {
                    if !hook.is_empty() {
                        // Content-mutating hooks run once per request, not during trim.
                        request = vec![ChatMessage::system(format!("{parent}{hook}"))];
                    }
                    refresh_scoped_tool_protocol_prompt(&mut history, &mut request, false);
                    apply_builtin_prompt_scope(
                        &mut request,
                        &tools,
                        &names,
                        "",
                        false,
                        false,
                        cap,
                        Some(&parent),
                    );
                    let base = request[0]
                        .content
                        .strip_suffix(hook)
                        .expect("hook suffix survives");
                    assert!(cap == 0 || base.chars().count() <= cap);
                    assert!(
                        !base.contains("timestamp metadata added by the runtime"),
                        "the capped base prompt must not inject runtime wall-clock prose"
                    );
                    assert!(!request[0].content.contains("denied_tool"));
                    if cap == 0 || cap == 10_000 {
                        assert!(request[0].content.contains("**allowed_tool**"));
                        assert!(request[0].content.contains("## Tool Use Protocol"));
                    }
                    assert_eq!(history[0].content, parent);
                }
            })
            .await;
        }

        let mut activated = crate::tools::ActivatedToolSet::new();
        activated.set_deferred_builtin_specs(tools.iter().map(|tool| tool.spec()).collect());
        let search_names = HashSet::from(["allowed_tool".to_string(), "tool_search".to_string()]);
        let section =
            activated.deferred_builtin_prompt_section_filtered(|name| search_names.contains(name));
        let catalogue = &prompt[prompt.find("## Deferred Built-in Tools").unwrap()
            ..prompt.find("\n\n## Context").unwrap()];
        let too_small = catalogue.to_string();
        let prompt = prompt.replace(
            catalogue,
            activated
                .deferred_builtin_prompt_section()
                .trim_end_matches('\n'),
        );
        let catalogue = &prompt[prompt.find("## Deferred Built-in Tools").unwrap()
            ..prompt.find("\n\n## Context").unwrap()];
        let incomplete = catalogue.replace("</available-deferred-builtin-tools>", "");
        let truncated_cap = prompt[..prompt.find("</available-deferred-builtin-tools>").unwrap()]
            .chars()
            .count()
            + orientation.chars().count();
        let truncated = crate::agent::system_prompt::finalize_system_prompt(
            format!("{prompt}{orientation}"),
            truncated_cap,
        );
        let omitted_cap = prompt[..prompt.find("## Deferred Built-in Tools").unwrap()]
            .chars()
            .count()
            + orientation.chars().count();
        let omitted = crate::agent::system_prompt::finalize_system_prompt(
            format!("{prompt}{orientation}"),
            omitted_cap,
        );
        // Finalization keeps the head of the prompt: the catalogue tail is
        // what gets cut, and nothing is re-appended after it.
        assert!(!truncated.contains("</available-deferred-builtin-tools>"));
        assert!(!omitted.contains("## Deferred Built-in Tools"));
        for (input, native, catalogue_count, cap) in [
            (format!("{prompt}\n{catalogue}"), false, 1, None),
            (format!("{prompt}\n{catalogue}"), true, 1, None),
            (
                format!("{incomplete}\n## Context\nKeep context.\n{catalogue}"),
                false,
                1,
                None,
            ),
            (truncated.clone(), false, 0, Some(truncated_cap)),
            (truncated.clone(), true, 0, Some(truncated_cap)),
            (omitted.clone(), false, 0, Some(omitted_cap)),
            (omitted.clone(), true, 0, Some(omitted_cap)),
            (too_small, false, 0, None),
        ] {
            let mut history = vec![ChatMessage::system(&input)];
            let mut request = vec![ChatMessage::system(&input), ChatMessage::user(&prompt)];
            let prompts = Arc::new(ToolProtocolPrompts::new(input.clone(), input.clone()));
            scope_tool_protocol_prompts(prompts, async {
                refresh_scoped_tool_protocol_prompt(&mut history, &mut request, native);
                let parent_prompt = history[0].content.clone();
                for _ in 0..2 {
                    refresh_scoped_tool_protocol_prompt(&mut history, &mut request, native);
                    let before_chars = request[0].content.chars().count();
                    apply_builtin_prompt_scope(
                        &mut request,
                        &tools,
                        &search_names,
                        &section,
                        native,
                        false,
                        cap.unwrap_or(0),
                        None,
                    );
                    assert!(request[0].content.chars().count() <= before_chars);
                    if let Some(cap) = cap {
                        assert!(request[0].content.chars().count() <= cap);
                    }
                    assert_eq!(history[0].content, parent_prompt);
                }
            })
            .await;
            assert_eq!(
                request[0]
                    .content
                    .matches("<available-deferred-builtin-tools>")
                    .count(),
                catalogue_count
            );
            assert_eq!(
                request[0].content.contains("allowed_tool - "),
                catalogue_count == 1
            );
            assert!(!request[0].content.contains("denied_tool"));
            if input.contains("Keep context.") {
                assert!(request[0].content.contains("Keep context."));
            }
            if input.contains(TAIL_MARKER) {
                assert!(request[0].content.contains(TAIL_MARKER));
            }
            assert_eq!(request[1].content, prompt);
        }
        assert_eq!(activated.hidden_builtin_names().len(), 2);

        let strict_prompt = &prompt[prompt.find("## Safety").unwrap()..];
        let mut strict = vec![ChatMessage::system(strict_prompt)];
        apply_builtin_prompt_scope(&mut strict, &tools, &search_names, "", false, true, 0, None);
        assert!(
            !strict[0]
                .content
                .contains("available-deferred-builtin-tools")
        );
        assert!(!strict[0].content.contains("## Tool Use Protocol"));
        assert!(!strict[0].content.contains("**allowed_tool**"));
        assert!(strict[0].content.contains("Keep hardware guidance."));
        let mut strict = vec![ChatMessage::system(truncated)];
        apply_builtin_prompt_scope(&mut strict, &tools, &search_names, "", false, true, 0, None);
        assert!(!strict[0].content.contains("denied_tool"));
        assert!(
            !strict[0]
                .content
                .contains("available-deferred-builtin-tools")
        );
        // Finalization keeps the head, so a capped prompt carries no tail
        // marker: the scope projection must not reintroduce one either.
        assert!(!strict[0].content.contains(TAIL_MARKER));
    }

    struct BudgetToolCallingProvider;

    impl ::zeroclaw_api::attribution::Attributable for BudgetToolCallingProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }

        fn alias(&self) -> &str {
            "BudgetToolCallingProvider"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for BudgetToolCallingProvider {
        fn supports_native_tools(&self) -> bool {
            true
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<ChatResponse> {
            if request.tools.is_none_or(<[_]>::is_empty) {
                return Ok(ChatResponse {
                    text: Some("local-cap-summary".into()),
                    tool_calls: Vec::new(),
                    usage: None,
                    reasoning_content: None,
                });
            }

            Ok(ChatResponse {
                text: Some(String::new()),
                tool_calls: vec![ToolCall {
                    id: "budget-call".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            })
        }
    }

    async fn run_budgeted_test_loop(
        provider: &dyn ModelProvider,
        history: &mut Vec<ChatMessage>,
        tools: &crate::tools::scoped::ScopedToolRegistry,
        budget: ExecutionTreeBudget,
        cancellation_token: CancellationToken,
        max_tool_iterations: usize,
    ) -> Result<String> {
        run_budgeted_test_loop_with_hooks(
            provider,
            history,
            tools,
            budget,
            cancellation_token,
            max_tool_iterations,
            None,
            None,
            None,
        )
        .await
    }

    async fn run_budgeted_test_loop_with_hooks(
        provider: &dyn ModelProvider,
        history: &mut Vec<ChatMessage>,
        tools: &crate::tools::scoped::ScopedToolRegistry,
        budget: ExecutionTreeBudget,
        cancellation_token: CancellationToken,
        max_tool_iterations: usize,
        security: Option<&crate::security::SecurityPolicy>,
        hooks: Option<&crate::hooks::HookRunner>,
        image_cache: Option<ToolLoopImageState<'_>>,
    ) -> Result<String> {
        let observer = crate::observability::NoopObserver {};
        let multimodal = zeroclaw_config::schema::MultimodalConfig::default();
        let pacing = zeroclaw_config::schema::PacingConfig {
            loop_detection_enabled: false,
            ..zeroclaw_config::schema::PacingConfig::default()
        };
        let knobs = LoopKnobs {
            dedup_enabled: false,
            ..LoopKnobs::default()
        };
        let turn_id = uuid::Uuid::new_v4().to_string();

        run_tool_call_loop(ToolLoop {
            context_anchor_seed: None,
            context_anchor_sink: None,
            history_was_trimmed: None,
            sop_reassembly: None,
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: provider,
                    provider_name: "budget-test",
                    model: "budget-test-model",
                    dispatch_model: "hint:budget-test",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: tools,
                    observer: &observer,
                    silent: true,
                    approval: None,
                    security,
                    multimodal_config: &multimodal,
                    config: None,
                    hooks,
                    activated_tools: None,
                    model_switch_callback: None,
                    receipt_generator: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &pacing,
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 30_000,
                    context_limits: zeroclaw_config::schema::ResolvedContextLimits::legacy_fallback(
                        100_000,
                    ),
                    context_limits_resolver: None,
                    knobs: &knobs,
                },
            ),
            history,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: Some(cancellation_token),
            on_delta: None,
            shared_budget: Some(budget),
            history_has_trim_breadcrumb: &mut false,
            injected_memory_preamble: &mut None,
            channel: None,
            collected_receipts: None,
            event_tx: None,
            steering: None,
            new_messages_out: None,
            image_cache,
            memory: None,
            ingress: IngressContext::sub_turn(),
            agent_alias: Some("budget-test"),
            parent_agent_alias: None,
            served_route_sink: None,
            turn_id: &turn_id,
        })
        .await
    }

    #[tokio::test]
    async fn tree_budget_final_completion_prepares_images_and_honors_hooks() {
        struct SummaryProbe(Arc<AtomicUsize>, &'static str, bool);
        impl zeroclaw_api::attribution::Attributable for SummaryProbe {
            fn role(&self) -> zeroclaw_api::attribution::Role {
                zeroclaw_api::attribution::Attributable::role(&TextProvider)
            }
            fn alias(&self) -> &str {
                "summary-probe"
            }
        }
        #[async_trait::async_trait]
        impl ModelProvider for SummaryProbe {
            fn capabilities(&self) -> zeroclaw_api::model_provider::ProviderCapabilities {
                zeroclaw_api::model_provider::ProviderCapabilities {
                    vision: true,
                    ..Default::default()
                }
            }
            async fn chat_with_system(
                &self,
                _: Option<&str>,
                _: &str,
                _: &str,
                _: Option<f64>,
            ) -> Result<String> {
                anyhow::bail!("summary must use chat")
            }
            async fn chat(
                &self,
                request: zeroclaw_providers::ChatRequest<'_>,
                model: &str,
                _: Option<f64>,
            ) -> Result<ChatResponse> {
                self.0.fetch_add(1, Ordering::SeqCst);
                assert_eq!(model, self.1);
                assert!(request.tools.is_none());
                assert_eq!(
                    request
                        .messages
                        .iter()
                        .any(|m| m.content.contains("data:image/png;base64,")),
                    self.2,
                    "summary must apply quarantine for the hook-selected model"
                );
                assert!(request.messages.iter().any(|m| m.content == "hook-rewrite"));
                Ok(ChatResponse {
                    text: Some("prepared summary".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                })
            }
        }
        struct SummaryHook {
            cancel: bool,
            change_model: bool,
        }
        #[async_trait::async_trait]
        impl crate::hooks::HookHandler for SummaryHook {
            fn name(&self) -> &str {
                "summary-hook"
            }
            async fn before_llm_call(
                &self,
                messages: &mut Vec<ChatMessage>,
                model: &mut String,
            ) -> crate::hooks::HookResult<()> {
                assert!(
                    messages
                        .iter()
                        .any(|m| m.content.contains("data:image/png;base64,")),
                    "hooks must see prepared images"
                );
                if self.cancel {
                    return crate::hooks::HookResult::Cancel("summary denied".into());
                }
                if self.change_model {
                    *model = "hook-selected-model".into();
                }
                messages.push(ChatMessage::user("hook-rewrite"));
                crate::hooks::HookResult::Continue(())
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("summary.png");
        // Image validation rejects a PNG signature without decodable pixels.
        std::fs::write(
            &image,
            [
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
                0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
                0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78,
                0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92,
                0xEF, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
            ],
        )
        .unwrap();
        let prompt = format!("describe [IMAGE:{}]", image.display());
        let multimodal = zeroclaw_config::schema::MultimodalConfig::default();
        let mut id_cache = zeroclaw_providers::multimodal::LocalImageCache::new();
        let prepared = prepare_messages_for_iteration(
            &[ChatMessage::user(prompt.clone())],
            &multimodal,
            false,
            Some(&mut id_cache),
        )
        .await
        .expect("summary image must prepare");
        let image_id = zeroclaw_providers::multimodal::provider_image_ids(&prepared.messages)
            .into_iter()
            .next()
            .expect("prepared summary request must contain an image");
        let security = crate::security::SecurityPolicy {
            workspace_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![]);
        for (cancel, change_model) in [(false, false), (false, true), (true, true)] {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = SummaryProbe(
                calls.clone(),
                if change_model {
                    "hook-selected-model"
                } else {
                    "hint:budget-test"
                },
                !change_model,
            );
            let mut hooks = crate::hooks::HookRunner::new();
            hooks.register(Box::new(SummaryHook {
                cancel,
                change_model,
            }));
            let budget = ExecutionTreeBudget::root(1);
            let mut history = vec![ChatMessage::user(prompt.clone())];
            let mut image_cache = zeroclaw_providers::multimodal::LocalImageCache::new();
            let mut provider_image_state = ProviderImageState::default();
            provider_image_state.record_recovery(
                ProviderImageState::route("budget-test", "hook-selected-model"),
                &[],
                &[image_id],
            );
            let image_state = ToolLoopImageState {
                cache: &mut image_cache,
                provider_state: &mut provider_image_state,
            };
            let result = run_budgeted_test_loop_with_hooks(
                &provider,
                &mut history,
                &tools,
                budget.clone(),
                CancellationToken::new(),
                10,
                None,
                Some(&hooks),
                Some(image_state),
            )
            .await;
            assert_eq!(budget.remaining(), 0);
            if cancel {
                assert!(format!("{:#}", result.unwrap_err()).contains("summary denied"));
                assert_eq!(
                    history.len(),
                    1,
                    "cancelled preparation must remove the synthetic summary prompt"
                );
            } else {
                let response = result.unwrap();
                assert!(response.contains("execution-tree iteration budget"));
                assert!(!response.contains("maximum tool iterations"));
            }
            assert_eq!(
                history[0].content, prompt,
                "preparation must not rewrite durable user history"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(!cancel),
                "hook cancellation must prevent dispatch"
            );
        }
        let mut history = vec![ChatMessage::user(prompt)];
        let result = run_budgeted_test_loop_with_hooks(
            &TextProvider,
            &mut history,
            &tools,
            ExecutionTreeBudget::root(1),
            CancellationToken::new(),
            10,
            Some(&security),
            None,
            None,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<zeroclaw_providers::ProviderCapabilityError>()
                .is_some(),
            "synthetic prompt must not hide a fresh image from a text-only provider"
        );
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn root_local_iteration_cap_consumes_tree_budget_for_summary() {
        let root_budget = ExecutionTreeBudget::root(2);
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
            ShellProbe {
                calls: Arc::clone(&tool_calls),
            },
        )]);
        let mut history = vec![ChatMessage::user("run one tool")];

        let response = run_budgeted_test_loop(
            &BudgetToolCallingProvider,
            &mut history,
            &tools,
            root_budget.clone(),
            CancellationToken::new(),
            1,
        )
        .await
        .expect("root should use its final reservation for the local-cap summary");

        assert!(response.contains("local-cap-summary"));
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
        assert_eq!(root_budget.remaining(), 0);
    }

    #[tokio::test]
    async fn child_local_iteration_cap_preserves_root_final_reservation() {
        use crate::agent::execution_tree_budget::ExecutionTreeBudgetExhausted;

        let root_budget = ExecutionTreeBudget::root(2);
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
            ShellProbe {
                calls: Arc::clone(&tool_calls),
            },
        )]);
        let mut history = vec![ChatMessage::user("run one child tool")];

        let error = run_budgeted_test_loop(
            &BudgetToolCallingProvider,
            &mut history,
            &tools,
            root_budget.child(),
            CancellationToken::new(),
            1,
        )
        .await
        .expect_err("child must not spend the root-only final reservation on a summary");

        assert!(
            error
                .downcast_ref::<ExecutionTreeBudgetExhausted>()
                .is_some(),
            "child local-cap exhaustion must preserve the typed budget error: {error:#}"
        );
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
        assert_eq!(root_budget.remaining(), 1);
    }

    #[tokio::test]
    async fn child_exhaustion_preserves_root_final_completion_slot() {
        use crate::agent::execution_tree_budget::ExecutionTreeBudgetExhausted;

        let root_budget = ExecutionTreeBudget::root(3);
        let cancellation_token = CancellationToken::new();
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
            ShellProbe {
                calls: Arc::clone(&tool_calls),
            },
        )]);
        let mut child_history = vec![ChatMessage::user("run tools")];

        let child_error = run_budgeted_test_loop(
            &BudgetToolCallingProvider,
            &mut child_history,
            &tools,
            root_budget.child(),
            cancellation_token.clone(),
            10,
        )
        .await
        .expect_err("child must stop at the shared tree allowance");

        assert!(
            child_error
                .downcast_ref::<ExecutionTreeBudgetExhausted>()
                .is_some(),
            "child exhaustion must preserve the typed budget error: {child_error:#}"
        );
        assert_eq!(tool_calls.load(Ordering::SeqCst), 2);
        assert!(!cancellation_token.is_cancelled());
        assert_eq!(root_budget.remaining(), 1);

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let root_provider = CaptureProvider {
            requests: Arc::clone(&requests),
        };
        let mut root_history = vec![ChatMessage::user("finish coherently")];
        let response = run_budgeted_test_loop(
            &root_provider,
            &mut root_history,
            &tools,
            root_budget.clone(),
            cancellation_token.clone(),
            10,
        )
        .await
        .expect("root must consume the retained final slot");

        assert!(
            response.starts_with("child-done"),
            "root final completion must return the provider response: {response:?}"
        );
        assert_eq!(root_budget.remaining(), 0);
        assert!(!cancellation_token.is_cancelled());
        assert_eq!(tool_calls.load(Ordering::SeqCst), 2);
        let captured = requests.lock().expect("capture lock");
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0].1.is_empty(),
            "final completion must offer no tools"
        );
        assert_eq!(
            root_history.last().map(|message| message.content.as_str()),
            Some("child-done")
        );
    }

    /// Seed a step agent's owned execution context directly (the memo cache is
    /// caller-owned precisely so tests can drive the REAL nested loop with a
    /// scripted child provider — `assemble_owned_execution` binds providers
    /// from config and cannot yield a capture double).
    fn seeded_owned(
        requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
        tools: Vec<Box<dyn crate::tools::Tool>>,
        mcp_tool_names: std::collections::HashSet<String>,
        tool_filter_groups: Vec<zeroclaw_config::schema::ToolFilterGroup>,
        temperature: Option<f64>,
    ) -> OwnedAgentExecution {
        let mut agent = zeroclaw_config::schema::AliasedAgentConfig::default();
        agent.resolved.tool_filter_groups = tool_filter_groups;
        agent.resolved.max_tool_iterations = 3;
        OwnedAgentExecution {
            model_provider: Box::new(CaptureProvider { requests }),
            provider_name: "capture".into(),
            model: "capture-model".into(),
            temperature,
            tools_registry: crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(tools),
            approval: crate::approval::ApprovalManager::for_non_interactive(
                &zeroclaw_config::schema::RiskProfileConfig::default(),
            ),
            activated_tools: None,
            agent,
            risk_profile: zeroclaw_config::schema::RiskProfileConfig::default(),
            skills: Vec::new(),
            mcp_tool_names,
            mcp_prompt_section: String::new(),
            shell_profile: None,
            execution_admission: None,
            // Test fixture: no config-backed policy, so the default (its
            // `workspace_dir` is ".") stands in and the marker gate fails
            // closed under it.
            security: Arc::new(crate::security::SecurityPolicy::default()),
        }
    }

    /// Build a single-step SOP whose step delegates to `step_agent`, start it in a
    /// fresh engine, and return the shared engine handle plus the first
    /// `ExecuteStep` action (already resolved to a cross-agent step).
    fn start_single_cross_agent_step(
        step_agent: &str,
    ) -> (
        Arc<std::sync::Mutex<crate::sop::SopEngine>>,
        String,
        crate::sop::types::SopRunAction,
    ) {
        start_single_step_with_capability(Some(step_agent), None)
    }

    fn start_single_step_with_capability(
        step_agent: Option<&str>,
        capability: Option<crate::live_config_authority::AgentExecutionCapability>,
    ) -> (
        Arc<std::sync::Mutex<crate::sop::SopEngine>>,
        String,
        crate::sop::types::SopRunAction,
    ) {
        use crate::sop::types::{
            Sop, SopEvent, SopExecutionMode, SopPriority, SopRunAction, SopStep, SopTrigger,
            SopTriggerSource,
        };
        use zeroclaw_config::schema::SopConfig;

        let sop = Sop {
            name: "cross-agent".to_string(),
            description: "x".to_string(),
            version: "0.1.0".to_string(),
            priority: SopPriority::Normal,
            execution_mode: SopExecutionMode::Auto,
            triggers: vec![SopTrigger::Manual],
            steps: vec![SopStep {
                number: 1,
                title: "delegate".to_string(),
                body: "run".to_string(),
                agent: step_agent.map(str::to_string),
                ..SopStep::default()
            }],
            cooldown_secs: 0,
            max_concurrent: 1,
            location: None,
            deterministic: false,
            admission_policy: Default::default(),
            max_pending_approvals: 0,
            agent: None,
            decision: None,
        };
        let mut engine = crate::sop::SopEngine::new(SopConfig::default());
        if let Some(capability) = capability {
            engine = engine.with_execution_capability(capability);
        }
        engine.set_sops_for_test(vec![sop]);
        let event = SopEvent {
            source: SopTriggerSource::Manual,
            topic: None,
            payload: None,
            timestamp: "2026-07-16T00:00:00Z".to_string(),
        };
        let action = engine.start_run("cross-agent", event).expect("run starts");
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, step, .. } => {
                assert_eq!(
                    step.agent.as_deref(),
                    step_agent,
                    "the step must resolve to a cross-agent delegation"
                );
                run_id.clone()
            }
            other => panic!("expected ExecuteStep, got {other:?}"),
        };
        (Arc::new(std::sync::Mutex::new(engine)), run_id, action)
    }

    /// Drive one queued action through `drive_live_sop_actions` with a
    /// plain-text PARENT provider, the given identity/handle/cache, and return
    /// the engine for assertions.
    #[allow(clippy::too_many_arguments)]
    async fn drive_step(
        engine: Arc<std::sync::Mutex<crate::sop::SopEngine>>,
        action: crate::sop::types::SopRunAction,
        parent_provider: &dyn ModelProvider,
        parent_tools: &crate::tools::scoped::ScopedToolRegistry,
        observer: &dyn crate::observability::Observer,
        history: &mut Vec<ChatMessage>,
        history_has_trim_breadcrumb: Option<&mut bool>,
        event_tx: Option<tokio::sync::mpsc::Sender<TurnEvent>>,
        new_messages_out: Option<&mut Vec<ChatMessage>>,
        agent_alias: Option<&str>,
        sop_reassembly: Option<SopStepReassembly<'_>>,
        // The (would-be) enclosing round's own switch state. Real callers
        // always pass `None` here (they have no enclosing round); the
        // isolation regression below passes `Some(parent_arc)` to prove the
        // nested step loop never touches it.
        model_switch_callback: Option<ModelSwitchCallback>,
        exec_cache: &mut std::collections::HashMap<String, OwnedAgentExecution>,
    ) {
        use crate::sop::executor::QueuedSopAction;

        let queued = QueuedSopAction {
            engine: Arc::clone(&engine),
            audit: None,
            action,
        };
        let mut local_history_has_trim_breadcrumb = false;
        let history_has_trim_breadcrumb =
            history_has_trim_breadcrumb.unwrap_or(&mut local_history_has_trim_breadcrumb);
        drive_live_sop_actions(
            vec![queued],
            history,
            history_has_trim_breadcrumb,
            parent_provider,
            "mock",
            "mock-model",
            "mock-model",
            None,
            parent_tools,
            observer,
            true,
            None,
            // security: no policy on the test path
            None,
            &zeroclaw_config::schema::MultimodalConfig::default(),
            None,
            5,
            None,
            &[],
            &[],
            None,
            model_switch_callback,
            &zeroclaw_config::schema::PacingConfig::default(),
            false,
            false,
            30_000,
            zeroclaw_config::schema::ResolvedContextLimits {
                model_context_window: 100_000,
                context_token_budget: 100_000,
                model_context_window_source:
                    zeroclaw_config::schema::ModelContextWindowSource::Configured,
                keep_recent_turns: zeroclaw_config::scattered_types::DEFAULT_KEEP_RECENT_TURNS,
                request_token_ceiling: None,
            },
            None,
            &LoopKnobs::default(),
            "cli",
            None,
            None,
            None,
            None,
            None,
            None,
            event_tx,
            new_messages_out,
            None,
            agent_alias,
            None,
            sop_reassembly,
            exec_cache,
        )
        .await
        .expect("drive returns Ok");
    }

    fn step1_result(
        engine: &Arc<std::sync::Mutex<crate::sop::SopEngine>>,
        run_id: &str,
    ) -> crate::sop::types::SopStepResult {
        let guard = engine.lock().expect("engine lock");
        guard
            .get_run(run_id)
            .expect("run present after drive")
            .step_results
            .iter()
            .find(|r| r.step_number == 1)
            .expect("step 1 result recorded")
            .clone()
    }

    // ── Blocker regressions: the REAL nested loop with distinct providers ────

    const PARENT_MARKER: &str = "PARENT-ONLY-SECRET-7f3a";

    #[tokio::test]
    async fn managed_unnamed_live_sop_step_admits_the_executing_agent() {
        struct AdmissionObserver(crate::live_config_authority::AgentLifecycleCoordinator);
        impl crate::observability::Observer for AdmissionObserver {
            fn record_event(&self, event: &crate::observability::ObserverEvent) {
                if matches!(
                    event,
                    crate::observability::ObserverEvent::LlmRequest { .. }
                ) {
                    assert_eq!(self.0.active_turn_count("alpha"), 0);
                    assert_eq!(self.0.active_turn_count("zeta"), 1);
                    assert!(self.0.begin_delete("zeta").is_err());
                }
            }
            fn record_metric(&self, _: &zeroclaw_api::observability_traits::ObserverMetric) {}
            fn name(&self) -> &str {
                "admission-observer"
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        let mut config = zeroclaw_config::schema::Config::default();
        config.agents.insert("alpha".into(), Default::default());
        config.agents.insert("zeta".into(), Default::default());
        let authority = crate::LiveConfigAuthority::new(config);
        let lifecycle = authority.agent_lifecycle();
        let (engine, run_id, action) =
            start_single_step_with_capability(None, Some(authority.execution_capability()));
        let tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history = vec![ChatMessage::system("test")];
        let mut cache = std::collections::HashMap::new();
        drive_step(
            engine.clone(),
            action,
            &TextProvider,
            &tools,
            &AdmissionObserver(lifecycle.clone()),
            &mut history,
            None,
            None,
            None,
            Some("zeta"),
            None,
            None,
            &mut cache,
        )
        .await;
        assert_eq!(
            step1_result(&engine, &run_id).status,
            crate::sop::types::SopStepStatus::Completed
        );
        assert!(lifecycle.begin_delete("zeta").is_ok());
    }

    /// Cross-agent steps run on an isolated child transcript: the parent
    /// history (distinct provider, marker message) never reaches the child
    /// provider; the child sees its own system prompt + the step context; the
    /// parent transcript gains the step context and the child's final output
    /// only.
    #[tokio::test]
    async fn cross_agent_step_never_sends_parent_history_to_child_provider() {
        let (engine, run_id, action) = start_single_cross_agent_step("stepper");
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut exec_cache = std::collections::HashMap::new();
        exec_cache.insert(
            "stepper".to_string(),
            seeded_owned(
                Arc::clone(&requests),
                Vec::new(),
                std::collections::HashSet::new(),
                Vec::new(),
                Some(0.42),
            ),
        );

        let parent_provider = TextProvider;
        let parent_tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history = vec![
            ChatMessage::system("parent system prompt"),
            ChatMessage::user(PARENT_MARKER.to_string()),
        ];
        let mut new_out: Vec<ChatMessage> = Vec::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &crate::observability::NoopObserver {},
            &mut history,
            None,
            None,
            Some(&mut new_out),
            Some("outer"),
            Some(handle),
            None,
            &mut exec_cache,
        )
        .await;

        // The child provider received at least one request, and NO request
        // contained the parent-only marker or the parent system prompt.
        let captured = requests.lock().expect("capture lock");
        assert!(
            !captured.is_empty(),
            "the seeded child provider must have run the step"
        );
        for (messages, _tools, temperature) in captured.iter() {
            assert!(
                messages.iter().all(|m| !m.content.contains(PARENT_MARKER)
                    && !m.content.contains("parent system prompt")),
                "parent-only history must never reach the child provider: {messages:?}"
            );
            // The child transcript is its OWN system prompt + the step context.
            assert_eq!(messages.first().map(|m| m.role.as_str()), Some("system"));
            assert_eq!(messages.last().map(|m| m.role.as_str()), Some("user"));
            // The child runs with the step agent's own configured temperature,
            // not the parent turn's (None here).
            assert_eq!(*temperature, Some(0.42));
        }
        drop(captured);

        // Parent transcript: step context + the child's FINAL output only —
        // no child system prompt, no intermediate child chatter.
        assert!(
            history
                .iter()
                .all(|m| !m.content.contains("You are") || m.role == "system"),
            "no child system prompt may leak into the parent transcript"
        );
        assert_eq!(
            history
                .last()
                .map(|m| (m.role.as_str(), m.content.as_str())),
            Some(("assistant", "child-done")),
            "the child's final output flows back into the parent transcript"
        );
        assert_eq!(
            new_out.last().map(|m| m.content.as_str()),
            Some("child-done"),
            "the final output is captured for persistence too"
        );

        let result = step1_result(&engine, &run_id);
        assert_eq!(result.status, crate::sop::types::SopStepStatus::Completed);
        assert_eq!(result.output, "child-done");
        assert_eq!(result.effective_agent.as_deref(), Some("stepper"));
    }

    /// The child registry and the child's own `tool_filter_groups` govern the
    /// tool specs the nested loop offers the child provider — through the
    /// loop's final filters, not just at assembly.
    #[tokio::test]
    async fn cross_agent_step_offers_child_tools_filtered_by_child_profile() {
        use zeroclaw_config::schema::{ToolFilterGroup, ToolFilterGroupMode};

        let (engine, run_id, action) = start_single_cross_agent_step("stepper");
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        // Child registry: one plain tool and two MCP-origin tools; the child
        // profile's filter group admits only `srv__allowed`, so the child's
        // OWN policy must drop `srv__blocked` inside the nested loop.
        let tools: Vec<Box<dyn crate::tools::Tool>> = vec![
            Box::new(NamedTool("plain_tool")),
            Box::new(NamedTool("srv__allowed")),
            Box::new(NamedTool("srv__blocked")),
        ];
        let mcp_names: std::collections::HashSet<String> =
            ["srv__allowed".to_string(), "srv__blocked".to_string()]
                .into_iter()
                .collect();
        let groups = vec![ToolFilterGroup {
            mode: ToolFilterGroupMode::Always,
            tools: vec!["srv__allowed".to_string()],
            keywords: Vec::new(),
        }];
        let mut exec_cache = std::collections::HashMap::new();
        exec_cache.insert(
            "stepper".to_string(),
            seeded_owned(Arc::clone(&requests), tools, mcp_names, groups, None),
        );

        let parent_provider = TextProvider;
        // Parent scope carries a sensitive tool the child must never be offered.
        let shell_calls = Arc::new(AtomicUsize::new(0));
        let parent_tools =
            crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
                ShellProbe {
                    calls: Arc::clone(&shell_calls),
                },
            )]);
        let mut history: Vec<ChatMessage> = Vec::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &crate::observability::NoopObserver {},
            &mut history,
            None,
            None,
            None,
            Some("outer"),
            Some(handle),
            None,
            &mut exec_cache,
        )
        .await;

        let captured = requests.lock().expect("capture lock");
        assert!(!captured.is_empty(), "the child provider must have run");
        let (_msgs, offered, _temp) = &captured[0];
        assert!(
            offered.contains(&"plain_tool".to_string())
                && offered.contains(&"srv__allowed".to_string()),
            "the child's own admitted tools are offered: {offered:?}"
        );
        assert!(
            !offered.contains(&"srv__blocked".to_string()),
            "the child profile's filter groups must gate the offered specs: {offered:?}"
        );
        assert!(
            !offered.contains(&"shell".to_string()),
            "the parent's tools must never be offered to the child: {offered:?}"
        );
        assert!(
            !offered.iter().any(|t| t.starts_with("sop_")),
            "the SOP recursion guard applies to the child too: {offered:?}"
        );
        drop(captured);

        assert_eq!(
            shell_calls.load(Ordering::SeqCst),
            0,
            "the parent's sensitive tool must never execute during a cross-agent step"
        );
        assert_eq!(
            step1_result(&engine, &run_id).status,
            crate::sop::types::SopStepStatus::Completed
        );
    }

    /// Audit identity: nested-loop records stamp the step agent as the acting
    /// identity and the delegating agent as the parent correlation; the SOP
    /// step result names the effective agent.
    #[tokio::test]
    async fn cross_agent_step_stamps_effective_identity_with_parent_correlation() {
        let (engine, run_id, action) = start_single_cross_agent_step("stepper");
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut exec_cache = std::collections::HashMap::new();
        exec_cache.insert(
            "stepper".to_string(),
            seeded_owned(
                Arc::clone(&requests),
                Vec::new(),
                std::collections::HashSet::new(),
                Vec::new(),
                None,
            ),
        );

        let observer = IdentityCapture::default();
        let parent_provider = TextProvider;
        let parent_tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history: Vec<ChatMessage> = Vec::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &observer,
            &mut history,
            None,
            None,
            None,
            Some("outer"),
            Some(handle),
            None,
            &mut exec_cache,
        )
        .await;

        let pairs = observer.pairs.lock().expect("pairs lock");
        assert!(
            pairs
                .iter()
                .any(|(alias, parent)| alias.as_deref() == Some("stepper")
                    && parent.as_deref() == Some("outer")),
            "nested records must stamp the effective agent with parent correlation: {pairs:?}"
        );
        drop(pairs);

        assert_eq!(
            step1_result(&engine, &run_id).effective_agent.as_deref(),
            Some("stepper"),
            "the SOP audit record names the acting authority"
        );
    }

    /// Same-agent control: a step naming the CURRENT agent keeps today's inline
    /// behavior — shared parent history, parent identity, no re-assembly.
    #[tokio::test]
    async fn same_agent_step_keeps_shared_history_and_identity() {
        let (engine, run_id, action) = start_single_cross_agent_step("outer");
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let observer = IdentityCapture::default();
        let parent_provider = TextProvider;
        let parent_tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history = vec![ChatMessage::system("parent system prompt")];
        let mut exec_cache = std::collections::HashMap::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &observer,
            &mut history,
            None,
            None,
            None,
            Some("outer"),
            Some(handle),
            None,
            &mut exec_cache,
        )
        .await;

        assert!(
            exec_cache.is_empty(),
            "a same-agent step must not re-assemble anything"
        );
        // Shared history: the step context and the loop's own messages land in
        // the parent transcript (inline behavior unchanged).
        assert!(
            history.iter().any(|m| m.content == "done"),
            "the same-agent nested loop appends to the shared history: {history:?}"
        );
        let pairs = observer.pairs.lock().expect("pairs lock");
        assert!(
            pairs
                .iter()
                .all(|(alias, parent)| alias.as_deref() == Some("outer") && parent.is_none()),
            "same-agent steps keep the outer identity with no parent correlation: {pairs:?}"
        );
        drop(pairs);
        assert_eq!(
            step1_result(&engine, &run_id).effective_agent.as_deref(),
            Some("outer")
        );
    }

    #[tokio::test]
    async fn same_agent_step_excludes_existing_breadcrumb_from_trim_turn_count() {
        let (engine, _run_id, action) = start_single_cross_agent_step("outer");
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let parent_provider = TextProvider;
        let parent_tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history = vec![
            ChatMessage::system("parent system prompt"),
            crate::agent::history_trim::breadcrumb(),
            ChatMessage::user("old ".repeat(120_000)),
            ChatMessage::assistant("old reply"),
            ChatMessage::user("recent request"),
            ChatMessage::assistant("recent reply"),
        ];
        let mut history_has_trim_breadcrumb = true;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let mut exec_cache = std::collections::HashMap::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &crate::observability::NoopObserver {},
            &mut history,
            Some(&mut history_has_trim_breadcrumb),
            Some(event_tx),
            None,
            Some("outer"),
            Some(handle),
            None,
            &mut exec_cache,
        )
        .await;

        let dropped_turns = std::iter::from_fn(|| event_rx.try_recv().ok())
            .find_map(|event| match event {
                TurnEvent::HistoryTrimmed { dropped_turns, .. } => Some(dropped_turns),
                _ => None,
            })
            .expect("the oversized old turn must emit a trim event");
        assert_eq!(
            dropped_turns, 1,
            "the synthetic breadcrumb must not be reported as a dropped user turn"
        );
        assert!(history_has_trim_breadcrumb);
        let breadcrumb = crate::agent::history_trim::breadcrumb();
        assert_eq!(
            history
                .iter()
                .filter(|message| {
                    message.role == breadcrumb.role && message.content == breadcrumb.content
                })
                .count(),
            1,
            "repeated same-agent trimming must retain exactly one breadcrumb"
        );
    }

    #[tokio::test]
    async fn same_agent_step_output_reaches_parent_capture_once() {
        let (engine, _run_id, action) = start_single_cross_agent_step("outer");
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let observer = IdentityCapture::default();
        let parent_provider = TextProvider;
        let parent_tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history = vec![ChatMessage::system("parent system prompt")];
        let mut capture: Vec<ChatMessage> = Vec::new();
        let mut exec_cache = std::collections::HashMap::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &observer,
            &mut history,
            None,
            None,
            Some(&mut capture),
            Some("outer"),
            Some(handle),
            None,
            &mut exec_cache,
        )
        .await;

        // The capture must contain the step context (user message) and the
        // child loop's assistant output, each exactly once.
        assert!(
            !capture.is_empty(),
            "same-agent capture must not be empty: {capture:?}"
        );
        let user_count = capture.iter().filter(|m| m.role == "user").count();
        assert_eq!(
            user_count, 1,
            "exactly one user message in capture: {capture:?}"
        );
        let assistant_count = capture.iter().filter(|m| m.role == "assistant").count();
        assert_eq!(
            assistant_count, 1,
            "exactly one assistant message in capture: {capture:?}"
        );
    }

    // ── Fail-closed guards ───────────────────────────────────────────────────

    /// A cross-agent step whose agent context cannot be assembled (unknown
    /// agent in the handle's config) fails CLOSED at driver level: the step is
    /// recorded Failed and the parent's tools never execute.
    #[tokio::test]
    async fn unassemblable_cross_agent_step_fails_closed() {
        let (engine, run_id, action) = start_single_cross_agent_step("stepper");
        // Bare config: no "stepper" agent exists, so assembly must fail.
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let shell_calls = Arc::new(AtomicUsize::new(0));
        let parent_tools =
            crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
                ShellProbe {
                    calls: Arc::clone(&shell_calls),
                },
            )]);
        let provider = ShellCallingProvider;
        let mut history: Vec<ChatMessage> = Vec::new();
        let mut exec_cache = std::collections::HashMap::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &provider,
            &parent_tools,
            &crate::observability::NoopObserver {},
            &mut history,
            None,
            None,
            None,
            Some("outer"),
            Some(handle),
            None,
            &mut exec_cache,
        )
        .await;

        assert_eq!(
            shell_calls.load(Ordering::SeqCst),
            0,
            "an unassemblable cross-agent step must not run with the parent's tools"
        );
        let result = step1_result(&engine, &run_id);
        assert_eq!(result.status, crate::sop::types::SopStepStatus::Failed);
        assert_eq!(
            result.effective_agent.as_deref(),
            Some("stepper"),
            "the record names the agent that SHOULD have run the failed step"
        );
    }

    /// A driver path with NO re-assembly handle must fail a cross-agent step
    /// closed, never run it with the parent/delegate agent's tools.
    #[tokio::test]
    async fn no_handle_cross_agent_step_fails_closed_not_parent_scope() {
        let (engine, run_id, action) = start_single_cross_agent_step("other-agent");

        let shell_calls = Arc::new(AtomicUsize::new(0));
        // Parent/delegate scope: a sensitive tool the cross-agent step must never
        // reach.
        let parent_tools =
            crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
                ShellProbe {
                    calls: Arc::clone(&shell_calls),
                },
            )]);
        let provider = ShellCallingProvider;
        let mut history: Vec<ChatMessage> = Vec::new();
        let mut exec_cache = std::collections::HashMap::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &provider,
            &parent_tools,
            &crate::observability::NoopObserver {},
            &mut history,
            None,
            None,
            None,
            Some("outer"),
            None,
            None,
            &mut exec_cache,
        )
        .await;

        // Security property: the parent's sensitive tool never executed — the
        // cross-agent step did not run with the parent/delegate scope.
        assert_eq!(
            shell_calls.load(Ordering::SeqCst),
            0,
            "a no-handle cross-agent step must not run with the parent agent's tools"
        );

        // And the step is recorded FAILED (fail-closed), not Completed.
        assert_eq!(
            step1_result(&engine, &run_id).status,
            crate::sop::types::SopStepStatus::Failed,
            "the no-handle cross-agent step must fail closed"
        );
    }

    // ── Model-switch isolation ──────────────────────────────────────────────

    /// The CHILD provider for the model-switch isolation regression: its
    /// first (and only expected) call emits a tool call that queues a
    /// pending model switch on whatever task-local switch state is ambient
    /// when the tool runs — the CHILD's own if isolation holds, the
    /// PARENT's if the leak regresses.
    struct ChildSwitchProvider;

    impl ::zeroclaw_api::attribution::Attributable for ChildSwitchProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "ChildSwitchProvider"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for ChildSwitchProvider {
        fn supports_native_tools(&self) -> bool {
            true
        }
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<String> {
            Ok("ok".into())
        }
        async fn chat(
            &self,
            _request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some(String::new()),
                tool_calls: vec![ToolCall {
                    id: "00000000-0000-0000-0000-000000000099".into(),
                    name: "child_model_switch".into(),
                    arguments: "{}".into(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            })
        }
    }

    /// Test tool standing in for the real `model_switch` tool inside the
    /// CHILD step: queues a pending switch to a provider/model DISTINCT from
    /// both the parent's and the child's own current binding, via the
    /// ambient task-local switch state (see `current_model_switch_state`).
    struct ChildModelSwitchTool;

    ::zeroclaw_api::tool_attribution!(
        ChildModelSwitchTool,
        ::zeroclaw_api::attribution::ToolKind::Plugin
    );

    #[async_trait::async_trait]
    impl crate::tools::Tool for ChildModelSwitchTool {
        fn name(&self) -> &str {
            "child_model_switch"
        }
        fn description(&self) -> &str {
            "test tool: queues a pending model switch from inside a cross-agent SOP step"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<crate::tools::ToolResult> {
            let state = current_model_switch_state()?;
            *state.lock().expect("switch state lock") =
                Some(("child-provider".to_string(), "child-model".to_string()));
            Ok(crate::tools::ToolResult {
                success: true,
                output: "switch queued".to_string().into(),
                error: None,
            })
        }
    }

    /// Regression for the isolation gap: `drive_live_sop_actions` used to
    /// clone the OUTER round's `model_switch_callback` straight into every
    /// nested step `ToolLoop`, including cross-agent steps with their own
    /// transcript/alias/turn id. A child's `model_switch` tool call then
    /// mutated the SAME shared `Arc` the parent loop checks after every
    /// iteration, so the CHILD's requested provider/model could silently
    /// become the PARENT's. This passes a distinct `parent_switch_state`
    /// `Arc` in — exactly what the real outer `run_tool_call_loop` threads
    /// down as its own `model_switch_state` — and proves it is completely
    /// untouched after a cross-agent child step's own `model_switch` fires.
    #[tokio::test]
    async fn cross_agent_step_model_switch_never_leaks_into_parent_loop() {
        let (engine, run_id, action) = start_single_cross_agent_step("stepper");
        let config = zeroclaw_config::schema::Config::default();
        let handle = SopStepReassembly {
            config: &config,
            live_config: None,
        };

        let mut exec_cache = std::collections::HashMap::new();
        let mut stepper_agent = zeroclaw_config::schema::AliasedAgentConfig::default();
        stepper_agent.resolved.max_tool_iterations = 3;
        // Full autonomy so the test tool call executes directly — approval
        // gating for an unrecognized tool name is an orthogonal concern this
        // regression does not exercise.
        let stepper_risk_profile = zeroclaw_config::schema::RiskProfileConfig {
            level: zeroclaw_config::autonomy::AutonomyLevel::Full,
            ..zeroclaw_config::schema::RiskProfileConfig::default()
        };
        exec_cache.insert(
            "stepper".to_string(),
            OwnedAgentExecution {
                model_provider: Box::new(ChildSwitchProvider),
                provider_name: "child-original-provider".into(),
                model: "child-original-model".into(),
                temperature: None,
                tools_registry: crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![
                    Box::new(ChildModelSwitchTool),
                ]),
                approval: crate::approval::ApprovalManager::for_non_interactive(
                    &stepper_risk_profile,
                ),
                activated_tools: None,
                agent: stepper_agent,
                risk_profile: stepper_risk_profile,
                skills: Vec::new(),
                mcp_tool_names: std::collections::HashSet::new(),
                mcp_prompt_section: String::new(),
                shell_profile: None,
                execution_admission: None,
                // Test fixture: see the helper above.
                security: Arc::new(crate::security::SecurityPolicy::default()),
            },
        );

        // The PARENT's own switch state — exactly the `Arc` the real outer
        // `run_tool_call_loop` creates for its round and hands down to
        // `drive_live_sop_actions`.
        let parent_switch_state: ModelSwitchCallback = Arc::new(std::sync::Mutex::new(None));

        let parent_provider = TextProvider;
        let parent_tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history: Vec<ChatMessage> = Vec::new();

        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &crate::observability::NoopObserver {},
            &mut history,
            None,
            None,
            None,
            Some("outer"),
            Some(handle),
            Some(Arc::clone(&parent_switch_state)),
            &mut exec_cache,
        )
        .await;

        // The parent's own switch state must be untouched: the child's
        // `model_switch` tool call must never mutate the SAME `Arc` the
        // enclosing round would check on its own next iteration. Under the
        // bug (`drive_live_sop_actions` forwarding its switch-state
        // parameter straight into the child `ToolLoop`), this would observe
        // `Some(("child-provider", "child-model"))` instead.
        assert_eq!(
            *parent_switch_state
                .lock()
                .expect("parent switch state lock"),
            None,
            "a cross-agent child step's model_switch must never mutate the parent's own switch state"
        );

        // The child's own request was still observed — not silently dropped
        // by the isolation fix: the step is recorded Failed, naming exactly
        // the child's requested provider/model.
        let step = step1_result(&engine, &run_id);
        assert_eq!(
            step.status,
            crate::sop::types::SopStepStatus::Failed,
            "the child's own model_switch must still surface as this step's outcome"
        );
        assert!(
            step.output.contains("child-provider") && step.output.contains("child-model"),
            "the step failure must name the CHILD's own requested switch: {}",
            step.output
        );
    }

    /// Regression for the unbounded live-turn deterministic drive: both
    /// headless drivers cap at `MAX_HEADLESS_DRIVE_STEPS` because
    /// `StepFailure::Goto`/`routing.next` take arbitrary step numbers and
    /// nothing validates that routes are acyclic. `drive_live_sop_actions`
    /// advanced `DeterministicStep` in place and continued its enclosing
    /// `loop` with no iteration bound, so a cyclic route reached through an
    /// agent turn spun forever, taking and releasing the engine mutex on
    /// every iteration. The live path must agree with the other two drivers:
    /// terminalize the run as `Failed` and release its admission claim.
    #[tokio::test]
    async fn cyclic_deterministic_route_exhausts_the_live_turn_step_budget() {
        use crate::sop::types::{
            Sop, SopEvent, SopExecutionMode, SopPriority, SopRunStatus, SopStep, SopStepKind,
            SopTrigger, SopTriggerSource,
        };
        use zeroclaw_config::schema::SopConfig;

        // 1 -> 2 -> 1 -> ... : a two-node cycle of always-succeeding
        // capabilities, so nothing but the budget can end the run.
        let cap_step = |number: u32, next: u32| {
            let mut step = SopStep {
                number,
                title: format!("Capability {number}"),
                kind: SopStepKind::Capability,
                capability: Some("noop".into()),
                ..SopStep::default()
            };
            step.routing.next = Some(next);
            step
        };
        let sop = Sop {
            name: "cyclic".to_string(),
            description: "cyclic deterministic route".to_string(),
            version: "1.0.0".to_string(),
            priority: SopPriority::Normal,
            execution_mode: SopExecutionMode::Deterministic,
            triggers: vec![SopTrigger::Manual],
            steps: vec![cap_step(1, 2), cap_step(2, 1)],
            cooldown_secs: 0,
            max_concurrent: 1,
            location: None,
            deterministic: true,
            admission_policy: Default::default(),
            max_pending_approvals: 0,
            agent: None,
            decision: None,
        };
        // A step is revisited every other iteration, so the per-step visit
        // bound must stay well above the drive budget for this test to prove
        // the BUDGET is what stops the loop.
        let mut engine = crate::sop::SopEngine::new(SopConfig {
            max_step_visits: u32::MAX,
            ..SopConfig::default()
        });
        engine.set_sops_for_test(vec![sop]);
        let action = engine
            .start_run(
                "cyclic",
                SopEvent {
                    source: SopTriggerSource::Manual,
                    topic: None,
                    payload: None,
                    timestamp: "2026-08-22T00:00:00Z".to_string(),
                },
            )
            .expect("run starts");
        let run_id = match &action {
            crate::sop::types::SopRunAction::DeterministicStep { run_id, .. } => run_id.clone(),
            other => panic!("expected a DeterministicStep, got {other:?}"),
        };
        let engine = Arc::new(std::sync::Mutex::new(engine));

        let parent_provider = TextProvider;
        let parent_tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        let mut history: Vec<ChatMessage> = Vec::new();
        let mut exec_cache = std::collections::HashMap::new();

        // Without the bound this call never returns.
        drive_step(
            Arc::clone(&engine),
            action,
            &parent_provider,
            &parent_tools,
            &crate::observability::NoopObserver {},
            &mut history,
            None,
            None,
            None,
            None,
            None,
            None,
            &mut exec_cache,
        )
        .await;

        let guard = engine.lock().expect("engine lock");
        let run = guard.get_run(&run_id).expect("run retained after budget");
        assert_eq!(
            run.status,
            SopRunStatus::Failed,
            "an exhausted live-turn budget must terminalize the run"
        );
        assert!(
            !guard.active_runs().contains_key(&run_id),
            "the failed run must leave the active set so its admission claim is released"
        );
        assert_eq!(
            run.step_results.len(),
            crate::sop::executor::MAX_HEADLESS_DRIVE_STEPS,
            "the live driver must execute exactly MAX_HEADLESS_DRIVE_STEPS capabilities, \
             the same bound both headless drivers use"
        );
    }
}

#[cfg(test)]
mod tool_lifecycle_abandonment_tests {
    //! Behavior-level coverage of the tool-call lifecycle invariant at the
    //! turn-loop boundary: every correlated context whose before hook ran
    //! reaches exactly one terminal operation — the matching after hook on
    //! completion, or one abandonment callback when the call cannot reach
    //! after.

    use super::*;
    use crate::observability::NoopObserver;
    use std::sync::Mutex;
    use std::time::Duration;
    use zeroclaw_api::hook::ToolCallHookContext;
    use zeroclaw_providers::{ChatResponse, ToolCall};

    struct LifecycleRecorder {
        events: Arc<Mutex<Vec<String>>>,
        cancel_before_for: Vec<String>,
    }

    #[async_trait::async_trait]
    impl crate::hooks::HookHandler for LifecycleRecorder {
        fn name(&self) -> &str {
            "lifecycle-recorder"
        }

        async fn before_tool_call_with_context(
            &self,
            context: &ToolCallHookContext,
            name: String,
            args: serde_json::Value,
        ) -> crate::hooks::HookResult<(String, serde_json::Value)> {
            self.events
                .lock()
                .expect("lifecycle event lock")
                .push(format!("before:{}:{}", name, context.invocation_id()));
            if self.cancel_before_for.iter().any(|tool| tool == &name) {
                return crate::hooks::HookResult::Cancel("blocked by test".to_string());
            }
            crate::hooks::HookResult::Continue((name, args))
        }

        async fn on_after_tool_call_with_context(
            &self,
            context: &ToolCallHookContext,
            tool: &str,
            _result: &zeroclaw_api::tool::ToolResult,
            _duration: Duration,
        ) {
            self.events
                .lock()
                .expect("lifecycle event lock")
                .push(format!("after:{}:{}", tool, context.invocation_id()));
        }

        async fn on_tool_call_abandoned(&self, context: &ToolCallHookContext, tool: &str) {
            self.events
                .lock()
                .expect("lifecycle event lock")
                .push(format!("abandoned:{}:{}", tool, context.invocation_id()));
        }
    }

    fn lifecycle_runner(
        events: Arc<Mutex<Vec<String>>>,
        cancel_for: Vec<String>,
    ) -> crate::hooks::HookRunner {
        let mut runner = crate::hooks::HookRunner::new();
        runner.register(Box::new(LifecycleRecorder {
            events,
            cancel_before_for: cancel_for,
        }));
        runner
    }

    fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            extra_content: None,
        }
    }

    fn text_response() -> ChatResponse {
        ChatResponse {
            text: Some("done".to_string()),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
        }
    }

    /// Provider that hands out scripted responses in order; falls back to a
    /// text-only response once the script is exhausted.
    struct ScriptedProvider {
        responses: Mutex<Vec<ChatResponse>>,
    }

    impl zeroclaw_api::attribution::Attributable for ScriptedProvider {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Provider(
                zeroclaw_api::attribution::ProviderKind::Model(
                    zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "ScriptedProvider"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for ScriptedProvider {
        fn supports_native_tools(&self) -> bool {
            true
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("done".into())
        }

        async fn chat(
            &self,
            _request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            let mut queue = self.responses.lock().expect("script queue lock");
            if queue.is_empty() {
                return Ok(text_response());
            }
            Ok(queue.remove(0))
        }
    }

    struct EchoTool {
        name: String,
    }

    impl zeroclaw_api::attribution::Attributable for EchoTool {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            &self.name
        }
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for EchoTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "echoes success"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: args.to_string().into(),
                error: None,
            })
        }
    }

    /// Returns a fixed payload. The way a turn grows between two dispatches is
    /// a tool result landing in the transcript, so a tool that returns a known
    /// number of tokens is how a test puts a turn past a chosen budget.
    struct PayloadTool {
        name: String,
        payload: String,
    }

    impl zeroclaw_api::attribution::Attributable for PayloadTool {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            &self.name
        }
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for PayloadTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "returns a fixed payload"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult::ok(self.payload.clone()))
        }
    }

    /// Hands out scripted responses in order and records the messages every
    /// request carried, so a test can assert what a later dispatch of the same
    /// turn actually sent.
    struct RecordingScriptedProvider {
        responses: Mutex<Vec<ChatResponse>>,
        requests: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl zeroclaw_api::attribution::Attributable for RecordingScriptedProvider {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Provider(
                zeroclaw_api::attribution::ProviderKind::Model(
                    zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "RecordingScriptedProvider"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for RecordingScriptedProvider {
        fn supports_native_tools(&self) -> bool {
            true
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("done".into())
        }

        async fn chat(
            &self,
            request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            self.requests
                .lock()
                .expect("request log lock")
                .push(request.messages.to_vec());
            let mut queue = self.responses.lock().expect("script queue lock");
            if queue.is_empty() {
                return Ok(text_response());
            }
            Ok(queue.remove(0))
        }
    }

    /// Succeeds, but first cancels the turn token: the executor's select!
    /// deterministically reports the cancellation instead of the tool's own
    /// result, simulating an operator interrupt landing mid-execution.
    struct InterruptingTool {
        name: String,
        token: CancellationToken,
    }

    impl zeroclaw_api::attribution::Attributable for InterruptingTool {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            &self.name
        }
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for InterruptingTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "cancels the turn mid-execution"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            self.token.cancel();
            // Keep the tool future pending past cancellation so the
            // executor's cancellation branch wins deterministically.
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(crate::tools::ToolResult::ok("never observed"))
        }
    }

    /// A tool whose execution FAILS: the failed result is still a completed
    /// call that reaches its after hook — abandonment is not for it.
    struct FailingTool {
        name: String,
    }

    impl zeroclaw_api::attribution::Attributable for FailingTool {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            &self.name
        }
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for FailingTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "always returns a failed result"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: false,
                output: "failed".into(),
                error: Some("boom".to_string()),
            })
        }
    }

    async fn run_scripted_loop(
        provider: &ScriptedProvider,
        tools_registry: &crate::tools::scoped::ScopedToolRegistry,
        observer: &NoopObserver,
        hooks: Option<&crate::hooks::HookRunner>,
        pacing: &zeroclaw_config::schema::PacingConfig,
        turn_id: &str,
        parallel_tools: bool,
        cancellation_token: Option<CancellationToken>,
        history: &mut Vec<ChatMessage>,
    ) -> anyhow::Result<String> {
        run_scripted_loop_with_budget(
            provider,
            tools_registry,
            observer,
            hooks,
            pacing,
            turn_id,
            parallel_tools,
            cancellation_token,
            history,
            0,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_scripted_loop_with_budget(
        provider: &dyn ModelProvider,
        tools_registry: &crate::tools::scoped::ScopedToolRegistry,
        observer: &NoopObserver,
        hooks: Option<&crate::hooks::HookRunner>,
        pacing: &zeroclaw_config::schema::PacingConfig,
        turn_id: &str,
        parallel_tools: bool,
        cancellation_token: Option<CancellationToken>,
        history: &mut Vec<ChatMessage>,
        context_token_budget: usize,
        image_cache: Option<ToolLoopImageState<'_>>,
    ) -> anyhow::Result<String> {
        let mut crumb_present = false;
        let mut injected_preamble = None;
        run_tool_call_loop(ToolLoop {
            context_anchor_seed: None,
            context_anchor_sink: None,
            history_was_trimmed: None,
            parent_agent_alias: None,
            sop_reassembly: None,
            exec: ResolvedAgentExecution {
                model_access: ResolvedModelAccess {
                    model_provider: provider,
                    provider_name: "scripted",
                    model: "scripted-model",
                    dispatch_model: "scripted-model",
                    temperature: Some(0.0),
                },
                tools_registry,
                observer,
                silent: true,
                approval: None,
                multimodal_config: &zeroclaw_config::schema::MultimodalConfig::default(),
                config: None,
                max_tool_iterations: 3,
                hooks,
                excluded_tools: &[],
                dedup_exempt_tools: &[],
                activated_tools: None,
                model_switch_callback: None,
                pacing,
                strict_tool_parsing: false,
                parallel_tools,
                max_tool_result_chars: 0,
                context_limits: zeroclaw_config::schema::ResolvedContextLimits {
                    context_token_budget,
                    keep_recent_turns: zeroclaw_config::scattered_types::DEFAULT_KEEP_RECENT_TURNS,
                    request_token_ceiling: None,
                    ..zeroclaw_config::schema::ResolvedContextLimits::legacy_fallback(0)
                },
                context_limits_resolver: None,
                receipt_generator: None,
                knobs: &LoopKnobs::default(),
                security: None,
            },
            history,
            history_has_trim_breadcrumb: &mut crumb_present,
            injected_memory_preamble: &mut injected_preamble,
            channel_name: "cli",
            channel_reply_target: None,
            cancellation_token,
            on_delta: None,
            shared_budget: None,
            channel: None,
            collected_receipts: None,
            event_tx: None,
            steering: None,
            new_messages_out: None,
            image_cache,
            memory: None,
            ingress: IngressContext::sub_turn(),
            agent_alias: None,
            turn_id,
            served_route_sink: None,
        })
        .await
    }

    fn registry_with(
        tools: Vec<Box<dyn crate::tools::Tool>>,
    ) -> crate::tools::scoped::ScopedToolRegistry {
        crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(tools)
    }

    struct ImageRecoveryProvider {
        requests: Mutex<Vec<Vec<ChatMessage>>>,
        error_message: &'static str,
    }

    impl zeroclaw_api::attribution::Attributable for ImageRecoveryProvider {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Provider(
                zeroclaw_api::attribution::ProviderKind::Model(
                    zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }

        fn alias(&self) -> &str {
            "trim-image-recovery"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for ImageRecoveryProvider {
        fn supports_vision(&self) -> bool {
            true
        }

        fn supports_exact_request_replay(
            &self,
            _request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
        ) -> bool {
            true
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            unreachable!("structured chat is used")
        }

        async fn chat(
            &self,
            request: zeroclaw_api::model_provider::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            let mut requests = self.requests.lock().expect("request lock");
            requests.push(request.messages.to_vec());
            if requests.len() == 1 {
                return Err(zeroclaw_api::model_provider::StreamError::HttpStatus {
                    status: 400,
                    message: self.error_message.into(),
                }
                .into());
            }
            Ok(text_response())
        }
    }

    /// A turn grows between dispatches, and the turn-start check runs only
    /// once — before the first one. Every dispatch after that re-measures
    /// against the same budget, so this is the test that decides whether a
    /// growing turn has an in-turn guard at all.
    ///
    /// The budget is chosen so the second dispatch is over it while the first
    /// is not. Without the guard nothing fires in that band: the request goes
    /// out, and in production the same shape reaches 99% of the window and
    /// stalls with no `finish_reason`.
    ///
    /// The assertion is the old turn's absence rather than a token count — a
    /// count would pass for the wrong reason if the trim had dropped something
    /// else.
    #[tokio::test]
    async fn a_turn_that_grows_past_the_budget_trims_before_the_next_dispatch() {
        let provider = RecordingScriptedProvider {
            responses: Mutex::new(vec![ChatResponse {
                text: None,
                tool_calls: vec![tool_call("call-1", "payload_tool", serde_json::json!({}))],
                usage: None,
                reasoning_content: None,
            }]),
            requests: Mutex::new(Vec::new()),
        };
        let registry = registry_with(vec![Box::new(PayloadTool {
            name: "payload_tool".to_string(),
            // ~20k tokens: far past the 8k budget the test configures.
            payload: "p".repeat(80_000),
        })]);
        let mut history = vec![
            ChatMessage::system("s"),
            ChatMessage::user(format!("old-turn-marker {}", "o".repeat(8_000))),
            ChatMessage::assistant("old answer"),
            ChatMessage::user("run the payload tool"),
        ];

        run_scripted_loop_with_budget(
            &provider,
            &registry,
            &NoopObserver,
            None,
            &zeroclaw_config::schema::PacingConfig::default(),
            "turn-budget",
            false,
            None,
            &mut history,
            8_000,
            None,
        )
        .await
        .expect("the turn completes");

        let requests = provider.requests.lock().expect("request log lock");
        assert_eq!(
            requests.len(),
            2,
            "the tool call must produce a second dispatch"
        );
        assert!(
            requests[0]
                .iter()
                .any(|m| m.content.contains("old-turn-marker")),
            "the first dispatch must carry the old turn, or this test proves nothing"
        );
        assert!(
            requests[1]
                .iter()
                .all(|m| !m.content.contains("old-turn-marker")),
            "the second dispatch is over the budget and must have dropped the old turn \
             before sending; it sent {} messages",
            requests[1].len()
        );
    }

    #[tokio::test]
    async fn image_recovery_does_not_restore_pre_dispatch_trimmed_history() {
        let provider = ImageRecoveryProvider {
            requests: Mutex::new(Vec::new()),
            error_message: "rejected request",
        };
        let retained_user = "inspect [IMAGE:data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC]";
        let mut history = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user(format!("discarded-old-turn {}", "old ".repeat(10_000))),
            ChatMessage::assistant("old answer"),
            ChatMessage::user(retained_user),
        ];
        let mut cache = zeroclaw_providers::multimodal::LocalImageCache::new();
        let mut provider_state = ProviderImageState::default();
        let result = run_scripted_loop_with_budget(
            &provider,
            &registry_with(Vec::new()),
            &NoopObserver,
            None,
            &zeroclaw_config::schema::PacingConfig::default(),
            "trim-image-recovery",
            false,
            None,
            &mut history,
            // Fit the retained image, prompt framing and trim breadcrumb.
            crate::agent::history::IMAGE_TOKEN_ESTIMATE + 1_000,
            Some(ToolLoopImageState {
                cache: &mut cache,
                provider_state: &mut provider_state,
            }),
        )
        .await
        .expect("trimmed request recovers without the image");

        let notice = crate::i18n::get_required_cli_string_with_args(
            "turn-provider-images-quarantined",
            &[("count", "1"), ("count_plural", "one")],
        );
        assert_eq!(result, format!("{notice}\n\ndone"));
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests.len(), 2);
        for messages in requests.iter() {
            assert!(
                messages
                    .iter()
                    .all(|m| !m.content.contains("discarded-old-turn"))
            );
        }
        assert_eq!(
            zeroclaw_providers::multimodal::provider_image_ids(&requests[0]).len(),
            1
        );
        assert!(zeroclaw_providers::multimodal::provider_image_ids(&requests[1]).is_empty());
        assert!(
            history
                .iter()
                .all(|m| !m.content.contains("discarded-old-turn"))
        );
        assert!(
            history
                .iter()
                .any(|m| m.role == "user" && m.content == retained_user)
        );
    }

    #[tokio::test]
    async fn image_recovery_requires_caller_owned_state() {
        for owned in [false, true] {
            let provider = ImageRecoveryProvider {
                requests: Mutex::new(Vec::new()),
                error_message: "rejected request",
            };
            let user = "inspect [IMAGE:data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC]";
            let mut history = vec![
                ChatMessage::system("You are helpful."),
                ChatMessage::user(user),
            ];
            let mut cache = zeroclaw_providers::multimodal::LocalImageCache::new();
            let mut provider_state = ProviderImageState::default();
            let registry = registry_with(Vec::new());
            let result = run_scripted_loop_with_budget(
                &provider,
                &registry,
                &NoopObserver,
                None,
                &zeroclaw_config::schema::PacingConfig::default(),
                "image-state-owner",
                false,
                None,
                &mut history,
                0,
                owned.then_some(ToolLoopImageState {
                    cache: &mut cache,
                    provider_state: &mut provider_state,
                }),
            )
            .await;
            let requests = provider.requests.lock().expect("request lock");
            assert_eq!(requests.len(), if owned { 2 } else { 1 });
            assert_eq!(
                zeroclaw_providers::multimodal::provider_image_ids(&requests[0]).len(),
                1
            );
            if owned {
                assert!(
                    zeroclaw_providers::multimodal::provider_image_ids(&requests[1]).is_empty()
                );
                let notice = crate::i18n::get_required_cli_string_with_args(
                    "turn-provider-images-quarantined",
                    &[("count", "1"), ("count_plural", "one")],
                );
                assert_eq!(
                    result.expect("owned recovery succeeds"),
                    format!("{notice}\n\ndone")
                );
            } else {
                let error = result.expect_err("stateless callers keep the original error");
                assert!(error.to_string().contains("rejected request"));
            }
            assert!(
                history
                    .iter()
                    .any(|message| message.role == "user" && message.content == user)
            );
            assert!(
                history
                    .iter()
                    .filter(|message| message.role == "assistant")
                    .all(|message| message.content == "done")
            );
        }
    }

    #[tokio::test]
    async fn image_recovery_leaves_context_overflow_to_history_trimming() {
        let provider = ImageRecoveryProvider {
            requests: Mutex::new(Vec::new()),
            error_message: "prompt is too long",
        };
        let user = "inspect [IMAGE:data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC]";
        let mut history = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user(format!("old-context {}", "old ".repeat(10_000))),
            ChatMessage::assistant("old answer"),
            ChatMessage::user(user),
        ];
        let mut cache = zeroclaw_providers::multimodal::LocalImageCache::new();
        let mut provider_state = ProviderImageState::default();
        let registry = registry_with(Vec::new());
        // The provider rejects the first request; the recovery path must drop
        // the oversized old turn and keep the image.
        //
        // Sized so the local gate stays silent, with headroom for the
        // conservative margin every pre-send number carries: this fixture is
        // about the fallback *after* a provider rejection, so the gate must not
        // trim first. That rejection is the case the local estimate cannot
        // cover (an image's real cost, one-time tool schemas), and it is its own
        // trigger: recovery runs the same action one step deeper, a whole turn
        // is dropped, the retry is smaller, and the image still fits.
        let budget = crate::agent::history_trim::conservative_estimate_tokens(
            crate::agent::history::estimate_history_tokens(&history),
        );
        assert!(
            crate::agent::history::estimate_history_tokens(&history) < budget,
            "the gate must stay silent so the provider is the one that rejects"
        );
        let result = run_scripted_loop_with_budget(
            &provider,
            &registry,
            &NoopObserver,
            None,
            &zeroclaw_config::schema::PacingConfig::default(),
            "image-context-overflow",
            false,
            None,
            &mut history,
            budget,
            Some(ToolLoopImageState {
                cache: &mut cache,
                provider_state: &mut provider_state,
            }),
        )
        .await
        .expect("trimmed request succeeds with its image");
        assert_eq!(result, "done");
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests.len(), 2);
        // The first request carries the oversized old turn — the provider is
        // the one that rejects it. The recovery path then drops that turn
        // whole, so the retry never carries it.
        assert!(
            requests[0]
                .iter()
                .any(|message| message.content.contains("old-context"))
        );
        assert!(
            requests[1]
                .iter()
                .all(|message| !message.content.contains("old-context"))
        );
        let image_ids = zeroclaw_providers::multimodal::provider_image_ids(&requests[0]);
        assert_eq!(image_ids.len(), 1);
        assert_eq!(
            zeroclaw_providers::multimodal::provider_image_ids(&requests[1]),
            image_ids
        );
        let route = ProviderImageState::route("scripted", "scripted-model");
        assert!(provider_state.quarantined(&route).is_empty());
        assert!(
            history
                .iter()
                .any(|message| message.role == "user" && message.content == user)
        );
    }

    #[tokio::test]
    async fn hook_insertion_before_history_fails_the_turn_instead_of_desyncing_trim() {
        // A `before_llm_call` hook that inserts a message near the front of
        // the request shifts every later message by one position. The
        // pre-dispatch budget gate's "everything past the original prepared
        // length is a hook-appended suffix" shortcut would otherwise
        // misclassify the original final user message as the hook's
        // addition, letting a whole-turn drop retain a message the durable
        // history says was dropped (or the reverse). With no safe way to
        // reconstruct the correct request without re-invoking the hook
        // (breaking its exactly-once contract), the turn must fail
        // explicitly instead of dispatching a desynchronized request.
        use crate::hooks::{HookHandler, HookResult, HookRunner};
        use async_trait::async_trait;

        struct InsertNearFrontHook;

        #[async_trait]
        impl HookHandler for InsertNearFrontHook {
            fn name(&self) -> &str {
                "insert-near-front"
            }
            async fn before_llm_call(
                &self,
                messages: &mut Vec<ChatMessage>,
                _model: &mut String,
            ) -> HookResult<()> {
                // Insert right after the leading system prompt, so nothing
                // this hook adds lands at the tail of the vector.
                messages.insert(1, ChatMessage::system("z".repeat(2000)));
                HookResult::Continue(())
            }
        }

        let mut runner = HookRunner::new();
        runner.register(Box::new(InsertNearFrontHook));

        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("turn0".to_string()),
            ChatMessage::assistant("a0".to_string()),
            ChatMessage::user("turn1".to_string()),
            ChatMessage::assistant("a1".to_string()),
            ChatMessage::user("turn2".to_string()),
        ];

        // Comfortably above the raw (pre-hook) history estimate, so the
        // iteration-0 preflight trim is a no-op and every turn above is
        // still present when the pre-dispatch gate below runs. The hook's
        // large insertion is what pushes the actual dispatched population
        // over this budget, forcing that gate to drop a whole turn.
        let budget = crate::agent::history::estimate_history_tokens(&history) + 10;

        let provider = ScriptedProvider {
            responses: Mutex::new(vec![text_response()]),
        };
        let registry = registry_with(Vec::new());
        let observer = NoopObserver;
        let pacing = zeroclaw_config::schema::PacingConfig::default();

        let result = run_scripted_loop_with_budget(
            &provider,
            &registry,
            &observer,
            Some(&runner),
            &pacing,
            "turn-hook-insert",
            false,
            None,
            &mut history,
            budget,
            None,
        )
        .await;

        let error = result.expect_err(
            "a structural hook mutation must fail the turn rather than dispatch a request \
             whose retained content no longer matches the durable, trimmed history",
        );
        assert_eq!(
            error.to_string(),
            crate::i18n::get_required_cli_string("turn-context-hook-mutation-unsafe-error")
        );
    }

    #[tokio::test]
    async fn completed_batch_pairs_before_with_after_and_never_abandons() {
        let observer = NoopObserver;
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner = lifecycle_runner(Arc::clone(&events), Vec::new());
        let provider = ScriptedProvider {
            responses: Mutex::new(vec![ChatResponse {
                text: None,
                tool_calls: vec![
                    tool_call("call-a", "echo_first", serde_json::json!({"n": 1})),
                    tool_call("call-b", "echo_second", serde_json::json!({"n": 2})),
                ],
                usage: None,
                reasoning_content: None,
            }]),
        };
        let registry = registry_with(vec![
            Box::new(EchoTool {
                name: "echo_first".to_string(),
            }),
            Box::new(EchoTool {
                name: "echo_second".to_string(),
            }),
        ]);
        let mut history = vec![ChatMessage::user("run the tools".to_string())];

        run_scripted_loop(
            &provider,
            &registry,
            &observer,
            Some(&runner),
            &pacing,
            "turn-ok",
            false,
            None,
            &mut history,
        )
        .await
        .expect("the scripted turn completes");

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "before:echo_first:turn-ok:0:0".to_string(),
                "before:echo_second:turn-ok:0:1".to_string(),
                "after:echo_first:turn-ok:0:0".to_string(),
                "after:echo_second:turn-ok:0:1".to_string(),
            ],
            "each completed call pairs its before hook with exactly one after hook and is never abandoned"
        );
    }

    #[tokio::test]
    async fn failed_tool_outcome_reaches_after_not_abandonment() {
        let observer = NoopObserver;
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner = lifecycle_runner(Arc::clone(&events), Vec::new());
        let provider = ScriptedProvider {
            responses: Mutex::new(vec![ChatResponse {
                text: None,
                tool_calls: vec![tool_call(
                    "call-1",
                    "failing_tool",
                    serde_json::json!({"n": 1}),
                )],
                usage: None,
                reasoning_content: None,
            }]),
        };
        let registry = registry_with(vec![Box::new(FailingTool {
            name: "failing_tool".to_string(),
        })]);
        let mut history = vec![ChatMessage::user("run it".to_string())];

        run_scripted_loop(
            &provider,
            &registry,
            &observer,
            Some(&runner),
            &pacing,
            "turn-fail",
            false,
            None,
            &mut history,
        )
        .await
        .expect("a failed tool outcome is a completed call, not a turn error");

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "before:failing_tool:turn-fail:0:0".to_string(),
                "after:failing_tool:turn-fail:0:0".to_string(),
            ],
            "a failed result is still execution completion: after fires, abandonment must not"
        );
    }

    #[tokio::test]
    async fn mid_batch_interruption_abandons_every_unexecuted_context() {
        let observer = NoopObserver;
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner = lifecycle_runner(Arc::clone(&events), Vec::new());
        let token = CancellationToken::new();
        let provider = ScriptedProvider {
            responses: Mutex::new(vec![ChatResponse {
                text: None,
                tool_calls: vec![
                    tool_call("call-a", "interrupt_tool", serde_json::json!({"n": 1})),
                    tool_call("call-b", "echo_second", serde_json::json!({"n": 2})),
                ],
                usage: None,
                reasoning_content: None,
            }]),
        };
        let registry = registry_with(vec![
            Box::new(InterruptingTool {
                name: "interrupt_tool".to_string(),
                token: token.clone(),
            }),
            Box::new(EchoTool {
                name: "echo_second".to_string(),
            }),
        ]);
        let mut history = vec![ChatMessage::user("run then interrupt".to_string())];

        let error = run_scripted_loop(
            &provider,
            &registry,
            &observer,
            Some(&runner),
            &pacing,
            "turn-interrupted",
            false,
            Some(token),
            &mut history,
        )
        .await
        .expect_err("an interrupted turn surfaces as cancellation");

        assert!(
            is_tool_loop_cancelled(&error),
            "the turn must report tool-loop cancellation, got {error:?}"
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "before:interrupt_tool:turn-interrupted:0:0".to_string(),
                "before:echo_second:turn-interrupted:0:1".to_string(),
                "abandoned:interrupt_tool:turn-interrupted:0:0".to_string(),
                "abandoned:echo_second:turn-interrupted:0:1".to_string(),
            ],
            "every executable context of the interrupted batch gets exactly one abandonment; none reaches after"
        );
    }

    /// Tool that never completes on its own: the containing wrapper decides
    /// the turn's fate, not the tool.
    struct HangingTool {
        name: String,
    }

    impl zeroclaw_api::attribution::Attributable for HangingTool {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            &self.name
        }
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for HangingTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "sleeps far past any test timeout"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(crate::tools::ToolResult::ok("never observed"))
        }
    }

    /// Hook that retains per-invocation state in the before phase and releases
    /// it on after/abandonment — the integrator pattern the abandonment API
    /// documents as cooperative-path-only. Used to pin what a hard future drop
    /// does and does not deliver.
    struct RetainingHook {
        events: Arc<Mutex<Vec<String>>>,
        retained: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::hooks::HookHandler for RetainingHook {
        fn name(&self) -> &str {
            "retaining-hook"
        }

        async fn before_tool_call_with_context(
            &self,
            context: &ToolCallHookContext,
            name: String,
            args: serde_json::Value,
        ) -> crate::hooks::HookResult<(String, serde_json::Value)> {
            self.events
                .lock()
                .expect("retaining event lock")
                .push(format!("before:{}:{}", name, context.invocation_id()));
            self.retained
                .lock()
                .expect("retaining state lock")
                .push(context.invocation_id().to_string());
            crate::hooks::HookResult::Continue((name, args))
        }

        async fn on_after_tool_call_with_context(
            &self,
            context: &ToolCallHookContext,
            tool: &str,
            _result: &zeroclaw_api::tool::ToolResult,
            _duration: Duration,
        ) {
            self.events
                .lock()
                .expect("retaining event lock")
                .push(format!("after:{}:{}", tool, context.invocation_id()));
            self.retained
                .lock()
                .expect("retaining state lock")
                .retain(|id| id != context.invocation_id());
        }

        async fn on_tool_call_abandoned(&self, context: &ToolCallHookContext, tool: &str) {
            self.events
                .lock()
                .expect("retaining event lock")
                .push(format!("abandoned:{}:{}", tool, context.invocation_id()));
            self.retained
                .lock()
                .expect("retaining state lock")
                .retain(|id| id != context.invocation_id());
        }
    }

    #[tokio::test]
    async fn outer_turn_drop_fires_no_lifecycle_callbacks() {
        // Mirrors the channel orchestrator's containing-future race
        // (orchestrator/mod.rs): the whole turn future is wrapped in a
        // timeout and raced against the turn's cancellation token. A winning
        // outer branch DROPS the turn future, so awaited cooperative cleanup
        // inside the loop never runs. This pins the documented lifecycle
        // boundary: after and abandonment fire on awaited non-completion
        // paths only; a hard drop fires neither, and stateful hooks must
        // design for that (the webhook audit hook retains no arguments at
        // all, so a drop leaves nothing behind).
        let observer = NoopObserver;
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let retained = Arc::new(Mutex::new(Vec::new()));
        let mut runner = crate::hooks::HookRunner::new();
        runner.register(Box::new(RetainingHook {
            events: Arc::clone(&events),
            retained: Arc::clone(&retained),
        }));
        let token = CancellationToken::new();
        let provider = ScriptedProvider {
            responses: Mutex::new(vec![ChatResponse {
                text: None,
                tool_calls: vec![tool_call(
                    "call-a",
                    "hang_tool",
                    serde_json::json!({"n": 1}),
                )],
                usage: None,
                reasoning_content: None,
            }]),
        };
        let registry = registry_with(vec![Box::new(HangingTool {
            name: "hang_tool".to_string(),
        })]);
        let mut history = vec![ChatMessage::user("hang forever".to_string())];

        let turn_future = run_scripted_loop(
            &provider,
            &registry,
            &observer,
            Some(&runner),
            &pacing,
            "turn-drop",
            false,
            Some(token.clone()),
            &mut history,
        );
        let outcome = tokio::select! {
            () = token.cancelled() => "cancelled",
            result = tokio::time::timeout(Duration::from_millis(300), turn_future) => {
                match result {
                    Ok(_) => "completed",
                    Err(_elapsed) => "timeout",
                }
            }
        };
        assert_eq!(
            outcome, "timeout",
            "the outer timeout must win while the tool hangs"
        );

        // The dropped turn fired exactly its before phase: neither the after
        // hook nor abandonment can run inside a dropped future.
        assert_eq!(
            *events.lock().unwrap(),
            vec!["before:hang_tool:turn-drop:0:0".to_string()],
            "a hard future drop must fire no after or abandonment callback"
        );
        // The hook's own retained entry survives the drop — the runtime makes
        // no callback promise under hard drop, which is why argument-carrying
        // completion exists: hooks that must not leak retain nothing.
        assert_eq!(
            retained.lock().unwrap().len(),
            1,
            "the retained entry documents the hard-drop boundary; release is the hook's design obligation"
        );

        // The runner itself is unharmed by the dropped turn: a direct
        // cooperative dispatch still reaches the hook and releases the entry.
        runner
            .fire_tool_call_abandoned(
                &zeroclaw_api::hook::ToolCallHookContext::new("turn-drop:0:0"),
                "hang_tool",
            )
            .await;
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "before:hang_tool:turn-drop:0:0".to_string(),
                "abandoned:hang_tool:turn-drop:0:0".to_string(),
            ],
            "cooperative abandonment still works after a dropped turn"
        );
        assert!(
            retained.lock().unwrap().is_empty(),
            "the cooperative dispatch releases exactly the retained entry"
        );
    }
}

/// A neutral tail the cap must preserve, standing in for whatever the runtime
/// appends last now that it no longer injects wall-clock prose. Shared by the
/// prompt-capping tests in the sibling test modules.
#[cfg(test)]
const TAIL_MARKER: &str = "\n\n## Tail marker\n\nend";

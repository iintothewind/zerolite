use crate::model_provider::ConversationMessage;
use crate::plan::PlanEntry;
use std::fmt;

/// Structured metadata for a tool that produced a file artifact (e.g.
/// `deliver_file`). Carried on [`TurnEvent::ToolResult`] so a channel attaches
/// the file from typed fields instead of parsing a text trailer out of the
/// free-form `output` string. Trailer parsing let a crafted filename forge the
/// delivered path (arbitrary-file-read / confused-deputy class).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolArtifact {
    /// Absolute path of the delivered file on the agent host.
    pub path: String,
    /// Stable citation URI the client can reference (e.g. `attachment://…`).
    pub uri: String,
    /// Original filename.
    pub filename: String,
    /// Human-readable chat label; defaults to the filename.
    pub title: String,
    /// MIME type.
    pub mime: String,
    /// Size in bytes.
    pub size: u64,
}

impl ToolArtifact {
    /// Build from a tool's structured `output_data` when it declares a delivered
    /// file (`delivered: true` with a non-empty `path`). Returns `None` for any
    /// other structured output, keeping this a channel-neutral convention rather
    /// than a hook tied to one tool name.
    pub fn from_delivered_data(data: &serde_json::Value) -> Option<Self> {
        if data.get("delivered").and_then(serde_json::Value::as_bool) != Some(true) {
            return None;
        }
        let field = |key: &str| {
            data.get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let path = field("path");
        if path.is_empty() {
            return None;
        }
        Some(Self {
            uri: field("uri"),
            filename: field("filename"),
            title: field("title"),
            mime: field("mimeType"),
            size: data
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            path,
        })
    }
}
/// Provenance of a token count: whether it was measured, estimated, or
/// calibrated. The context meter uses it to label an estimated numerator as an
/// estimate rather than a measurement. Shared by the WS / SSE / RPC / ACP
/// surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TokenCountSource {
    /// Count comes from provider-reported usage.
    Provider,
    /// Count is locally estimated.
    #[serde(rename = "estimate")]
    Estimated,
    /// Count is calibrated from provider-reported usage and a local estimate.
    Calibrated,
}

impl TokenCountSource {
    /// Wire value used by the WS, SSE, RPC, and ACP surfaces.
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenCountSource::Provider => "provider",
            TokenCountSource::Estimated => "estimate",
            TokenCountSource::Calibrated => "calibrated",
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// A text chunk from the LLM response (may arrive many times).
    Chunk {
        delta: String,
    },
    /// A reasoning/thinking chunk from a thinking model (may arrive many times).
    Thinking {
        delta: String,
    },
    /// The agent is invoking a tool.
    ToolCall {
        /// Stable correlation ID shared with the matching [`TurnEvent::ToolResult`].
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// A tool has returned a result.
    ToolResult {
        /// Stable correlation ID shared with the originating [`TurnEvent::ToolCall`].
        id: String,
        name: String,
        output: String,
        /// Typed metadata for a file-producing tool (e.g. `deliver_file`), so
        /// channels attach the file structurally instead of parsing `output`.
        /// `None` for ordinary tools.
        artifact: Option<ToolArtifact>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    ApprovalRequest {
        /// Correlation ID. The matching response frame must echo it.
        request_id: String,
        tool_name: String,
        /// Human-readable, secret-redacted summary of the tool arguments.
        /// Synthesised by `crate::approval::summarize_args`; never the raw
        /// `args` value.
        arguments_summary: String,
        /// How long the channel will wait before auto-denying.
        timeout_secs: u64,
    },
    /// Older whole turns were dropped to fit either the context token budget or
    /// the configured turn limit. Surfaces a user-visible "context was cut
    /// here" marker so trimming is never silent. Emitted whenever a trim occurs.
    HistoryTrimmed {
        dropped_messages: usize,
        dropped_turns: usize,
        kept_turns: usize,
        reason: String,
        /// The budget this trim was measured against — the number the notice
        /// quotes as the "configured token budget". It is **not** one
        /// well-known quantity, so a client must not treat it as a fit target
        /// it can reason about on its own; which budget it is depends on the
        /// guard that fired:
        ///
        /// - the post-response guard quotes the **water-line**
        ///   (`context_token_budget`): that is what the operator configured and
        ///   therefore what the notice should name;
        /// - overflow recovery and the durable water-line trim quote the
        ///   **send budget** (`effective_context_window − reserve_tokens`), the
        ///   size the trim was asked to reach;
        /// - the hard-limit and max-iteration paths quote the model window or
        ///   their own event budget.
        ///
        /// `None` for message-limit trims (no token accounting) and when no
        /// budget applied.
        token_budget: Option<u64>,
        /// Token count before trimming.
        tokens_before: Option<u64>,
        /// Token count after trimming.
        tokens_after: Option<u64>,
        /// Provenance of `tokens_before`.
        tokens_before_source: Option<TokenCountSource>,
        /// Provenance of `tokens_after`.
        tokens_after_source: Option<TokenCountSource>,
        /// The retained provider-facing request cannot be brought under the
        /// configured budget because only the protected newest turn (plus
        /// tool schemas) remains. History MAY have been trimmed on the way to
        /// that floor, so this flag — not `dropped_messages == 0` — is the
        /// authoritative "unsatisfiable" signal. `None`/absent for ordinary
        /// trims.
        unsatisfiable_floor: Option<bool>,
        /// Native RPC-only provider context captured by the owning turn before
        /// the trim is announced. Wire and observer adapters deliberately
        /// ignore this field; it is never serialized as a turn event.
        retained_context: Option<RetainedContextSnapshot>,
    },
    /// The assembled request size, published once per turn after the
    /// water-line decision and before dispatch.
    ///
    /// Deliberately **not** [`TurnEvent::Usage`]: `Usage` means the provider
    /// answered and billed the call, and clients fold it into their step
    /// boundary. This is a pre-send local estimate — no provider has seen the
    /// request yet — so folding it as usage would invent a step that does not
    /// exist. Its only consumer is the context meter, which would otherwise
    /// keep showing the previous turn's number until the response arrives.
    ContextEstimate {
        /// Size of the assembled provider request: the local estimate when
        /// nothing measured it, the provider-measured anchor plus a local
        /// estimate for the rows the provider has not billed yet when a
        /// previous turn's report applies to this population.
        input_tokens: u64,
        /// Provenance of `input_tokens`. `estimate` when the local estimator
        /// priced the whole request; `calibrated` when a provider-reported
        /// count from an earlier turn anchors the unchanged prefix and the
        /// estimator prices only the unbilled tail. NEVER `provider`: this
        /// frame is published before dispatch, so no provider measurement of
        /// the request being announced can exist yet.
        input_tokens_source: Option<TokenCountSource>,
        /// The **water-line** in effect at assembly time — the proactive-trim
        /// threshold, not the send budget. Always `Some` in practice:
        /// `context_trim_budget()` is `trim_threshold_tokens().max(1)` over a
        /// `1..=100` percent clamp, so there is no "trimming disabled" state to
        /// encode. It stays `Option` only because it shares the
        /// `ContextUsage` wire shape with older daemons.
        ///
        /// A client must not read the value as "how much may be sent" — that is
        /// the send budget, `effective_context_window − reserve_tokens`. It
        /// reaches the wire as [`Self::HistoryTrimmed::token_budget`] only on
        /// the guards that trim against it; the post-response guard quotes the
        /// water-line in that same field, so the two are not interchangeable.
        context_token_budget: Option<u64>,
        /// Configured model capacity for the route, when authoritative.
        model_context_window: Option<u64>,
        /// The meter denominator: capacity clamped by the input ceiling.
        effective_context_window: Option<u64>,
    },
    /// Per-LLM-call token usage and cost; a turn may emit several, one per
    /// model call. `None` means "unavailable for this call", not zero.
    /// The `provider_ref` and `model` identify the provider and model used
    /// for cost attribution and context window resolution.
    ///
    /// For vision routing and reliable fallback, these carry the actual served
    /// provider and model. For dynamic routing models (e.g. `openrouter/auto`)
    /// where the provider selects the concrete model and the `ModelProvider`
    /// trait cannot expose the response-reported model, `model` carries the
    /// requested routing model (e.g. `openrouter/auto`) rather than the
    /// upstream-selected concrete model. This is a known limitation of the
    /// current provider trait.
    Usage {
        input_tokens: Option<u64>,
        /// Tokens served from the provider's prompt cache (e.g. Anthropic
        /// `cache_read_input_tokens`, OpenAI `cached_tokens`). Subset of
        /// `input_tokens` — adding would double-count.
        cached_input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
        /// Proactive trim threshold resolved for the provider/model route that
        /// produced this usage sample. Zero means proactive trimming is disabled.
        context_token_budget: Option<u64>,
        /// Configured full context capacity for that same provider/model route.
        /// `None` means the runtime used the compatibility fallback because no
        /// authoritative capacity was configured.
        model_context_window: Option<u64>,
        /// The effective context window for that route: the model's capacity
        /// clamped by the configured input ceiling. This is the meter
        /// denominator — the population the request is actually measured
        /// against, and the ceiling the model side enforces.
        effective_context_window: Option<u64>,
        /// The `<type>.<alias>` config reference of the provider that served
        /// this call. Used for accurate context window resolution and cost
        /// attribution when vision routing or provider switches are active.
        provider_ref: String,
        /// The model that actually served this call. When vision routing or
        /// a reliable fallback selects a different model, this carries the
        /// served model — not the turn-start model.
        model: String,
        /// Whether this usage event corresponds to the semantically accepted
        /// response (`true`) or a rejected physical attempt (`false`). Rejected
        /// attempts are billing-only telemetry; they do not update the accepted
        /// context snapshot (ACP session token count, context-meter ceiling).
        accepted: bool,
        /// Locally estimated prompt size, published **only** when the provider
        /// omitted `input_tokens`. Local OpenAI-compatible servers (llama.cpp
        /// and similar) routinely omit `usage`, which leaves a context meter
        /// with a budget but no numerator.
        ///
        /// This is **not** measured usage and carries a different contract
        /// from every other field on this event:
        ///
        /// - It must never be billed, summed into provider usage totals, or
        ///   persisted as a provider token count.
        ///   Accounting and persistence consumers read `input_tokens`, which
        ///   stays `None` exactly when the provider reported nothing.
        /// - A consumer rendering a context meter may use it as a display
        ///   numerator, and must label it as an estimate
        ///   (`TokenCountSource::Estimated`) rather than provider-reported.
        /// - It is an approximation in both directions, not a bound. The
        ///   history heuristic does not price separately supplied native tool
        ///   schemas, so a tool-heavy request reads low; its byte-count and
        ///   fixed-per-image costs can also read high against a provider's
        ///   real tokenizer. That is acceptable for a labelled display value
        ///   and is not acceptable for accounting — another reason it is kept
        ///   off `input_tokens`.
        /// - When the provider reported `cached_input_tokens` without a total,
        ///   that subset is a known minimum for the prompt and the estimate is
        ///   floored at it, so the meter cannot read near-zero for a prompt
        ///   already known to be large.
        ///
        /// `None` means either the provider reported a real count (use
        /// `input_tokens`) or no estimate could be formed.
        estimated_input_tokens: Option<u64>,
    },
}

#[cfg(test)]
mod plan_event_tests {
    use super::*;
    use crate::plan::{PlanEntry, PlanPriority, PlanStatus};

    #[test]
    fn plan_turn_event_carries_entries() {
        let ev = TurnEvent::Plan {
            entries: vec![PlanEntry {
                content: "Step one".to_string(),
                status: PlanStatus::Pending,
                priority: PlanPriority::Medium,
                active_form: None,
            }],
        };
        match ev {
            TurnEvent::Plan { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].content, "Step one");
            }
            _ => panic!("expected TurnEvent::Plan"),
        }
    }
}

#[cfg(test)]
mod tool_artifact_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_delivered_data_into_typed_fields() {
        let data = json!({
            "delivered": true,
            "uri": "attachment://deliver/report.pdf",
            "path": "/ws/uploads/ab.pdf",
            "filename": "report.pdf",
            "title": "Quarterly report",
            "mimeType": "application/pdf",
            "bytes": 1234,
        });
        let a = ToolArtifact::from_delivered_data(&data).expect("delivered data yields artifact");
        assert_eq!(a.path, "/ws/uploads/ab.pdf");
        assert_eq!(a.uri, "attachment://deliver/report.pdf");
        assert_eq!(a.filename, "report.pdf");
        assert_eq!(a.title, "Quarterly report");
        assert_eq!(a.mime, "application/pdf");
        assert_eq!(a.size, 1234);
    }

    #[test]
    fn non_delivered_data_is_ignored() {
        // Ordinary structured tool output must not be mistaken for a file artifact.
        assert!(ToolArtifact::from_delivered_data(&json!({"result": 42})).is_none());
        assert!(
            ToolArtifact::from_delivered_data(&json!({"delivered": false, "path": "/x"})).is_none()
        );
    }

    #[test]
    fn delivered_without_path_is_ignored() {
        assert!(ToolArtifact::from_delivered_data(&json!({"delivered": true})).is_none());
        assert!(
            ToolArtifact::from_delivered_data(&json!({"delivered": true, "path": ""})).is_none()
        );
    }
}

/// Provider-facing context captured at a trim boundary. This remains attached
/// to the internal turn event so the serial RPC consumer can persist it before
/// forwarding the public trim notice without borrowing the live agent again.
#[derive(Clone)]
pub struct RetainedContextSnapshot {
    pub retained_messages: Vec<ConversationMessage>,
    pub breadcrumb: bool,
}

impl fmt::Debug for RetainedContextSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedContextSnapshot")
            .field("retained_messages", &self.retained_messages.len())
            .field("breadcrumb", &self.breadcrumb)
            .finish()
    }
}

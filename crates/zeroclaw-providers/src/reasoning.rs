//! Shared reasoning-effort helpers for provider implementations.

use zeroclaw_api::model_provider::{ReasoningEffort, ReasoningSupport};

/// Whether `model` belongs to an OpenAI family that accepts `reasoning_effort`
/// (o1*/o3*/o4*/gpt-5*/gpt-*codex*). Routing prefixes (`vendor/model`) are
/// ignored.
pub(crate) fn is_openai_reasoning_model(model: &str) -> bool {
    let id = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    // gpt-5*-chat-latest (gpt-5-chat-latest, gpt-5.1-chat-latest, ...) are
    // OpenAI's non-reasoning chat-router models; the Chat Completions API
    // rejects reasoning_effort for them. Treat them as a distinct family, the
    // same way the native openai.rs provider already special-cases them.
    let is_gpt5_chat_latest = id.starts_with("gpt-5") && id.ends_with("-chat-latest");
    let is_openai_reasoning_model = id == "o1"
        || id.starts_with("o1-")
        || id == "o3"
        || id.starts_with("o3-")
        || id == "o4"
        || id.starts_with("o4-")
        || (id.starts_with("gpt-5") && !is_gpt5_chat_latest);
    let is_likely_codex_supported = id.contains("codex") && id.starts_with("gpt-");

    is_openai_reasoning_model || is_likely_codex_supported
}

/// Graded levels of an endpoint that takes an OpenAI-style `reasoning_effort`
/// string; they are sent verbatim.
pub(crate) fn openai_effort_support() -> ReasoningSupport {
    ReasoningSupport::from_levels([
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
    ])
}

/// The effort the user chose for this turn, when `support` lists it. An
/// unsupported choice (a turn that reached another model) is ignored so the
/// configured behavior applies.
pub(crate) fn supported_effort_override(support: &ReasoningSupport) -> Option<ReasoningEffort> {
    zeroclaw_api::reasoning_effort_override().filter(|effort| support.supports(*effort))
}

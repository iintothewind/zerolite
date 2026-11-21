//! ZeroClaw API layer — trait definitions and shared types.

pub mod a2a_wire;
pub mod agent;
pub mod attribution;
pub mod channel;
pub mod conversation_binding;
pub mod elicitation;
pub mod grants;
pub mod hook;
pub mod ingress;
pub mod jsonrpc;
pub mod lifecycle;
pub mod media;
pub mod memory_traits;
pub mod model_provider;
pub mod observability_traits;
pub mod peripherals_traits;
pub mod plan;
pub mod platform;
pub mod plugin;
pub mod plugin_egress;
pub mod plugin_key;
pub mod principal;
pub mod runtime_status;
pub mod runtime_traits;
pub mod schema;
pub mod session_keys;
pub mod tool;
pub mod tool_carrier;
pub mod turn_stop;
pub mod vad;
pub mod webhook;

tokio::task_local! {
    /// Current thread/sender ID for per-sender rate limiting.
    /// Set by the agent loop, read by SecurityPolicy.
    pub static TOOL_LOOP_THREAD_ID: Option<String>;

    /// Override for tool choice mode, set by the agent loop.
    /// Read by model_providers that support native tool calling.
    pub static TOOL_CHOICE_OVERRIDE: Option<String>;

    /// Session key for the currently active session.
    /// Scoped by gateway and channel turns, read by SessionsCurrentTool.
    pub static TOOL_LOOP_SESSION_KEY: Option<String>;

    /// The conversation the current turn is serving, when its surface
    /// supports conversation binding. Scoped beside `TOOL_LOOP_SESSION_KEY`
    /// by that surface; read by tools that record a binding.
    pub static TOOL_LOOP_ACTIVE_CONVERSATION:
        Option<crate::conversation_binding::ActiveConversation>;

    /// Native extended thinking parameters, set by the outer orchestration
    /// functions and read by `run_tool_call_loop` when building `ChatRequest`.
    pub static NATIVE_THINKING_OVERRIDE: Option<crate::model_provider::NativeThinkingParams>;

    /// Reasoning effort the user chose for the current turn. Scoped by the
    /// surface that offers the choice; read by providers when building a
    /// request.
    pub static REASONING_EFFORT_OVERRIDE: Option<crate::model_provider::ReasoningEffort>;

    /// Set once an endpoint refused the current turn's
    /// [`REASONING_EFFORT_OVERRIDE`]. Scoped beside it.
    pub static REASONING_EFFORT_REJECTED: std::sync::Arc<std::sync::atomic::AtomicBool>;
}

/// Per-turn effort chosen by the user, if a surface scoped one and no
/// endpoint refused it earlier in the turn.
pub fn reasoning_effort_override() -> Option<model_provider::ReasoningEffort> {
    let rejected = REASONING_EFFORT_REJECTED
        .try_with(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    if rejected {
        return None;
    }
    REASONING_EFFORT_OVERRIDE.try_with(|v| *v).ok().flatten()
}

/// Records that an endpoint refused the turn's effort: the rest of the turn
/// runs without it and the surface can tell the user.
pub fn reject_reasoning_effort() {
    let _ = REASONING_EFFORT_REJECTED
        .try_with(|flag| flag.store(true, std::sync::atomic::Ordering::Relaxed));
}

/// Runs `future` with a per-turn reasoning effort. `rejected` is the flag
/// [`reject_reasoning_effort`] sets; the caller keeps a clone to read it.
pub fn scope_reasoning_effort<F: Future>(
    effort: Option<model_provider::ReasoningEffort>,
    rejected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    future: F,
) -> impl Future<Output = F::Output> {
    REASONING_EFFORT_OVERRIDE.scope(effort, REASONING_EFFORT_REJECTED.scope(rejected, future))
}

/// Carries the calling task's reasoning-effort scope into `future`, for a
/// future about to be spawned: task-locals do not cross a spawn. Call it on
/// the spawning task.
pub fn carry_reasoning_effort<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let effort = REASONING_EFFORT_OVERRIDE.try_with(|v| *v).ok().flatten();
    let rejected = REASONING_EFFORT_REJECTED
        .try_with(std::sync::Arc::clone)
        .unwrap_or_default();
    scope_reasoning_effort(effort, rejected, future)
}

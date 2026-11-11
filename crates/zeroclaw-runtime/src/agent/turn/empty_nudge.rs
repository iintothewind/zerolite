//! ZeroLite: bounded continuation nudge after a semantic-empty terminal
//! completion (`docs/design/delegation-turn.md` R5).
//!
//! A reply with no answer text and no tool call cannot finish a turn. Once
//! the provider retry policy is exhausted, the turn loop appends one
//! runtime-written message asking the model to answer or continue, instead
//! of failing the turn at once. Bounded twice so it cannot loop: at most one
//! nudge per empty streak, and a hard ceiling per turn.

use zeroclaw_api::tool_carrier::render_prompt_tool_carrier;
use zeroclaw_providers::ChatMessage;

use super::outcome::is_semantic_empty_terminal_completion;

/// Hard ceiling on nudges within one `run_tool_call_loop` invocation.
pub(crate) const MAX_EMPTY_COMPLETION_NUDGES_PER_TURN: usize = 3;

/// Model-facing nudge text. English and not localized, like the parser
/// feedback in the turn loop: it is never delivered as assistant output.
pub(crate) const EMPTY_COMPLETION_NUDGE: &str = "[Empty response]\n\
Your previous response contained no answer text and no tool call, so nothing was delivered. \
If the task is complete, reply with the final answer for the user. Otherwise continue from the \
current state with the next tool call. Do not repeat work that already succeeded.";

#[derive(Debug, Default)]
pub(crate) struct EmptyCompletionNudge {
    sent: usize,
    awaiting_progress: bool,
}

impl EmptyCompletionNudge {
    /// Admit one nudge for `error` and return the history message to append.
    /// `None` leaves the error terminal: it is not a semantic-empty
    /// completion, the previous nudge has not been followed by a tool round
    /// yet, or the per-turn ceiling is reached.
    pub(crate) fn admit(&mut self, error: &anyhow::Error) -> Option<ChatMessage> {
        if self.awaiting_progress
            || self.sent >= MAX_EMPTY_COMPLETION_NUDGES_PER_TURN
            || !is_semantic_empty_terminal_completion(error)
        {
            return None;
        }
        self.sent += 1;
        self.awaiting_progress = true;
        Some(ChatMessage::user(render_prompt_tool_carrier(
            EMPTY_COMPLETION_NUDGE,
            &[],
        )))
    }

    /// A response carrying tool calls was accepted: the model is working
    /// again, so a later empty completion may be nudged once more.
    pub(crate) fn record_tool_round(&mut self) {
        self.awaiting_progress = false;
    }

    pub(crate) fn sent(&self) -> usize {
        self.sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic_empty() -> anyhow::Error {
        anyhow::Error::new(zeroclaw_api::model_provider::SemanticEmptyTerminalCompletion)
    }

    #[test]
    fn admits_one_nudge_per_empty_streak_and_rearms_after_a_tool_round() {
        let mut nudge = EmptyCompletionNudge::default();
        assert!(nudge.admit(&semantic_empty()).is_some());
        assert!(nudge.admit(&semantic_empty()).is_none());
        nudge.record_tool_round();
        assert!(nudge.admit(&semantic_empty()).is_some());
        assert_eq!(nudge.sent(), 2);
    }

    #[test]
    fn stops_at_the_per_turn_ceiling() {
        let mut nudge = EmptyCompletionNudge::default();
        for _ in 0..MAX_EMPTY_COMPLETION_NUDGES_PER_TURN {
            assert!(nudge.admit(&semantic_empty()).is_some());
            nudge.record_tool_round();
        }
        assert!(nudge.admit(&semantic_empty()).is_none());
        assert_eq!(nudge.sent(), MAX_EMPTY_COMPLETION_NUDGES_PER_TURN);
    }

    #[test]
    fn leaves_other_errors_terminal() {
        let mut nudge = EmptyCompletionNudge::default();
        assert!(
            nudge
                .admit(&anyhow::Error::msg("connection reset"))
                .is_none()
        );
        assert_eq!(nudge.sent(), 0);
    }

    #[test]
    fn nudge_is_runtime_machinery_not_an_operator_turn() {
        let msg = EmptyCompletionNudge::default()
            .admit(&semantic_empty())
            .expect("semantic-empty completion is nudged");
        assert_eq!(msg.role, "user");
        assert!(crate::agent::history_trim::is_synthetic_user_message(&msg));
        let parts = zeroclaw_api::tool_carrier::parse_prompt_tool_carrier(&msg.content)
            .expect("nudge is a prompt tool carrier");
        assert!(parts.declared);
        assert!(parts.attachments.is_empty());
        assert_eq!(parts.text, EMPTY_COMPLETION_NUDGE);
    }
}

//! WebSocket-backed [`Channel`] implementation that surfaces tool approval
//! prompts and agent questions (`ask_user`, `poll`) to the gateway client and
//! waits for the operator's answer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zeroclaw_api::agent::TurnEvent;
use zeroclaw_api::channel::{
    ApprovalSource, AttributedApprovalResponse, Channel, ChannelApprovalRequest,
    ChannelApprovalResponse, ChannelMessage, SendMessage,
};

use crate::ws_question::{PendingQuestions, new_pending_questions};

/// Shared map keyed by `request_id`. Consumed by the receive loop to resolve
/// the oneshot when an `approval_response` frame arrives.
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ChannelApprovalResponse>>>>;

/// Construct an empty pending-approvals registry for a fresh connection.
pub fn new_pending_approvals() -> PendingApprovals {
    Arc::new(Mutex::new(HashMap::new()))
}

/// `Channel` implementation that emits approval frames over a connection's
/// existing `event_tx` and parks on a oneshot until the matching response
/// arrives or `timeout` elapses.
pub struct WsApprovalChannel {
    event_tx: mpsc::Sender<TurnEvent>,
    pending: PendingApprovals,
    timeout: Duration,
    questions: PendingQuestions,
}

impl WsApprovalChannel {
    pub fn new(
        event_tx: mpsc::Sender<TurnEvent>,
        pending: PendingApprovals,
        timeout: Duration,
    ) -> Self {
        Self {
            event_tx,
            pending,
            timeout,
            questions: new_pending_questions(),
        }
    }

    /// The registry the socket loop resolves `question_response` frames into.
    pub fn pending_questions(&self) -> PendingQuestions {
        Arc::clone(&self.questions)
    }

    /// Show a question card to the viewer and wait for the answer. Never
    /// yields "no answer" as `Ok`: every unanswered outcome is a named error.
    async fn ask(
        &self,
        question: &str,
        choices: &[String],
        multi_select: bool,
        timeout: Duration,
    ) -> anyhow::Result<Vec<String>> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.questions.lock().insert(request_id.clone(), tx);

        let event = TurnEvent::QuestionRequest {
            request_id: request_id.clone(),
            question: question.to_string(),
            choices: choices.to_vec(),
            multi_select,
            timeout_secs: timeout.as_secs(),
        };
        if self.event_tx.send(event).await.is_err() {
            self.questions.lock().remove(&request_id);
            anyhow::bail!("the web chat connection is closed");
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Some(answers))) => Ok(answers),
            Ok(Ok(None)) => anyhow::bail!("the user dismissed the question without answering"),
            Ok(Err(_)) => anyhow::bail!("no web chat viewer is attached to answer the question"),
            Err(_) => {
                self.questions.lock().remove(&request_id);
                anyhow::bail!("no answer within {} seconds", timeout.as_secs())
            }
        }
    }
}

impl ::zeroclaw_api::attribution::Attributable for WsApprovalChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(
            ::zeroclaw_api::attribution::ChannelKind::Webhook,
        )
    }
    fn alias(&self) -> &str {
        "ws_approval"
    }
}

#[async_trait]
impl Channel for WsApprovalChannel {
    fn name(&self) -> &str {
        "ws"
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        // The gateway WS path streams agent output via TurnEvent::Chunk /
        // ::Thinking / ::ToolCall / ::ToolResult; it does not deliver
        // free-form `send()` messages. Returning Ok here keeps any caller
        // that probes for a generic delivery target from erroring out.
        Ok(())
    }

    /// `send` above is a deliberate no-op (see comment). Surfaces that must
    /// genuinely deliver — `poll`'s formatted-text fallback and
    /// `escalate_to_human` — check this so they fail honestly instead of
    /// claiming success for a message that was never rendered.
    fn supports_outbound_send(&self) -> bool {
        false
    }

    async fn listen(&self, _tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        // The gateway WS path does not act as a message source for the
        // channel orchestrator; turns are driven directly by the WS
        // handler loop. Listen is a no-op for this transport.
        Ok(())
    }

    fn supports_free_form_ask(&self) -> bool {
        false
    }

    // The question methods never return `Ok(None)`: on this channel `ask_user`
    // would report that as "no free-form fallback". The viewer's answer may be
    // free text outside `choices`; `min_items`/`max_items` are not enforced
    // because the answer card returns at least one entry and may add its
    // "other" text.
    async fn request_choice(
        &self,
        question: &str,
        choices: &[String],
        timeout: Duration,
    ) -> anyhow::Result<Option<String>> {
        let answers = self.ask(question, choices, false, timeout).await?;
        Ok(answers.into_iter().next())
    }

    async fn request_multi_choice(
        &self,
        question: &str,
        choices: &[String],
        _min_items: usize,
        _max_items: usize,
        timeout: Duration,
    ) -> anyhow::Result<Option<Vec<String>>> {
        Ok(Some(self.ask(question, choices, true, timeout).await?))
    }

    async fn request_text(
        &self,
        question: &str,
        timeout: Duration,
    ) -> anyhow::Result<Option<String>> {
        let answers = self.ask(question, &[], false, timeout).await?;
        Ok(answers.into_iter().next())
    }

    // The trait default for reactions is a silent no-op that reports success.
    // The web chat has no agent-visible message ids and renders no reactions,
    // so refuse instead of claiming a reaction that never happened.
    async fn add_reaction(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _emoji: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("the web chat has no message reactions; nothing was changed")
    }

    async fn remove_reaction(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _emoji: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("the web chat has no message reactions; nothing was changed")
    }

    /// Delegates to [`Self::request_approval_attributed`] and drops the
    /// provenance, so the prompt/timeout logic lives in exactly one place.
    async fn request_approval(
        &self,
        recipient: &str,
        request: &ChannelApprovalRequest,
    ) -> anyhow::Result<Option<ChannelApprovalResponse>> {
        Ok(self
            .request_approval_attributed(recipient, request)
            .await?
            .map(|attributed| attributed.response))
    }

    async fn request_approval_attributed(
        &self,
        _recipient: &str,
        request: &ChannelApprovalRequest,
    ) -> anyhow::Result<Option<AttributedApprovalResponse>> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(request_id.clone(), tx);

        let event = TurnEvent::ApprovalRequest {
            request_id: request_id.clone(),
            tool_name: request.tool_name.clone(),
            arguments_summary: request.arguments_summary.clone(),
            timeout_secs: self.timeout.as_secs(),
        };
        if self.event_tx.send(event).await.is_err() {
            // Forward task has gone away; the WS is closing. Clean up the
            // pending entry and let the agent's caller treat this the same
            // as any other channel that returns None: fall through to
            // auto-deny per ApprovalManager policy.
            self.pending.lock().remove(&request_id);
            return Ok(None);
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => Ok(Some(AttributedApprovalResponse::operator(decision))),
            Ok(Err(_)) => {
                // Sender dropped without responding (connection closed
                // mid-prompt). Treat as deny rather than None so the agent
                // does not silently fall back to "no channel handled this" —
                // but mark it Unreachable, because nobody answered.
                self.pending.lock().remove(&request_id);
                Ok(Some(AttributedApprovalResponse::from_runtime(
                    ChannelApprovalResponse::Deny,
                    ApprovalSource::Unreachable,
                )))
            }
            Err(_) => {
                // Timeout: pop and deny. Mirrors Telegram / Slack behaviour
                // when the operator does not tap a button in time. This deny is
                // the runtime's, not the operator's.
                self.pending.lock().remove(&request_id);
                Ok(Some(AttributedApprovalResponse::from_runtime(
                    ChannelApprovalResponse::Deny,
                    ApprovalSource::TimedOut,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ws_approval_channel_declines_free_form_ask() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending, Duration::from_secs(30));
        assert!(
            !channel.supports_free_form_ask(),
            "WsApprovalChannel must refuse free-form ask_user; \
             its send() is a no-op and listen() drops immediately"
        );
    }

    fn approval_request() -> ChannelApprovalRequest {
        ChannelApprovalRequest {
            tool_name: "file_write".to_string(),
            arguments_summary: "path=a.txt".to_string(),
            raw_arguments: None,
            position: None,
        }
    }

    /// The operator never taps anything and the prompt times out. The channel
    /// still synthesizes `Some(Deny)` so the agent does not fall through to
    /// "no channel handled this" — but that deny is the RUNTIME's, and the
    /// provenance has to say so. Before this, the identical `Some(Deny)` was
    /// reported to the model as "Denied by user." on a run where no human was
    /// ever asked.
    #[tokio::test]
    async fn timed_out_prompt_denies_with_runtime_provenance() {
        // Keep the receiver alive so the send succeeds and we reach the timeout
        // rather than the "forward task gone" early return.
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending, Duration::from_millis(50));

        let attributed = channel
            .request_approval_attributed("operator", &approval_request())
            .await
            .expect("timeout is not an error")
            .expect("a timeout still yields a deny, not None");

        assert_eq!(attributed.response, ChannelApprovalResponse::Deny);
        assert_eq!(
            attributed.source,
            ApprovalSource::TimedOut,
            "a prompt nobody answered is not an operator decision"
        );
        assert!(attributed.source.is_runtime_fail_closed());
    }

    /// The connection closes mid-prompt: the pending sender is dropped. Same
    /// contract as the timeout — a deny, but not the operator's.
    #[tokio::test]
    async fn dropped_responder_denies_with_runtime_provenance() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, Arc::clone(&pending), Duration::from_secs(30));

        let call = async {
            channel
                .request_approval_attributed("operator", &approval_request())
                .await
        };
        // Drop the registered oneshot sender out from under the waiter, which is
        // what a closing WebSocket does.
        let dropper = async {
            for _ in 0..100 {
                {
                    let mut guard = pending.lock();
                    if let Some(key) = guard.keys().next().cloned() {
                        guard.remove(&key);
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (result, ()) = tokio::join!(call, dropper);

        let attributed = result
            .expect("a dropped responder is not an error")
            .expect("a dropped responder still yields a deny, not None");
        assert_eq!(attributed.response, ChannelApprovalResponse::Deny);
        assert_eq!(
            attributed.source,
            ApprovalSource::Unreachable,
            "a responder that went away is not an operator decision"
        );
    }

    /// The legacy entry point must keep its existing shape: provenance is an
    /// addition, not a change to what `request_approval` returns.
    #[tokio::test]
    async fn legacy_request_approval_still_returns_a_bare_deny_on_timeout() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending, Duration::from_millis(50));

        let response = channel
            .request_approval("operator", &approval_request())
            .await
            .expect("timeout is not an error");
        assert_eq!(response, Some(ChannelApprovalResponse::Deny));
    }

    /// Read the `QuestionRequest` that `ask` emitted.
    async fn next_question(rx: &mut mpsc::Receiver<TurnEvent>) -> (String, Vec<String>, bool, u64) {
        match rx.recv().await.expect("a question event") {
            TurnEvent::QuestionRequest {
                request_id,
                choices,
                multi_select,
                timeout_secs,
                ..
            } => (request_id, choices, multi_select, timeout_secs),
            other => panic!("expected a QuestionRequest, got {other:?}"),
        }
    }

    /// Answer through the frame parser, exactly as the socket loop does.
    fn answer(channel: &WsApprovalChannel, request_id: &str, answers: serde_json::Value) {
        assert!(crate::ws_question::resolve_question(
            &channel.pending_questions(),
            &serde_json::json!({"type": "question_response", "request_id": request_id, "answers": answers}),
        ));
    }

    fn choices(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[tokio::test]
    async fn an_answered_question_returns_the_answer() {
        let (tx, mut rx) = mpsc::channel(8);
        let channel = WsApprovalChannel::new(tx, new_pending_approvals(), Duration::from_secs(30));
        let offered = choices(&["A", "B"]);
        let call = channel.request_choice("Which?", &offered, Duration::from_secs(30));
        let answerer = async {
            let (id, sent, multi, timeout_secs) = next_question(&mut rx).await;
            assert_eq!(sent, offered);
            assert!(!multi);
            assert_eq!(timeout_secs, 30);
            answer(&channel, &id, serde_json::json!([" B "]));
        };
        let (result, ()) = tokio::join!(call, answerer);
        assert_eq!(result.unwrap(), Some("B".to_string()));
    }

    #[tokio::test]
    async fn a_free_form_question_carries_no_choices() {
        let (tx, mut rx) = mpsc::channel(8);
        let channel = WsApprovalChannel::new(tx, new_pending_approvals(), Duration::from_secs(30));
        let call = channel.request_text("Name?", Duration::from_secs(30));
        let answerer = async {
            let (id, sent, multi, _) = next_question(&mut rx).await;
            assert!(sent.is_empty());
            assert!(!multi);
            answer(&channel, &id, serde_json::json!(["zerolite"]));
        };
        let (result, ()) = tokio::join!(call, answerer);
        assert_eq!(result.unwrap(), Some("zerolite".to_string()));
    }

    #[tokio::test]
    async fn a_multi_select_question_returns_every_pick() {
        let (tx, mut rx) = mpsc::channel(8);
        let channel = WsApprovalChannel::new(tx, new_pending_approvals(), Duration::from_secs(30));
        let offered = choices(&["A", "B", "C"]);
        let call = channel.request_multi_choice("Which?", &offered, 1, 3, Duration::from_secs(30));
        let answerer = async {
            let (id, _, multi, _) = next_question(&mut rx).await;
            assert!(multi);
            answer(
                &channel,
                &id,
                serde_json::json!(["A", "C", "something else"]),
            );
        };
        let (result, ()) = tokio::join!(call, answerer);
        assert_eq!(
            result.unwrap(),
            Some(choices(&["A", "C", "something else"]))
        );
    }

    #[tokio::test]
    async fn a_dismissed_question_is_an_error() {
        let (tx, mut rx) = mpsc::channel(8);
        let channel = WsApprovalChannel::new(tx, new_pending_approvals(), Duration::from_secs(30));
        let offered = choices(&["A"]);
        let call = channel.request_choice("Which?", &offered, Duration::from_secs(30));
        let answerer = async {
            let (id, ..) = next_question(&mut rx).await;
            answer(&channel, &id, serde_json::Value::Null);
        };
        let (result, ()) = tokio::join!(call, answerer);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("dismissed the question"), "{err}");
    }

    #[tokio::test]
    async fn a_question_whose_responder_is_dropped_fails_at_once() {
        let (tx, mut rx) = mpsc::channel(8);
        let channel = WsApprovalChannel::new(tx, new_pending_approvals(), Duration::from_secs(300));
        let offered = choices(&["A"]);
        let call = channel.request_choice("Which?", &offered, Duration::from_secs(300));
        let dropper = async {
            let _ = next_question(&mut rx).await;
            channel.pending_questions().lock().clear();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(call, dropper)
        })
        .await
        .expect("a dropped responder must not wait for the timeout");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no web chat viewer is attached"), "{err}");
    }

    #[tokio::test]
    async fn an_unanswered_question_times_out() {
        let (tx, _rx) = mpsc::channel(8);
        let channel = WsApprovalChannel::new(tx, new_pending_approvals(), Duration::from_secs(30));
        let err = channel
            .request_choice("Which?", &choices(&["A"]), Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no answer within 0 seconds"), "{err}");
        assert!(channel.pending_questions().lock().is_empty());
    }

    #[tokio::test]
    async fn reactions_are_refused_not_dropped() {
        let (tx, _rx) = mpsc::channel(8);
        let channel = WsApprovalChannel::new(tx, new_pending_approvals(), Duration::from_secs(30));
        let add = channel.add_reaction("chat", "1", "🔥").await.unwrap_err();
        let remove = channel
            .remove_reaction("chat", "1", "🔥")
            .await
            .unwrap_err();
        assert!(add.to_string().contains("no message reactions"));
        assert!(remove.to_string().contains("no message reactions"));
    }
}

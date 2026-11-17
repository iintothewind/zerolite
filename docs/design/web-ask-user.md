# Web chat questions (ask_user, poll) and honest reactions

> **Status: specification.** This page records how the web chat answers an
> agent's questions and why `reaction` is refused there. For the merge rule,
> see `docs/merge_update_rules.md`. Decisions here are precedent: cite them as
> `web-ask-user RN` in merge reports.

## Decision record: why this shape

- **R1 — A question is a blocking prompt answered on the socket that carries
  the turn.** `ask_user` and `poll` park the tool on a question card above the
  composer, like a tool approval. The operator chat parks it in-process
  (`WsApprovalChannel`); the scoped (OIDC non-admin) chat relays the core's
  ACP `elicitation/create` (`chat_core.rs` `TurnRelay::elicit`).
- **R2 — A question dies with its viewer, its turn's cancel, or its deadline,
  with a named cause, and is not re-shown on reconnect.** The tool result
  reads `no web chat viewer is attached to answer the question`,
  `the user dismissed the question without answering`, or
  `no answer within N seconds`; the turn continues. A reconnecting socket
  builds a new agent and does not attach to the running turn (same rule as
  approvals).
- **R3 — Free text is always available on the operator chat, never on the
  scoped chat.** On the operator chat `request_choice` may return text
  outside `choices`, and a no-choices `ask_user` uses
  `Channel::request_text`. The scoped chat answers by choice index, so only
  listed choices can be returned and free-form questions stay refused by the
  core (upstream free-form elicitation, "Phase 2", is not implemented).
- **R4 — The `ask_user` contract adds only `multi_select`.** Several
  questions are several calls; the web queues them and shows one at a time
  (`1/N`). `poll` rides the same card: one viewer's answer, counting down the
  poll's `duration_minutes`; Skip ends it early.
- **R5 — `reaction` on a chat back-channel is refused, never a silent
  no-op.** The `Channel` trait default for reactions reports success while
  doing nothing. The web chat has no agent-visible message id and renders no
  reactions, so `wss` (`WsApprovalChannel`) and `rpc` (`RpcApprovalChannel`,
  scoped web chat and zerocode Code tab) bail, as `acp` already does.

## Wire contract (`/ws/chat`)

Server → browser:

```json
{"type":"question_request","request_id":"<uuid v4>","question":"<text>","choices":["A","B"],"multi_select":false,"free_text":true,"timeout_secs":300}
```

- `choices`: always present; `[]` is a free-form question.
- `free_text`: `true` on the operator chat, `false` on the scoped chat.
- `timeout_secs`: seconds, or `null` on the scoped chat (the core does not
  publish its deadline).

Browser → server:

```json
{"type":"question_response","request_id":"<uuid>","answers":["B"]}
{"type":"question_response","request_id":"<uuid>","answers":null}
```

- `answers`: non-empty array of non-blank strings = answered (multi-select
  sends the picked choices in listed order, then the "other" text). `null`,
  missing, not an array, or all-blank = dismissed.

Frame building and parsing live in one module,
`crates/zeroclaw-gateway/src/ws_question.rs`.

## Behaviors

| # | Behavior | Anchor |
|---|---|---|
| 1 | Operator chat: `question_request` from `TurnEvent::QuestionRequest`; answers resolve the parked oneshot; viewer leave / cancel drops it | `ws_approval.rs` (`ask`), `ws.rs` (`approval_event_rx` arms, `detach_mid_turn`) |
| 2 | Scoped chat: core connections advertise `elicitation.form`; `elicitation/create` becomes a card; the answer goes back as `choice-<i>` | `dashboard_core.rs` (`handshake`, `CoreChat::requests`), `chat_core.rs` (`TurnRelay::elicit`/`answer`) |
| 3 | `ask_user` free-form questions use `Channel::request_text` before the `supports_free_form_ask` gate; `multi_select` routes to `request_multi_choice` | `crates/zeroclaw-tools/src/ask_user.rs` |
| 4 | Question card: option rows, multi-select checkboxes, "other" input (operator chat only), Skip, countdown when known, `1/N` queue | `web/src/components/QuestionBanner.tsx`, `web/src/lib/pendingQuestions.logic.ts`, `AgentContext.tsx` |
| 5 | `reaction` on `wss` / `rpc` fails with "no message reactions; nothing was changed" | `ws_approval.rs`, `crates/zeroclaw-runtime/src/rpc/approval_channel.rs` |

Tests that must survive: `ask_user::tests::{free_form_question_uses_the_native_text_prompt, free_form_question_without_a_native_prompt_fails_fast, multi_select_joins_the_picked_choices}`;
`ws_question::tests::*`; `ws_approval::tests::{an_answered_question_returns_the_answer, a_free_form_question_carries_no_choices, a_multi_select_question_returns_every_pick, a_dismissed_question_is_an_error, a_question_whose_responder_is_dropped_fails_at_once, an_unanswered_question_times_out, reactions_are_refused_not_dropped}`;
`ws::tests::{a_question_is_answered_over_the_socket, a_question_fails_at_once_when_its_viewer_leaves, a_poll_question_is_answered_over_the_socket}`;
`dashboard_core::tests::a_scoped_chat_answers_a_question_with_one_of_its_choices`;
`rpc::approval_channel::tests::reactions_are_refused_not_dropped`;
`web/src/lib/pendingQuestions.logic.test.ts`; the question case in
`web/src/contexts/sessionLifecycle.test.ts`.

## Merge notes

- **Merge watch:** `Channel::request_text` (fork-added trait method; the only
  forwarding wrapper is `PacedChannel`), `TurnEvent::QuestionRequest`
  (fork-added variant; any exhaustive `match` must handle it like
  `ApprovalRequest`), the `approval_event_rx` arms in `ws.rs`, `handshake`'s
  `client_capabilities` in `dashboard_core.rs`, the reaction overrides on
  `WsApprovalChannel` / `RpcApprovalChannel`.
- **Upstream free-form elicitation ("Phase 2")** would let the scoped chat
  carry free text; adopting it changes R3 and the scoped `free_text: false`.
- **Verify post-merge by behavior:** on a web chat, `ask_user` with choices
  shows the card and the tool result carries the click; `poll` does not
  return `did not complete the single-select poll`; `reaction` fails with the
  named cause.

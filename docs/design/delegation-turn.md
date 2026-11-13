# Delegation and turn semantics

> **Status: specification.** This page records ZeroLite's agent turn and
> delegation behaviors. For the merge rule, see `docs/merge_update_rules.md`.
> Decisions here are precedent: cite them as
> `delegation-turn #N` in merge reports.

## Decision record: why this shape

- **R1 — A caller must be able to see its delegates.** Nested background
  delegates stay visible to their caller, and foreground subagent tasks
  are inspectable. A delegation that disappears from the caller's view
  cannot be steered, cancelled, or accounted for.
- **R2 — Cancellation propagates to the whole subtree.** Cancelling a
  turn cancels its parallel and agentic delegate children with it. Orphan
  delegates that outlive their turn are a resource and correctness bug,
  not an acceptable leftover.
- **R3 — Settlement survives worker exit.** A delegate settlement is
  recovered after its worker exits instead of being lost with the worker.
  Turn accounting must not depend on the worker staying alive.
- **R4 — Peer turns are durable, not in-memory gossip.** Peer-agent inbox
  turns persist, with a bounded lifecycle, so a peer turn is not lost to a
  restart and cannot grow unbounded.
- **R5 — An empty final reply is not a finished turn.** A terminal
  response without tool calls whose text is empty, whitespace, think-only,
  or a lone `.` filler is rejected and never persisted. Provider retry
  policy runs first; once exhausted, the turn loop appends one
  continuation nudge per empty streak (at most 3 per turn), carried as a
  `[Tool results]` prompt carrier at the history tail so it stays runtime
  machinery and keeps the request prefix cache-stable. A second
  consecutive empty reply fails the turn with the explicit semantic-empty
  error. Observed cause: a local model that writes `.` beside every tool
  call ended turns silently whenever the tool call was missing.
- **R6 — A message the runtime writes into a turn is never an operator
  turn.** Text the runtime appends in the `user` role mid-turn — the R5
  nudge, the max-iteration / execution-tree summary prompt, the
  `[Tool call parse error]` feedback, the repeated tool-failure correction
  (#11507) — rides a `[Tool results]` prompt
  carrier (`render_prompt_tool_carrier(text, &[])`): the gateway stores it
  as a `runtime` row the dashboard hides, and trim, vision routing and
  provider truncation do not count it as a turn boundary, so `kept_turns`
  matches the user bubbles the dashboard counts. A live SOP step context is
  the exception in shape, not in rule: it stays a plain `user` message and a
  turn boundary (a long SOP run trims on it) and is classified as machinery
  by its `[SOP: ` prefix. Observed cause: after a reload the summary prompt
  rendered as a user bubble and split one turn into a tool group and a
  detached answer.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | Delegates tracked per-worker; nested background delegates may be invisible to the caller | Nested background delegates remain visible to their caller; foreground subagent tasks inspectable in zerocode | R1 |
| 2 | Turn cancellation does not reliably reach parallel/agentic delegate children | Cancellation propagates to the whole delegate subtree with the turn | R2 |
| 3 | Delegate settlement tied to worker lifetime | Settlement recovered after worker exit | R3 |
| 4 | Peer inbox turns transient or unbounded | Peer-agent inbox turns persisted with a bounded lifecycle | R4 |
| 5 | Semantic-empty is empty/whitespace/think-only text; exhaustion fails the turn (#9421) | A lone `.` is semantic-empty too; exhaustion gets one bounded continuation nudge per empty streak (≤3 per turn) before the explicit failure | R5 |
| 6 | Summary prompt and parser feedback are plain `user` messages; a live SOP step context is stored as an operator row | Summary prompt and parser feedback ride the `[Tool results]` prompt carrier; a live SOP step context is stored as a `runtime` row and stays a turn boundary | R6 |
| 7 | The repeated tool-failure correction (#11507) is appended as a `system` row mid-turn | It rides the `[Tool results]` prompt carrier; the closing message stays an assistant row (owner decision 2026-11-12) | R6 |

## Merge notes

- **Verify post-merge by scenario, not by diff:** cancel a turn with a
  live parallel delegate and confirm the child stops; kill a worker
  mid-settlement and confirm the settlement is recovered; restart with a
  pending peer turn and confirm it is still there and bounded.
- **Upstream runtime/turn refactors are semantic changes here even when
  they are framed as cleanups.** If a refactor moves turn ownership, it
  must preserve R1–R6 or be reported as new semantics.
- **Code anchors:** `crates/zeroclaw-runtime/src/agent/` (turn, delegate
  settlement, peer inbox), `crates/zeroclaw-tools/src/` (peer send);
  R5: `zeroclaw-api/src/model_provider.rs::is_semantically_empty_text`,
  `agent/turn/empty_nudge.rs`, and its two call sites in
  `agent/turn/mod.rs` (`empty_completion_nudge.admit` in the `Err` arm of
  the provider call, `record_tool_round` after the terminal-response
  block).
- **R5 tests:** `lone_period_filler_is_semantically_empty_only_without_tool_calls`
  (zeroclaw-api); `a_filler_only_stream_is_rejected_but_filler_beside_a_tool_call_is_kept`,
  `empty_nudge::tests::*`,
  `run_tool_call_loop_nudges_after_filler_only_reply_then_completes`,
  `run_tool_call_loop_fails_semantic_empty_when_nudge_is_answered_with_filler`
  (zeroclaw-runtime).
- **R6 code anchors:** `agent/turn/max_iter.rs` (`summary_prompt`); the
  `[Tool call parse error]` block and the `repetition_disposition` correction
  in `agent/turn/mod.rs`;
  `agent/history_trim.rs::is_synthetic_user_message`;
  `sop/engine.rs::STEP_CONTEXT_PREFIX`.
- **R6 tests:** `safety_net_graceful_summary_persists_assistant_summary`,
  `run_tool_call_loop_retries_malformed_tool_protocol_without_leaking_json`,
  `a_step_context_is_runtime_machinery_and_still_a_turn_boundary`,
  `repeated_failure_correction_rides_the_tool_results_carrier`,
  `repeated_failure_correction_is_machinery_and_not_a_turn_boundary`
  (zeroclaw-runtime).

## Sources

- Fork commits `176e5b534` (nested background delegates visible to
  caller), `101746055` (recover delegate settlement after worker exit),
  `124e58ee3` (cancel turn parallel and agentic delegate children),
  `6274772bd` (persist peer-agent inbox turns), `5f1ec801f` (inspect
  foreground subagent tasks).
- Upstream `zeroclaw-labs/zeroclaw#9421` (semantic-empty terminal
  completion: retry + fallback, then fail).
- Fork: empty-completion nudge (R5).

# Analysis: Context Trim Does Not Shrink Visible Message History

**Status:** Phases 1–6 landed (Phase 6: send-budget vs water-line, stage-2 warn / stage-3 error-only, orchestrator durable rewrite, WebUI + Zerocode turn-boundary purge) — see §4.7 and plan `durable_context_trim`  
**Date:** 2026-09-09  
**Scope:** conversation history trimming across runtime, gateway/session, and clients; WebUI context meter  
**Related prior note:** `.workbuddy-ai/reports/context-trim-analysis.md` (implementation-oriented draft; facts largely agree; product intent below supersedes its “meter is secondary / B1-first” framing)  
**Execution plan:** `.cursor/plans/durable_context_trim_318a4b50.plan.md`

## 1. Observed phenomenon

When replayed context crosses `trim_threshold_percent` (the token water-line):

1. The UI surfaces the existing trim notice (breadcrumb / `HistoryTrimmed`), for example:

   > Earlier conversation history was trimmed: context token budget exceeded  
   > (N messages dropped; M turns kept).

2. The context utilization bar under the WebUI input **does not drop** after that notice.
3. Continued conversation pushes the bar higher until it approaches **100%**.
4. The chat transcript still shows the full prior conversation; trimmed turns are not removed from the message stream.
5. (Strong cross-turn signal) Dropped-message counts often **grow** across successive trims — consistent with durable history never shrinking, so each turn reloads a longer prefix before trimming a working copy again.

Operators reasonably interpret the notice as “older history was removed from context,” but neither the visible message stream nor the meter behave that way.

## 2. Understanding (what the system actually does today)

### 2.1 Two histories

| Store | Role | Trimmed today? |
| --- | --- | --- |
| Durable message history (`Agent.history` + session backend) | Continuity / UI / reload source of truth for the transcript | **No** — append-only; trim never rewrites it |
| Working copy (`loop_history` / turn `history`) | Provider-facing prompt for the current tool-loop | **Yes** — when the water-line fires |

Each turn roughly:

1. Copy durable history into a working buffer.
2. Decide whether to trim using that working buffer’s size (local estimate and/or provider-reported `input_tokens`).
3. If over the water-line, trim **the working buffer** to the newest `keep_recent_turns` whole turns (default **5**), plus leading system messages and a breadcrumb.
4. Send the (possibly trimmed) working buffer to the provider.
5. Persist only the **new** turn delta back into durable history — not the trimmed prefix.

So: **trigger and action both run on the working copy**; the copy is seeded from full durable history. Durable history itself is never reduced.

Ephemeral trim of the working copy **can** still be real for a given provider send. Calling that “trim fully ineffective” overstates the in-turn path; what fails is **durable / visible** compaction and honest post-trim metering.

### 2.2 Trim semantics

Documented in `docs/book/src/agents/context-management.md` / `history-management.md`:

- **Trigger:** replayed context tokens `> trim_threshold` (derived from `effective_context_window` and `trim_threshold_percent`, optionally bounded by `reserve_tokens`). Token water-line only.
- **Action (Phase 4):** prefer `keep_recent_turns`, then cascade whole turns down to `kept_turns == 1` until under the fit budget; hard-fail if the floor still exceeds. See §4.5.
- A “turn” is a real user message through the following assistant/tool exchange until the next user message.

Seeing “90 messages dropped; M turns kept” means M whole turns were retained after the cascade (plus system/breadcrumb policy), not a soft summary of the dropped prefix.

### 2.3 What the WebUI progress bar measures

- **Denominator:** `effective_context_window` (same window used for trim budget math).
- **Numerator:** `done.last_input_tokens` — the **last accepted provider-reported** prompt size for the turn (`usage.input_tokens`), not a post-trim local recount of durable history.
- `history_trimmed` only appends the existing ephemeral notice; it **does not** update the meter.
- Gateway WebSocket consumes mid-turn `TurnEvent::Usage` internally and forwards utilization primarily on the terminal `done` frame.

Provider `input_tokens` for that request are typically the **full billed prompt** for the call (messages as sent, and whatever else that API counts — often including tool definitions). The bar is not a separate hand-built sum of “system + skills + tools + history,” but it is intended to track that billed prompt size.

### 2.4 Clarifications

- After `sync_pending`, the current user message is already in the working buffer before preemptive trim, so a successful trim keeps **at most N whole turns including the current user turn** — not a durable “N + 1 = 6” shape.
- Long-term memory (SQLite / other memory backends) is a **separate** store from message history and is **out of scope**.
- Local token estimate is script-aware (Phase 5): Latin ≈4 chars/token; CJK ≈1
  char/token. Provider `input_tokens` remains the occupancy support fact after
  a billed response; `cached_tokens` is cache observability only.

### 2.5 Implementation constraint (for any durable fix)

`Agent.history` is `Vec<ConversationMessage>` (including structured `AssistantToolCalls`), not a bare `Vec<ChatMessage>`. Durable trim must apply **whole-turn** rules on that representation (or an equivalent mapping). Naively overwriting durable history with the provider `loop_history` copy would drop structure and risk splitting tool calls from results.

## 3. Root cause

The user-visible failure is not “trim never runs.” `HistoryTrimmed` proves the working-copy path runs. Two causes explain the two symptoms; a third is secondary.

### 3.1 Durable history is not trimmed (stream / cross-turn rebound)

Trim mutates only the per-turn working copy. Durable `Agent.history` and append-only session storage keep the full transcript.

Consequences:

- The message stream the user sees stays long after context for the model was (or should have been) shortened.
- Every new turn reloads the full mountain into a new working copy; correct provider behavior depends on **re-trimming every time**, rather than on a single durable compaction.
- Visible history and provider-facing interactive context **diverge**.
- Dropped counts tend to increase across turns (prefix grows, copy is re-trimmed).

This is the root cause of **“the information stream does not match what is actually eligible as interactive context.”**

### 3.2 Metering samples the wrong moment (bar does not drop)

Post-response trim (`enforce_reported_budget`) often runs **after** `usage.input_tokens` for that call has already been recorded. If the turn ends without another LLM call, `done.last_input_tokens` remains the **pre-trim** bill. The bar therefore stays high even when the working copy was subsequently shortened.

Within a tool loop, later iterations can grow again on top of a trimmed base; the bar updates from **end-of-turn peak** bills, so operators mostly see climbing peaks, not post-trim valleys.

This is the root cause of **“trim notice fired but the progress bar did not drop.”**  
It is **not** solved by durable history alone, and it is **not** “fake the number downward” — it is refreshing the meter to post-trim occupancy.

Durable non-trim alone does not fully explain a stuck bar if a later send were billed on a short copy and that bill refreshed the UI; the observed “notice then bar unchanged” pattern needs this sampling gap as well.

### 3.3 Secondary amplifier (not the main story)

Keeping N whole turns does not guarantee occupancy falls below the water-line. That can matter in tool-heavy sessions, but it is **not** required to explain the reported UX, and it must not be mistaken for “the last five turns usually hold ~99% of an 80-turn session.”

## 4. Desired change direction (confirmed)

### 4.1 Product intent

1. **Trim real message history**, not only a disposable working copy. Durable interactive history (`Agent.history` + session persistence) must be compacted when trim fires so it matches what will be sent as the interactive conversation portion of the provider prompt.
2. **Message stream consistency:** what the user sees for user↔model interaction must match what is eligible to be sent to the provider. Trimmed messages **disappear** from the UI. The **existing** trim notice / breadcrumb (`HistoryTrimmed` / “Earlier conversation history was trimmed…”) is **sufficient** — no new summary UX is required beyond keeping that notification.
3. **Honest context meter after trim:** after a real history trim, the progress bar must reflect post-trim occupancy (full billed-prompt semantics). Syncing history alone is not enough; refresh the meter on trim (authoritative post-trim usage or calibrated equivalent). This is part of the fix, not a substitute for durable trim, and not a display-only workaround.
4. **Long-term memory:** out of scope.
5. **Breadth:** not WebUI-only. Any path that retains or displays session message history applies the same compaction.

### 4.2 Agreed approach (do this)

**1. Single durable whole-turn compaction (prefer one semantic, not two drifting trims)**

When the water-line fires:

- Apply the same whole-turn keep-N policy to durable `ConversationMessage` history (preserving tool-call / tool-result pairing and leading system + existing breadcrumb rules).
- Prefer a **single compaction point** that drives both “what we store/show” and “what we send” (trim durable history, then build the provider request from that shortened history; keep in-loop trim only as a safety net for mid-turn growth / overflow). Avoid “loop trims a `ChatMessage` copy, agent later guesses an equivalent cut” unless locked by shared `dropped_turns` / identical boundary rules and tests.

**2. Session storage must truly shrink (not seed-only)**

- Do **not** stop at “trim only on `seed_history_with_event` while SQLite keeps the full transcript” as the end state: other clients reading the store would still see the long history.
- Compact or prune the session backend so reload/reconnect yields the same shortened transcript. Seed-time trim may exist as a transitional guard, not as the product solution.

**3. Meter refresh as a companion of real trim**

- On durable trim (and/or the next authoritative provider `input_tokens` after a shortened send), update the context bar so it tracks post-trim occupancy.
- Clients that render history must drop purged messages when they receive the existing trim notification (or an explicit purge signal carrying the same semantics).

**4. Follow-ups** — superseded by §4.5 (Phase 4 cascade) and §4.6 (Phase 5 estimate + usage facts). First slice (Phases 1–3) stayed **consistency + honest metering** without redefining keep-N; Phase 4 is the product decision to add budget cascade.

### 4.3 Explicit non-goals

- Changing long-term memory write/recall behavior.
- Display-only meter tricks that invent a lower number without durable / provider-aligned occupancy.
- Leaving full history in the session store forever while only trimming in-memory views.

### 4.4 Success criteria

- After a trim event, durable session history contains only the retained interactive turns (plus required system/breadcrumb policy), not the dropped prefix.
- Reloading the session / reconnecting any client shows the same shortened transcript (dropped bubbles gone; existing trim notice retained as today).
- The context bar updates to a value consistent with post-trim prompt occupancy, rather than remaining stuck on the pre-trim last bill.
- Subsequent turns do not re-inflate durable history with previously trimmed turns; dropped counts stop monotonically exploding across an idle growing prefix.
- Structural integrity: no orphan tool results / `AssistantToolCalls`; system + breadcrumb rules preserved.

### 4.5 Phase 4 design — budget-aware keep-N cascade (product decision 2026-09-09)

| Stage | Behavior | Stop when |
| --- | --- | --- |
| **1. Turns first** | Water-line fires → keep newest `keep_recent_turns` (default 5, clamp `1..=10`) | Estimated / calibrated tokens **≤ send budget** |
| **2. Budget cascade** | Still over → largest `kept_turns` in `1..=preferred` that fits (drop oldest whole turns in one compaction) | Fitting keep found, **or** only `kept_turns == 1` left |
| **3. Hard fail** | Floor still over send budget | Alert + abort turn; do **not** drop below 1 turn |

Invariants:

- **`kept_turns` minimum is 1** (newest whole turn always retained; leading system never dropped; whole-turn pairing intact).
- Trigger remains the token **water-line**; stages 2–3 compare against **send budget** (same number as provider send / window−reserve), not the water-line again.
- Durable persist + `HistoryTrimmed` + meter refresh from Phases 1–3 stay the write path; Phase 4 only changes **how many** turns helpers keep.
- Events report **actual** post-cascade `kept_turns` / `tokens_after` (may be `< keep_recent_turns`).

### 4.6 Phase 5 — script-aware estimate (A) + provider usage support fact (E) — **implemented**

**Problem:** `content.len().div_ceil(4) + 4` (UTF-8 bytes) under-counts CJK (~3 bytes/char → ~0.75 est. tokens/char vs often ≥1 real BPE token/char). `ContextCalibration` fixes the billed prefix after provider usage; cold start and unbilled tails stay biased. Phase 4 cascade amplifies that bias.

**Shipped stack:**

| Layer | Decision |
| --- | --- |
| **5a Script-aware heuristic (A)** | **Done.** `estimate_text_tokens` / `estimate_message_tokens` weight by Unicode script (Latin ~4 chars/token; CJK/kana/Hangul ~1 char/token; other conservative). Shared by history / floor / conversation / calibration tail. |
| **5b Provider usage support fact (E)** | **Done (docs + comments).** Prefer `input_tokens` via `ContextCalibration`; streaming requests `include_usage` when stream options are on. `cached_tokens` opportunistic subset for cache metrics only — never added into occupancy. |
| Bundled tiktoken / HF | Still **out**. |
| Pre-send `count_tokens` API | Still **not required**. |

### 4.7 Phase 6 — send-budget correctness + durable owners + client purge — **implemented**

Post-Phase-4/5 review found the cascade **implementation** still fitted against
`context_trim_budget()` (water-line), contradicting §4.5 / product lock and the
book docs. Channel paths that own append-only history also rebounded. Clients
purged incompletely.

| Gap | Fix shipped |
| --- | --- |
| Fit target | `ResolvedRuntime::context_send_budget()`; cascade / hard-fail use it; trigger stays water-line |
| Stage 2 | `below_preferred_keep` + `warn_if_below_preferred_keep` |
| Stage 3 | Hard-fail = Err / error text only; no `HistoryTrimmed` / no `CompactingContext` on inevitable fail |
| Token accounting | Durable pre-send via `trim_conversation_to_fit_with` + `to_provider_messages` |
| Orchestrator | `history_was_trimmed` + `replace_sender_history_after_trim` (LRU + `replace_messages`) |
| WebUI | Re-fetch + functional merge; fallback purge by user-turn boundaries |
| Zerocode | Purge by user-turn boundaries (not `drain` by message count) |

## 5. Alignment with the prior workbuddy report

| Topic | Prior report | This report | Resolution |
| --- | --- | --- | --- |
| Working copy vs durable | Same facts | Same facts | Aligned |
| “Occupancy won’t drop” main cause | Emphasized durable non-writeback as main | Durable desync for stream/rebound; **meter timing** for bar | Keep this split |
| Meter | Deprioritized as primary means | Required honest refresh after real trim | Per confirmed intent |
| Session | B1 seed-trim acceptable first | True store compaction required | Per confirmed intent |
| Notice UX | Breadcrumb / notice | Existing notice sufficient | Aligned |
| Channels | Flagged for sync | Must clean all history paths | Aligned |
| A1 vs A2 | A1 first, A2 later | Prefer single semantic (A2-like), loop as safety net | Agreed approach §4.2 |
| keep-N vs budget | Unconditional keep-N | Phase 4 cascade §4.5 | Product decision after Phases 1–3 |
| CJK / estimate | Called out | Phase 5 §4.6 implemented (script-aware A + usage fact E) | Done |

## 6. Key code / doc pointers

| Area | Location |
| --- | --- |
| Trim action | `crates/zeroclaw-runtime/src/agent/history_trim.rs` (`trim_to_recent_turns`) |
| Pre-send / reported-budget trim | `crates/zeroclaw-runtime/src/agent/turn/mod.rs`, `context_pipeline.rs` |
| Working copy vs durable merge | `crates/zeroclaw-runtime/src/agent/agent.rs` (`loop_history` / `round_added` → `self.history`) |
| Seed comment (defers trim to in-turn copy) | `agent.rs` `seed_history_with_event` |
| WS meter + trim frames | `crates/zeroclaw-gateway/src/ws.rs` |
| WebUI meter + trim notice | `web/src/contexts/AgentContext.tsx`, `web/src/pages/AgentChat.tsx` |
| Session append path | `crates/zeroclaw-gateway/src/ws.rs` (`backend.append`) |
| Documented semantics | `docs/book/src/agents/context-management.md`, `history-management.md` |
| Lifecycle boundary | `docs/book/src/architecture/memory-payload-lifecycle.md` |

## 7. Summary

| Question | Answer |
| --- | --- |
| Does trim run? | Yes — when the water-line fires (Phases 1–3: on durable history + session, not only a disposable copy). |
| Does durable / visible message history shrink? | **Yes** after Phases 1–3 (was no at analysis time). |
| Why didn’t the bar drop (original bug)? | It tracked **last provider bill**, often recorded **before** post-response trim, and ignored trim for meter updates — addressed by `tokens_after` + client refresh. |
| What shipped (1–3)? | Durable whole-turn compaction + session `replace_messages` + meter/UI purge. |
| What next (4)? | Budget-aware cascade: keep-N → drop to `kept_turns >= 1` until under send budget → else hard-fail (§4.5). |
| What next (5)? | Script-aware local estimate (A) + opportunistic provider `input_tokens` / cache details as support facts (E) (§4.6) — **implemented**. |
| What next (6)? | Wire cascade to **send budget** (not water-line); stage-2 warn; stage-3 error-only; orchestrator rewrite; full WebUI/Zerocode turn-boundary purge (§4.7). |

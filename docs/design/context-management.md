# Context management

> **Status: specification.** This page defines ZeroLite's context-management
> semantics. The code conforms to this page, not the other way round. Where
> upstream ZeroClaw differs, upstream is the one that is different — see the
> decision record (§1) and the merge guideline (§2) below.

## 1. Decision record: why these semantics

ZeroLite's context management is an independent design, not a modified copy
of upstream's. The two diverged from a common ancestor and evolved
separately; neither is a bug-fixed version of the other.
The semantics on this page rest on seven reasons. They are the actual grounds
for the decisions — a future change that cannot answer them has not made its
case.

- **R1 — Cache economics come first.** Replaying the message prefix to a
  provider is the dominant cost of a turn: every byte that changes forces a
  re-prefill, in latency and in money, and on local inference prefill is the
  slowest part of the pipeline. Every decision below is subordinate to
  keeping the replayed prefix byte-stable. This is also why the runtime never
  injects wall-clock time or other per-turn noise into the prefix (see
  "Locked prompt / catalog decisions").
- **R2 — The window is a property of the model, not of the config file.**
  The selected model can change mid-session — routing, a provider fallback, a
  user switching models. A budget written as an absolute token count is tuned
  for one model and silently wrong for the next. Budgets are therefore
  derived from the effective window (a percentage, a reserve), so they travel
  with the model.
- **R3 — One decision, one knob.** If two settings can each change the same
  outcome, the operator cannot tell which one is in effect, and incoherent
  combinations become representable. Context policy therefore lives in
  exactly one table, each field with exactly one job.
- **R4 — Fail loud, never silently pretend.** A trim that did not happen must
  not look like one that did, and a request that cannot fit must fail where
  the cause is known, not later at the provider as an opaque error. There is
  no "disable trimming" switch and no silent no-op: both defer a knowable
  failure to a place where it cannot be diagnosed.
- **R5 — The provider's reported size is ground truth.** A local estimate
  cannot account for tool schemas, multimodal payloads, or tokenizer
  differences, so it drifts from what the provider actually charges for.
  Sizing decisions anchor on the provider-reported `usage.input_tokens` and
  price only the not-yet-billed tail locally.
- **R6 — Retention is by dropping, never by rewriting.** History is retained
  or dropped in whole turns only. Summarization-based compaction rewrites
  the record: it destroys the byte-stable prefix (R1), inserts model-written
  text into history as if it were fact, costs an extra model call, and its
  errors compound invisibly. Dropping whole turns is lossy but honest, and
  the surviving turns remain exactly what the model produced.
- **R7 — There is no context compaction in this system.** Compaction is a
  distinct mechanism, not a stronger trim: a model call that rewrites history
  into a smaller surrogate and stores the surrogate as history. This system
  implements it in no form — no summarization pass, no mid-turn compaction,
  no provider-side compaction hook, and no config key that turns one on. R6
  forbids the rewriting; R7 records that the mechanism is *absent*, not
  merely switched off, so an upstream change that brings one in is adding a
  subsystem rather than restoring a lost default. The vocabulary follows the
  same decision: a shrinking history is a **trim**, its action is **dropping
  whole turns**, and no `compact_*` name exists for context retention — in the
  code, the config surface, or the log.

The divergences that follow from these reasons, decision by decision:

| # | Upstream | ZeroLite (this page) | Grounded in |
| --- | --- | --- | --- |
| 1 | One resolved number, `context_token_budget`, serves as **both** the proactive-trim trigger and the fit target. It is absolute by default (`max_context_tokens`, else the 32k legacy default); `context_compact_ratio` can derive it from the window, but in every mode the same number does both jobs | Two different numbers derived from the effective window: the **water-line** (`min(window × trim_threshold_percent/100, window − reserve_tokens)`) decides when trimming engages; the **send / fit budget** (`window − reserve_tokens`) decides whether the retained set is small enough | R2, R3 — conflating the two roles makes "engage early, fit later" inexpressible |
| 2 | No keep-N preference: `trim_to_recent_turns(budget)` keeps whatever fits the budget | `keep_recent_turns` is the **preferred** whole-turn retention; if those turns do not fit, the runtime **cascades** down to `kept_turns == 1` | The preference is the operator's (a stable working memory across models); the cascade is the runtime's. Budget-only retention makes retained history a pure function of the budget, so the same conversation keeps wildly different amounts of context on different models |
| 3 | Five layered knobs on the runtime profile — `max_history_messages`, `max_context_tokens`, `context_compact_ratio`, `history_pruning.*`, `context_compression.*` — combined by a `min()` chain in which any one can become the binding constraint | **One** table, `[runtime_profiles.<alias>.context]`, four fields, each with exactly one job | R3 |
| 4 | `max_context_tokens = Some(0)` is a sentinel that disables proactive trimming (upstream's own comment: "Preserve zero as the explicit proactive-trimming disable sentinel") | No disable switch. Trimming is always on; every field has a non-zero default | R4 — a disabled trim silently lets the request exceed the window and fail at the provider |
| 5 | `max_context_tokens` can cap the *budget*, but the model window itself cannot be clamped by an operator ceiling, so the utilization meter's denominator is whatever the provider or table claims | `request_token_ceiling` clamps the effective window: `model_window.min(request_token_ceiling)`; the clamped window is also the meter denominator | Without a clamp, an operator whose provider reports an unknown or inflated window has no way to bound it, and the meter is dishonest |
| 6 | No output-reserve concept in context policy | `reserve_tokens` bounds the water-line from above **and** defines the send budget | Without a reserve, a successful trim can leave no room for the model's own output, and the request fails immediately after |
| 7 | No `ContextCalibration`. A reported-budget trim path exists (`trim_to_reported_budget`) that proportionally scales a local estimate by the reported/estimated ratio — but the proactive trigger is sized from the local estimate alone | `ContextCalibration` re-anchors on the provider-reported `usage.input_tokens`; only the unbilled tail is priced locally. The calibrated size feeds **both** the trigger measurement and the trim | R5 — proportional scaling preserves the local estimate's error; anchoring replaces it at the anchor point |
| 8 | A trim that cannot fit is a silent no-op: even with only the floor remaining and still over budget, the trim returns the oversized history unchanged with `trimmed: false` and no error | A floor that cannot fit is a **hard failure**: an error to the caller, and **no** `HistoryTrimmed` notice | R4 |
| 9 | `context_compression.*`: summarization-based compaction rewrites older history into a model-written summary | Removed entirely. There is no summarization anywhere in the retention path, and no compaction subsystem to disable | R6, R7, R1 |

The practical consequence: **`[runtime_profiles.<alias>.context]` is the
only place context policy is configured.** A knob that appears in upstream
and not here was not overlooked; absent a new decision record that answers
R1–R7, it was rejected.

## 2. Merge guideline

**The rule has two lanes, decided by whether ZeroLite has decided the
question before:**

- **A prior ZeroLite decision covers it → apply that decision directly.**
  Decisions already made — in §1, in "Locked prompt / catalog decisions", or
  in any other recorded fork decision — are precedent. When an upstream
  change conflicts with one, the upstream change does not enter the merge
  and the fork behavior stays, without re-argument and without escalation:
  the question is already answered. Do not "adapt" fork code to upstream
  semantics, and do not re-introduce an upstream knob under a fork name. A
  one-line entry in the merge report (upstream commit, file, decision
  number, "skipped per §1 #N") is the whole procedure.
- **No prior decision covers it — it is new semantics → report it.** An
  upstream change that introduces semantics ZeroLite has never decided (a
  new knob, a new retention mechanism, a new failure behavior) is not the
  merge agent's to accept or to reject. It does not enter the merge. Report
  it to the maintainer: the upstream commit, what the new semantics is, and
  what adopting or refusing it would change. The maintainer decides; the
  outcome is written into §1, after which it is precedent and the first lane
  applies. When it is unclear whether a prior decision covers a case, treat
  it as new semantics and report — silence is not a decision.

What counts as semantics: the config surface and the meaning of a field; the
trim trigger and the fit target; the retention policy (what is kept, in what
units); failure behavior on an un-fittable floor; how context size is
measured; and every "Locked prompt / catalog decisions" item. The semantic
surface to watch is (paths as of writing; the semantics, not the paths, are
the invariant):
`crates/zeroclaw-config/src/scattered_types.rs` (`ContextConfig`),
`crates/zeroclaw-config/src/schema.rs` (runtime resolution and validation),
`crates/zeroclaw-config/src/context_window.rs`,
`crates/zeroclaw-runtime/src/agent/history.rs` (estimates),
`crates/zeroclaw-runtime/src/agent/history_trim.rs`, and
`crates/zeroclaw-runtime/src/agent/turn/context*.rs`.

- **What may merge freely.** Anything that touches no semantics at all —
  bug fixes, providers, channels, security, performance — including changes
  inside the same files, as long as the decisions above are preserved.
- **Post-merge check.** A type, table, or test surviving a merge does not
  prove the semantics survived — this has already happened once:
  `ContextConfig` still parsed and its unit tests still passed while no
  runtime code read it. Verify the resolution path actually consumes the fork config: the
  invariants are that `[context]` is the only effective config surface, the
  water-line differs from the send budget, keep-N cascades to a floor of 1
  and hard-fails there, trimming cannot be disabled, size is anchored by
  `ContextCalibration`, and no summarization compaction exists in the
  retention path.
- **An action must not survive a merge as a trigger.** The v0.8.6 merge
  (`1f379c6de`) swapped the post-loop re-sync for an upstream
  `Agent::trim_history` — keep-N with no water-line test — and hung it on the
  turn's success path. It compiled, and no test went red; the only symptom was
  a trim notice on *every* turn. The rule: a function that drops turns is an
  **action**; each of its call sites must be a trigger that can name its own
  evidence — the water-line was crossed, or the loop itself dropped turns.
  A call site that cannot name its trigger does not belong there.

Context management in ZeroClaw is, at its core, **cache management**. Every
byte that changes in the message prefix replayed to a provider forces that
provider to re-prefill the prefix, which costs both latency and money. The
runtime therefore measures context against a single, shared budget and prunes
history so the replayed prefix stays as byte-stable as possible. Trimming is
engaged by a single token water-line: when replayed context exceeds
`trim_threshold`, the runtime prefers the most recent `keep_recent_turns`
whole turns, then cascades down to `kept_turns == 1` until under the fit
budget (or hard-fails). The trigger is token-based; the action is
turn-count-first with a token fit check. "Replayed context" is the request the
provider receives — the messages **plus the tool definitions** — not the
messages alone; pricing only the messages put the effective trigger a whole
tool catalog above the configured percentage. See the upstream book's *History management* page.

This page is the reference for the knobs and the arithmetic. For how whole-turn
trimming physically drops messages, see the upstream book's *History management* page;
for the provider send path, see the upstream book's *Runtime internals* page.

## The effective context window

Everything trims against one number: the agent's **effective context window**.
It is resolved in this order (first match wins):

1. **Explicit provider `context_window`** (`[providers.<id>].context_window`) —
   always wins. This is the operator's source of truth.
2. **Built-in per-model table** — a conservative lower bound keyed by normalized
   model id, so an unconfigured provider does not silently fall back to a stub.
   For example `llama-3.1-70b` resolves to `131,072` and `gpt-4o` to `128,000`.
   Where a family ships variants with different windows, the smallest widely
   available value is recorded; an explicit `context_window` is the knob to raise
   it when you know your deployment gets more.
3. **Fallback `32_000`** (`UNCONFIGURED_CONTEXT_WINDOW_FALLBACK`) — last resort
   for a genuinely unknown model. Treat this as "not configured", not as the
   model's real capacity.

The resolved window is then clamped by the `[context]` input ceiling:

```text
effective_context_window = model_window.min(request_token_ceiling)   # request_token_ceiling: None ⇒ no clamp
```

The same `effective_context_window` is the denominator for the context
utilization meter (the Zerocode / web UI `ctx: used / max` bar). Trim uses two
derived quantities from that window — the **water-line** (when to engage) and
the **send / fit budget** (whether retained history is enough) — described
below. They must not be conflated.

## Configuration: `[runtime_profiles.<alias>.context]`

Context policy lives in one table per runtime profile. It replaces the legacy
`[runtime_profiles.<alias>.history_pruning]` and
`[runtime_profiles.<alias>.context_compression]` tables. Range-checked fields
(`trim_threshold_percent`, `reserve_tokens`) are validated at load time
(`Config::validate`) and are **never silently clamped** — an invalid value is
a hard config error. `keep_recent_turns` is the exception: it is clamped at
resolution to `1..=10` (see the field table below).

```toml
[runtime_profiles.coder.context]
request_token_ceiling = 100_000    # hard ceiling on request input tokens (optional)
trim_threshold_percent = 80    # where history trimming engages (default: 80)
reserve_tokens = 16_384        # output headroom held back from the threshold (optional)
keep_recent_turns = 5          # preferred whole turns retained when trimming fires (default: 5)
```

| Field | Type / default | Meaning |
| --- | --- | --- |
| `request_token_ceiling` | `Option<usize>`, default `None` | Hard ceiling on total request input tokens. `None` means the effective context window is the sole limit. This is the only override knob for providers whose real window is unknown. It is a **ceiling** and does **not** by itself trigger trimming. When set, it clamps the effective window (and thus the UI meter denominator) down via `model_window.min(request_token_ceiling)`. |
| `trim_threshold_percent` | `usize`, default `80` | Percentage of the effective context window at which history trimming engages. Must be in `1..=100`; `0` or `>100` is a hard config error. |
| `reserve_tokens` | `Option<usize>`, default `None` | Output headroom. (1) Bounds the water-line from above: `trim_threshold ≤ window − reserve`. (2) Defines the **send / fit budget** after trim: `send_budget = window − reserve` (or the full window when unset). Must not exceed **50%** of the effective window — beyond that the reserve would strand most of the window, which is a hard config error checked against the resolved model window. |
| `keep_recent_turns` | `usize`, default `5` | Preferred how many of the newest **whole turns** the trim action retains when it fires. Clamped at resolution to `1..=10` (`0` becomes `1`, `>10` becomes `10`). If those turns still exceed **`send_budget`**, the runtime cascades down to `kept_turns == 1`; it never drops the newest turn. |

All four have defaults, so an existing `config.toml` needs no changes to keep
working: omit the table entirely and you get `request_token_ceiling = None`,
`trim_threshold_percent = 80`, `reserve_tokens = None`, `keep_recent_turns = 5`.

## The trim threshold (water-line) vs the send / fit budget

Two different numbers share the same window. Do not use the water-line as the
cascade fit target.

```text
trim_threshold (water-line) = min(window × trim_threshold_percent / 100,
                                  window − reserve_tokens)   # reserve term omitted when None

send_budget (fit)           = window − reserve_tokens        # or window when reserve is None
```

- **Water-line** — *when* trimming engages (`tokens > trim_threshold`). Default
  percent is `80`. This is **not** the post-trim success criterion.
- **Send / fit budget** — *whether* retained history is enough after the action.
  Ideal end state:

  ```text
  estimate(system + tool/skill prompt prefix + retained turns)
    ≤ send_budget
  ```

  Equivalently: preferred `keep_recent_turns` (default 5) should fit under
  `send_budget` together with the non-droppable prefix; if not, cascade further.

When replayed context tokens exceed `trim_threshold`, the trigger fires and the
trim action runs on **durable** agent/session history (not only a disposable
working copy): see the upstream book's *History management* page.
`HistoryTrimmed` carries optional `tokens_after` so clients can refresh the
context meter immediately after a **successful** trim.

### How size is measured

Context size for the water-line and cascade uses two layers:

1. **Local heuristic** — `history::estimate_history_tokens` /
   `estimate_message_tokens`: a **script-aware** char walk (Latin ≈4 chars/token;
   CJK / kana / Hangul ≈1 char/token; other scripts mildly conservative) plus
   ~4 framing tokens per message. Not a provider tokenizer; good enough for
   cold start and unbilled tails. Working-copy `ChatMessage` values have only
   `role` + `content`; native tool calls and reasoning are serialized into
   `content` before append, so the content-only estimate already prices those
   payloads. Durable `ConversationMessage` history uses a structured estimate
   (or the provider view via `to_provider_messages`) instead.
2. **Provider support fact** — after an accepted response,
   `ContextCalibration` re-anchors on `usage.input_tokens` (`prompt_tokens`).
   Only the unbilled tail (assistant / tool rows appended since that report)
   is priced locally. Streaming paths request `stream_options.include_usage`
   when stream options are enabled; if usage is missing, the turn continues on
   the local estimate.

Optional `prompt_tokens_details.cached_tokens` (vLLM
`--enable-prompt-tokens-details`, OpenAI, etc.) maps to
`cached_input_tokens`. For OpenAI-compatible backends it is a **subset** of
`input_tokens` — used for cache hit / cost observability, **not** added into
context occupancy for trim or the meter. Absence of details is normal and must
not affect trim correctness.

## The trim action: prefer keep-N, then cascade to send budget

The token water-line above is the **only** trim trigger. When it fires, the
action is a three-stage cascade (`history_trim::trim_to_fit` /
`trim_conversation_to_fit`). Fit checks use **`send_budget`**, never
`trim_threshold_percent` / the water-line.

| Stage | Behavior | Stop when |
|-------|----------|-----------|
| 1. Turns first | Keep the newest `keep_recent_turns` whole turns (default `5`, clamped `1..=10`) plus leading system (tools/skills live in that non-droppable prefix) | Estimated / calibrated tokens **≤ `send_budget`** |
| 2. Budget cascade | Still over → choose the **largest** `kept_turns` in `1..=preferred` whose estimate fits `send_budget` (drop the oldest whole turns in one pass; never keep a gap in the middle). **Warn** when the chosen keep is below preferred | A fitting `kept_turns` exists, **or** only `kept_turns == 1` remains to try |
| 3. Hard fail | Floor (system/tools/skills prefix + newest turn) still over `send_budget` | Abort the turn with an **error message only** (no `HistoryTrimmed` notice); never drop below one turn |

How stage 2 is decided does not matter to operators: the runtime may probe by decrementing keep, binary-search, or estimate turn sizes once. The contract is the outcome — maximal keep that fits, whole turns only, floor 1.

### When the provider reports no usage

Two decision points read different evidence. Do not collapse them:

| Decision point | Evidence | Is the estimate the trigger? |
| --- | --- | --- |
| **Pre-send**, before a request leaves | the population about to be sent; no report for it can exist yet | **yes** — the water-line above *is* the local estimate |
| **Post-response**, after a reply lands | `usage.input_tokens` for the request just sent | **no** — without a report the estimate is not a measurement of what was billed |

The water-line trigger runs **pre-send**. Nothing has been sent, so no provider
report for this request exists and none can; refusing to trim until one arrives
would mean never trimming the request that is about to overflow. The local
estimate is the correct — and only — evidence there.

The **post-response** check is the one that must not act on a guess. It re-reads
the just-sent request's size to decide whether the *next* request needs to
shrink. The provider-reported `input_tokens` is authoritative there because it
is what the provider actually billed. Not every endpoint reports it:
OpenAI-compatible servers, local runtimes, and some streaming responses omit
usage entirely. Without a reported count the estimate cannot see what the
provider really charged, and trimming on it would cut history away on a guess —
the breadcrumb that records the cut can itself push the retained set past the
window. So on that path, with no reported usage, the runtime:

- **does not trim proactively** — the post-response water-line check stays
  inactive, because the estimate is not a measurement of the request just sent;
- **only acts as an overflow guard** — when the estimate plus the output
  reserve cannot fit the effective window (`estimate > send_budget`), the
  runtime trims and retries;
- **skips the guard when there is nothing to drop** — if only one whole turn
  remains, trimming cannot make the request smaller, so the request is
  dispatched rather than failed on a difference the trim cannot close.

The guard's threshold is `send_budget`, not the raw window, for two reasons.
The reserve is **physical output space, not a preference**: a prompt that fits
the window but leaves the model no room to answer in has already failed, so the
comparison is a correctness check rather than the proactive strategy the
water-line implements. And `send_budget` is what carries
`request_token_ceiling` into the check — the ceiling is an operator-declared
hard limit, not a strategy knob, so a guard measured against the raw model
window would ignore it.

A provider that reports usage keeps the full behaviour on both paths: pre-send
the water-line triggers the trim, post-response the reported count anchors it,
and the send budget decides what still fits.

**The floor is the model's capacity, not the send budget.** A retention that
misses the send budget but still fits the **effective window** (the model window
clamped by `request_token_ceiling`) is a *soft floor*: the newest turn is
dispatched rather than rejected. Only a population the model cannot take is an
unsatisfiable floor.

**Settlement.** Stage 2 selects by summing per-message estimates, which is only
exact when the estimator is additive. A provider view that prices the whole
slice higher than the sum of its messages — per-call framing, one-time tool
schemas, attachment accounting — would otherwise keep more turns than actually
fit and let the request overflow the window. After choosing a keep, the runtime
therefore re-prices the **retained set as a whole** and drops one more turn
until the whole-slice estimate fits `send_budget` or only the floor remains.
`tokens_after` reports the larger of the two prices, so a settled retention is
never reported as fitting when the whole-slice view disagrees.

| | Value |
|---|---|
| Trigger | replayed context tokens `> trim_threshold` (water-line; default 80%) |
| Fit target | `send_budget = window − reserve` (full window if no reserve) |
| Preferred retain | `keep_recent_turns` newest whole turns |
| Floor | `kept_turns >= 1` (newest whole turn always retained; system never dropped) |

Trimming is always whole-turn — a turn is never cut in half. An oversized newest
turn alone may still exceed `send_budget`; that is a hard failure (stage 3),
not a license to slice the turn.

## Pathological windows: the floor cannot fit

When the effective context window is so small that the **floor** — leading
system messages plus the one newest whole turn that trimming must always
retain — does not fit, there is **no path below keep=1**: the runtime never
truncates the system prompt and never drops the newest turn. It fails
explicitly instead, with these guards:

| Condition | Where | Behavior |
|---|---|---|
| System floor alone ≥ send budget | turn start (`iteration == 0`) | **Warning only** (log `error_key: context_floor_exceeds_budget`): remediation points at raising the window / shrinking tools. Treated as a configuration problem — the turn may still proceed until cascade / provider overflow hard-fails. |
| Cascade to `kept_turns == 1` still exceeds **send budget** | pre-send / reported-budget / overflow recovery | **Hard failure**: error message to the caller (CLI / UI); **no** successful-trim `HistoryTrimmed` notice. |

The overflow-recovery path is reached when a provider rejects the request with
a context-window error (matched by `reliable::is_context_window_exceeded` across
patterns such as `"exceeds the context window"`, `"maximum context length"`,
`"prompt is too long"`). Recovery runs the same keep-N → cascade action; it
retries only when the cascade lands under **`send_budget`**.

In short: `keep_recent_turns` is the **preferred** retain count; the cascade may
keep fewer turns (down to 1) to fit. When the window cannot hold the floor, the
correct fixes are operational — raise
`[runtime_profiles.<name>].context.request_token_ceiling` (or the provider
`context_window`), disable unused integrations to shrink the tool surface, or
point the agent at a larger-window model.

## Token estimation: script-aware, not bytes

The local estimate that sizes the water-line and the cascade is **script-aware**:
Latin text keeps the historical ~4 characters per token, while CJK, kana, and
Hangul characters are charged one token each (they tokenize near 1:1 with common
BPE families). A CJK caption therefore costs the same whether it rides inside an
image carrier or stands alone — the text around image parts is priced with the
same rule as any other message, and only the image parts themselves take the
fixed per-image charge.

## Cache-economics feedback

Trimming is cache-aware. The provider send path reconciles the message list
through an append-only log that preserves the longest byte-stable prefix, so
pruning rewrites only the tail past the first divergent message and earlier
bytes stay cacheable across turns. The cacheable prefix length is emitted at
`DEBUG` (`"provider prefix cache: N of M messages byte-stable"`), and the
provider-reported prefix-cache hit count is surfaced as a first-class metric
`zeroclaw_tokens_cached_input_total` (labels `model_provider`, `model`) and the
OTel span attribute `zeroclaw.usage.cached_input_tokens`. Together they let you
compare re-prefill cost directly against prefix-cache hits. Providers that
report the field include Anthropic, OpenAI, OpenRouter, compatible, and
OpenAI-Codex; providers that do not (Bedrock, Gemini, Azure, Copilot, Ollama)
report `None` and the metric simply stays at zero for them.

## Updating the meter

The meter's numerator is a *display* value, and it has exactly three update
points. They are ordered by increasing authority, and a later point always
overwrites an earlier one:

| When | Event | Source label | Why |
|---|---|---|---|
| A turn starts — after the water-line decision, before dispatch | `context_usage` | `estimate` | The assembled request exists and no provider has seen it yet. Without this, the meter keeps showing the previous turn's number for the whole turn. |
| A trim fires mid-turn | `history_trimmed` → `tokens_after` | `estimate` | The history just got smaller. Waiting for the next `done` leaves the meter above 100% of what will actually be sent. |
| A provider call returns | `usage` → `input_tokens` | `provider`, or `estimate` when the provider is silent | The only measured number on the path. It replaces the assembled estimate. |
| The turn ends | `done` → `last_input_tokens` | `provider` | Authoritative for the turn, and it clears the numerator when the accepted route reported no usage. |

Two different budgets appear in these events. Reading one as the other is the
easy mistake, because both are "the context budget" in conversation:

| Field | On | Is |
|---|---|---|
| `context_token_budget` (RPC) / `max_context_tokens` (WS) | `context_usage`, `usage`, `done` | The **water-line**: `trim_threshold_tokens().max(1)`. It answers *when* a trim engages. |
| `token_budget` | `history_trimmed` | The **send budget**: `effective_context_window − reserve_tokens`. It answers *how small* the trim was asked to make the request. |

The water-line is never absent in practice — `trim_threshold_percent` is
validated to `1..=100` and `context_trim_budget()` takes `.max(1)`, so there is
no "trimming disabled" state that could make it `None`. The field is `Option`
only because it shares the `ContextUsage` wire shape.

Every number in that table prices the same population: the messages **plus the
tool definitions the request carries**. A provider bills `input_tokens` for
both, so a count that omits the schemas is short by the whole tool catalog. That
omission is not cosmetic — it made the meter drop mid-turn and it put the
effective water-line a catalog above the configured percentage, because the
trigger compared a messages-only estimate against a threshold derived from the
full window.

Two rules follow from this and are not negotiable:

- **`context_usage` is not `usage`.** Clients fold `usage` into their step
  boundary — it means "a call completed". The assembled estimate means "a
  request is about to go out", so folding it would invent a step that never
  happened. They are separate events for exactly that reason, even though both
  end up as a `ContextUsage` notification on the RPC surface.
- **A stale estimate never overwrites a newer provider value — but ordering,
  not identity, is what enforces that.** These frames carry **no** turn id.
  What holds today: a session's turns are serialized by the per-session actor
  queue (`zeroclaw_infra::session_queue`, which rejects or times out a second
  concurrent turn instead of running it), a turn's frames are written in order
  on the connection that owns it, and `done` wins at the end of the turn — so
  an estimate is always emitted before the `usage` it would otherwise
  contradict. What would break it: any future path that runs two turns on one
  session at once, or replays frames after a reconnect — a replayed old
  estimate would land on a newer provider number and the meter would jump
  backwards. Both need `turn_id` on `context_usage` and `history_trimmed` plus
  a per-turn monotonic guard on the client **before** they can be allowed.
  Until then the guarantee is "serialized turns, one connection, in order" and
  nothing stronger.

The local estimator over-counts CJK-heavy text (measured 0.44–0.95 tokens per
character against the flat 1.0 charge, depending on the BPE merges). That is
why every estimate carries a source label, and why the meter must keep showing
it: the number is a trigger and a progress indicator, never a bill.

## Troubleshooting the meter

- **The denominator looks wrong (too small).** The agent's `request_token_ceiling` is
  clamping the window. Check `[runtime_profiles.<alias>.context].request_token_ceiling`.
- **The denominator is `32,000`.** No provider `context_window` is set and the
  model id is not in the per-model table. Set an explicit
  `[providers.<id>].context_window`, or fix the model id so it matches a table
  entry.
- **Config fails to load with `reserve_tokens` / `trim_threshold_percent`.**
  These are hard validation errors, not clamps: `trim_threshold_percent` must be
  `1..=100`, and `reserve_tokens` must not exceed 50% of the resolved model
  window.

## Locked prompt / catalog decisions

These choices are intentional and should not be "fixed" without an ADR:

- **No runtime wall-clock injection.** The runtime does **not** inject the
  current date or time into the system prompt, channel turn-context preamble,
  channel history user turns, or embedded-agent user turns. Put calendar /
  timezone guidance in workspace instructions (`AGENTS.md`) or load it via
  tools when needed. This keeps the replayed prefix byte-stable for provider
  prompt caching. Display-only strip helpers may still recognize legacy
  `[CURRENT DATE & TIME: …]` prefixes in old transcripts.
- **`HEARTBEAT.md` is load-on-use.** It is **not** part of the interactive
  system prompt. The heartbeat engine reads it from disk when heartbeat is
  enabled; if the file is missing, that path is skipped. Agent startup does
  **not** unconditionally seed `HEARTBEAT.md` (the personality editor still
  exposes it for manual setup).
- **Skills and Deferred catalogs are session-frozen and name-sorted.** Within
  a session the catalog text is treated as a stable cacheable prefix. Rendering
  sorts skills by name and deferred MCP stubs by `prefixed_name` so catalog
  order does not depend on discovery order.
- **Deferred: multi-breakpoint / Project Context physical split.** Splitting
  Project Context across multiple physical prompt breakpoints is explicitly
  out of scope for now; keep a single Project Context block until that design
  lands.
- **Compact skills.** In compact injection mode, skill summaries stay in the
  system prompt; full skill bodies are loaded via `read_skill` into history,
  not re-inlined into the system prefix.

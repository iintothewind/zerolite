# Context management

> **Status: specification.** This page defines ZeroLite's context-management
> semantics. The code conforms to this page, not the other way round. Where
> upstream ZeroClaw differs, upstream is the one that is different — see the
> decision record (§1) and the merge guideline (§2) below.

## 1. Decision record: why these semantics

ZeroLite's context management is an independent design, not a modified copy
of upstream's. The two diverged from a common ancestor and evolved
separately; neither is a bug-fixed version of the other.
The semantics on this page rest on nine reasons. They are the actual grounds
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
  for one model and silently wrong for the next. The budget is therefore
  derived from the effective window (a percentage, a reserve), so it travels
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
  price only the not-yet-billed tail locally. The anchor spans turns: the
  measured count, the provider and model it was measured on, and the
  fingerprint of the population prefix it covers are recorded at each
  accepted turn, and the next turn prices its request as that measurement
  plus the conservative estimate of the rows appended after the prefix. A
  number the estimator produced is never used raw: every estimate the meter
  publishes and every trigger reads `estimate × 11/10`, because a raw
  estimate has never proven sufficient as a pre-send number (the estimator
  prices characters, not tokens, and the families these deployments bill
  have charged more than it counted; one tenth is the margin — see
  `context-meter.md` R5). The ratio-scaled estimate is not the
  carrier: multiplying a biased estimate by a reported/estimated ratio
  multiplies the estimator's bias into the dispatch upper bound, so a
  request the provider measured as fitting can be trimmed on a number the
  provider never accepted.
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
- **R8 — One retention semantic: `keep_recent_turns`.** The trim action has
  exactly one shape, at every trigger: keep the newest `keep_recent_turns`
  whole turns, then drop one more whole turn at a time while the retained set
  still exceeds the budget. There is no second retention rule — no
  "drop as few turns as will clear the budget", no per-dispatch nibble, no
  "shrink to a fraction of the current size" recovery variant. A minimal cut
  is the most expensive shape available: it leaves the population just under
  the line, so the next turn crosses it again and rewrites the provider prefix
  again under R1. One deep cut now, held for `keep_recent_turns` turns, is the
  cheaper and more predictable trade.
- **R9 — One number: the trim budget is the trigger and the fit target.**
  A separate "engage here, fit there" pair makes the retained population a
  function of two numbers whose relationship no operator can see, and it lets
  a trim legally stop above the number that will trigger the next one. There
  is one budget; crossing it triggers, and the same number bounds what
  survives.

The divergences that follow from these reasons, decision by decision:

| # | Upstream | ZeroLite (this page) | Grounded in |
| --- | --- | --- | --- |
| 1 | One resolved number, `context_token_budget`, serves as **both** the proactive-trim trigger and the fit target. It is absolute by default (`max_context_tokens`, else the 32k legacy default); `context_compact_ratio` can derive it from the window, but in every mode the same number does both jobs | One number too, but **derived from the effective window**: `min(window × trim_threshold_percent/100, window − reserve_tokens)`. It is the trigger and the fit target — and it re-derives when the model changes | R2, R3, R9 |
| 2 | No keep-N preference: `trim_to_recent_turns(budget)` keeps whatever fits the budget | `keep_recent_turns` **is** the retention: keep that many newest whole turns, then cascade down to `kept_turns == 1`. Every trigger uses that action, including the in-turn guard and overflow recovery | R1, R8 — a budget-only retention makes the retained history a pure function of the budget, and cuts to whatever barely fits |
| 3 | Five layered knobs on the runtime profile — `max_history_messages`, `max_context_tokens`, `context_compact_ratio`, `history_pruning.*`, `context_compression.*` — combined by a `min()` chain in which any one can become the binding constraint | **One** table, `[runtime_profiles.<alias>.context]`, four fields, each with exactly one job | R3 |
| 4 | `max_context_tokens = Some(0)` is a sentinel that disables proactive trimming (upstream's own comment: "Preserve zero as the explicit proactive-trimming disable sentinel") | No disable switch. Trimming is always on; every field has a non-zero default | R4 — a disabled trim silently lets the request exceed the window and fail at the provider |
| 5 | `max_context_tokens` can cap the *budget*, but the model window itself cannot be clamped by an operator ceiling, so the utilization meter's denominator is whatever the provider or table claims | `request_token_ceiling` clamps the effective window: `model_window.min(request_token_ceiling)`; the clamped window is also the meter denominator | Without a clamp, an operator whose provider reports an unknown or inflated window has no way to bound it, and the meter is dishonest |
| 6 | No output-reserve concept in context policy | `reserve_tokens` bounds the budget from above: `budget ≤ window − reserve` | Without a reserve, a successful trim can leave no room for the model's own output, and the request fails immediately after |
| 7 | No `ContextCalibration`. A reported-budget trim path exists (`trim_to_reported_budget`) that proportionally scales a local estimate by the reported/estimated ratio — but the proactive trigger is sized from the local estimate alone | `ContextCalibration` re-anchors on the provider-reported `usage.input_tokens`; only the unbilled tail is priced locally. The calibrated size feeds **both** the trigger measurement and the trim | R5 — proportional scaling preserves the local estimate's error; anchoring replaces it at the anchor point |
| 8 | A trim that cannot fit is a silent no-op: even with only the floor remaining and still over budget, the trim returns the oversized history unchanged with `trimmed: false` and no error | A floor that cannot fit the **model window** is a **hard failure**: an error to the caller, and **no** `HistoryTrimmed` notice | R4 |
| 9 | `context_compression.*`: summarization-based compaction rewrites older history into a model-written summary | Removed entirely. There is no summarization anywhere in the retention path, and no compaction subsystem to disable | R6, R7, R1 |

The practical consequence: **`[runtime_profiles.<alias>.context]` is the
only place context policy is configured.** A knob that appears in upstream
and not here was not overlooked; absent a new decision record that answers
R1–R9, it was rejected.

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
  invariants are that `[context]` is the only effective config surface, one
  resolved budget is both trigger and fit target, `keep_recent_turns` is the
  only retention rule (cascading to a floor of 1 and hard-failing there),
  trimming cannot be disabled, size is anchored by `ContextCalibration`, and
  no summarization compaction exists in the retention path.
- **An action must not survive a merge as a trigger.** The v0.8.6 merge
  (`1f379c6de`) swapped the post-loop re-sync for an upstream
  `Agent::trim_history` — keep-N with no trigger test — and hung it on the
  turn's success path. It compiled, and no test went red; the only symptom was
  a trim notice on *every* turn. The rule: a function that drops turns is an
  **action**; each of its call sites must be a trigger that can name its own
  evidence — the budget was crossed, or the loop itself dropped turns, or a
  provider rejected the request. A call site that cannot name its trigger does
  not belong there.

Context management in ZeroClaw is, at its core, **cache management**. Every
byte that changes in the message prefix replayed to a provider forces that
provider to re-prefill the prefix, which costs both latency and money. The
runtime therefore measures context against a single budget and prunes
history so the replayed prefix stays as byte-stable as possible: when
replayed context exceeds the budget, it retains the most recent
`keep_recent_turns` whole turns in one cut, then drops one turn at a time
further down only while the retained request still exceeds that same budget
(or hard-fails at the floor). The trigger is a token test; the action is
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
utilization meter (the Zerocode / web UI `ctx: used / max` bar), and the base
the trim budget is derived from:

```text
trim_budget = max(1, min(window × trim_threshold_percent / 100,
                         window − reserve_tokens))     # reserve term omitted when None
```

`trim_budget` is **one** number with two jobs: crossing it triggers a trim,
and a trim drives the retained population down to it (R9). It is never
compared against a second "fit" number, and the model window stays the hard
capacity behind it.

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
trim_threshold_percent = 80    # budget: where trimming engages (default: 80)
reserve_tokens = 16_384        # output headroom, bounds the budget from above (optional)
keep_recent_turns = 5          # whole turns retained when trimming fires (default: 5)
```

| Field | Type / default | Meaning |
| --- | --- | --- |
| `request_token_ceiling` | `Option<usize>`, default `None` | Hard ceiling on total request input tokens. `None` means the effective context window is the sole limit. This is the only override knob for providers whose real window is unknown. It is a **ceiling** and does **not** by itself trigger trimming. When set, it clamps the effective window (and thus the UI meter denominator) down via `model_window.min(request_token_ceiling)`. |
| `trim_threshold_percent` | `usize`, default `80` | Percentage of the effective context window that, with `reserve_tokens`, derives the **trim budget**. Must be in `1..=100`; `0` or `>100` is a hard config error. |
| `reserve_tokens` | `Option<usize>`, default `None` | Output headroom. Bounds the budget from above: `trim_budget ≤ window − reserve`. Must not exceed **50%** of the effective window — beyond that the reserve would strand most of the window, which is a hard config error checked against the resolved model window. |
| `keep_recent_turns` | `usize`, default `5` | How many of the newest **whole turns** the trim action retains when it fires. Clamped at resolution to `1..=10` (`0` becomes `1`, `>10` becomes `10`). If those turns still exceed the **trim budget**, the runtime drops one more turn at a time down to `kept_turns == 1`; it never drops the newest turn. |

All four have defaults, so an existing `config.toml` needs no changes to keep
working: omit the table entirely and you get `request_token_ceiling = None`,
`trim_threshold_percent = 80`, `reserve_tokens = None`, `keep_recent_turns = 5`.

## The trim budget

One number, two jobs (R9):

```text
trim_budget = min(window × trim_threshold_percent / 100,
                   window − reserve_tokens)      # reserve term omitted when None
```

- **Trigger** — the trim fires when replayed context tokens exceed
  `trim_budget`. Default percent is `80`.
- **Fit target** — the same value is the success criterion: the retained
  population (system/tool/skill prefix + retained turns) must fit under
  `trim_budget`. There is no second number to fit against, so "engage early,
  fit later" is not expressible — and not needed: the cut is deep enough that
  the next turn starts far below the line.

A derived budget of `0` is not reachable from a validated config
(`percent ∈ 1..=100`, and the planner takes `.max(1)`); the `0` case exists
only for stub routes that resolve no budget, where callers read it as
"no constraint" and measure against the model window.

When replayed context tokens exceed `trim_budget`, the trigger fires and the
trim action runs on **durable** agent/session history (not only a disposable
working copy): see the upstream book's *History management* page.
`HistoryTrimmed` carries optional `tokens_after` so clients can refresh the
context meter immediately after a **successful** trim.

### The same budget bounds the in-turn guard

The budget is evaluated **once per turn**, before the first dispatch. Every
dispatch after that — a tool loop re-sending after each result — is
re-measured against the same `trim_budget`.

That re-measure is not an optimisation. A turn grows between dispatches —
every tool result lands in the transcript — so a guard that fired only at
turn start would let the population climb toward the window with nothing
firing. The provider accepts the request, the model runs out of space
mid-reply, and the stream never terminates: no `finish_reason`, no error,
just an idle connection until the client's read bound expires.

The in-turn guard runs the **same action** as the turn-start check (R8): on
the first pass it cuts straight to the newest `keep_recent_turns` turns, and
on later passes it drops one more turn each time the rebuilt request still
exceeds the budget. It never drops a single turn to just clear the line —
that shape would change the provider prefix on nearly every turn.

**What the guard measures (R5, and the bug this rule exists for).** The
in-turn guard prices the population about to be dispatched as
`reported + margin(estimate of the rows appended since the reported request)`,
where `reported` is the `usage.input_tokens` of the call this turn already
made. Before any report exists — the first dispatch of a turn, or a route that
reports no usage — it prices `margin(estimate)`.

It must **not** scale the whole estimate by the observed reported/estimated
ratio. That shape keeps the estimator's bias inside the bound and prices the
tail with the same biased estimator, so a multi-step turn whose tool results
grow faster than the previous population did reaches the provider's window
while the guard still reports "fits" — the request goes out, and the trim lands
only after the response, i.e. one round trip too late. Anchoring on the
provider's number and margin-ing only the tail is the same rule the turn-start
trigger follows; the two paths cannot drift apart.

### How size is measured

Context size for the budget and the cascade uses two layers:

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

   The anchor is recorded per turn (a `ContextAnchorSink` on the turn loop)
   and read back at the next turn start, so a new turn's meter and
   budget check read `reported + conservative(estimate of the rows appended
   after the anchored prefix)` — the label is `calibrated` (see
   `context-meter.md` R4–R7). The fingerprint covers the provider-prefix
   rows as the provider saw them at accept, so the memory preamble and other
   per-iteration injections stay inside `reported_tokens`: they are
   over-counted, never priced twice, and never priced in the tail. The
   anchor is cleared by a trim, a history replacement starting inside the
   anchored rows, a model/provider switch, a config-generation change, and a
   tool-catalog change; it is not persisted, so a restored session starts on
   the conservative estimate. Non-streamed `Agent::turn`, nested SOP turns
   (`tools/delegate.rs`, `skills/review.rs`), and channel-gated turns carry
   no anchor: they publish no meter, so the UI number comes from streamed
   turns.

Optional `prompt_tokens_details.cached_tokens` (vLLM
`--enable-prompt-tokens-details`, OpenAI, etc.) maps to
`cached_input_tokens`. For OpenAI-compatible backends it is a **subset** of
`input_tokens` — used for cache hit / cost observability, **not** added into
context occupancy for trim or the meter. Absence of details is normal and must
not affect trim correctness.

## The trim action: keep-N, then cascade to the budget

The budget above is the **only** trim trigger. When it fires, the action is
one rule at every call site (`history_trim::trim_to_fit` /
`trim_conversation_to_fit`, and the in-turn guard):

| Stage | Behavior | Stop when |
|-------|----------|-----------|
| 1. Turns first | Keep the newest `keep_recent_turns` whole turns (default `5`, clamped `1..=10`) plus leading system (tools/skills live in that non-droppable prefix). One cut, not a nibble | The retained set is at the configured retention |
| 2. Budget cascade | Still over → drop **one more** whole turn at a time (never a gap in the middle, never the newest turn), re-measuring between drops. **Warn** when the chosen keep is below preferred | Retained tokens **≤ `trim_budget`**, **or** only `kept_turns == 1` remains to try |
| 3. Hard fail | Floor (system/tools/skills prefix + newest turn) still exceeds the **model window** | Abort the turn with an **error message only** (no `HistoryTrimmed` notice); never drop below one turn |

Why stage 1 is a cut and not a nibble is R1 + R8: the retained population is
what the next turns replay, so a minimal cut guarantees the very next turn
crosses the budget again and rewrites the prefix. Cutting to the operator's
working set keeps the prefix byte-stable for `keep_recent_turns` turns.

How stage 2 is decided does not matter to operators: the runtime may probe by
decrementing keep, binary-search, or estimate turn sizes once. The contract is
the outcome — configured retention first, whole turns only, floor 1.

### When the provider reports no usage

Two decision points read different evidence. Do not collapse them:

| Decision point | Evidence | Is the estimate the trigger? |
| --- | --- | --- |
| **Pre-send**, before a request leaves | the population about to be sent; no report for it can exist yet | **yes** — the budget check *is* the local estimate |
| **Post-response**, after a reply lands | `usage.input_tokens` for the request just sent | **no** — without a report the estimate is not a measurement of what was billed |

The budget trigger runs **pre-send**. Nothing has been sent, so no provider
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

- **does not trim proactively** — the post-response check stays inactive on the
  estimate alone, because the estimate is not a measurement of the request just
  sent;
- **only acts as an overflow guard** — when the estimate cannot fit the same
  `trim_budget` the next turn will be held to (`estimate > trim_budget`), the
  runtime trims and retries;
- **skips the guard when there is nothing to drop** — if only one whole turn
  remains, trimming cannot make the request smaller, so the request is
  dispatched rather than failed on a difference the trim cannot close.

A provider that reports usage keeps the full behaviour on both paths: pre-send
the budget triggers the trim, post-response the reported count anchors it, and
the same budget decides what still fits.

**The floor is the model's capacity, not the budget.** A retention that
misses the trim budget but still fits the **effective window** (the model window
clamped by `request_token_ceiling`) is a *soft floor*: the newest turn is
dispatched rather than rejected. Only a population the model cannot take is an
unsatisfiable floor.

**Capacity is compared in the same conservative units as the budget.** The
number the guard weighs against the window is the pre-send number (R5), not the
raw estimate, so a population whose conservative size exceeds the window fails
closed even when the raw estimate fits it exactly. That is deliberate: the raw
estimator is known to under-count (R5 exists because of it), so "the raw number
fits" is not evidence the model can take the request. The cost is availability —
a request within the margin of capacity is refused instead of sent — and the
trade is accepted because a rejected turn is diagnosable (a typed capacity
error), while an over-capacity request that the provider takes and then runs out
of room mid-reply is not.

**Settlement.** The cascade selects by summing per-message estimates, which is
only exact when the estimator is additive. A provider view that prices the
whole slice higher than the sum of its messages — per-call framing, one-time tool
schemas, attachment accounting — would otherwise keep more turns than actually
fit. After choosing a keep, the runtime therefore re-prices the **retained set
as a whole** and drops one more turn until the whole-slice estimate fits the
budget or only the floor remains. `tokens_after` reports the larger of the two
prices, so a settled retention is never reported as fitting when the
whole-slice view disagrees.

| | Value |
|---|---|
| Trigger | replayed context tokens `> trim_budget` (default 80% of the window, bounded by the reserve) |
| In-turn trigger | the same budget, re-measured before every dispatch after the first |
| Fit target | the same `trim_budget` |
| Preferred retain | `keep_recent_turns` newest whole turns, cut in one pass |
| Floor | `kept_turns >= 1` (newest whole turn always retained; system never dropped) |

Trimming is always whole-turn — a turn is never cut in half. An oversized newest
turn alone may still exceed `trim_budget`; if it still fits the model window
that is a soft floor (stage 2 stops, the request is dispatched), and only a
turn the window cannot take is a hard failure (stage 3) — never a license to
slice the turn.

## Pathological windows: the floor cannot fit

When the effective context window is so small that the **floor** — leading
system messages plus the one newest whole turn that trimming must always
retain — does not fit, there is **no path below keep=1**: the runtime never
truncates the system prompt and never drops the newest turn. It fails
explicitly instead, with these guards:

| Condition | Where | Behavior |
|---|---|---|
| System floor alone ≥ budget | turn start (`iteration == 0`) | **Warning only** (log `error_key: context_floor_exceeds_budget`): remediation points at raising the window / shrinking tools. Treated as a configuration problem — the turn may still proceed until cascade / provider overflow hard-fails. |
| Cascade to `kept_turns == 1` still exceeds the **model window** | pre-send / reported-budget / overflow recovery | **Hard failure**: error message to the caller (CLI / UI); **no** successful-trim `HistoryTrimmed` notice. |

The overflow-recovery path is reached when a provider rejects the request with
a context-window error (matched by `reliable::is_context_window_exceeded` across
patterns such as `"exceeds the context window"`, `"maximum context length"`,
`"prompt is too long"`). A rejection is its **own trigger**, because it carries
information the local estimate cannot see (an image's real cost, one-time tool
schemas): recovery runs the same keep-N action, but at least one step deeper
than the estimate — one turn fewer than is currently retained when the
configured retention would keep them all. The retry happens only when the
cascade actually dropped something and the retained population still fits the
model window; otherwise the floor failure applies.

In short: `keep_recent_turns` is the retain count; the cascade may
keep fewer turns (down to 1) to fit the budget. When the window cannot hold the floor, the
correct fixes are operational — raise
`[runtime_profiles.<name>].context.request_token_ceiling` (or the provider
`context_window`), disable unused integrations to shrink the tool surface, or
point the agent at a larger-window model.

## Token estimation: script-aware, not bytes

The local estimate that sizes the budget and the cascade is **script-aware**:
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

The action's shape is part of that: one cut to `keep_recent_turns`, not the
smallest cut that clears the budget (R8), so the prefix changes once and then
stays stable while the retained turns are replayed.

## Updating the meter

The meter's numerator is a *display* value, and it has exactly three update
points. They are ordered by increasing authority, and a later point always
overwrites an earlier one:

| When | Event | Source label | Why |
|---|---|---|---|
| A turn starts — after the budget decision, before dispatch | `context_usage` | `estimate` | The assembled request exists and no provider has seen it yet. Without this, the meter keeps showing the previous turn's number for the whole turn. |
| A trim fires mid-turn | `history_trimmed` → `tokens_after` | `estimate` | The history just got smaller. Waiting for the next `done` leaves the meter above 100% of what will actually be sent. |
| A provider call returns | `usage` → `input_tokens` | `provider`, or `estimate` when the provider is silent | The only measured number on the path. It replaces the assembled estimate. |
| The turn ends | `done` → `last_input_tokens` | `provider`, or the trim's own post-trim estimate when a trim followed the last call | The size the turn *ends* with. Authoritative for the turn, and it clears the numerator when the accepted route reported no usage. |

Three fields carry that budget across the wire, and with one budget they agree:

| Field | On | Is |
|---|---|---|
| `context_token_budget` (RPC) / `max_context_tokens` (WS) | `context_usage`, `usage`, `done` | The **trim budget**: `trim_threshold_tokens().max(1)`. Crossing it engages a trim, and it is the fit target. |
| `token_budget` | `history_trimmed` | The budget the guard that fired measured against: the trim budget for every budget trigger and for overflow recovery, the model window for the hard-limit path. It is a quoted figure, not a separate knob. |

The budget is never absent in practice — `trim_threshold_percent` is
validated to `1..=100` and `context_trim_budget()` takes `.max(1)`, so there is
no "trimming disabled" state that could make it `None`. The field is `Option`
only because it shares the `ContextUsage` wire shape.

Every number in that table prices the same population: the messages **plus the
tool definitions the request carries**. A provider bills `input_tokens` for
both, so a count that omits the schemas is short by the whole tool catalog. That
omission is not cosmetic — it made the meter drop mid-turn and it put the
effective trigger a catalog above the configured percentage, because the
trigger compared a messages-only estimate against a threshold derived from the
full window.

Where those definitions live depends on the route, and that has a trap for
anyone testing this. They are in the request's `tools` field only when the
provider advertises native tool calling
(`ProviderCapabilities::native_tool_calling`); otherwise the XML dispatcher
writes the descriptions into the system prompt, where the message estimate
already covers them. A fixture built on a provider that does not advertise it
therefore passes "the schema is counted" without the schema term doing anything
— which is exactly how the first version of this guard's test passed with the
term forced to zero. Any test asserting that the schemas are priced must use a
provider that advertises native tool calling.

Three rules follow from this and are not negotiable:

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
- **A trim invalidates the measured size, and the terminal frame must not
  resurrect it.** The post-response guard trims *after* the turn's final
  `usage` event — it needs that provider count to decide the trim is warranted
  — and `done` is written after both. Emitted in order, the trim's
  `tokens_after` moved the meter and `done` then put the pre-trim number back,
  so a completed trim looked like it had not happened at all. The gateway's
  `UsageFold::note_trim` replaces the accepted-input snapshot with
  `tokens_after`; a later accepted `usage` overwrites it with a measurement of
  a request sent *after* the trim, which is newer information about the same
  population and therefore wins. The ordering rule above is unchanged — what
  changed is that "`done` wins" is no longer true when a trim landed between
  the measurement and the terminal frame.

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
- **Trimming fires earlier than the percentage suggests.** `reserve_tokens`
  bounds the budget from above: the effective budget is
  `min(window × percent, window − reserve)`.
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

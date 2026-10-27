# Context management

> **Status: specification.** The code conforms to this page, not the other way
> round. Where upstream ZeroClaw differs, upstream is the one that is different
> — cite `§1 #N` or `R1–R9` in merge reports.

## 1. Decision record: why these semantics

ZeroLite's context management is an independent design, not a modified copy of
upstream's. Nine reasons; a change that cannot answer them has not made its case.

- **R1 — Cache economics come first.** Replaying the prefix is the dominant cost
  of a turn: every changed byte forces a re-prefill. Everything below is
  subordinate to keeping that prefix byte-stable — including the rule that no
  wall-clock or other per-turn noise enters it.
- **R2 — The window belongs to the model, not the config file.** The model can
  change mid-session (routing, a fallback, a user switch), so the budget is
  derived from the effective window (a percentage, a reserve) and re-derives
  with it.
- **R3 — One decision, one knob.** Two settings that can change the same outcome
  make incoherent combinations representable. One table, one job per field.
- **R4 — Fail loud.** A trim that did not happen must not look like one that
  did, and a request that cannot fit must fail where the cause is known. No
  disable switch, no silent no-op.
- **R5 — The provider's reported size is ground truth.** Sizing anchors on
  `usage.input_tokens` and prices only the unbilled tail locally; no local
  estimate is used raw — every pre-send number is `estimate × 11/10`
  (`context-meter.md` R5). A reported/estimated ratio is never multiplied into a
  bound: that carries the estimator's bias into the dispatch upper bound.
- **R6 — Retention by dropping, never rewriting.** Whole turns only.
  Summarization rewrites the record: it breaks the prefix (R1), inserts
  model-written text as if it were fact, costs a call, and compounds its errors
  invisibly.
- **R7 — No context compaction, in any form.** No summarization pass, no
  mid-turn compaction, no provider hook, no key that enables one. R6 forbids the
  rewriting; R7 records the mechanism as *absent*, so an upstream change that
  adds one is a new subsystem, not a restored default. Vocabulary: a shrinking
  history is a **trim**, its action is **dropping whole turns**, and no
  `compact_*` name exists for retention.
- **R8 — One retention semantic: `keep_recent_turns`.** Every trigger runs the
  same action: keep the newest `keep_recent_turns` whole turns, then drop one
  more at a time while the retained set still exceeds the budget. No minimal
  cut, no per-dispatch nibble, no "shrink to a fraction of the current size"
  recovery: a minimal cut leaves the population just under the line, so the next
  turn crosses it again and rewrites the prefix under R1.
- **R9 — One number.** The trim budget is both the trigger and the fit target.
  An "engage here, fit there" pair lets a trim stop above the value that
  triggers the next one, and makes the retained population a function of a
  relationship no operator can see.

Divergences, decision by decision (`#N` is the merge-report citation):

| # | Upstream | ZeroLite | Grounded in |
|---|---|---|---|
| 1 | One absolute `context_token_budget` for both trigger and fit | One number, but **derived from the window**: `min(window × percent/100, window − reserve)`; it travels with the model | R2, R3, R9 |
| 2 | `trim_to_recent_turns(budget)`: keep whatever fits | `keep_recent_turns` **is** the retention; every trigger uses it, including the in-turn guard and recovery | R1, R8 |
| 3 | Five layered knobs combined by a `min()` chain | One table, `[runtime_profiles.<alias>.context]`, four fields | R3 |
| 4 | `max_context_tokens = 0` disables proactive trimming | No disable switch; trimming is always on | R4 |
| 5 | The model window cannot be clamped by the operator | `request_token_ceiling` clamps it, and the clamped window is the meter denominator | honesty of the meter |
| 6 | No output-reserve concept | `reserve_tokens` bounds the budget from above | a trim must leave room for the reply |
| 7 | Ratio-scaled `trim_to_reported_budget`; the proactive trigger sizes from the estimate alone | `ContextCalibration` anchors on the report; only the tail is estimated, and the anchored size feeds trigger and trim | R5 |
| 8 | An un-fittable floor is a silent no-op | A floor the **model window** cannot take is a hard failure: an error, and no `HistoryTrimmed` | R4 |
| 9 | `context_compression.*` rewrites history into a summary | Removed entirely | R6, R7, R1 |

**`[runtime_profiles.<alias>.context]` is the only place context policy is
configured.** A knob that exists upstream and not here was rejected, not
overlooked.

## 2. Merge guideline

Two lanes, decided by whether ZeroLite has decided the question before:

- **A prior decision covers it → apply it directly.** Decisions in §1, in "Locked
  prompt / catalog decisions", or in any recorded fork decision are precedent.
  The upstream change does not enter; do not "adapt" fork code to upstream
  semantics, and do not re-introduce an upstream knob under a fork name. One line
  in the merge report (commit, file, `skipped per §1 #N`) is the procedure.
- **No prior decision covers it → report it.** New semantics (a knob, a retention
  mechanism, a failure behavior) is not the merge agent's to accept. Report the
  commit, the semantics, and what adopting it would change; the maintainer
  decides, the outcome is written into §1, and it becomes precedent. Unclear
  whether a decision covers it? Treat it as new and report — silence is not a
  decision.

What counts as semantics: the config surface and each field's meaning; the
trigger and the fit target; the retention policy; failure behavior at the floor;
how size is measured; and every locked decision. The surface to watch (paths as
of writing; the semantics are the invariant):
`crates/zeroclaw-config/src/scattered_types.rs` (`ContextConfig`),
`.../schema.rs` (resolution, validation), `.../context_window.rs`,
`crates/zeroclaw-runtime/src/agent/history.rs` (estimates),
`.../agent/history_trim.rs`, `.../agent/turn/context*.rs`. Anything touching no
semantics — bugs, providers, channels, security, performance, even inside the
same files — merges freely.

**Post-merge check.** A surviving type, table or test proves nothing; this has
already happened once: `ContextConfig` still parsed and its unit tests passed
while no runtime code read it. Verify the resolution path is consumed, and that:
`[context]` is the only effective surface; one budget is both trigger and fit
target; `keep_recent_turns` is the only retention rule, cascading to a floor of
one and hard-failing there; trimming cannot be disabled; size is anchored by
`ContextCalibration`; no summarization exists in the retention path.

**An action must not survive a merge as a trigger.** The v0.8.6 merge
(`1f379c6de`) swapped the post-loop re-sync for an upstream `Agent::trim_history`
— keep-N with no trigger test — on the turn's success path. It compiled, no test
went red, and the only symptom was a trim notice on *every* turn. A function that
drops turns is an **action**; each call site must be a trigger that can name its
evidence — the budget was crossed, the loop dropped turns, or a provider rejected
the request.

## The effective window and the budget

Everything trims against one number, the **effective context window**, resolved
first-match-wins: explicit `[providers.<id>].context_window`; the built-in
per-model table (a conservative lower bound, so an unconfigured provider never
falls back to a stub); `32_000` as "not configured", not as capacity.

```text
effective_context_window = model_window.min(request_token_ceiling)   # None ⇒ no clamp
trim_budget              = max(1, min(window × trim_threshold_percent / 100,
                                       window − reserve_tokens))      # reserve omitted when unset
```

The window is the meter denominator and the base the budget derives from. The
budget is one number with two jobs: crossing it triggers a trim, and a trim
drives the retained population down to it (R9). The model window stays the hard
capacity behind it.

## Configuration

```toml
[runtime_profiles.coder.context]
request_token_ceiling = 100_000    # optional ceiling on request input tokens
trim_threshold_percent = 80        # default 80
reserve_tokens = 16_384            # output headroom, optional
keep_recent_turns = 5              # default 5 (clamped 1..=10)
```

| Field | Meaning |
|---|---|
| `request_token_ceiling` | Clamps the effective window (`model_window.min(ceiling)`), and therefore the meter denominator. A **ceiling**, never a trigger. |
| `trim_threshold_percent` | Percentage of the window in the budget. `1..=100`; `0` or `>100` is a hard config error. |
| `reserve_tokens` | Output headroom; bounds the budget from above (`budget ≤ window − reserve`). At most 50% of the window — beyond that it strands the window, a hard error. |
| `keep_recent_turns` | Whole turns retained when a trim fires. Clamped, not validated: `0→1`, `>10→10`, `None→5`. |

Validation is loud: `trim_threshold_percent` and `reserve_tokens` are hard errors
when out of range, never silent clamps. An unset budget cannot happen in
practice (`percent ∈ 1..=100` plus `.max(1)`); the `0` case exists only for stub
routes that resolve no budget, where callers measure against the model window.

## How a trim works

One trigger (replayed tokens `> trim_budget`) and one action, at every site:

| Stage | Behavior | Stop when |
|---|---|---|
| 1. Retention | Keep the newest `keep_recent_turns` whole turns, plus the non-droppable system/tools prefix, in **one cut** | at the configured retention |
| 2. Cascade | Drop **one more** whole turn at a time, re-measuring between drops; warn when the keep is below preferred | retained ≤ `trim_budget`, or `kept_turns == 1` |
| 3. Floor | The floor (system prefix + newest turn) still exceeds the **model window** | hard failure: error only, no `HistoryTrimmed` |

Trimming is whole-turn: a turn is never split, and the newest turn is never
dropped. An oversized newest turn that still fits the window is a *soft floor* —
dispatched, not failed. Trimming is always on (R4), and "replayed context" means
the messages **plus the tool definitions** the request carries; pricing only the
messages put the effective trigger a whole catalog above the configured
percentage.

**Capacity is compared in the same conservative units as the budget.** The
number weighed against the window is the pre-send number (R5), so a population
whose margined size exceeds the window fails closed even when the raw estimate
fits it exactly. The raw estimator is known to under-count, a typed capacity
error is diagnosable, and a mid-reply stall is not; the cost is availability, and
it is accepted.

## What the guard measures

Size is a local **script-aware** estimate (Latin ≈4 chars/token, CJK/kana/Hangul
≈1 char/token, ~4 framing tokens per message), corrected by the provider: after
an accepted response, `ContextCalibration` records `usage.input_tokens` plus the
fingerprint of the rows it covers, and the next turn prices
`reported + margin(tail)`. The anchor is cleared by a trim, a history
replacement inside it, a model/provider switch, a config-generation change and a
tool-catalog change; it is not persisted, so a restored session starts on the
local estimate. Details and labels: `context-meter.md`.

The **in-turn guard** re-measures before every dispatch and prices the same way:
`reported + margin(tail)` when the route's report applies and the population is
still a superset of the rows it measured, else `margin(estimate)`. It never
scales the whole population — or the meter — by a ratio: that prices a growing
tail with the estimator's own bias, which is how a multi-step turn reaches the
window while the guard still reports "fits".

Any test asserting that the tool schemas are priced must use a provider that
advertises native tool calling; otherwise the dispatcher writes the descriptions
into the system prompt and the assertion passes with the schema term zeroed.

## When the provider reports no usage

| Decision point | Evidence | Is the estimate the trigger? |
|---|---|---|
| Pre-send, before a request leaves | the population about to be sent | **yes** — no report for it can exist yet, so the local estimate (with its margin) is the only evidence |
| Post-response, after a reply lands | `usage.input_tokens` for the request just sent | **no** — without a report the estimate is not a measurement of what was billed |

On the post-response path with no report the runtime does **not** trim
proactively; it only acts as an overflow guard when the estimate cannot fit the
budget the next turn will be held to, and it skips the guard when only one whole
turn remains (trimming could not make the request smaller). With a report, the
pre-send budget triggers, the reported count anchors, and the same budget decides
what fits.

## The meter

The numerator is a display value with four update points, each overwriting the
previous:

| When | Frame | Label |
|---|---|---|
| A trim fires | `history_trimmed` → `tokens_after` | `estimate` |
| A turn starts, after the budget decision | `context_usage` → `input_tokens` | `estimate`, or `calibrated` when an anchor prices it |
| A provider call returns | `usage` → `input_tokens` | `provider`, or `estimate` when the provider is silent |
| The turn ends | `done` → `last_input_tokens` | `provider`, or the trim's post-trim estimate when a trim followed the last call |

The client folds all four in one place, `web/src/contexts/contextMeter.logic.ts`
(tested beside it). The trim row has a deadline: it lands *before* the request is
dispatched, which is why the runtime publishes the re-read post-trim estimate on
`context_usage`, and a test asserts the two numbers are equal.

The route snapshot — `max_context_tokens` (`context_token_budget` on RPC),
`model_context_window`, `effective_context_window` — rides on **all four** frames,
not only on `done`. A turn can switch models mid-flight (a tool asks for another
route) and keep running, so the ring must adopt the new route's `limit` and
`trim budget` from the first post-switch frame; otherwise it shows the previous
route's numbers until the turn ends while the guard has been budgeting against
the new one. A frame that carries none of the three (an older daemon) means "no
route information here", not "no window": the client keeps the snapshot it has.
`history_trimmed`'s `token_budget` is the one exception in kind — it quotes
whichever budget the guard that fired measured against (the model window for the
hard-limit path), which is a figure, not a knob. Non-negotiable:

- **`context_usage` is not `usage`.** `usage` means "a call completed" and closes
  a client step; the assembled estimate means "a request is about to go out".
  Folding the latter would invent a step that never happened.
- **Ordering, not identity, keeps a stale estimate from overwriting a newer
  measurement.** Frames carry no turn id; what holds is "serialized turns (the
  per-session actor queue), one connection, in order". Running two turns on one
  session concurrently, or replaying frames after a reconnect, would break it and
  needs `turn_id` plus a per-turn monotonic client guard first.
- **A trim's post-trim number survives `done`.** The gateway's
  `UsageFold::note_trim` replaces the accepted-input snapshot with
  `tokens_after`; a later accepted `usage` (a request sent *after* the trim) is
  newer information about the same population and wins. "`done` wins" therefore
  has this one exception.

## Token estimation

Script-aware, not bytes: Latin ≈4 chars/token, CJK/kana/Hangul one token each
(they tokenize near 1:1 with common BPE families), plus per-image and framing
charges. The estimator over-counts CJK-heavy text against real BPE merges, so the
number is a trigger and a progress indicator, never a bill.

## Cache-economics feedback

Sends keep the longest byte-stable prefix (an append-only log rewrites only the
tail past the first divergent message). Cacheable prefix length is logged at
`DEBUG`, and provider-reported cache hits surface as
`zeroclaw_tokens_cached_input_total` (labels `model_provider`, `model`) and the
span attribute `zeroclaw.usage.cached_input_tokens`; providers that do not report
the field stay at zero. The action's shape is part of this: one cut to
`keep_recent_turns`, not the smallest cut that clears the budget (R8).

## Troubleshooting

- **Denominator too small** — `request_token_ceiling` is clamping the window.
- **Denominator is `32,000`** — no provider `context_window` and no per-model
  table entry: set the provider window or fix the model id.
- **Trimming earlier than the percentage** — `reserve_tokens` bounds the budget
  from above.
- **Config refuses to load** — `trim_threshold_percent` must be `1..=100` and
  `reserve_tokens` at most 50% of the window; both are hard errors, not clamps.

## Locked prompt / catalog decisions

Intentional; do not "fix" without an ADR.

- **No runtime wall-clock injection** into the system prompt, channel preamble,
  channel history turns, or embedded-agent turns — the prefix stays byte-stable
  (R1). Put calendar/timezone guidance in `AGENTS.md` or load it with a tool.
  Display-only strip helpers may still recognise legacy
  `[CURRENT DATE & TIME: …]` prefixes.
- **`HEARTBEAT.md` is load-on-use** — not part of the interactive prompt; the
  heartbeat engine reads it when enabled and skips a missing file. Startup does
  not seed it.
- **Skills and Deferred catalogs are session-frozen and name-sorted** (skills by
  name, deferred MCP stubs by `prefixed_name`), so catalog order does not depend
  on discovery order.
- **Compact skills** — summaries stay in the system prompt; full bodies load via
  `read_skill` into history, never re-inlined into the prefix.
- **Deferred: multi-breakpoint / Project Context physical split** — one Project
  Context block until that design lands.

# Context meter and usage estimation

> **Status: specification.** This page defines what the context meter
> measures and what happens when the provider reports no usage. It is a
> companion to `context-management.md`, which defines the trim semantics.
> For the merge rule, see `docs/merge_update_rules.md`. Decisions
> here are precedent: cite them as `context-meter #N` in merge reports.

## Decision record: why this shape

- **R1 — The meter denominator is the effective window, never the trigger.**
  The ring's contract says "effective context window". Rendering the
  water-line (the token count at which trimming engages) as the limit
  understates the window and makes the meter lie — the exact bug that
  shipped once (ring showed 104857 where the model side allows 131072).
- **R2 — A ceiling must be representable.** A configured
  `request_token_ceiling` scales the water-line but is neither the raw
  model capacity nor the water-line. The runtime therefore publishes the
  clamped window (`model_window.min(request_token_ceiling)`) as
  `effective_context_window` on the usage event, the WS done frame, and
  the RPC context-usage surface. Every consumer reads the same number.
- **R3 — No usage report is not a zero.** When the provider omits token
  counts, the meter falls back to the local script-aware estimate instead
  of reporting 0 or freezing. An absent number degrades the meter; it
  must not zero it.
- **R4 — The turn-start meter carries the provider's last measurement.**
  Re-estimating the population from bytes at every turn start makes the
  meter jump down: GLM-5.2 billed 25,787 for a population the estimator
  counted as 20,826 (ratio 0.808), so the next turn started ~20% under
  the number the provider had just charged for the same rows. The
  runtime records a **context anchor** per turn — the provider-reported
  input count, the provider name and model it was measured on, the
  fingerprint of the population prefix it covers, and the tool-schema
  size it covers — and prices a new turn as `reported +
  conservative(estimate of the rows appended after the anchored
  prefix)`. Mid-session, the meter never falls below the provider's last
  measurement.
- **R5 — The estimate the meter publishes is conservative.** Every
  estimate the meter publishes — turn-start meter, water-line trigger,
  trim-notice `tokens_before` / `tokens_after` — is `estimate × 5/4`
  (`conservative_estimate_tokens`). A raw estimate has never proven
  sufficient as the pre-send number: GLM-5.2 billed 1.24× what the
  estimator counted, and the margin factor covers the observed bias. The
  margin biases the water-line toward trimming early — the safe
  direction, since trim is cheap and missing the provider's real size is
  not — and keeps the dispatch guard's upper bound from under-committing.
  The estimator heuristics themselves are unchanged; the margin lives at
  the consuming sites.
- **R6 — Every published meter number carries its face.** Three sources
  exist: `provider` (the provider's own usage report), `estimate` (a
  local conservative estimate), `calibrated` (a provider measurement
  plus the estimated tail). The label rides on `ContextEstimate`, the WS
  `context_usage` frame (field `input_tokens_source`, defaulting to
  `estimate` for senders that omit it), and the RPC context-usage
  payload; the web ring shows it in its detail panel. `ContextEstimate`
  frames never claim `provider`. The billing surface (`tokenStats`)
  stays provider-driven: cost numbers must not be fed by meter numbers.
- **R7 — The anchor dies with the population it measured.** A trim, a
  history replacement starting inside the anchored rows, a model/provider
  switch, a config-generation change, and a tool-catalog change all
  clear the anchor; a fresh `Agent` (session restore) has none. After a
  clear the meter falls back to the conservative estimate labelled
  `estimate`, and the water-line follows it. The anchor is not persisted
  across sessions.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | The wire carries a single budget number; consumers reuse it as both trigger and denominator | Three distinct published numbers: effective window (denominator), water-line (trigger), send budget (fit target). The wire field `max_context_tokens` carries the water-line — it is not a limit and must never be rendered as one | R1, R2 |
| 2 | No published effective window | `effective_context_window` is published on usage events, WS done frames, and the RPC context-usage surface | R2 |
| 3 | No estimation fallback specified | Provider omits usage → local estimate (script-aware, see `context-management.md` § token estimation) | R3 |
| 4 | The turn-start meter re-estimates the population from bytes, so it jumps down at every turn start | The turn-start meter is the anchor (provider measurement + conservative tail) or the conservative estimate; the raw estimator stays as the pre-anchor face and as the dispatch guard's ratio input | R4, R5 |

## Merge notes

- **Verify post-merge by behavior:** set a ceiling below the model window
  and confirm the ring denominator equals the clamped window, not the
  water-line and not the raw model capacity; run a provider that omits
  usage and confirm the meter shows an estimate, not 0.
- **Field rename (history, not a decision):** `max_input_tokens` was
  renamed to `request_token_ceiling` because it is a ceiling on request
  input tokens, not a window. A merge that reintroduces the old name or
  reinterprets the ceiling as a window conflicts with R1/R2.
- **Code anchors:** `crates/zeroclaw-config/src/scattered_types.rs`
  (`ContextConfig.request_token_ceiling`),
  `crates/zeroclaw-config/src/schema.rs` (`effective_context_window`,
  `context_send_budget`), web `contextBar` / `historyTrimNotice`.
- **Verify post-merge by behavior:** run two turns where the provider
  reports 25,787 input tokens on the first: the second turn's meter must
  sit at or above 25,787, labelled `calibrated`; the usage frame reports
  the provider's own number; switch the model route and confirm the next
  turn is labelled `estimate`; fire a trim and confirm the notice's
  `tokens_before` equals the number that triggered it.
- **Code anchors (anchor layer):**
  `crates/zeroclaw-runtime/src/agent/history_trim.rs` (`ContextAnchor`,
  `provider_prefix_fingerprint`, `conservative_estimate_tokens`),
  `agent.rs` (`calibrated_turn_start_size`, `context_anchor`),
  `turn/outcome.rs` (`ContextAnchorSink`),
  `crates/zeroclaw-api/src/agent.rs` (`TokenCountSource`), web
  `ContextRing.tsx` (source row).
- **Meter regression tests:** `cargo test -p zeroclaw-runtime --lib --
  --test-threads=1 meter` runs the anchor matrix; `cargo test -p
  zeroclaw-gateway --lib -- --test-threads=1 context_estimate` and
  `cargo test -p zeroclaw-api --lib -- --test-threads=1 estimate` the
  wire labels; `npm --prefix web test` the ring source row. These are
  fork tests and must survive the merge.
- **Tests:** the meter-denominator tests and the estimate-fallback tests
  are fork tests; they must survive the merge.

## Sources

- Fork commits `06a229f69`, `01a0a2fe3` (estimated usage on the meter,
  estimate when the provider omits counts), `22ec17f5e` (publish the
  effective window, rename the ceiling).
- 2026-10 session diagnostics: a turn billed 25,787 input tokens; the
  next turn's turn-start estimate published 20,826 (byte estimate of the
  same rows, ratio 0.808) and the meter fell ~20% below the number just
  charged; the anchor + 5/4 margin (20,826 × 5/4 = 26,033 ≥ 25,787) is
  the fix.

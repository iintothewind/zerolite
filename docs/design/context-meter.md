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

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | The wire carries a single budget number; consumers reuse it as both trigger and denominator | Three distinct published numbers: effective window (denominator), water-line (trigger), send budget (fit target). The wire field `max_context_tokens` carries the water-line — it is not a limit and must never be rendered as one | R1, R2 |
| 2 | No published effective window | `effective_context_window` is published on usage events, WS done frames, and the RPC context-usage surface | R2 |
| 3 | No estimation fallback specified | Provider omits usage → local estimate (script-aware, see `context-management.md` § token estimation) | R3 |

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
- **Tests:** the meter-denominator tests and the estimate-fallback tests
  are fork tests; they must survive the merge.

## Sources

- Fork commits `06a229f69`, `01a0a2fe3` (estimated usage on the meter,
  estimate when the provider omits counts), `22ec17f5e` (publish the
  effective window, rename the ceiling).

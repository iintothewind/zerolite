# ZeroLite feature list

> **Purpose.** The single index a merge agent reads before touching
> upstream code. Every fork feature maps to exactly one design doc in
> `docs/design/`. If a feature is not listed here, it has no recorded
> decision — treat upstream changes to it as new semantics and report
> (see `docs/merge_update_rules.md`).
>
> **Baseline.** Built from the `zerolite` branch at `1.2.2`
> (`5f8567576`, 2026-10-22). Re-verify this list against the branch
> before the first merge-update run if the branch has moved since.
>
> **Canonical location.** `docs/design/` is the only home for design
> decisions. There is no published mirror: the former copy under
> `docs/book/` was deleted, so a design doc is the single place a decision
> is written and read.

## Features

| Feature | Design doc | Code anchors | Tests that must survive | Merge watch |
|---|---|---|---|---|
| Context management (water-line vs. send budget, keep-N cascade, calibration, no compaction, no disable switch) | `docs/design/context-management.md` | `ContextConfig`, `ResolvedRuntime` resolution, `history_trim`, `context_pipeline`/`context_recovery` | Config schema + history_trim + context tests | Any upstream budget/trim/compaction change |
| Context meter & usage estimation | `docs/design/context-meter.md` | `request_token_ceiling`, `effective_context_window` publication, `contextBar` | Meter-denominator + estimate-fallback tests | Meter/wire-field changes; the wire `max_context_tokens` is the water-line, not a limit |
| Session search + anchored history reads | `docs/design/session-search.md` | `crates/zeroclaw-tools/src/sessions.rs` (`session_search`, `around_message_id`) | Session-search + anchored-read tests | Sessions-tool or FTS `search()` contract changes; ownership scoping must survive |
| Master pair code (gateway recovery credential) | `docs/design/master-pair-code.md` | `GatewayConfig`, `api_pairing`, daemon gateway | `crates/zeroclaw-config/tests/master_pair_code.rs`, paircode-box width test | Pairing-endpoint, daemon-hosting, or config-schema changes — verify **end to end** (page + daemon path), not just unit tests |
| Prompt-cache prefix stability | `docs/design/prompt-cache-prefix.md` | Tool-spec sort, skill/catalog sort + freeze, append-only send log, `llm_request` fingerprint | Prompt/catalog ordering tests | Any prompt-construction, registration-order, or catalog-rendering change — a "cosmetic" reorder is a cache regression |
| Gateway session robustness | `docs/design/gateway-session.md` | `crates/zeroclaw-gateway/src/`, daemon gateway | Gateway session scenario tests | Gateway/session refactors; verify by scenario (disconnect mid-turn, POST after delete) |
| Delegation & turn semantics | `docs/design/delegation-turn.md` | Turn/delegate settlement, peer inbox | Turn-cancellation + settlement-recovery + peer-inbox tests | Runtime/turn refactors that move turn ownership |
| Web UI fork behavior | `docs/design/webui.md` | `web/src/lib/contextBar.ts`, `historyTrimNotice.ts`, transcript handling, zerocode keymap | `*.test.ts` beside each web logic file | Transcript-store, context-bar, or branding changes |
| Provider compatibility shims | `docs/design/provider-compat.md` | `crates/zeroclaw-providers/src/`, seam sanitizers, media handling | Per-provider shim tests | Provider refactors that delete/rename a shim or its tests |
| Fork identity & release | `docs/design/fork-identity.md` | `Cargo.toml`, `README.md`, `.github/workflows/`, `docs/releases/` | Release-workflow assertions (where present) | Version/README/workflow changes; restored upstream automation |

## Explicitly undecided — not precedent

The following entered the tree through the full upstream merge, not
through a fork decision. The maintainer has **not** accepted them as
ZeroLite semantics. Do not cite them as precedent, and do not treat
their presence as approval. If a future merge touches them, report it.

- Upstream context-compression anchor (`#9535` territory, overlapping
  the fork's `#9453` meter work) — conflicts with
  `context-management.md` R6 (no summarization) until decided.
- Turn parity over RPC (`#11132`).
- The plugin cluster (incl. WASM plugin host in release artifacts).
- The OIDC cluster.

## Merge constraints (not features, but binding)

- One test command at a time; `--test-threads=1`; known shared-state
  flaky set — see `AGENTS.md`. Gateway/channel parallel failures are
  concurrency limits, not flakes to retry away.
- No parallel subagents on this repo (provider rate limiting) — see
  `AGENTS.md`.
- Post-merge verification is per-feature behavior (this table's
  "Tests" + each design doc's "Merge notes"), never a commit count or
  a commit-presence check: merge timelines are scrambled and a present
  commit does not prove a surviving behavior (see the
  `ContextConfig`-orphan case in `context-management.md` §2).
- Post-merge docs: after merge tests pass, update every design doc and
  this list touched by a conflict. A merge is not complete
  until its docs are updated; a new decision written back here becomes
  precedent for the next merge.

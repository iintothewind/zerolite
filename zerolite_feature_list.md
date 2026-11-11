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
| Session search + anchored history reads | `docs/design/session-search.md` | `crates/zeroclaw-tools/src/sessions.rs` (`session_search`, `around_message_id`, `include_tools`), `crates/zeroclaw-infra/src/session_fts.rs`, `crates/zeroclaw-infra/src/session_sqlite.rs` (`append_turn`, `load_context`, `set_context_start`), session tool registration in `crates/zeroclaw-runtime/src/tools/mod.rs` | Session-search + anchored-read tests; `session_sqlite`/`session_fts` transcript tests | Sessions-tool or FTS `search()` contract changes; FTS schema / `sessions` columns; ownership scoping must survive |
| Master pair code (gateway recovery credential) | `docs/design/master-pair-code.md` | `GatewayConfig`, `api_pairing`, daemon gateway | `crates/zeroclaw-config/tests/master_pair_code.rs`, paircode-box width test | Pairing-endpoint, daemon-hosting, or config-schema changes — verify **end to end** (page + daemon path), not just unit tests |
| Prompt-cache prefix stability | `docs/design/prompt-cache-prefix.md` | Tool-spec sort, skill/catalog sort + freeze, append-only send log, `llm_request` fingerprint | Prompt/catalog ordering tests | Any prompt-construction, registration-order, or catalog-rendering change — a "cosmetic" reorder is a cache regression |
| Gateway session robustness | `docs/design/gateway-session.md` | `crates/zeroclaw-gateway/src/`, daemon gateway | Gateway session scenario tests | Gateway/session refactors; verify by scenario (disconnect mid-turn, POST after delete) |
| Delegation & turn semantics | `docs/design/delegation-turn.md` | Turn/delegate settlement, peer inbox, empty-completion nudge (`turn/empty_nudge.rs`), `is_semantically_empty_text`, runtime-written turn messages as prompt carriers / runtime rows (R6) | Turn-cancellation + settlement-recovery + peer-inbox tests + empty-completion nudge tests + R6 machinery tests | Runtime/turn refactors that move turn ownership; upstream changes to `is_semantically_empty_terminal` or the semantic-empty error arm of the turn loop |
| Web UI fork behavior | `docs/design/webui.md` | `web/src/lib/contextBar.ts`, `historyTrimNotice.ts`, transcript handling, zerocode keymap, gateway CSP `img-src` | `*.test.ts` beside each web logic file; `security_headers` CSP test | Transcript-store, context-bar, branding, or dashboard CSP changes |
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
  The v0.8.7 plugin fixes (#11807, #11808, #11809, #11811, #11812,
  #11814, #11851) and the later plugin-host fixes (#11848, #12040,
  #11855) were cherry-picked on the owner's approval as bug/security
  fixes; this does not make the plugin cluster a decided feature.
- Memory cluster. #11144 (credential-URL scan) and #11143 (qdrant time
  bounds) were cherry-picked on the owner's approval (2026-11-09) as
  hygiene fixes; this does not make memory a decided feature. #11225
  (`c54e002a8`, owner kept through delegation) was attempted 2026-11-09
  and dropped: its production blocks in `agent/agent.rs`
  (`rebind_memory_tools`) and `tools/delegate.rs` (delegate memory
  resolution) conflict with fork lines. #11266 (`72167d12e`, private
  subagent/pipeline memory) was attempted and dropped: every conflict
  block in `agent/agent.rs` (6) and `agent/turn/mod.rs` (12) holds fork
  lines. Both need a memory design decision first; #11266 also needs the
  composition API (#11174, not taken).
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
- Cargo.lock is resolved per pick against the fork's lock, never copied
  from upstream; only the versions a pick requires change, all other
  fork pins stay.
- Upstream context-management changes are not taken (owner decision
  2026-10-27, applied to v0.8.7): a pick that touches context management
  is left unmerged and reported (see `context-management.md`).
- Tests that need inputs the fork's `zeroclaw-test` image cannot supply
  are removed, not kept failing (owner decision 2026-11-02): the upstream
  late ToolSearch namespace tests (`r5_namespace_tests.rs`
  `late_tool_search_*`, which need upstream-CI fixture receipts) with their
  test-only `late_tool_search_observer` hooks in `tools/mod.rs`,
  `tools/scoped.rs`, `tools/skill_tool.rs` and `skills/mod.rs`, and the
  `expect`-driven PTY test
  `daemon_feedback_follows_foreground_job_control_transitions`. An
  upstream hunk that re-adds them is dropped.
- Upstream `028c50f5e` (#11925, container CA stores) is not taken
  (2026-11-09): it changes Dockerfiles, container CI and their TLS
  fixtures, and the fork publishes no container image; the Rust side is
  already on `webpki-roots` 1.0.10.
- Upstream #11901 (gateway connection drain), #11900 (concurrent store
  open), #11523 (settle turns a killed process left running) and #11822
  (POSIX command splitting) were taken 2026-11-09 as hardening; no fork
  semantics touched (`gateway-session.md` rows 8–9). #11507 (`fcc23a22f`)
  was reported and left out: it rewrites the turn loop in
  `agent/turn/mod.rs` where the fork's in-turn context guard lives, and
  writes the fork's trim breadcrumb.
- Small security/Windows fixes taken 2026-11-09 as class (a): #12114
  (X-Forwarded-For rate-limit key), #12177 (MCP tool listing sorted by
  name, under `prompt-cache-prefix.md` R2), #11559 (bubblewrap rejection
  reasons), #11656 (configured `firejail_args` applied), #11400 (model
  cache replaced while a reader holds it, Windows), #11398 (JSONL session
  rewrite replaced while a reader holds it, Windows).
- The test `acp_session_store::tests::legacy_null_trim_breadcrumb_is_inferred_once_then_recorded`
  is removed (owner decision 2026-11-09): it required inferring a legacy
  `NULL` trim-breadcrumb from message text, which contradicts the fork's
  design (provenance is a canonical column, never inferred from text; see
  `trim_breadcrumb_provenance_is_a_canonical_column_not_inferred_from_text`).
  A merge or test restore that re-adds it is dropped.

# ZeroLite feature list

The index a merge agent reads before touching upstream code
(`docs/merge_update_rules.md` §1). Every fork feature maps to exactly one
design doc in `docs/design/`, the only home for design decisions. A feature
not listed here has no recorded decision: treat an upstream change to it as
new semantics and report it.

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

These entered the tree through the full upstream merge, not through a fork
decision. The maintainer has **not** accepted them as ZeroLite semantics: do
not cite them as precedent, and report any merge that touches them. A fix
cherry-picked inside a cluster (see the pick log) does not make the cluster a
decided feature.

- Upstream context-compression anchor (`#9535` territory, overlapping the
  fork's `#9453` meter work): conflicts with `context-management.md` R6 (no
  summarization) until decided.
- Turn parity over RPC (`#11132`).
- The plugin cluster, including the WASM plugin host in release artifacts.
- The memory cluster. The ownership picks (#11225, #11266) need a memory
  design decision first; #11266 also needs the composition API (#11174, not
  taken).
- The OIDC cluster.

## Standing merge decisions

Binding on every merge, on top of `AGENTS.md` (one test command at a time,
`--test-threads=1`, the known shared-state set, no parallel subagents) and
`docs/merge_update_rules.md` (verify per feature by behavior; docs are a
completion criterion). Gateway/channel failures that appear only under
parallel execution are concurrency limits, not flakes to retry away
(`gateway-session.md` R3).

- **Cargo.lock** is resolved per pick against the fork's lock, never copied
  from upstream; only the versions a pick requires change, all other fork
  pins stay.
- **Upstream context-management changes are not taken** (owner decision
  2026-10-27, applied to v0.8.7): a pick that touches context management is
  left unmerged and reported (see `context-management.md`).
- **Removed tests stay removed.** An upstream hunk or a test restore that
  re-adds one is dropped:
  - Tests that need inputs the `zeroclaw-test` image cannot supply (owner
    decision 2026-11-02): the upstream late ToolSearch namespace tests
    (`r5_namespace_tests.rs` `late_tool_search_*`, which need upstream-CI
    fixture receipts) with their test-only `late_tool_search_observer` hooks
    in `tools/mod.rs`, `tools/scoped.rs`, `tools/skill_tool.rs` and
    `skills/mod.rs`, and the `expect`-driven PTY test
    `daemon_feedback_follows_foreground_job_control_transitions`.
  - `acp_session_store::tests::legacy_null_trim_breadcrumb_is_inferred_once_then_recorded`
    (owner decision 2026-11-09): it required inferring a legacy `NULL` trim
    breadcrumb from message text, which contradicts the fork's design
    (provenance is a canonical column, never inferred from text; see
    `trim_breadcrumb_provenance_is_a_canonical_column_not_inferred_from_text`).

## Upstream pick log

One row per decided upstream change outside a full merge. Add a row when a
pick is taken, dropped or left out; a row is precedent for the next merge.

| Merge | Upstream | Outcome | Why / where recorded |
|---|---|---|---|
| v0.8.7 | #11807, #11808, #11809, #11811, #11812, #11814, #11851 | taken | Plugin-host bug/security fixes, on the owner's approval |
| 2026-11-09 | #11848, #12040, #11855 | taken | Later plugin-host fixes, on the owner's approval |
| 2026-11-09 | #11144 (credential-URL scan), #11143 (qdrant time bounds) | taken | Memory hygiene fixes, on the owner's approval |
| 2026-11-09 | #11225 (`c54e002a8`, owner kept through delegation) | dropped | Its production blocks in `agent/agent.rs` (`rebind_memory_tools`) and `tools/delegate.rs` (delegate memory resolution) conflict with fork lines |
| 2026-11-09 | #11266 (`72167d12e`, private subagent/pipeline memory) | dropped | Every conflict block in `agent/agent.rs` (6) and `agent/turn/mod.rs` (12) holds fork lines |
| 2026-11-09 | #11925 (`028c50f5e`, container CA stores) | not taken | Changes Dockerfiles, container CI and their TLS fixtures; the fork publishes no container image, and the Rust side is already on `webpki-roots` 1.0.10 |
| 2026-11-09 | #11901 (gateway connection drain), #11900 (concurrent store open), #11523 (settle turns a killed process left running), #11822 (POSIX command splitting) | taken | Hardening; no fork semantics touched (`gateway-session.md` rows 8–9) |
| 2026-11-09 | #11507 (`fcc23a22f`) | reported, left out | Rewrites the turn loop in `agent/turn/mod.rs` where the fork's in-turn context guard lives, and writes the fork's trim breadcrumb |
| 2026-11-09 | #12114 (X-Forwarded-For rate-limit key), #12177 (MCP tool listing sorted by name, under `prompt-cache-prefix.md` R2), #11559 (bubblewrap rejection reasons), #11656 (configured `firejail_args` applied), #11400 (model cache replaced while a reader holds it, Windows), #11398 (JSONL session rewrite replaced while a reader holds it, Windows) | taken | Class (a) security/Windows fixes |

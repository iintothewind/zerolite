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
| Context meter & usage estimation | `docs/design/context-meter.md` | `request_token_ceiling`, `effective_context_window` publication (RPC wire: `crates/zeroclaw-rpc-proto/src/types.rs`), `contextBar` | Meter-denominator + estimate-fallback tests | Meter/wire-field changes; the wire `max_context_tokens` is the water-line, not a limit |
| Session search + anchored history reads | `docs/design/session-search.md` | `crates/zeroclaw-tools/src/sessions.rs` (`session_search`, `around_message_id`, `include_tools`), `crates/zeroclaw-infra/src/session_fts.rs`, `crates/zeroclaw-infra/src/session_sqlite.rs` (`append_turn`, `load_context`, `set_context_start`), session tool registration in `crates/zeroclaw-runtime/src/tools/mod.rs`, the `requires_unrestricted_principal` marker on `session_search` | Session-search + anchored-read tests; `session_sqlite`/`session_fts` transcript tests; `session_search_is_withheld_from_non_admin_principals` | Sessions-tool or FTS `search()` contract changes; FTS schema / `sessions` columns; ownership scoping and principal withholding must survive |
| Master pair code (gateway recovery credential) | `docs/design/master-pair-code.md` | `GatewayConfig`, `api_pairing`, daemon gateway | `crates/zeroclaw-config/tests/master_pair_code.rs`, paircode-box width test | Pairing-endpoint, daemon-hosting, or config-schema changes — verify **end to end** (page + daemon path), not just unit tests |
| Prompt-cache prefix stability | `docs/design/prompt-cache-prefix.md` | Tool-spec sort, skill/catalog sort + freeze, append-only send log, `llm_request` fingerprint | Prompt/catalog ordering tests | Any prompt-construction, registration-order, or catalog-rendering change — a "cosmetic" reorder is a cache regression |
| Gateway session robustness | `docs/design/gateway-session.md` | `crates/zeroclaw-gateway/src/`, daemon gateway | Gateway session scenario tests | Gateway/session refactors; verify by scenario (disconnect mid-turn, POST after delete) |
| Delegation & turn semantics | `docs/design/delegation-turn.md` | Turn/delegate settlement, peer inbox, empty-completion nudge (`turn/empty_nudge.rs`), `is_semantically_empty_text`, runtime-written turn messages as prompt carriers / runtime rows (R6) | Turn-cancellation + settlement-recovery + peer-inbox tests + empty-completion nudge tests + R6 machinery tests (incl. `repeated_failure_correction_rides_the_tool_results_carrier`, `repeated_failure_correction_is_machinery_and_not_a_turn_boundary`) | Runtime/turn refactors that move turn ownership; upstream changes to `is_semantically_empty_terminal` or the semantic-empty error arm of the turn loop |
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
- The memory cluster, including memory ownership: #11225 (owner kept through
  delegation) and #11266 (owned-session subagent and pipeline memory), which
  entered with the v0.9.0 full merge.
- The OIDC cluster, including dashboard sign-in (#11965, #12233, #12157) and
  zerocode pairing-free OIDC login with saved remote credentials (#12317) and
  their follow-ups.
- Account isolation: principal-unaware tools and SOP execution withheld from
  non-admin principals (#12232), queued session ownership judged by current
  authority (#11234), authority held through session effects (#12239),
  persisted log reads refused to scoped principals (#11326), cron approval and
  authority enforced at the write (#11149).
- Scoped dashboard chat through the core RPC on a shared client, with
  `zeroclaw-rpc-proto` / `zeroclaw-rpc-client` (#12244).
- The runtime composition API: capability-taking turn entry points (#11174).
- The relay-served dashboard (`[relay.dashboard]`, #12270).
- Orphaned remote-session retirement (#11964).
- Config schema version 4 with retired-key migration (#11218) and the
  `[gateway.pairing_dashboard]` pairing limits (#12213).
- The CLI-only build and its install/update path (#11269, #11975, #12111).
- The conversation-binding port and channel conversation owner (#11438).
- Cost `warn_at_percent` warnings surfaced as `TurnEvent::BudgetWarning`
  (#11692).
- `relocate` as the default tool-result image policy on OpenAI-compatible
  endpoints (#10502).

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
| v0.9.0 | `c47322131..6c78ba59b`, 203 commits | merged in full except the rows below | Full merge of tag `v0.9.0` (owner decision 2026-11-12, D1): new upstream semantics with no fork precedent enter as undecided (see above) |
| v0.9.0 | #11297 (keep_tool_context_turns tool-context collapse) | left out | Reverted before the merge; standing decision 2026-10-27, `context-management.md` R1/R6/R8, divergence #10 |
| v0.9.0 | #9579 (built-in `zeroclaw-docs` skill) | left out | Reverted before the merge; owner decision 2026-11-12 (D3), `prompt-cache-prefix.md` |
| v0.9.0 | #11507 (repeated tool-failure containment) | taken | Owner decision 2026-11-12 (D4); the mid-turn correction rides the `[Tool results]` carrier, `delegation-turn.md` R6. Supersedes the 2026-11-09 "reported, left out" row |
| v0.9.0 | #11578 (overflow breakdown on `ContextWindowExceeded`) | taken as diagnostics | Owner decision 2026-11-12 (D5); `estimated_tokens` stays the fork's decided number, `context-management.md` §2 |
| v0.9.0 | #11174, #11225, #11266, #11965 (whole), #11925 | taken | Entered with the full merge. Supersede the 2026-11-09 "not taken" / "dropped" rows and the #11965 "TLS hunk only" pick, which recorded pick conflicts or pick value, not refusals |
| v0.9.0 | #11591, #11329, #11141, #12243, #11268, #12365, #12364, #12327, #12328, #12329, #11840, #12370, #12019, #12079, #12380, #11686 (`docs/book` sides); #11832, #11927, #11854, #11833, #11941, #11988, #11976, #11270, #12219, #12161, #12253, #12321, #11967 (workflow sides) | docs-site / workflow sides dropped | `fork-identity.md` R2/R3; code-level parts (doc comments, helper scripts, `deny.toml` / `.cargo/audit.toml` entries) taken |
| v0.9.0 | #12011, #12377, #11841, #11639, #11415, #12084 | version, changelog, README, `CLAUDE.md` sides dropped | `fork-identity.md` R1; fork version stays 1.3.3 |
| v0.9.0 | #11933 (chat header on narrow layouts) | layout hunks skipped | `webui.md` #9; the fork's 320px header stays |
| v0.9.0 | #11863, #11974 | hunks on removed tests dropped | "Removed tests stay removed" (late ToolSearch tests and `late_tool_search_observer` hooks; the PTY job-control test and its helpers) |
| v0.9.0 | #11218 | `context_compression` / `summary_model` hunks skipped | `context-management.md` §1 #9; the retired-key migration itself is taken |
| v0.9.0 | #11801 (drop wall-clock date from channel and agent prompts) | conflict hunks skipped | Locked decision #1: the fork already injects no wall-clock; upstream's strip helpers and per-message timestamp prefix stay out |
| v0.9.0 | #11438 `channel_history_cap` | adapted | Reads the retired `max_history_messages` upstream; the fork returns `MAX_CHANNEL_HISTORY` (`context-management.md` §1 #3) |
| v0.9.0 | #11234/#12239 SQLite overrides (`load_with_timestamps`, `rewrite_messages`, `replace_conversation_state`) | left out | The fork owns the transcript schema (`session-search.md` R3); authorized effects write through the fork writers |
| v0.9.0 | #12213 hand-built gateway/daemon pairing guards | adapted | Guards are built `from_gateway_config(..).with_live_limits(..)` so the master code is carried (`master-pair-code.md` R3) |
| v0.9.0 | #12111 `zeroclaw update --standard` | disabled | `fork-identity.md` R1/R2: self-update stays off on fork builds |
| v0.9.0 | `crates/zeroclaw-runtime/AGENTS.md` exception table (#11980, #11963, #12274, #12295, #12297, #12342 and the rows the fork table lagged) | taken, two rows left out | The manual-compaction row stays removed (`context-management.md` R7); the `zeroclaw-docs` row is left out with #9579 |
| v0.9.0 | #11174 test `skill_review_routes_vision_through_the_turns_binding` | adapted | Collides with fork `dadf41301` (carried-over images keep the selected model); owner ruling 2026-11-12: fork behavior kept, test pins it (`provider-compat.md` #10) |
| v0.9.0 | Upstream checks meeting fork behavior: `builtin_selection_boundary` fixture, `direct_cli_terminal_completion` semantic-empty request count, `runtime-composition-consumer` retained edges, the master-recovery banner line | adapted | Fork `session_search` in the built-in set; one R5 nudge before the semantic-empty failure (`delegation-turn.md` R5); `zeroclaw-context` recorded as a retained runtime edge; banner line `i18n-exempt` |

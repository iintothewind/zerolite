# Merge Report: upstream v0.8.6 Merge — Recovered Fork Behaviour and Tests

**Status:** complete — 56 tests recovered, 26 abandoned, 11 fork behaviours restored, 6 upstream tests removed (design conflicts), 6 upstream tests fixed, and all 19 baseline failures cleared
**Date:** 2026-10-21
**Scope:** the upstream v0.8.6 merge (`1f379c6de`) and everything it dropped from the fork's own tree
**Baseline for comparison:** `git stash` against `HEAD` (`7eb34ba4b`, ZeroLite 1.2.1) on each crate, run in the same container

## 1. What the merge dropped

`1f379c6de` ("Merge upstream v0.8.6") was a squash merge. A squash merge
replaces the whole tree with the merge result, so any fork-side change that
upstream did not also carry was silently discarded — no conflict marker, no
merge log entry. The fork's own commits between `1e884d2e2` and `31fb2d06b`
survive in `git log`, but their *content* was gone from the files.

Two independent sweeps found the loss:

- **symbol sweep** — functions and constants present in the fork's pre-merge
  tree but absent from `HEAD`;
- **test sweep** — `#[test]` / `#[tokio::test]` functions the fork added
  (identified by the fork's own commits) that `HEAD` no longer contained.

The two sweeps agreed, and the test sweep is the subject of this report.

## 2. The reported bug

`gateway.master_pair_code` had stopped working: an operator set it in
`config.toml` and pairing by that code was refused.

The configuration field itself was gone. Its implementation had lived in the
fork's commit `1e884d2e2` ("feat(gateway): add master_pair_code recovery
credential and fix paircode box"), and the merge dropped the whole commit's
content — not only the code, but the `GatewayConfig` field, the
`PairingGuard` methods, the two request handlers, and the schema entry. A
config key that does not exist cannot take effect, so the symptom was
"configured but ignored" rather than a runtime error.

## 3. Fork behaviours restored

Beyond the master code, the sweep found ten more fork behaviours that the
merge had dropped. Each is a behaviour, not only a test: the fork's tree had
to be changed back, not just covered.

| # | Behaviour | Where it was dropped | Restored as |
|---|---|---|---|
| 1 | `gateway.master_pair_code` recovery credential | `1e884d2e2` | `GatewayConfig` field + `PairingGuard::with_master_code` / `has_master_code` / `try_master` |
| 2 | Per-step `usage` frame forwarded to the WebSocket client | `b5cfe173f` | `UsageFold` regained `steps` / `cached_input_tokens`; `apply` returns the per-step frame; `done` and `aborted` carry the totals |
| 3 | `aborted` frame carries the turn's token totals | `b5cfe173f` | `aborted_frame(&UsageFold)` |
| 4 | `synthetic` flag on session messages | upstream | `handle_api_session_messages` sets `synthetic` from `is_synthetic_user_message` |
| 5 | Session-delete race: a queued POST must not resurrect a deleted session | upstream | re-check of `session_exists` **under** the session permit, before `append` |
| 6 | Pairing-code box sized to the code | `1e884d2e2` | `pairing_code_box_lines` (the box was hard-coded to 14 columns) |
| 7 | No runtime wall-clock prose in the system prompt | `54b9e8a1f` (fork decision, later lost) | injection points removed from `system_prompt.rs`, `agent.rs`, `orchestrator` |
| 8 | Channel prompts do not refresh a date section | `1.0.0` fork tree | `refresh_channel_prompt_date_section` removed |
| 9 | Script-aware token estimation (CJK / kana / Hangul ≈ 1 token per character) | `1.0.0` fork tree | `estimate_text_tokens` + `char_token_units` + `is_cjk_kana_or_hangul` |
| 10 | Durable conversation trim keeps whole turns, never half a turn | `1.0.0` fork tree | `keep_recent_whole_turns` primitive plus the `trim_to_fit` cascade |

Item 2 deserves a note: the web client still consumed the per-step `usage`
frame (`kind: 'usage'` with `cached_input_tokens` in
`web/src/lib/sessionStats.logic.ts`), so after the merge the client waited for
a frame the gateway no longer sent. The fork behaviour was restored, not the
client.

### 3.1 The `trim_to_budget` concept

The fork's trim API was split across two names that the merge had merged into
one, and the merged name (`trim_to_budget`) described a capability the design
does not have — "trim to a token budget" — where the spec's action is
"keep the newest `keep_recent_turns` whole turns, then cascade down until the
retained set fits `send_budget`".

The concept was removed from the tree rather than renamed around:

- `trim_to_budget` / `trim_to_budget_with_crumb` → `trim_to_fit` /
  `trim_to_fit_with_crumb`
- `trim_conversation_to_budget*` → `trim_conversation_to_fit*`
- the public `trim_to_recent_turns` (keep-N) was removed; the count-based cut
  survives as the private `keep_recent_whole_turns` primitive

`grep -rn "trim_to_budget"` over the tree (sources, docs, tests) now returns
zero hits.

## 4. Tests recovered (56)

Every recovered test kept its **original name**. Where the API it exercised
had changed, the test was ported to the current API rather than rewritten
into a different assertion; where the behaviour it asserted no longer
existed, the production code was changed back (section 3) instead of the
expectation being weakened.

| File | Count | Tests |
|---|---|---|
| `config/src/pairing.rs` | 5 | `try_master_issues_token_on_match_and_is_reusable`, `try_master_rejects_mismatch_and_blank_is_off`, `try_master_is_inert_when_require_pairing_false`, `try_master_leaves_distinct_one_time_code_intact`, `try_master_consumes_one_time_slot_on_value_collision` |
| `config/src/schema.rs` | 12 | `context_send_budget_*` (3), `effective_context_window_applies_input_ceiling`, `keep_recent_turns_*` (2), `trim_threshold_*` (4), `validate_rejects_out_of_range_trim_threshold`, `validate_rejects_reserve_exceeding_half_the_window` |
| `runtime/.../history_trim.rs` | 28 | the budget cascade (`budget_cascade_*` ×4), the calibration (`calibration_*` ×3), the conversation cascade (×2), `dropped_messages_counts_all_dropped_body_rows`, `exactly_at_keep_is_untouched`, `keeps_leading_system_after_trim`, `keeps_most_recent_keep_turns`, `oversized_retained_turn_is_still_kept_by_design`, `reanchor_after_trim_never_double_counts`, `resolve_send_budget_*` (2), `single_turn_never_trimmed`, `synthetic_*` (2), `tool_results_pseudo_user_is_not_a_turn_boundary`, `trim_to_budget_uses_send_budget_not_token_budget` (now `trim_to_fit_…`), `trimmed_reports_token_reduction`, `under_keep_is_untouched`, `zero_keep_normalises_to_one_and_keeps_last_turn`, `a_breadcrumb_is_machinery_and_still_a_turn_boundary` |
| `runtime/.../history.rs` | 3 | `estimate_cjk_is_higher_than_utf8_byte_heuristic`, `estimate_mixed_cjk_latin_between_pure_scripts`, `estimate_ascii_matches_legacy_four_chars_per_token` |
| `runtime/.../system_prompt.rs` | 1 | `final_prompt_budget_truncates_cleanly_at_boundaries` |
| `runtime/.../loop_.rs` | 1 | `pre_send_trim_fits_to_send_budget_distinct_from_water_line` |
| `gateway/src/ws.rs` | 4 | `aborted_frame_carries_the_turn_totals`, `agent_start_broadcast_reaches_only_its_own_session`, `usage_frame_accumulates_steps_tokens_and_cache_across_calls`, `usage_frame_without_reported_numbers_still_closes_a_step` |
| `gateway/src/sse.rs` | 1 | `the_chat_turn_boundary_is_not_a_public_sse_event` |
| `gateway/src/api.rs` | 2 | `session_message_post_does_not_resurrect_a_session_deleted_while_queued`, `session_messages_flags_runtime_machinery_as_synthetic` |
| `gateway/src/api_pairing.rs` | 2 | `submit_pairing_enhanced_accepts_master_code_and_mints_token`, `submit_pairing_enhanced_wrong_master_code_feeds_shared_auth_limiter` |
| `gateway/src/lib.rs` | 1 | `legacy_pair_accepts_master_code_and_mints_token` |
| `providers/src/compatible.rs` | 2 | `app_idle_*` (recovered, then removed — see section 6) |
| `channels/.../acp_server.rs` | 1 | `append_transcript_surfaces_persistence_failure` |
| `channels/.../orchestrator/mod.rs` | 2 | `build_channel_system_prompt_does_not_refresh_stale_date_section`, `build_channel_system_prompt_leaves_user_date_headings_untouched` |
| `src/main.rs` | 1 | `pairing_code_box_sized_to_code_width` |

### 4.1 Ported rather than copied

Three groups needed a port instead of a verbatim copy, because the API they
exercised had been reshaped by the 1.2.x context engine:

- **`trim_to_recent_turns` took a token budget where it now takes a turn
  count.** The fork's semantics are turn count (the spec is explicit:
  "Preferred how many of the newest **whole turns** the trim action retains").
  The function was changed back to keep-N, and the budget-driven entry point
  was split out as `trim_to_fit`.
- **The conversation-side cascade.** `trim_conversation_to_recent_turns` had
  gained a `has_leading_breadcrumb` parameter; the recovered tests were
  updated to the three-argument form.
- **The image estimate.** With script-aware estimation restored, the
  per-image text charge is priced with `estimate_text_tokens` too, which
  required the carrier's retained text to be available as text rather than
  as a byte count — hence the new `multimodal::message_image_text`.

## 5. Tests abandoned (26)

These were **not** restored. Each is listed with the reason and the evidence
that the behaviour it asserted no longer exists — restoring them would have
meant resurrecting a feature the tree deliberately does not have.

| Tests | Count | Why not restored |
|---|---|---|
| `nevis_client_secret_encrypt_decrypt_roundtrip`, `nevis_config_debug_redacts_client_secret`, `nevis_config_validate_accepts_valid_enabled_config`, `nevis_config_validate_disabled_accepts_empty_fields`, `nevis_config_validate_rejects_empty_client_id`, `nevis_config_validate_rejects_empty_instance_url`, `nevis_config_validate_rejects_empty_realm`, `nevis_config_validate_rejects_invalid_token_validation`, `nevis_config_validate_rejects_local_without_jwks`, `nevis_config_validate_rejects_zero_session_timeout` | 8 | The `nevis` table is retired upstream; `schema::tests::retire_nevis_table_in_doc_handles_every_spelling` asserts the retirement. |
| `inherit_context_token_budget_clamps_child_to_parent_when_child_is_larger`, `inherit_context_token_budget_disabled_parent_does_not_constrain_child`, `inherit_context_token_budget_keeps_smaller_child_budget`, `inherit_context_token_budget_without_parent_keeps_child`, `inherit_send_budget_is_not_clamped_to_parent_water_line` | 5 | The 1.2.x context engine removed the delegate context-token-budget inheritance; `inherit_context_token_budget` no longer exists in `tools/delegate.rs`. |
| `recompact_after_loop_trim_converges_durable_history`, `recompact_after_loop_trim_emits_history_trimmed_with_tokens_after`, `recompact_after_loop_trim_floor_exceed_logs_without_panic` | 3 | `recompact_durable_history_after_loop_trim` no longer exists in `agent/agent.rs`. |
| `context_usage_max_tokens_is_effective_context_window`, `context_usage_notification_wire_reports_effective_context_window` | 2 | `context_usage_max_tokens` now resolves the trim budget, not the effective window; `context_usage_max_tokens_is_trim_budget_from_window` and `context_usage_notification_wire_reports_model_context_window` cover the current contract. |
| `truncation_marker_survives_finite_budget`, `truncation_respects_budgets_at_and_below_marker_length` | 2 | The truncation marker was part of the wall-clock orientation that the fork removed; truncation now keeps the head only. |
| `context_overflow_trim_fail_message_distinguishes_nothing_droppable` | 1 | `context_overflow_trim_fail_message` / `…_attrs` no longer exist. |
| `replace_sender_history_after_trim_rewrites_memory_and_store`, `water_line_trim_replace_reload_keeps_only_retained_turns` | 2 | The function was renamed and re-shaped (`resync_sender_history_after_trim`, six parameters); six `channel_trim_resync_*` tests cover the current contract, including a restart-survival case. |
| `failed_image_with_prompt_tool_results_degrades_media_on_both_paths` | 1 | Renamed upstream to `failed_image_with_tool_result_carrier_degrades_media_on_both_paths` (same scenario, current name). |
| `history_pruner_percentage_at_or_above_floor_is_kept`, `history_pruner_percentage_below_floor_is_raised_to_seventy`, `history_pruner_percentage_defaults_to_eighty`, `history_pruner_percentage_deserializes_from_toml` | 4 | The `history_pruner_percentage` knob was replaced by `ContextConfig::trim_threshold_percent`; reintroducing the old field would create a second way to say the same thing. |
| `websocket_turn_forwards_usage_and_agent_start_frames`, `ws_egress_failure_reports_the_viewer_gone_without_cancelling_the_turn`, `cancel_plus_grace_force_tears_down_hung_join` | 3 | They depend on helpers the current tree no longer has (`send_ws_text_bounded` and the old end-to-end WebSocket fixture). |
| `app_idle_emits_final_when_completion_precedes_keepalive_comments`, `app_idle_finishes_when_only_sse_comments_follow_content` | 2 | Recovered first, then removed: the current SSE stream does not emit `Final` / a truncation error on application idle, so the 1.0.0 expectations describe a different stream. |

Two of the four `nevis` rows above count separately in the table because the
config-validation ones and the encryption one assert different surfaces.

## 6. Upstream tests removed (6)

Six upstream tests asserted behaviour that the fork's design decisions
overrule. They were removed rather than inverted: inverting them would have
produced tests that pass while asserting nothing, and keeping them would
have kept a contradiction in the tree.

| Test | Conflicting decision |
|---|---|
| `compact_context_carries_timestamp_orientation` | The runtime injects no wall-clock prose (fork decision `54b9e8a1f`, kept for prompt-cache stability). |
| `non_compact_context_keeps_full_channel_capabilities_section` (its orientation assertion) | Same. |
| `timestamp_orientation_survives_finite_budget_truncation` | Same. |
| `timestamp_orientation_respects_budgets_at_and_below_reserved_tail` | Same. |
| `scoped_protocol_switch_keeps_orientation_when_cap_removed_safety` | Same. |
| `scoped_switch_to_native_keeps_safety_when_text_history_was_cut_in_tools` | Same. |

Two further upstream tests asserted the *channel message* timestamp prefix
and the date-section refresh, and were removed for the same reason:

- `build_channel_system_prompt_refreshes_legacy_datetime_section_to_date_only`
- `timestamp_channel_user_content_adds_wall_clock_prefix`

The latter was replaced by the fork's
`channel_user_history_content_has_no_wall_clock_prefix`, which asserts the
opposite — that the history content carries no runtime wall-clock prefix.

## 7. Semantics settled during the recovery

Three questions surfaced while porting the tests, because the 1.0.0 fork tree
and the 1.2.x context engine disagreed. Each is now recorded in
`docs/book/src/agents/context-management.md`.

### 7.1 The hard floor is the model's capacity, not the send budget

A retention that misses the send budget but still fits the model's window is
a **soft floor**: the newest turn is dispatched rather than rejected. Only a
population the model cannot take is an unsatisfiable floor. `exceeds_budget`
was redefined accordingly, and the fit functions gained a `model_window`
parameter.

### 7.2 Settlement

Stage 2 of the cascade selects by summing per-message estimates, which is
only exact when the estimator is additive. A provider view that prices the
whole slice higher than the sum of its messages — per-call framing, one-time
tool schemas, attachment accounting — would otherwise keep more turns than
actually fit. After choosing a keep, the runtime re-prices the **retained
set as a whole** and drops one more turn until the whole-slice estimate fits
`send_budget` or only the floor remains.

### 7.3 No reported usage

The trigger reads the **provider-reported** `input_tokens`, which is
authoritative because it is what the provider billed. Not every endpoint
reports it: OpenAI-compatible servers, local runtimes, and some streaming
responses omit usage entirely.

Without a reported count the runtime has no authoritative number, and the
local estimate is not a substitute for it — it cannot see what the provider
really charged, and the breadcrumb that records a cut can itself push the
retained set past the window. So with no reported usage the runtime:

- **does not trim proactively** — the water-line trigger stays inactive,
  because an estimate is not a measurement;
- **only acts as an overflow guard** — when the local estimate says the
  request could not fit the model's window at all, the runtime trims and
  retries;
- **skips the guard when there is nothing to drop** — if only one whole turn
  remains, trimming cannot make the request smaller, so the request is
  dispatched rather than failed on a difference the trim cannot close.

A provider that reports usage keeps the full behaviour.

### 7.4 The pre-dispatch guard trims before sending

Whether the provider would accept a request is knowable locally, so the
pre-dispatch guard checks it and trims **before** sending, rather than
discovering the answer by having the provider reject the request and wasting
a round trip. Its target is the send budget; the recovery cascade remains as
the fallback for the case the local estimate cannot see (a real image's cost,
one-time tool schemas).

## 8. Verification

Run in Docker (`rust:1-bookworm`, the toolchain the project builds with) with
the workspace mounted, one crate at a time, and `-- --test-threads=1` (see
below).

| Crate | Passed | Failed |
|---|---|---|
| `zeroclaw-config` | 1815 | 0 |
| `zeroclaw-infra` | 305 | 0 |
| `zeroclaw-channels` | 1982 | 0 |
| `zeroclaw-runtime` | 5799 | 0 |
| `zeroclaw-gateway` | 662 | 0 |
| `zeroclaw-providers` | 1964 | 0 |

Each crate was also run against the **baseline** (`git stash` of the whole
change set, same container) to separate pre-existing failures from
regressions. The baseline had **19** failing tests; all of them are fixed
here, and **every failure this work introduced was eliminated**.

### 8.1 Baseline failures fixed

| Baseline failure | Why it failed | Fix |
|---|---|---|
| `done_context_limits_prefer_active_usage_route`, `done_context_limits_fall_back_to_agent_active_route_without_usage`, `done_context_limits_omit_unknown_compatibility_capacity`, `done_frame_falls_back_when_no_call_served`, `done_frame_model_context_window_presence_tracks_provider_config`, `done_frame_model_window_omitted_on_same_profile_model_fallback` | the expectations encoded the old `window − reserve` effective-window rule; the current rule is `min(window, request_token_ceiling)` | expectations rewritten against the current rule |
| `orchestrator::tests::hydration_*` (5) | the file-backed `SessionStore` never implemented `set_session_trim_breadcrumb` / `get_session_trim_breadcrumb`, so the trait's no-op default was used and the provenance flag could never survive a restart | `SessionStore` now persists the flag in a `<key>.trim_breadcrumb` sidecar (one byte: `1` / `0`), written atomically; anything else is corruption and surfaces as an `Err` so hydration fails closed instead of falling back to a text guess |
| `acp_server::session_load_replays_the_retained_turn_when_a_persisted_crumb_leads_the_seed` | `Agent::trim_history` did not tell the trim that the population carries a leading breadcrumb, so the breadcrumb consumed a keep-N slot and was counted in `dropped_messages`; the client replay boundary was then mapped one row too far | `trim_history` passes the agent's breadcrumb flag to the trim, so the crumb is neither a turn nor a dropped message |
| `tts::edge_tts_removes_temp_output_when_read_fails` | the provider returned the read error without removing the temp artifact | `synthesize` now removes the output artifact before reporting the read failure |
| `api_config::unreadable_existing_config_survives_failed_save_rollback` | the test made the source unreadable with `chmod 000`, which root ignores | the fixture uses a directory in the config file's place: reading it fails for every user, so the test no longer depends on permission bits |
| `grok_cli::tests::unix_fake_child::*` (3) | a descendant whose parent already exited lingers as an unreaped zombie; `kill(pid, 0)` succeeds for a zombie, so the probe reported it alive | the probe reads the process state from `/proc` and treats a zombie as exited — a zombie has already finished and only waits to be reaped |
| `ws::websocket_connect_persists_a_restore_time_trim_before_going_live` | the restore-time trim did not persist the breadcrumb it recorded | fixed by the same breadcrumb-provenance work as above |

### 8.2 Concurrency-sensitive tests

A handful of tests share process-global state (mock HTTP endpoints, the
pricing handle, spawned children) and fail intermittently when the harness
runs them concurrently, while passing in isolation. `AGENTS.md` rule 16
records the set and requires `-- --test-threads=1`; a failure in that set is
a harness artefact, not a regression.

## 9. Toolchain note

The Windows host could not build the tree: `ring` and `libsqlite3-sys` ship
C sources, and the `cc` crate could not assemble an `INCLUDE`/`LIB` for the
MSVC toolchain because the VS 2022 Build Tools install on this machine is not
registered (`HKLM\SOFTWARE\Microsoft\VisualStudio\SxS\VS7` is absent), so the
compiler was invoked without its SDK include path and failed with
`cl : Command line error D8050`. The tree was therefore built and tested in
the Linux container instead; nothing in the change set depends on the host
toolchain.

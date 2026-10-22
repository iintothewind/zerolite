# AGENTS.md

## Principles

1. Follow the user's goal, not the literal request.
2. Prefer simple, maintainable, low-lifecycle-cost solutions.
3. Establish goals, constraints, facts, and acceptance criteria first.
4. Verify important claims; separate facts, inferences, assumptions, and uncertainty.
5. Correct critical false premises before proceeding.
6. Requirements → research → plan → approval → execution. No writes before the approved plan.
7. If new evidence invalidates the plan, stop and revise it.
8. Act directly; explain only when analysis affects the result.
9. Discuss in Chinese; repository artifacts in English.

## Upstream-merge-update

This is the standing workflow for every upstream merge. Details live in
`zerolite_feature_list.md` and `docs/design/`; this section is the
outline the agent must not deviate from.

1. **Pre-check.** Read `zerolite_feature_list.md` and every design doc in
   `docs/design/` before touching upstream code. Classify each upstream
   change: (a) no semantics touched → may merge; (b) covered by a
   recorded fork decision → precedent lane; (c) new semantics with no
   precedent → report lane.
2. **Merge plan, then STOP.** Generate the merge plan from the pre-check
   and present it for user review. Do not execute until the review
   passes. The plan lists, per upstream change: classification, the
   design-doc decision it cites (or "no precedent"), conflicts, and the
   tests guarding each affected feature.
3. **Execute.** Documented conflict → favor ZeroLite: skip the
   conflicting upstream hunk, keep fork behavior, log one line (commit,
   file, decision number, "skipped per `<doc>` #N"). Never adapt fork
   code to upstream semantics; never reintroduce an upstream knob under
   a fork name. Undocumented conflict → STOP, report (commit, what the
   new semantics is, what adopting or refusing it changes), and wait
   for a decision. Never silently adopt upstream semantics.
4. **Post-merge verification — per feature, by behavior.** For each
   feature in the feature list, run its design doc's "Merge notes"
   checks: the behavior exists, its tests exist and pass. Never verify
   by commit count or commit presence: merge timelines are scrambled,
   and a present commit does not prove a surviving behavior (the
   `ContextConfig` orphan: the type parsed, its tests passed, and no
   runtime code read it). Fork tests that upstream rewrote away must be
   re-added. Upstream logic changes, semantic-concept changes, and
   conflict resolutions each get new tests. Gateway-facing features
   (master pair code is the standing example) are verified end to end:
   unit tests passing while a gateway path fails does not close the
   feature.
5. **Post-merge docs — part of the merge, not follow-up.** After merge
   tests pass, update every design doc and the feature list touched by
   a conflict, plus mirrors (`docs/book/.../context-management.md`).
   Record each new decision in the design doc it belongs to; once
   written back, it is precedent for the next merge. A merge is not
   complete until its docs are updated.
6. **Completion.** A merge is done only when: the plan was reviewed,
   conflicts were handled by lane, per-feature behavior checks pass,
   fork tests survive (or were re-added), new tests were added, and
   docs were updated.

## Hard constraints (do not work around these)

- One cargo test command at a time. Test runs share `CARGO_TARGET_DIR`
  (`target-docker`) and the cargo lock; parallel runs block each other
  for hours while producing no output.
- Run cargo tests with `-- --test-threads=1`. Known shared-state set —
  failures here under parallelism that pass alone are harness artefacts,
  re-run alone to confirm: `zeroclaw-providers` —
  `openai::tests::responses_model_listing_carries_header_only_profile_and_surfaces_auth_failure`,
  `openai::tests::responses_provider_honors_runtime_proxy_config`,
  `openai::tool_round_policy_tests::native_tool_round_flags_reach_stream_and_chat_wires_without_changing_defaults`,
  `grok_cli::tests::unix_fake_child::fake_child_permission_response_follows_explicit_bypass_policy`;
  `zeroclaw-runtime` —
  `tools::send_message_to_peer::tests::peer_turn_cost_scope_through_execute_boundary_attributes_recipient_and_shares_budget`;
  `zeroclaw-channels` — `orchestrator::tests::compiled_channel_*`.
- NEVER spawn parallel subagents on this repo (provider rate limiting).
  One agent at a time, or do the work inline.
- Builds and deployment: see `docs/maintainers/build-cli-binaries.md`
  and `docs/maintainers/build-cli-local.md`. Local CLI build = Docker
  cross-compile Linux arm64 → `dist/bin/aarch64-unknown-linux-gnu/zeroclaw`
  (`scripts/dev/build-cli-local.sh`), always
  `--features embedded-web,plugins-wasm-cranelift`.
- Disk reclaim after any cargo/Docker compile or test (mandatory,
  residue only): run `scripts/dev/reclaim-cargo-disk.sh` (Windows:
  `scripts\dev\reclaim-cargo-disk.cmd`) before ending the turn. Clear
  repo `target/`, `%TEMP%\zc-cargo-home`, `%TEMP%\zeroclaw\`, and
  `%TEMP%\zc-*`. Deploy only from `dist/bin/<triple>/zeroclaw`. Do not
  blind-`docker system prune -a`; if Docker disk is still tight, prune
  unused `rust:1-bookworm` cross-compile dangling layers only.

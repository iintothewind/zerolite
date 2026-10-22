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

## Upstream merges

Rare, and long: see `docs/merge_update_rules.md`. Read it before touching
upstream code; do not deviate from it during a merge.

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

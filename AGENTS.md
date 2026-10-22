# AGENTS.md

1. Follow the user's goal, not the literal request.
2. Prefer simple, maintainable, low-lifecycle-cost solutions.
3. Establish goals, constraints, facts, and acceptance criteria first.
4. Verify important claims; separate facts, inferences, assumptions, and uncertainty.
5. Correct critical false premises before proceeding.
6. Require: requirements → research → plan → approval → execution.
7. Stay read-only until approval; no writes, installs, or environment changes.
8. If new evidence invalidates the plan, stop and revise it.
9. Act directly; explain only when analysis affects the result.
10. Discuss in Chinese; repository artifacts in English.
11. Building or deploying fork CLI binaries (Windows/Linux x64, Linux arm64): see `docs/maintainers/build-cli-binaries.md`.
12. Local CLI build = Docker cross-compile Linux arm64 → `dist/bin/aarch64-unknown-linux-gnu/zeroclaw` (`scripts/dev/build-cli-local.sh`, `docs/maintainers/build-cli-local.md`). Always `--features embedded-web,plugins-wasm-cranelift`.
13. NEVER run ZeroClaw cargo tests in parallel — not as concurrent shell/Docker invocations, not as `cargo test` on several packages at once, not as duplicated background jobs. Every test run shares `CARGO_TARGET_DIR` (`target-docker`) and the cargo lock, so parallel runs block each other for hours while producing no output. Run exactly one test command at a time, wait for it to finish, then start the next.
14. Disk reclaim after ZeroClaw cargo / Docker compile or test (mandatory, compile-test residue only): after any `cargo` build/test/check or `build-cli-local` — success or failure — run `scripts/dev/reclaim-cargo-disk.sh` (Windows: `scripts\dev\reclaim-cargo-disk.cmd`) before ending the turn. Must clear: repo `target/` (can be 100s of GB; `build-cli-local.sh` EXIT trap also clears it), `%TEMP%\zc-cargo-home`, `%TEMP%\zeroclaw\`, and `%TEMP%\zc-*` (e.g. `zc-check.ps1`, `zc-baseline-check.log`, `zc-docker-*.sh`). Deploy only from `dist/bin/<triple>/zeroclaw`. Do not blind-`docker system prune -a`; if Docker disk is still tight, prune unused `rust:1-bookworm` cross-compile dangling layers only.
15. NEVER spawn parallel subagents on this repo — not a `task` tool `tasks[]` batch, not concurrent background agents of any kind. They trigger provider rate limiting. Delegate one agent at a time, or do the work inline in the main session.
16. Run cargo tests with `-- --test-threads=1`. A handful of tests share process-global state (mock HTTP endpoints, the pricing handle, spawned children) and fail intermittently when the harness runs them concurrently, while passing in isolation. The known set: `zeroclaw-providers` — `openai::tests::responses_model_listing_carries_header_only_profile_and_surfaces_auth_failure`, `openai::tests::responses_provider_honors_runtime_proxy_config`, `openai::tool_round_policy_tests::native_tool_round_flags_reach_stream_and_chat_wires_without_changing_defaults`, `grok_cli::tests::unix_fake_child::fake_child_permission_response_follows_explicit_bypass_policy`; `zeroclaw-runtime` — `tools::send_message_to_peer::tests::peer_turn_cost_scope_through_execute_boundary_attributes_recipient_and_shares_budget`; `zeroclaw-channels` — `orchestrator::tests::compiled_channel_*`. Treat a failure in that set as a harness artefact, not a regression: re-run it alone to confirm.
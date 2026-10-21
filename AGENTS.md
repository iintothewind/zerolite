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
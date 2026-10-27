# Handoff — 2026-10-27: in-turn trim guard

Branch `zerolite`, HEAD `0762173f3 fix(context): anchor the in-turn dispatch
guard on the provider measurement`. Working tree clean.

## What landed today

1. **One budget, one action.** `trim_budget = max(1, min(window ×
   trim_threshold_percent/100, window − reserve_tokens))` is both the trigger
   and the fit target. `context_send_budget` / `resolve_send_budget` deleted;
   `water-line` retired as a name across Rust, docs and README (design record
   gains R8 one-retention-semantic, R9 one-number).
2. **One retention action everywhere.** Keep the newest `keep_recent_turns`
   whole turns in a single cut, then drop one more turn at a time while the
   retained request still exceeds the budget. The per-dispatch nibble
   (`surface_oversized_dispatch_if_needed`) and the 2/3 overflow-recovery
   shrink are gone.
3. **Conservative margin 11/10** (`conservative_estimate_tokens`), now applied
   to the in-turn guard as well.
4. **In-turn guard anchored** (`DispatchTokenCounter`, `turn/mod.rs`):
   `reported + margin(tail)` when the route's report applies and the
   population is still a superset of the measured rows, else `margin(estimate)`.
   The old `estimate × ratio` scaling is gone. **This is what caused the red
   tests below.**
5. Web: `waterline`/`waterlineTokens` → `trimBudget`/`trimBudgetTokens`,
   i18n `ring_detail_trim_budget` (wire `max_context_tokens` unchanged).
6. `scripts/dev/reclaim-cargo-disk.sh`: root-owned `target/` (written by the
   cross-compile container) is removed from inside a local image instead of
   failing the reclaim step. Verified both paths by hand.
7. Shipped: arm64 binary on `ivarvps:~/Download/zeroclaw`
   (`sha256 3b6cac41…`, `zeroclaw 1.2.5`, `plugin` present, remote is aarch64).

## Open item 1 — 9 red tests (blocking the next commit)

`cargo test -p zeroclaw-runtime --lib -- --test-threads=1` → 5802 passed,
9 failed. All nine encode the retired contract or a fixture tuned to the old
under-count. Decide per group whether the **fixture** is stale or the **code**
is wrong; do not blanket-rewrite assertions.

**(a) "calibrated" label + exact post-trim count** — `agent/loop_.rs`
- `next_request_keeps_same_model_reported_usage_calibration` (:21789 label,
  :21794 count). The count assertion still models `estimate × usage_multiplier`;
  the guard now returns `reported + margin(tail)`.
- `hook_plain_to_native_trims_the_actual_next_request` (:21783)
- `hook_selected_native_schema_soft_floor_still_dispatches` (:21794)
- `max_iteration_summary_trims_the_prepared_request` (:21794)
- `max_iteration_summary_floor_preserves_the_latest_real_turn` (:21780)
- `max_iteration_summary_soft_floor_still_dispatches` (:21783)
  Shared helper `assert_next_request…` around :21760-21800.

**(b) fixture expected "no trim"** — the margin moved the population over the
budget:
- `hook_native_to_plain_preserves_history_not_needed_for_schemas` (:21698,
  `untrimmed` expectation).
- `soft_context_floor_dispatches_the_complete_latest_turn` (:22163): zero
  provider requests now — the margin pushed the population past the **model
  window**, so the guard hard-failed instead of dispatching a soft floor.
  This one is a real risk to weigh: a conservative pre-send number can now
  fail a request the provider would have accepted. Decide between retuning the
  fixture, or exempting the soft floor from the margin.

**(c) premise overtaken** — `agent/turn/mod.rs:9968`
`tool_lifecycle_abandonment_tests::image_recovery_leaves_context_overflow_to_history_trimming`
assumed the provider rejects first, then recovery trims. The local guard now
trims before the request goes out (the desired behaviour), so the fixture no
longer reaches the rejection path. Rewrite the premise, or lower the fixture
budget so the guard stays silent and the rejection path is still exercised.

## Open item 2 — context ring shows a stale number after a trim

Reported: after the trim notice, the web ring keeps the trim's number and does
not move on the next turn. Evidence points at a **stale browser bundle**, not
runtime logic:

- runtime publishes the post-trim estimate at turn start
  (`agent.rs:4849-4886`), gateway forwards `TurnEvent::ContextEstimate`
  unconditionally (`ws.rs:2229-2241`), client applies it
  (`AgentContext.tsx:486-494`);
- the deployed binary embeds the current bundle: `trim budget {count}` present,
  `trim trigger {count}` absent, `assets/index-Em65wyAm.js`;
- the user's panel reads **"trim trigger"** — a string that only exists in a
  pre-refactor bundle, so the page is not served from this binary (or is
  browser-cached).

Next: on `ivarvps`, `curl -s http://127.0.0.1:42617/ | grep -oE
'assets/index-[A-Za-z0-9_-]+\.js'` and compare with `index-Em65wyAm.js`; check
`readlink -f /proc/$(pgrep -o zeroclaw)/exe`; hard refresh + unregister any
service worker. If the panel then reads "trim budget" and the ring still
sticks, capture the WS frame order (`history_trimmed` / `context_usage` /
`usage` / `done`) — that would be a real bug.

## Environment / how to run

- Tests, fmt, clippy all run in the repo image, one cargo command at a time:
  ```bash
  MSYS_NO_PATHCONV=1 docker run --rm -v "$PWD:/work" -v "$HOME/.cargo:/cargo-host" \
    -e CARGO_HOME=/cargo-host -e CARGO_TARGET_DIR=/work/target-docker -w /work \
    zeroclaw-test cargo test -p zeroclaw-runtime --lib -- --test-threads=1
  ```
- `target-docker` (12 GB) was deliberately kept warm for tomorrow; delete it
  with `docker run --rm -v "$PWD:/work" -w /work zeroclaw-test rm -rf
  /work/target-docker` if the disk is needed.
- `cargo fmt --all` was **not** re-run after the final edit — run it before the
  next commit.
- Rebuild + redeploy: `bash scripts/dev/build-cli-local.sh` (reclaims
  `target/` itself now), then `scp -p
  dist/bin/aarch64-unknown-linux-gnu/zeroclaw ivarvps:~/Download/`.

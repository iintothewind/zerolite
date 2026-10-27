# Handoff — 2026-10-27/28: in-turn trim guard (context)

Branch `zerolite`. Everything today is committed except the work described under
"What is still open".

## Why the guard missed (the question, answered)

The check-before-every-dispatch *position* has existed since the v0.8.6 merge —
it was not missing. What was wrong was **the number it compared**:

- With no report yet it used the **raw** estimate, with no conservative margin.
  `pending_reported_usage` is `None` at the top of the loop (`turn/mod.rs:1848`)
  and is only assigned after a response (`:3561`), so the **first request of
  every turn** was priced without the margin — while the turn-start check it had
  just passed used `calibrated_turn_start_size` (`agent.rs:1486-1503`), which is
  `reported + margin(tail)` or `margin(estimate)`. A population in that band
  (raw ≤ budget < margined) slipped past the guard, was dispatched, and was
  trimmed only after the provider answered — the observed "trim lands after the
  response".
- With a report it multiplied the **whole** population by a ratio fitted on the
  *previous* population — so the tail (a tool result, where the estimator is
  worst) was priced by an old factor, and `ratio ≤ 1.0` fell back to the raw
  estimate again. The guard and the trigger never read the same number.

Fix (`0762173f3`): `DispatchTokenCounter::measure` returns
`reported + margin(tail)` when the route's report applies and the population is
still a superset of the rows it measured, else `margin(estimate)`; the label
follows the number actually used (a population a mid-turn trim pushed below the
anchor is `estimate`, not `calibrated`).

## What is committed

| commit | content |
|---|---|
| `0762173f3` | one `trim_budget` (trigger == fit target), one retention action (single cut to `keep_recent_turns`, then one turn at a time), margin `11/10`, anchored in-turn guard, web `waterline*` → `trimBudget*`, reclaim helper removes root-owned `target/` from a local image, docs (context-management, context-meter, webui, build-cli-local, README) |
| `840a7c2e9` | the first handoff |

Both request-construction sites in a turn are guarded: `turn/mod.rs:2368` →
`call_provider(… &provider_request_messages …)` at `:2734`, and `max_iter.rs:254`
→ the summary request at `:356`. Retries inside `call_provider` (reliable
fallback, image recovery) reuse the same or a smaller request.

## Verified after the red-test fix

- `cargo test -p zeroclaw-runtime --lib -- --test-threads=1` → **5812 passed, 0
  failed**, 8 ignored.
- `cargo fmt --all -- --check` → 0 diffs.
- `cargo test -p zeroclaw-gateway --lib` → 682 passed; `-p zeroclaw-config --test
  profile_context` → 10 passed (both were green before this round; the fix
  touched runtime only).

## The nine red tests: what each needed, and the two decisions

They fell into three shapes. Fixtures were tuned to the *unmargined* number, and
two assertions encoded the retired contract.

1. **Expectation encoded the old formula** — `agent/loop_.rs`
   `assert_hook_selected_request_budget` now derives the expected number from the
   captured requests using the documented rule (anchor when both requests went to
   the same model and the population did not shrink, else margin), and checks the
   label from the same predicate. A fixture that encoded `estimate × multiplier`
   and one that re-checked `×2` against the budget were removed rather than
   re-pinned; the anchored count assertion is what proves the report is reused.
2. **Budgets sized against the raw population** — `NativeToPlain*` and `Summary*`
   budgets now carry the margin, so the opening request fits and the trim being
   tested is the one that fires. `Summary*` had to move because iteration 0's
   guard fired first (its event was the one the harness then compared against the
   summary request).
3. **Premise overtaken by the fix** —
   `image_recovery_leaves_context_overflow_to_history_trimming` sized its budget
   so the *provider* rejected first; with the margin the local gate trims first
   and the rejection path is never reached. Budget re-derived from the fixture
   population × margin, with an assertion that the gate stays silent, so the test
   still covers the rejection fallback it is named for.

**Decision A — capacity is compared in conservative units (recorded in
`docs/design/context-management.md`).** A population whose *conservative* size
exceeds the model window now **fails closed**, even when the raw estimate fits it
exactly. The old fixture pinned the opposite (`capacity == estimate` dispatches).
Rationale: R5 exists because the raw estimate under-counts, so "raw fits" is not
evidence the model can take it; a typed capacity error is diagnosable, a
mid-reply stall is not. **Cost: availability** — a request within the margin of
capacity is refused instead of sent. If that trade is wrong, the fix is two
numbers (margin for the trigger/fit, raw for the accept/reject decision); say so
and it is a small change.

**Decision B — the floor may still land over budget.** Stage 2 stops at
`kept_turns == 1` even when that is over the trim budget, so the tests now assert
"over budget ⇒ `kept_turns == 1`" instead of a per-scenario `over_soft_budget`
flag. This follows from the cascade rule already in the spec.

Tests added: `agent::turn::trim_budget_tests::the_in_turn_guard_anchors_on_the_reported_request_not_a_ratio`,
`…::the_in_turn_guard_margins_a_population_it_cannot_anchor`,
`…::the_in_turn_guard_ignores_a_report_from_another_route`,
`…::a_population_that_shrank_below_the_anchor_is_labelled_an_estimate`,
`…::the_first_pass_cuts_to_the_configured_retention_not_the_minimum`,
`agent::loop_::tests::a_population_within_the_margin_of_capacity_fails_closed`.

## What is still open

1. **`soft_context_floor_dispatches_the_complete_latest_turn`** — kept, with its
   boundary case re-expressed as Decision A's fail-closed pin. If Decision A is
   reversed, both tests change together.
2. **Context ring shows a stale number after a trim** (user report). Evidence
   still points at a **stale browser bundle**, not runtime logic: the runtime
   publishes the post-trim estimate at turn start (`agent.rs:4849-4886`), the
   gateway forwards `TurnEvent::ContextEstimate` unconditionally
   (`ws.rs:2229-2241`), and the client applies it (`AgentContext.tsx:486-494`).
   The user's panel reads **"trim trigger"**, a string that exists only in a
   pre-refactor bundle, while the deployed binary embeds `trim budget` and
   `index-Em65wyAm.js`.
   Check on `ivarvps`:
   ```bash
   readlink -f /proc/$(pgrep -o zeroclaw)/exe
   curl -s http://127.0.0.1:42617/ | grep -oE 'assets/index-[A-Za-z0-9_-]+\.js'
   curl -s http://127.0.0.1:42617/assets/index-Em65wyAm.js | grep -c 'trim budget'
   ```
   Then hard-refresh and unregister any service worker. If the panel reads "trim
   budget" and the ring still sticks, capture the WS frame order
   (`history_trimmed` / `context_usage` / `usage` / `done`) — that would be a real
   bug, and the fix belongs next to `UsageFold::note_trim` (`ws.rs:1640`).
3. **Rebuild + redeploy** after this round: `bash scripts/dev/build-cli-local.sh`
   (it now reclaims root-owned `target/` itself), then
   `scp -p dist/bin/aarch64-unknown-linux-gnu/zeroclaw ivarvps:~/Download/`.
   `ivarvps` is aarch64; `~/Download/zeroclaw` currently holds the *previous*
   build (`sha256 3b6cac41…`), which predates the guard fix.

## Environment

```bash
MSYS_NO_PATHCONV=1 docker run --rm -v "$PWD:/work" -v "$HOME/.cargo:/cargo-host" \
  -e CARGO_HOME=/cargo-host -e CARGO_TARGET_DIR=/work/target-docker -w /work \
  zeroclaw-test cargo test -p zeroclaw-runtime --lib -- --test-threads=1
```

One cargo command at a time (shared `target-docker` + lock). `target-docker` is
kept warm (~12 GB); remove it with
`docker run --rm -v "$PWD:/work" -w /work zeroclaw-test rm -rf /work/target-docker`
(root-owned files need the container).

# Gateway session robustness

> **Status: specification.** This page records ZeroLite's gateway session
> behaviors that upstream gateway/session changes must not silently break.
> These are behavioral fixes, not a single feature: each one was a real
> failure first. For the merge rule, see `AGENTS.md`
> § upstream-merge-update. Decisions here are precedent: cite them as
> `gateway-session #N` in merge reports.

## Decision record: why these behaviors are load-bearing

- **R1 — A turn outlives its transport.** A chat WebSocket disconnect
  must not kill the agent turn, and a queued session-message POST after
  its session is deleted must be refused, not applied to a ghost session.
  Transport state and turn state are different things.
- **R2 — Locks and timeouts must fail in the open, with a cause.** Session
  locks, SSE idle, and hard timeouts surface named errors (which lock,
  which timeout) instead of hanging or failing generically. An operator
  cannot debug a hang they cannot name.
- **R3 — Concurrency limits are real constraints, not flakes.** Gateway
  and channel paths share process-global state in places (mock endpoints,
  pricing, spawned children). Tests that fail only under parallel
  execution are reporting a concurrency limit, not a flaky test. The
  response is serialization (`--test-threads=1`, one test command at a
  time), not retries.

## Behaviors (each is a merge check, not background)

| # | Behavior | Fork evidence |
|---|---|---|
| 1 | Agent turn keeps running when the chat WebSocket disconnects | `fix(gateway): keep the agent turn running when the chat WebSocket disconnects` |
| 2 | A queued session-message POST after session deletion is refused | `fix(gateway): refuse a queued session-message POST after its session is deleted` |
| 3 | Hard-timeout abort, SSE app-idle handling, and named session-lock errors | `fix(gateway): hard-timeout abort, SSE app idle, and clearer session-lock errors` |
| 4 | Dashboard reads are confined to opened file handles | `fix(gateway): confine dashboard reads to opened file handles` |
| 5 | Webhook-started SOP agent steps are driven by the gateway | `fix(gateway): drive webhook-started SOP agent steps` |
| 6 | WS memory is consolidated on the agent's own provider | `fix(gateway): consolidate WS memory on the agent's own provider` |
| 7 | Daemon-hosted gateway carries the configured master code | See `master-pair-code.md` R3 |

## Merge notes

- **Verify post-merge by behavior, per row above.** Upstream gateway
  refactors routinely touch exactly these paths; a green compile plus a
  passing unit suite does not prove row 1 or 2 survived. The verification
  for this doc is scenario checks (disconnect mid-turn, POST after
  delete), and for the master-code row, the end-to-end checks in
  `master-pair-code.md`.
- **Concurrency:** gateway/channel tests that fail in parallel but pass
  alone are not to be "fixed" by retrying or by deleting the test. They
  are covered by the test-execution constraints in `AGENTS.md`
  (one test command at a time, `--test-threads=1`, known shared-state
  set). An upstream change that makes a previously serial-safe test
  parallel-unsafe is a behavior change: report it.
- **Code anchors:** `crates/zeroclaw-gateway/src/`,
  `crates/zeroclaw-runtime/src/daemon/mod.rs`.

## Sources

- Fork commits listed in the table above, plus `360c84242` (live-session
  refresh scope-filter, TUI alive on RPC timeout) and `81aeae088`-era
  gateway fixes.

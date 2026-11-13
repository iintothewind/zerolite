# Gateway session robustness

> **Status: specification.** This page records ZeroLite's gateway session
> behaviors that upstream gateway/session changes must not silently break.
> These are behavioral fixes, not a single feature: each one was a real
> failure first. For the merge rule, see `docs/merge_update_rules.md`.
> Decisions here are precedent: cite them as
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
| 8 | WebSocket upgrades complete over the native TLS listener, as over plain HTTP | `fix(gateway): serve WebSocket upgrades over the native TLS listener` |
| 9 | A stopped gateway admits no new request (503 `gateway_restarting`, GOAWAY on h2) and drains its connections within 300 ms; the next instance binds at once | upstream #11901 |
| 10 | A gateway turn appends its rows (chat and tool rows, one dense `turn` number) at turn end and moves the session's context start; a trim never deletes history, and a reconnect restores only the chat rows from the context start on. `GET /api/sessions/{id}/messages` pages whole turns (`?before_turn=&turns=`, default 20) with tool output omitted; `GET /api/sessions/{id}/messages/{message_id}` returns one row in full | `ws.rs` tests `transcript_rows_classifies_every_history_variant`, `retained_chat_rows_excludes_the_breadcrumb_and_tool_rows`, `finished_turns_are_appended_with_their_turn_number`, `persist_turn_skips_deleted_session`; `api.rs` `session_messages_pages_turns_and_omits_tool_output`; `session_sqlite.rs` `context_start_bounds_load_context`. See `session-search.md` R3 |

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
- **TLS WebSocket (row 8):** since #11901 one accept loop serves both
  listeners; both branches must call `serve_connection_with_upgrades`
  (upstream's #11901 hunk still had `serve_connection` on the TLS branch;
  #11965 fixed it later). Test: `a_websocket_over_native_tls_stays_open`.
- **Connection drain (row 9):** tests
  `a_stopped_tls_gateway_closes_its_connections_and_the_next_one_serves`,
  `a_stopped_plain_gateway_closes_its_connections_and_the_next_one_serves`,
  `a_stopped_gateway_releases_its_port_before_its_drain_ends`. Scenario:
  `POST /admin/reload` with a chat socket open closes it `1001`, and the
  next chat turn completes on the new instance.
- **Dashboard sign-in (#11965, undecided upstream semantics):** a signed-in
  socket is closed when its session ends or loses access; the running turn
  keeps going unattended and persists its rows, consistent with R1. The fork
  transcript reads (row 10) are operator routes: a non-admin session gets 403
  `operator_required` (`fork_session_routes_refuse_a_non_admin_dashboard_session`).
- **Code anchors:** `crates/zeroclaw-gateway/src/`,
  `crates/zeroclaw-runtime/src/daemon/mod.rs`.

## Sources

- Fork commits listed in the table above, plus `360c84242` (live-session
  refresh scope-filter, TUI alive on RPC timeout) and `81aeae088`-era
  gateway fixes.

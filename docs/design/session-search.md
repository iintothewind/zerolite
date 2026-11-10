# Session search and anchored history reads

> **Status: specification.** This page defines ZeroLite's agent-facing
> session-history search semantics. The code conforms to this page. For the
> merge rule (precedent lane vs. report lane), see `docs/merge_update_rules.md`.
> Decisions here are precedent: cite them as
> `session-search #N` in merge reports.

## Decision record: why this shape

- **R1 — Search without a read is a dead link.** Returning message IDs that
  the agent cannot dereference forces it to list and read whole sessions by
  hand. `session_search` and the anchored `sessions_history` read were built
  as one closed loop, not as two features.
- **R2 — Ownership is a safety property, not a filter option.** An agent
  must only ever see its own sessions. Scoping is enforced at the tool
  layer by the caller's agent/channel identity, not left to the query.
- **R3 — The storage layer is forked for the gateway transcript.** The
  original rule (leave upstream's FTS index, `search()` and query builder
  untouched) could not hold: whole-transcript rewrites deleted history on
  every trim, tool calls were never stored, and `unicode61` matched Chinese
  only as whole untokenized runs. The fork now owns the transcript schema:
  rows are append-only and carry `kind` (`user`, `assistant`, `runtime`,
  `step`, `tool_call`, `tool_result`), a dense per-session `turn`,
  `tool_name` and `tool_call_id`; a trim moves the session's
  `context_start_id` instead of deleting rows; the index is a contentless
  FTS5 table with every CJK/kana/Hangul character indexed as its own token
  and queried as a phrase, snippets are built in Rust, and hits are read in
  descending FTS rowid order so a common term stops at the limit instead of
  sorting every match.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | No agent-facing search tool: `sessions_list` lists, `sessions_history` reads only the last N messages | `session_search` returns matching message IDs (`sessions.id` = FTS rowid) with session key, turn, kind, role, tool name, timestamp, and snippet; tool calls and tool results are searched too. `sessions_history` accepts `around_message_id` (+ before/after) to read around any hit, always starting with the anchor turn's `user` row, and `include_tools` (default false) to show tool rows. A tool row's body is shown one window of lines at a time (`line_offset` / `line_limit`, default the first 50 lines, each line cut at 2000 characters); a cut body ends with the `line_offset` that continues it. Chat rows are never cut | R1 |
| 2 | No ownership scoping at the tool layer | `session_search` and history reads are scoped to the calling agent/channel; cross-session leakage is a defect, not a misconfiguration. Registration lives in `crates/zeroclaw-runtime/src/tools/mod.rs` (`SessionOwnershipScope::with_channels`); it was lost in the v0.8.6 merge (`1f379c6de`) and restored with the transcript rework | R2 |
| 3 | External-content FTS5 index over `sessions(session_key, content)` with sync triggers; transcripts rewritten wholesale on trim | Schema version 2 (`PRAGMA user_version`): `sessions` gains `kind`/`turn`/`tool_name`/`tool_call_id`, `session_metadata` gains `context_start_id`; contentless `sessions_fts` fed by the writers (`session_fts.rs`: `index_text`, `match_query`, `snippet`); gateway turns use `append_turn` + `set_context_start`, restore uses `load_context`. A v1 store is migrated in place on open (kinds and turns backfilled, index rebuilt) | R3 — an upstream change to the `sessions` columns or the FTS schema is a conflict with this page: report it, do not take it |

## Merge notes

- **Verify post-merge by behavior, not by commit presence:** the
  `session_search` tool exists in the agent tool set; a keyword query
  returns IDs with session key + snippet; a Chinese substring (`查询` in
  `数据库查询效率`) matches; tool output is found; `sessions_history` with
  `around_message_id` returns messages around the anchor starting with the
  turn's user row; a query from one agent/channel cannot return another's
  sessions; a trim leaves every stored turn in `GET
  /api/sessions/{id}/messages`. If any of these fails, the feature is gone
  even if the commits are present.
- **Tests:** the tool's tests, the anchored-read tests, and the
  `session_sqlite.rs` / `session_fts.rs` transcript tests
  (`append_turn_*`, `context_start_bounds_load_context`,
  `fts_matches_cjk_substrings_and_tool_output`,
  `search_messages_reads_hits_in_rowid_order_without_a_sort`,
  `opening_a_v1_store_migrates_rows_and_rebuilds_the_index`) are fork
  tests; they must survive the merge. If upstream rewrote `sessions.rs` or
  `session_sqlite.rs`, re-add them — do not accept their absence as
  "upstream refactored it away".
- **Code anchors:** `crates/zeroclaw-tools/src/sessions.rs`
  (`session_search`, `sessions_history`, `around_message_id`,
  `include_tools`); `crates/zeroclaw-infra/src/session_fts.rs`;
  `crates/zeroclaw-infra/src/session_sqlite.rs` (`migrate_to_v2`,
  `append_turn`, `load_context`, `set_context_start`, `load_turn_page`,
  `search_messages_sql`); `crates/zeroclaw-runtime/src/tools/mod.rs`
  (session tool registration).

## Sources

- Fork commits `775c0aab3` (message-level FTS search + anchored history
  read), `2f246a59d` (ownership scoping by caller agent/channel).
- Feature ticket: `zeroclaw-sessions-search-ticket.md` (message-level
  search + anchor-read loop, FTS5 `sessions_fts`).
- Gateway transcript rework (2026-11-10): append-only history with tool
  rows, turn paging, CJK search.

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
  Because that scope is agent/channel and never principal, `session_search`
  is withheld from a non-admin principal, like upstream's cross-session
  tools under #12232 (`requires_unrestricted_principal`; owner decision
  2026-11-12).
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
  and queried as a phrase, snippets are built in Rust, and exact hits are
  read in descending FTS rowid order so a common term stops at the limit
  instead of sorting every match.
- **R4 — A keyword that matches nothing falls back to its two-character
  terms, ranked.** The exact tier is unchanged: each whitespace-separated
  term as written, any term matching, newest first. When it returns
  nothing and `has_window_terms` holds (the keyword yields a two-character
  term it does not already contain as a whole word), the tool searches
  again with `MessageMatch::Approximate`: the keyword's `recall_terms`
  (shared with memory recall, capped at 64 windows), one phrase each,
  OR-joined, `ORDER BY sessions_fts.rank`, same limit. The output's first
  line says `No exact match. N closest message(s) by shared two-character
  terms, best first:`; the fallback is announced only in that line, not in
  the tool description or schema. Tiered, so a keyword that matches
  as written costs nothing extra. BM25, not newest first: on the 8,555-row
  production store the target message reached the top 20 for 29/300
  questions newest first and 300/300 ranked (1/150 newest first at 100k
  rows). No 8-term cap: a question's leading windows are function words
  (`我上次是不是说过要去斯坦利公园散步`), and dropping the tail left the
  target in 2 of the top 20 instead of 20. Cost: median 3.0 ms, max 7.4 ms
  at 8.5k rows on the production device; an estimated 50–355 ms at 100k
  rows, linear in rows × windows. The match is literal character overlap,
  not semantics.
- **R5 — The search tool's own rows are never hits.** A `session_search`
  call holds the query verbatim and its result holds the hits' snippets.
  Unexcluded, a repeated question matches the previous call as written, so
  the approximate tier never runs; these rows were 2.1% of the production
  store and 16 of the 20 newest hits for `斯坦利`. Both tiers exclude
  them: the tool passes its own name in `MessageQuery::exclude_tools`;
  storage knows no tool names.
- **R6 — Tool rows are searched unless the caller opts out.**
  `session_search` takes `ignore_tool_calls` (boolean, default false; owner
  decision 2026-11-19). When true the tool sets `MessageQuery::chat_only`
  and both tiers return chat rows only (`user`, `assistant`, `runtime`):
  `step`, `tool_call` and `tool_result` rows are left out, the same rows
  `sessions_history` hides without `include_tools`. The default stays R1's:
  tool output is found. On the production store (8,854 rows) 77% of rows
  are not chat rows, and a document a tool read once matched a question as
  written, so the approximate tier never reached the conversation. The
  parameter is part of the tool schema: a session open across the upgrade
  re-prefills its prompt prefix once. A chat message that quotes the query
  still matches as written; the parameter does not change that.
- **R7 — One hit per distinct message.** Rows with the same kind, tool
  name and content are one hit: the first in search order (newest in the
  exact tier, best ranked in the approximate tier) is returned with
  `duplicates=N` in its line, the others are not listed. The session is
  not part of the key, so a text repeated across sessions is one hit. Full
  text is compared, not a hash. On the production store 1,262 of 8,854
  rows (14%) repeat another row (`.` as a `step` 566 times, `No memories
  found.` 56, `(no output)` 42), and a `limit=10` approximate search
  returned one `memory_recall` result seven times. `search_messages` reads
  `limit × 5` rows (at most 200, never fewer than `limit`) and folds while
  reading, so `N` counts repeats among the rows read (a lower bound) and a
  text repeated more often than that window can still crowd out older
  hits. Reading 100 rows instead of 20 cost at most 3.4 ms in the exact
  tier; the approximate tier ranks every match either way (17 ms for 43
  windows at both sizes).

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | No agent-facing search tool: `sessions_list` lists, `sessions_history` reads only the last N messages | `session_search` returns matching message IDs (`sessions.id` = FTS rowid) with session key, turn, kind, role, tool name, timestamp, and snippet; tool calls and tool results are searched too unless `ignore_tool_calls` is true (R6). `sessions_history` accepts `around_message_id` (+ before/after) to read around any hit, always starting with the anchor turn's `user` row, and `include_tools` (default false) to show tool rows. A tool row's body is shown one window of lines at a time (`line_offset` / `line_limit`, default the first 50 lines, each line cut at 2000 characters); a cut body ends with the `line_offset` that continues it. Chat rows are never cut | R1 |
| 2 | No ownership scoping at the tool layer | `session_search` and history reads are scoped to the calling agent/channel; cross-session leakage is a defect, not a misconfiguration. Registration lives in `crates/zeroclaw-runtime/src/tools/mod.rs` (`SessionOwnershipScope::with_channels`); it was lost in the v0.8.6 merge (`1f379c6de`) and restored with the transcript rework | R2 |
| 3 | External-content FTS5 index over `sessions(session_key, content)` with sync triggers; transcripts rewritten wholesale on trim | Schema version 2 (`PRAGMA user_version`): `sessions` gains `kind`/`turn`/`tool_name`/`tool_call_id`, `session_metadata` gains `context_start_id`; contentless `sessions_fts` fed by the writers (`session_fts.rs`: `index_text`, `match_query`, `snippet`); gateway turns use `append_turn` + `set_context_start`, restore uses `load_context`. A v1 store is migrated in place on open (kinds and turns backfilled, index rebuilt). Upstream's authorized session effects (#11234, #12239) write through the same writers: `append_authorized` inserts via `append_on`/`insert_row` inside its authorizing transaction, both deletes share one row-delete helper, `try_load` returns chat rows only | R3 — an upstream change to the `sessions` columns or the FTS schema is a conflict with this page: report it, do not take it |
| 4 | No fallback for a keyword that matches nothing | Exact tier, then (when it is empty and `has_window_terms` holds) an approximate tier over the keyword's `recall_terms`, BM25-ranked (`MessageMatch::Approximate`, `ORDER BY sessions_fts.rank`); output first line `No exact match. N closest message(s) …` | R4 |
| 5 | Search returns every matching row | `session_search` passes `exclude_tools: ["session_search"]`; `search_messages_sql` drops rows whose `tool_name` is listed, in both tiers | R5 |
| 6 | Search always includes tool rows | `ignore_tool_calls: true` sets `MessageQuery::chat_only`; `search_messages_sql` adds `AND s.kind IN ('user', 'assistant', 'runtime')` (`CHAT_KINDS`) in both tiers; default false | R6 |
| 7 | One hit per matching row | `search_messages` folds rows with equal kind, tool name and content into the first one read (`MessageHit::duplicates`) and reads `scanned_rows(limit)` rows; the tool prints ` duplicates=N` before `snippet=` when N > 0 | R7 |

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
  `exact_search_leaves_out_rows_of_excluded_tools`,
  `approximate_search_ranks_the_closest_row_first`,
  `chat_only_search_leaves_out_step_and_tool_rows`,
  `search_folds_rows_repeating_an_earlier_hit`,
  `approximate_match_query_joins_window_phrases`,
  `snippet_centres_on_the_span_with_the_most_distinct_terms`,
  `recall_terms_*`,
  `opening_a_v1_store_migrates_rows_and_rebuilds_the_index`,
  `authorized_append_is_classified_and_searchable_like_append_turn`,
  `authorized_delete_leaves_no_search_hit`), the tool tests
  `session_search_falls_back_to_the_closest_messages`,
  `session_search_ignore_tool_calls_searches_chat_rows_only` and
  `session_search_folds_identical_messages`, and the principal test
  `session_search_is_withheld_from_non_admin_principals` are fork
  tests; they must survive the merge. If upstream rewrote `sessions.rs` or
  `session_sqlite.rs`, re-add them — do not accept their absence as
  "upstream refactored it away".
- **Code anchors:** `crates/zeroclaw-tools/src/sessions.rs`
  (`session_search`, `ignore_tool_calls`, `sessions_history`,
  `around_message_id`, `include_tools`);
  `crates/zeroclaw-infra/src/session_fts.rs` (`recall_terms`,
  `approximate_match_query`, `has_window_terms`, `snippet`);
  `crates/zeroclaw-infra/src/session_backend.rs` (`MessageMatch`,
  `MessageQuery::exclude_tools`, `MessageQuery::chat_only`,
  `MessageHit::duplicates`); `crates/zeroclaw-infra/src/session_sqlite.rs`
  (`migrate_to_v2`, `append_turn`, `load_context`, `set_context_start`,
  `load_turn_page`, `search_messages_sql`, `search_messages`,
  `scanned_rows`); `crates/zeroclaw-runtime/src/tools/mod.rs`
  (session tool registration).

## Sources

- Fork commits `775c0aab3` (message-level FTS search + anchored history
  read), `2f246a59d` (ownership scoping by caller agent/channel).
- Gateway transcript rework (2026-11-10): append-only history with tool
  rows, turn paging, CJK search.

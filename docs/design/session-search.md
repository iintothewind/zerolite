# Session search and anchored history reads

> **Status: specification.** This page defines ZeroLite's agent-facing
> session-history search semantics. The code conforms to this page. For the
> merge rule (precedent lane vs. report lane), see `AGENTS.md`
> § upstream-merge-update. Decisions here are precedent: cite them as
> `session-search #N` in merge reports.

## Decision record: why this shape

- **R1 — Search without a read is a dead link.** Returning message IDs that
  the agent cannot dereference forces it to list and read whole sessions by
  hand. `session_search` and the anchored `sessions_history` read were built
  as one closed loop, not as two features.
- **R2 — Ownership is a safety property, not a filter option.** An agent
  must only ever see its own sessions. Scoping is enforced at the tool
  layer by the caller's agent/channel identity, not left to the query.
- **R3 — Do not fork the storage layer.** The FTS index, the `search()`
  semantics, and the query builder are upstream's and are untouched. The
  fork adds only the agent-tool surface on top of them, so upstream storage
  changes merge cleanly unless they change the `search()` contract itself.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | No agent-facing search tool: `sessions_list` lists, `sessions_history` reads only the last N messages | `session_search` returns matching message IDs (`sessions.id` = FTS rowid) with session key, role, timestamp, and snippet; `sessions_history` accepts `around_message_id` (+ before/after) to read around any hit | R1 |
| 2 | No ownership scoping at the tool layer | `session_search` and history reads are scoped to the calling agent/channel; cross-session leakage is a defect, not a misconfiguration | R2 |
| 3 | — (storage layer shared) | FTS index, `search()` semantics, and the query builder are upstream's, unchanged; the fork's `replace_messages` trim hook is the only storage-side fork behavior | R3 — an upstream change to `search()` semantics is new semantics: report, do not silently adapt the tool to it |

## Merge notes

- **Verify post-merge by behavior, not by commit presence:** the
  `session_search` tool exists in the agent tool set; a keyword query
  returns IDs with session key + snippet; `sessions_history` with
  `around_message_id` returns messages around the anchor; a query from one
  agent/channel cannot return another's sessions. If any of these fails,
  the feature is gone even if the commits are present.
- **Tests:** the tool's tests and the anchored-read tests are fork tests;
  they must survive the merge. If upstream rewrote `sessions.rs`, re-add
  them — do not accept their absence as "upstream refactored it away".
- **Code anchors:** `crates/zeroclaw-tools/src/sessions.rs`
  (`session_search`, `sessions_history`, `around_message_id`).

## Sources

- Fork commits `775c0aab3` (message-level FTS search + anchored history
  read), `2f246a59d` (ownership scoping by caller agent/channel).
- Feature ticket: `zeroclaw-sessions-search-ticket.md` (message-level
  search + anchor-read loop, FTS5 `sessions_fts`).

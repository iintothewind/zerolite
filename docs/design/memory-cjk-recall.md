# Memory recall for CJK text

> **Status: specification.** This page defines how the SQLite memory backend
> indexes and queries Chinese, Japanese and Korean text: one FTS token per CJK
> character, queries expanded into two-character windows, the same terms used
> by the LIKE fallback, LIKE fallback scores graded by matched keywords, and
> a two-character minimum: a keyword query with no term of two or more
> letters, digits or CJK characters recalls nothing. ASCII index text is
> byte-for-byte unchanged; ASCII queries differ only in that terms with fewer
> than two letters or digits are dropped. For the merge rule, see
> `docs/merge_update_rules.md`. Decisions here are precedent: cite them as
> `memory-cjk-recall #N` in merge reports.

## Decision record: why this shape

- **R1 — Per-character tokens, not `trigram`.** Measured on SQLite 3.45 with
  the fork's corpus: under `trigram`, two-character words (`召回`, `上周`,
  `散步`) match nothing; the acceptance pair `你上周说的那个方案怎么样了` /
  `我上周决定用 Rust 重写召回层` shares only a two-character substring, so an
  OR of trigram windows still matches nothing; and English turns into
  substring matching (`rust` hits `trust`, `frustrated`) while one- and
  two-character tokens (`CI`, `AI`, `db`) stop being searchable. The fork
  already solved this for session search (`session-search.md` R3): keep
  `unicode61` and surround every CJK character with spaces, so each is one
  token. Memory reuses `zeroclaw_infra::session_fts::{index_text, is_cjk}`
  rather than copying the range table again.
- **R2 — Index text comes from one SQL function called by the sync
  triggers.** `zerolite_fts_text(text)` returns `index_text(text)` (non-text
  values pass through). The insert/update triggers call it, so no write site
  of `memories` changes. The table is `content=''` with
  `contentless_delete=1`: the delete trigger is plain SQL (`DELETE ... WHERE
  rowid = old.rowid`), so connections without the function can still delete
  (hygiene pruning) and update non-indexed columns; a stray
  `'rebuild'` fails loudly (`'rebuild' may not be used with a contentless
  fts5 table`) instead of silently re-indexing unsegmented text. Rebuilding
  is `delete-all` followed by re-inserting `zerolite_fts_text(key/content)`
  (`cjk_fts::rebuild_index`, used by `reindex`). The function is registered
  `DETERMINISTIC | INNOCUOUS`, never `DIRECTONLY`, so triggers can call it
  under `trusted_schema=OFF`.
- **R3 — Upstream creates, the fork converts.** `init_schema` and the
  upstream v3 migration (`zeroclaw-config` `schema/v2.rs`
  `migrate_sqlite_memory_to_v3`) keep the upstream external-content DDL; the
  v3 migration rebuilds that shape on every new store. `ensure_index` runs at
  the end of `SqliteMemory::open_and_initialize` and converts by shape (table
  marker `contentless_delete=1`, triggers calling
  `zerolite_fts_text(new.content)`, the plain delete trigger), not by a
  version number, inside an `IMMEDIATE` transaction. It is idempotent; two
  processes converting at once only repeat the work. Trigger names stay
  `memories_ai/ad/au` so upstream's `IF NOT EXISTS` DDL is a no-op afterwards.
- **R4 — Query expansion.** `cjk_fts::recall_terms` emits only terms with
  at least two letters, digits or CJK characters (`MIN_TERM_CHARS`); shorter
  ones are dropped. Whitespace words without CJK text, and prefix wildcards
  (`wild*`, `召回*`, `A股*`), are kept verbatim when long enough. A word with
  CJK text is read as units: each CJK character, and each stretch of other
  text between them. A stretch with two or more letters or digits (`Rust`)
  is a term on its own; one with none (`？`, `、`) only separates its
  neighbours; one with a single letter or digit counts as a single-character
  unit. Every two adjacent single-character units form one window, so
  `A股`, `T恤` and `3月` are searchable and `A股行情` gives `A股 股行 行情`.
  Windows are deduplicated and capped at 64 per query (`MAX_CJK_WINDOWS`;
  about the first 65 characters), bounding MATCH cost for long input.
  `fts5_term_query` turns each term into a phrase of its `index_text`
  tokens, OR-joined. The LIKE fallback takes its first 8 keywords from the
  same terms, so `上周` becomes `%上周%` instead of the whole sentence.
- **R5 — LIKE fallback score = matched-keyword share of the batch best.**
  Each fallback row scores `hits / best_hits`, where `hits` is the number of
  keywords found in its key or content (at least 1). This mirrors the
  BM25-only path's batch-max normalization: single-keyword queries still
  score 1.0, ordering stays `updated_at DESC`. A constant (the change
  request suggested 0.5) cannot tell strong from weak matches and, under the
  7-day half-life decay and the 0.4 injection threshold, drops every
  fallback hit about 2.25 days after it was written.
- **R6 — Two-character minimum.** Per-character tokens make any single CJK
  character searchable; frequent ones (`的`, `是`, `用`) and single Latin
  letters hit many unrelated rows, and keyword-only scores are normalized so
  the batch best is always 1.0, which `min_relevance_score` cannot filter.
  Acknowledgements such as `好` or `对` then injected unrelated memories.
  `cjk_fts::is_too_short_recall_query` (a keyword query whose
  `recall_terms` is empty) applies in three places: `recall_scoped` returns
  empty before computing the query embedding, so the vector leg cannot
  answer either; `MemoryRecallTool::execute` returns `No memories found. A
  query needs ...` without calling the backend, independent of backend and
  of the principal plane (whose `recall_for_principal` does its own
  whole-query `LIKE`); `render_memory_context` returns no context without
  calling the backend and emits no `MemoryRecall` event. An empty query or a
  bare `*` is a recent/time-only query, not a keyword query, and is never
  too short. The rule is length, not a stopword list: `好的` and `ok` still
  query.

### Consequences

- A connection that inserts into `memories` or updates its `key`/`content`
  must call `cjk_fts::register_fts_text_fn` first; otherwise SQLite reports
  `no such function: zerolite_fts_text`. In-tree production writers go
  through `open_and_initialize`, which registers it.
- Older binaries and the `sqlite3` CLI cannot write to a converted store.
  Rollback: drop `memories_ai`, `memories_ad`, `memories_au` and
  `memories_fts`, open the store with the old binary (it recreates the
  upstream shape) and run reindex.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | External-content `memories_fts`, raw `key`/`content` indexed, `unicode61` keeps a CJK run as one token | Contentless-delete table indexed through `zerolite_fts_text`, one token per CJK character | R1, R2 |
| 2 | Store open leaves the FTS shape as `init_schema`/v3 migration made it | `ensure_index` converts at the end of `open_and_initialize` | R3 |
| 3 | `reindex` runs `'rebuild'` | `delete-all` + re-insert through `cjk_fts::rebuild_index` | R2 |
| 4 | Each whitespace word is one phrase | CJK runs become two-character window phrases, capped at 64 | R4 |
| 5 | LIKE fallback matches `%whole word%`, score 1.0 | Same terms as FTS, score = matched-keyword share of batch best | R4, R5 |
| 6 | Any non-empty query is searched | A keyword query with no term of two or more characters returns nothing, in the backend, the `memory_recall` tool and injection | R6 |

## Merge notes

- **Verify post-merge by behavior:** a stored `我上周决定用 Rust 重写召回层`
  is found by the FTS leg for `召回` and `上周`
  (`SqliteMemory::fts5_search`, not only `recall`, which the LIKE fallback can
  satisfy), and `recall("你上周说的那个方案怎么样了")` returns it.
- **Conflict surface:** upstream adding a bare `Connection::open` that writes
  `memories` (register the function on it); a new `'rebuild'` call on
  `memories_fts`; changes to the FTS DDL or triggers in `init_schema` or the
  v3 migration (the shape markers in `index_is_current` must still detect
  the upstream form); changes to `fts5_term_query`, `fts5_search_scoped` or
  the LIKE fallback in `recall_scoped`; `MemoryRecallTool::execute` and the
  head of `render_memory_context` (the R6 gates).
- **Changing `is_cjk` ranges** changes both indexes' tokenization: rebuild the
  session index and the memory index (`reindex`) after such a change.
- **Code anchors:** `crates/zeroclaw-memory/src/cjk_fts.rs`
  (`register_fts_text_fn`, `index_is_current`, `rebuild_index`,
  `ensure_index`, `recall_terms`, `is_too_short_recall_query`);
  `crates/zeroclaw-memory/src/sqlite.rs` (`open_and_initialize`,
  `fts5_search_scoped`, `fts5_term_query`, the LIKE fallback and the
  too-short gate in `recall_scoped`, `reindex`);
  `crates/zeroclaw-tools/src/memory_recall.rs` (`execute`);
  `crates/zeroclaw-runtime/src/agent/memory_inject.rs`
  (`render_memory_context`); `zeroclaw_infra::session_fts::{index_text, is_cjk}`.
- **Tests that must survive:** `cjk_fts::tests::recall_terms_*`,
  `too_short_query_excludes_recent_recall_queries`;
  `recall_cjk_substring_matches`, `recall_cjk_natural_sentence_matches`,
  `recall_single_character_query_returns_nothing`,
  `recall_single_character_query_is_not_embedded`,
  `recall_pairs_a_letter_with_a_cjk_character`,
  `recall_cjk_scores_are_distinguishable`,
  `like_fallback_scores_follow_matched_keyword_share`,
  `opening_an_upstream_shaped_store_converts_the_index`,
  `a_new_store_is_recognized_as_current`,
  `reindex_keeps_cjk_substrings_searchable`;
  `memory_recall::tests::recall_refuses_a_query_without_a_two_character_term`;
  `memory_inject::tests::message_without_a_two_character_term_skips_recall`;
  and unchanged `fts5_syncs_on_*`, `reindex_rebuilds_fts`,
  `recall_prefix_wildcard_like_fallback_*`, the snapshot `hydrate_*` tests.

## Sources

- Change request:
  <https://gist.github.com/iintothewind/fe4b938d2ff31a0818bd339d8f4cf8e6>
  (CJK recall failure in the SQLite memory backend; proposed `trigram` and a
  constant 0.5 fallback score, both replaced here per R1 and R5).
- Prototype measurements (SQLite 3.45): acceptance sentence matched; 20k-row
  conversion 0.32 s; 64 windows on 20k rows of Zipf text about 45 ms, an
  uncapped 2000-character query 1.1 s.

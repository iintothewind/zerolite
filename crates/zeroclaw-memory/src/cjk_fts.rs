//! Full-text recall for Chinese, Japanese and Korean memory text.
//!
//! `memories_fts` uses the `unicode61` tokenizer, which would keep a run of
//! CJK text as one token. The index is therefore built from
//! `zeroclaw_infra::session_fts::index_text` (a space on both sides of every
//! CJK character, so each is its own token) through the SQL function
//! `zerolite_fts_text`, called by the sync triggers. Queries split CJK runs
//! into two-character windows, each a phrase of two tokens, OR-joined. A term
//! needs two or more letters, digits or CJK characters; a query with no such
//! term recalls nothing.
//!
//! Upstream code creates the external-content table and triggers; `ensure_index`
//! converts them to the contentless form below when a store is opened.

use anyhow::Context as _;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use zeroclaw_infra::session_fts::{index_text, recall_terms};

use crate::traits::is_recent_recall_query;

const DROP_INDEX_SQL: &str = "
    DROP TRIGGER IF EXISTS memories_ai;
    DROP TRIGGER IF EXISTS memories_ad;
    DROP TRIGGER IF EXISTS memories_au;
    DROP TABLE IF EXISTS memories_fts;
    CREATE VIRTUAL TABLE memories_fts USING fts5(
        key, content, content='', contentless_delete=1, tokenize='unicode61'
    );";

const CREATE_TRIGGERS_SQL: &str = "
    CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
        INSERT INTO memories_fts(rowid, key, content)
        VALUES (new.rowid, zerolite_fts_text(new.key), zerolite_fts_text(new.content));
    END;
    CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
        DELETE FROM memories_fts WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER memories_au AFTER UPDATE OF key, content ON memories BEGIN
        DELETE FROM memories_fts WHERE rowid = old.rowid;
        INSERT INTO memories_fts(rowid, key, content)
        VALUES (new.rowid, zerolite_fts_text(new.key), zerolite_fts_text(new.content));
    END;";

/// Register `zerolite_fts_text` on `conn`. Every connection that inserts into
/// `memories` or updates its `key`/`content` needs it: the triggers call it.
pub(crate) fn register_fts_text_fn(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "zerolite_fts_text",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            Ok(match ctx.get_raw(0) {
                ValueRef::Text(text) => Value::Text(index_text(&String::from_utf8_lossy(text))),
                ValueRef::Null => Value::Null,
                ValueRef::Integer(i) => Value::Integer(i),
                ValueRef::Real(r) => Value::Real(r),
                ValueRef::Blob(b) => Value::Blob(b.to_vec()),
            })
        },
    )
}

/// Whether the FTS table and its triggers already have the per-character
/// form. Judged by shape, not a version number: the upstream v3 migration
/// rebuilds them in the upstream form without touching any version marker.
pub(crate) fn index_is_current(conn: &Connection) -> rusqlite::Result<bool> {
    let matched: i64 = conn.query_row(
        "SELECT
            (SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories_fts'
                AND instr(sql, 'contentless_delete=1') > 0)
          + (SELECT COUNT(*) FROM sqlite_master WHERE type='trigger'
                AND name IN ('memories_ai','memories_au')
                AND instr(sql, 'zerolite_fts_text(new.content)') > 0)
          + (SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='memories_ad'
                AND instr(sql, 'DELETE FROM memories_fts WHERE rowid = old.rowid') > 0)",
        [],
        |row| row.get(0),
    )?;
    Ok(matched == 4)
}

/// Re-index every memory row. The caller owns the transaction.
pub(crate) fn rebuild_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "INSERT INTO memories_fts(memories_fts) VALUES('delete-all');
         INSERT INTO memories_fts(rowid, key, content)
         SELECT rowid, zerolite_fts_text(key), zerolite_fts_text(content) FROM memories;",
    )
}

/// Convert the FTS table and triggers to the per-character form unless they
/// already have it. Idempotent.
pub(crate) fn ensure_index(conn: &Connection) -> anyhow::Result<()> {
    let convert = || -> rusqlite::Result<()> {
        if index_is_current(conn)? {
            return Ok(());
        }
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        tx.execute_batch(DROP_INDEX_SQL)?;
        rebuild_index(&tx)?;
        tx.execute_batch(CREATE_TRIGGERS_SQL)?;
        tx.commit()
    };
    convert().context("SQLite memory FTS index conversion failed")
}

/// Whether `query` is a keyword query with no term of `MIN_TERM_CHARS` or
/// more characters ("好", "的", "a", "c++"). Such a query recalls nothing.
/// Recent/time-only queries (empty or a bare `*`) are not keyword queries
/// and are never too short.
pub fn is_too_short_recall_query(query: &str) -> bool {
    !is_recent_recall_query(query) && recall_terms(query).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_short_query_excludes_recent_recall_queries() {
        for query in ["的", "a", "好。", "好 的"] {
            assert!(is_too_short_recall_query(query), "{query:?}");
        }
        for query in ["", "   ", "*", "A股", "好的", "ok"] {
            assert!(!is_too_short_recall_query(query), "{query:?}");
        }
    }
}

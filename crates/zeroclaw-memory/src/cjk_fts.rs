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
use zeroclaw_infra::session_fts::{index_text, is_cjk};

use crate::traits::is_recent_recall_query;

/// Most two-character windows one query contributes to the keyword stage.
const MAX_CJK_WINDOWS: usize = 64;

/// Fewest letters, digits or CJK characters a keyword term may hold. Window
/// terms pair two single-character units, so this is also the window size.
const MIN_TERM_CHARS: usize = 2;

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

/// Keyword-stage terms for `query`. Every term holds at least
/// `MIN_TERM_CHARS` letters, digits or CJK characters; shorter ones are
/// dropped. Whitespace-separated words without CJK text, and prefix
/// wildcards, are kept verbatim. A word with CJK text is read as consecutive
/// units: each CJK character, and each stretch of other text between them. A
/// stretch with two or more letters or digits is a term on its own; one with
/// none separates its neighbours. Every two adjacent single-character units
/// (CJK characters, or a stretch with one letter or digit, as in `A股`) form
/// one window term. Windows are deduplicated and capped at `MAX_CJK_WINDOWS`
/// per query.
pub(crate) fn recall_terms(query: &str) -> Vec<String> {
    /// What one unit of a word with CJK text contributes.
    enum Unit {
        Single,
        Word,
        Break,
    }

    fn is_cjk_token(ch: char) -> bool {
        is_cjk(ch) && ch.is_alphanumeric()
    }
    fn letters_and_digits(text: &str) -> usize {
        text.chars().filter(|ch| ch.is_alphanumeric()).count()
    }

    let mut terms: Vec<String> = Vec::new();
    let mut windows = 0usize;

    for word in query.split_whitespace() {
        let prefix = word.strip_suffix('*').filter(|prefix| !prefix.is_empty());
        if prefix.is_some() || !word.chars().any(is_cjk_token) {
            if letters_and_digits(prefix.unwrap_or(word)) >= MIN_TERM_CHARS {
                terms.push(word.to_string());
            }
            continue;
        }

        // Units are consecutive slices of `word`; `previous_single` is the
        // start of the single-character unit right before the current one.
        let mut previous_single: Option<usize> = None;
        let mut chars = word.char_indices().peekable();
        while let Some((start, ch)) = chars.next() {
            let mut end = start + ch.len_utf8();
            let unit = if is_cjk_token(ch) {
                Unit::Single
            } else {
                while let Some(&(next, next_ch)) = chars.peek() {
                    if is_cjk_token(next_ch) {
                        break;
                    }
                    end = next + next_ch.len_utf8();
                    chars.next();
                }
                match letters_and_digits(&word[start..end]) {
                    0 => Unit::Break,
                    1 => Unit::Single,
                    _ => Unit::Word,
                }
            };
            match unit {
                Unit::Single => {
                    if let Some(from) = previous_single {
                        let window = &word[from..end];
                        if windows < MAX_CJK_WINDOWS && !terms.iter().any(|term| term == window) {
                            terms.push(window.to_string());
                            windows += 1;
                        }
                    }
                    previous_single = Some(start);
                }
                Unit::Word => {
                    terms.push(word[start..end].to_string());
                    previous_single = None;
                }
                Unit::Break => previous_single = None,
            }
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(query: &str) -> Vec<String> {
        recall_terms(query)
    }

    #[test]
    fn recall_terms_keeps_non_cjk_terms_verbatim() {
        assert_eq!(terms("quick dog wild* Q3"), ["quick", "dog", "wild*", "Q3"]);
        assert_eq!(terms("召回*"), ["召回*"]);
        assert_eq!(terms("A股*"), ["A股*"]);
        assert!(terms("   ").is_empty());
    }

    #[test]
    fn recall_terms_windows_cjk_runs() {
        assert_eq!(
            terms("你上周说的方案"),
            ["你上", "上周", "周说", "说的", "的方", "方案"]
        );
        assert_eq!(terms("用Rust重写"), ["Rust", "重写"]);
        assert_eq!(terms("怎么样了？"), ["怎么", "么样", "样了"]);
        assert_eq!(terms("苹果、香蕉"), ["苹果", "香蕉"]);
        assert_eq!(terms("好的"), ["好的"]);
        assert_eq!(terms("哈哈哈哈"), ["哈哈"]);
    }

    #[test]
    fn recall_terms_pairs_single_characters_across_scripts() {
        assert_eq!(terms("A股"), ["A股"]);
        assert_eq!(terms("T恤"), ["T恤"]);
        assert_eq!(terms("A股行情"), ["A股", "股行", "行情"]);
        assert_eq!(terms("3月5号开会"), ["3月", "月5", "5号", "号开", "开会"]);
        assert_eq!(terms("买A股"), ["买A", "A股"]);
    }

    #[test]
    fn recall_terms_drops_terms_under_two_characters() {
        for query in [
            "我", "的", "a", "x", "c++", "好。", "好 的", "我 a", "a*", "召*", "*",
        ] {
            assert!(terms(query).is_empty(), "{query:?} -> {:?}", terms(query));
        }
        assert_eq!(terms("plan b"), ["plan"]);
    }

    #[test]
    fn too_short_query_excludes_recent_recall_queries() {
        for query in ["的", "a", "好。", "好 的"] {
            assert!(is_too_short_recall_query(query), "{query:?}");
        }
        for query in ["", "   ", "*", "A股", "好的", "ok"] {
            assert!(!is_too_short_recall_query(query), "{query:?}");
        }
    }

    #[test]
    fn recall_terms_dedups_and_caps_windows() {
        let query: String = (0..200u32)
            .map(|i| char::from_u32(0x4E00 + i).unwrap())
            .collect();
        let windows = terms(&query);
        assert_eq!(windows.len(), MAX_CJK_WINDOWS);
        let mut unique = windows.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), MAX_CJK_WINDOWS);
    }
}

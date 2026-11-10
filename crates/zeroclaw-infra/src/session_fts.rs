//! Text the session full-text index is built from and queried with.
//!
//! The index is a contentless FTS5 table using the `unicode61` tokenizer,
//! which splits on whitespace and punctuation only: a run of Chinese,
//! Japanese or Korean text would be one token and only match whole. Every
//! CJK, kana and Hangul character is therefore surrounded by spaces before
//! it is indexed, so each one is its own token, and a query term becomes a
//! phrase of its characters, which matches any substring. Snippets are built
//! here as well, since a contentless table cannot return them.

use std::borrow::Cow;

use crate::session_backend::RowKind;

/// Characters before the first match a snippet keeps.
const SNIPPET_BEFORE: usize = 40;
/// Characters after the first match a snippet keeps.
const SNIPPET_AFTER: usize = 80;
/// Length of a snippet with no literal occurrence to centre on.
const SNIPPET_HEAD: usize = 120;

/// Text the index and the snippet are built from: tool calls index their
/// name with their arguments, every other kind indexes its content.
pub fn index_source<'a>(kind: RowKind, tool_name: Option<&str>, content: &'a str) -> Cow<'a, str> {
    match (kind, tool_name) {
        (RowKind::ToolCall, Some(name)) => Cow::Owned(format!("{name} {content}")),
        _ => Cow::Borrowed(content),
    }
}

/// `source` with a space on both sides of every CJK/kana/Hangul character, so
/// unicode61 tokenizes them one per token.
pub fn index_text(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + source.len() / 2);
    for ch in source.chars() {
        if is_cjk(ch) {
            out.push(' ');
            out.push(ch);
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// FTS5 MATCH expression for a keyword string: each whitespace-separated
/// term becomes one phrase of its `index_text` tokens (quotes doubled), terms
/// are OR-joined. Empty when no terms.
pub fn match_query(keyword: &str) -> String {
    keyword
        .split_whitespace()
        .map(|term| {
            let tokens = index_text(term);
            let phrase = tokens.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("\"{}\"", phrase.replace('"', "\"\""))
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Display excerpt: the first case-insensitive occurrence of any term in
/// `source` with up to 40 chars before and 80 after, cut edges marked `…`,
/// every term occurrence inside the window wrapped in `<b>…</b>`. No
/// occurrence (punctuation-separated phrase match): the first 120 chars.
pub fn snippet(source: &str, keyword: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let folded: Vec<char> = chars.iter().map(|&ch| fold(ch)).collect();
    let terms: Vec<Vec<char>> = keyword
        .split_whitespace()
        .map(|term| term.chars().map(fold).collect())
        .collect();

    let Some((first, first_len)) = next_occurrence(&folded, &terms, 0) else {
        let mut out: String = chars.iter().take(SNIPPET_HEAD).collect();
        if chars.len() > SNIPPET_HEAD {
            out.push('…');
        }
        return out;
    };

    let start = first.saturating_sub(SNIPPET_BEFORE);
    let end = (first + first_len + SNIPPET_AFTER).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut at = start;
    while at < end {
        match next_occurrence(&folded[..end], &terms, at) {
            Some((pos, len)) => {
                out.extend(&chars[at..pos]);
                out.push_str("<b>");
                out.extend(&chars[pos..pos + len]);
                out.push_str("</b>");
                at = pos + len;
            }
            None => {
                out.extend(&chars[at..end]);
                at = end;
            }
        }
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// Leftmost occurrence of any term at or after `from`, the longest term
/// winning at one position. Returns `(position, length)` in chars.
fn next_occurrence(haystack: &[char], terms: &[Vec<char>], from: usize) -> Option<(usize, usize)> {
    (from..haystack.len()).find_map(|pos| {
        terms
            .iter()
            .filter(|term| !term.is_empty() && haystack[pos..].starts_with(term))
            .map(Vec::len)
            .max()
            .map(|len| (pos, len))
    })
}

/// Case fold that keeps one char per char, so indices stay aligned with the
/// original text.
fn fold(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

/// The ranges of `zeroclaw-runtime` `history.rs::is_cjk_kana_or_hangul`,
/// duplicated because infra cannot depend on runtime.
fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{1100}'..='\u{11FF}' // Hangul Jamo
            | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
            | '\u{3040}'..='\u{309F}' // Hiragana
            | '\u{30A0}'..='\u{30FF}' // Katakana
            | '\u{3100}'..='\u{312F}' // Bopomofo
            | '\u{3130}'..='\u{318F}' // Hangul Compatibility Jamo
            | '\u{3400}'..='\u{4DBF}' // CJK Ext A
            | '\u{4E00}'..='\u{9FFF}' // CJK Unified
            | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
            | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
            | '\u{FF66}'..='\u{FF9D}' // Halfwidth katakana
            | '\u{20000}'..='\u{2FA1F}' // CJK Ext B-F (approx)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_text_splits_cjk_into_one_token_per_character() {
        let text = index_text("数据库 query");
        assert_eq!(
            text.split_whitespace().collect::<Vec<_>>(),
            ["数", "据", "库", "query"]
        );
    }

    #[test]
    fn match_query_builds_one_phrase_per_term() {
        assert_eq!(match_query("查询 shell"), "\"查 询\" OR \"shell\"");
        assert_eq!(match_query("say\"hi"), "\"say\"\"hi\"");
        assert_eq!(match_query("   "), "");
    }

    #[test]
    fn snippet_wraps_every_occurrence_and_marks_cut_edges() {
        let source = format!("{}Alpha beta alpha{}", "x".repeat(50), "y".repeat(100));
        let out = snippet(&source, "alpha");
        assert!(out.starts_with('…'), "{out}");
        assert!(out.ends_with('…'), "{out}");
        assert!(out.contains("<b>Alpha</b> beta <b>alpha</b>"), "{out}");
        assert_eq!(out.matches('x').count(), 40);

        assert_eq!(
            snippet("我们讨论查询效率", "查询"),
            "我们讨论<b>查询</b>效率"
        );
        assert_eq!(snippet("short text", "absent"), "short text");
    }

    #[test]
    fn tool_calls_index_their_name_with_the_arguments() {
        let source = index_source(RowKind::ToolCall, Some("shell"), "{\"command\":\"ls\"}");
        assert!(source.starts_with("shell "), "{source}");
        assert_eq!(
            index_source(RowKind::ToolResult, Some("shell"), "out"),
            "out"
        );
    }
}

//! Text the session full-text index is built from and queried with.
//!
//! The index is a contentless FTS5 table using the `unicode61` tokenizer,
//! which splits on whitespace and punctuation only: a run of Chinese,
//! Japanese or Korean text would be one token and only match whole. Every
//! CJK, kana and Hangul character is therefore surrounded by spaces before
//! it is indexed, so each one is its own token, and a query term becomes a
//! phrase of its characters, which matches any substring. Snippets are built
//! here as well, since a contentless table cannot return them. When a keyword
//! matches nothing as written, its two-character terms (`recall_terms`) are
//! matched instead and ranked by BM25.

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

/// Most two-character windows one query contributes to the keyword stage.
const MAX_CJK_WINDOWS: usize = 64;

/// Fewest letters, digits or CJK characters a keyword term may hold. Window
/// terms pair two single-character units, so this is also the window size.
const MIN_TERM_CHARS: usize = 2;

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
pub fn recall_terms(query: &str) -> Vec<String> {
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

/// One FTS5 phrase of `term`'s `index_text` tokens, quotes doubled.
fn phrase(term: &str) -> String {
    let tokens = index_text(term);
    let phrase = tokens.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("\"{}\"", phrase.replace('"', "\"\""))
}

/// FTS5 MATCH expression for a keyword string: each whitespace-separated
/// term becomes one phrase of its `index_text` tokens (quotes doubled), terms
/// are OR-joined. Empty when no terms.
pub fn match_query(keyword: &str) -> String {
    keyword
        .split_whitespace()
        .map(phrase)
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// FTS5 MATCH expression for the approximate tier: the `recall_terms` of
/// `keyword`, one phrase each, OR-joined. Empty when there are none.
pub fn approximate_match_query(keyword: &str) -> String {
    recall_terms(keyword)
        .iter()
        .map(|term| phrase(term))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Whether the approximate tier can match rows the exact tier cannot:
/// `recall_terms(keyword)` holds a term that is not one of the
/// whitespace-separated terms of `keyword`.
pub fn has_window_terms(keyword: &str) -> bool {
    let words: Vec<&str> = keyword.split_whitespace().collect();
    recall_terms(keyword)
        .iter()
        .any(|term| !words.contains(&term.as_str()))
}

/// Display excerpt: the occurrence of a term in `source` whose following span
/// holds the most distinct terms (the first occurrence for a single term),
/// case-insensitive, with up to 40 chars before and 80 after, cut edges
/// marked `…`, every term occurrence inside the window wrapped in
/// `<b>…</b>`. No occurrence (punctuation-separated phrase match): the first
/// 120 chars.
pub fn snippet(source: &str, keyword: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let folded: Vec<char> = chars.iter().map(|&ch| fold(ch)).collect();
    let terms: Vec<Vec<char>> = keyword
        .split_whitespace()
        .map(|term| term.chars().map(fold).collect())
        .collect();

    // Every occurrence, left to right without overlap.
    let mut occurrences: Vec<(usize, usize, usize)> = Vec::new();
    let mut at = 0;
    while let Some(found) = next_occurrence(&folded, &terms, at) {
        occurrences.push(found);
        at = found.0 + found.1;
    }
    // Centre on the occurrence whose span holds the most distinct terms; the
    // earliest wins a tie, so a single-term keyword centres on its first.
    let anchor = (0..occurrences.len()).max_by_key(|&i| {
        let (pos, len, _) = occurrences[i];
        let span_end = pos + len + SNIPPET_AFTER;
        let mut seen: Vec<usize> = Vec::new();
        for &(p, l, term) in &occurrences[i..] {
            if p + l > span_end {
                break;
            }
            if !seen.contains(&term) {
                seen.push(term);
            }
        }
        (seen.len(), std::cmp::Reverse(i))
    });
    let Some((first, first_len, _)) = anchor.map(|i| occurrences[i]) else {
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
            Some((pos, len, _)) => {
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
/// winning at one position. Returns `(position, length, term index)`,
/// position and length in chars.
fn next_occurrence(
    haystack: &[char],
    terms: &[Vec<char>],
    from: usize,
) -> Option<(usize, usize, usize)> {
    (from..haystack.len()).find_map(|pos| {
        terms
            .iter()
            .enumerate()
            .filter(|(_, term)| !term.is_empty() && haystack[pos..].starts_with(term))
            .max_by_key(|(_, term)| term.len())
            .map(|(index, term)| (pos, term.len(), index))
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

    #[test]
    fn approximate_match_query_joins_window_phrases() {
        assert_eq!(
            approximate_match_query("斯坦利散步"),
            "\"斯 坦\" OR \"坦 利\" OR \"利 散\" OR \"散 步\""
        );
        assert_eq!(approximate_match_query("的"), "");
        for keyword in ["斯坦利散步", "A股行情", "用Rust重写"] {
            assert!(has_window_terms(keyword), "{keyword:?}");
        }
        for keyword in ["纠缠", "好的", "quartz zz", "a zzqq", "的"] {
            assert!(!has_window_terms(keyword), "{keyword:?}");
        }
    }

    #[test]
    fn snippet_centres_on_the_span_with_the_most_distinct_terms() {
        let source = format!("公园{}斯坦利公园散步{}", "x".repeat(200), "y".repeat(200));
        let out = snippet(&source, "斯坦 坦利 公园 散步");
        assert!(out.starts_with('…'), "{out}");
        assert!(out.ends_with('…'), "{out}");
        assert!(out.contains("<b>斯坦</b>利<b>公园</b><b>散步</b>"), "{out}");
        assert_eq!(out.matches('x').count(), 40);
    }
}

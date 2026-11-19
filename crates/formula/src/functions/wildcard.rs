//! Pure `no_std` Excel wildcard matcher (`*`, `?`, `~*`, `~?`, `~~`).
//!
//! Used by `MATCH(..., 0)`, `VLOOKUP(..., FALSE)`, `HLOOKUP(..., FALSE)`,
//! `COUNTIF`, `SUMIF`, `AVERAGEIF`, and `SEARCH`. Matching is linear-time in
//! `pattern * text` without backtracking blowup.

use alloc::vec::Vec;

/// Compiled token in an Excel wildcard pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WildToken {
    /// Matches 0 or more arbitrary characters (`*`).
    AnyMany,
    /// Matches any single character (`?`).
    AnyOne,
    /// Matches a literal character (already lowercased).
    Char(char),
}

/// Compiles an Excel wildcard pattern into lowercase [`WildToken`]s, collapsing
/// consecutive `*` tokens. Returns `(tokens, has_wildcards_or_escapes)`.
#[must_use]
pub fn compile_wildcard(pattern: &str) -> (Vec<WildToken>, bool) {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut special = false;
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '~' => {
                special = true;
                if let Some(escaped) = chars.next() {
                    for lc in escaped.to_lowercase() {
                        tokens.push(WildToken::Char(lc));
                    }
                } else {
                    // Trailing standalone `~` in Excel matches literal `~`.
                    tokens.push(WildToken::Char('~'));
                }
            }
            '*' => {
                special = true;
                if tokens.last() != Some(&WildToken::AnyMany) {
                    tokens.push(WildToken::AnyMany);
                }
            }
            '?' => {
                special = true;
                tokens.push(WildToken::AnyOne);
            }
            other => {
                for lc in other.to_lowercase() {
                    tokens.push(WildToken::Char(lc));
                }
            }
        }
    }
    (tokens, special)
}

/// Unescapes `~*`, `~?`, `~~` when a pattern contains `~` without unescaped `*`
/// or `?`, or returns `true` if `candidate` matches the full compiled `tokens`
/// case-insensitively.
#[must_use]
pub fn wildcard_matches_full(tokens: &[WildToken], candidate: &str) -> bool {
    let text_chars: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    match_tokens_slice(tokens, &text_chars, true)
}

/// Finds the 0-based character index (in `within.chars()`, starting at
/// `start_char_idx`) of the first substring matching `pattern` under Excel
/// `SEARCH` rules (case-insensitive with `*`, `?`, `~`).
#[must_use]
pub fn wildcard_search_substring(
    pattern: &str,
    within: &str,
    start_char_idx: usize,
) -> Option<usize> {
    let within_chars: Vec<char> = within.chars().collect();
    if start_char_idx > within_chars.len() {
        return None;
    }
    if pattern.is_empty() {
        return Some(start_char_idx);
    }

    let (tokens, _) = compile_wildcard(pattern);
    // Map each original character position in `within` to its lowercased slice
    // offset so Unicode case-folding (even 1->N) reports exact 1-based char indices.
    let mut lower_chars: Vec<char> = Vec::with_capacity(within_chars.len());
    let mut char_to_lower_idx: Vec<usize> =
        Vec::with_capacity(within_chars.len().saturating_add(1));
    for &ch in &within_chars {
        char_to_lower_idx.push(lower_chars.len());
        lower_chars.extend(ch.to_lowercase());
    }
    char_to_lower_idx.push(lower_chars.len());

    for pos in start_char_idx..=within_chars.len() {
        let Some(&lo_start) = char_to_lower_idx.get(pos) else {
            continue;
        };
        let Some(tail) = lower_chars.get(lo_start..) else {
            continue;
        };
        if match_tokens_slice(&tokens, tail, false) {
            return Some(pos);
        }
    }
    None
}

/// Iterative two-pointer glob matcher (`O(|tokens| * |text|)` worst case, zero
/// recursion). When `require_full == false`, matches any prefix of `text`.
fn match_tokens_slice(tokens: &[WildToken], text: &[char], require_full: bool) -> bool {
    let mut ti = 0usize;
    let mut si = 0usize;
    let mut star_ti: Option<usize> = None;
    let mut star_si = 0usize;

    while si < text.len() {
        if !require_full && ti == tokens.len() {
            return true;
        }
        match tokens.get(ti) {
            Some(WildToken::AnyOne) => {
                ti = ti.saturating_add(1);
                si = si.saturating_add(1);
            }
            Some(WildToken::Char(c)) if text.get(si) == Some(c) => {
                ti = ti.saturating_add(1);
                si = si.saturating_add(1);
            }
            Some(WildToken::AnyMany) => {
                star_ti = Some(ti);
                star_si = si;
                ti = ti.saturating_add(1);
                if !require_full && ti == tokens.len() {
                    return true;
                }
            }
            _ => {
                if let Some(st) = star_ti {
                    ti = st.saturating_add(1);
                    star_si = star_si.saturating_add(1);
                    si = star_si;
                } else {
                    return false;
                }
            }
        }
    }

    while matches!(tokens.get(ti), Some(WildToken::AnyMany)) {
        ti = ti.saturating_add(1);
    }
    ti == tokens.len()
}

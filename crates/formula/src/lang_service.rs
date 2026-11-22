//! Formula-bar language service (`14-m1b.md` 6.1).
//!
//! Computes syntax highlight spans, error-recovering diagnostics, function and
//! live-worksheet completions, active-argument signature help, colored grid
//! reference overlays (`0..7`), point-and-click insertion state, and `F4`
//! `$`-anchor cycling in a single `< 0.2 ms` pass in Rust so the web client
//! needs zero heavy JavaScript language-service libraries.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::id::SheetId;
use reticula_core::ops::{MAX_FORMULA_CHARS, Workbook};
use reticula_core::value::CalcError;
use serde::{Deserialize, Serialize};

use crate::address::{
    CellAddr, parse_cell_addr, parse_col_only, parse_row_only, write_col_letters,
    write_sheet_prefix,
};
use crate::ast::MAX_FUNCTION_ARGS;
use crate::binder::BoundFormula;
use crate::functions::catalog::{FUNCTION_CATALOG, FunctionMeta, lookup_function_meta};

/// Number of cycling reference highlight palette slots (`0..8`).
pub const REF_COLOR_PALETTE_LEN: u8 = 8;

/// Maximum completion candidates returned per query.
const MAX_COMPLETIONS: usize = 12;

/// Semantic category of a syntax token span for CodeMirror 6 decoration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyntaxTokenKind {
    /// Leading `'='` formula introducer.
    Equals,
    /// Built-in or user-typed function identifier before `'('`.
    Function,
    /// Cell, area, full-row, or full-column reference (including sheet prefix).
    Reference,
    /// Standalone or incomplete sheet prefix.
    SheetRef,
    /// Numeric literal.
    Number,
    /// Double-quoted string literal.
    String,
    /// `TRUE` or `FALSE` keyword.
    Boolean,
    /// `#DIV/0!`, `#N/A`, `#REF!`, etc.
    Error,
    /// Arithmetic, comparison, concatenation, or `@` / `%` operator.
    Operator,
    /// `'('` or `')'`.
    Paren,
    /// `','`, `';'`, `'{'`, `'}'`, `':'`.
    Punctuation,
    /// Unrecognised identifier or unexpected character.
    Unknown,
}

/// One syntax-highlighted UTF-16 code-unit span in `source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxTokenSpan {
    /// Inclusive UTF-16 start offset in `source`.
    pub start_utf16: u32,
    /// Exclusive UTF-16 end offset in `source`.
    pub end_utf16: u32,
    /// Token category.
    pub kind: SyntaxTokenKind,
    /// Reference color index (`0..7`) when `kind == Reference` and resolved.
    pub color_index: Option<u8>,
    /// `true` when this parenthesis is part of the active cursor-matched pair.
    pub matched_bracket: bool,
}

/// Non-fatal syntax or semantic diagnostic underlined in the formula bar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormulaDiagnostic {
    /// Inclusive UTF-16 start offset.
    pub start_utf16: u32,
    /// Exclusive UTF-16 end offset.
    pub end_utf16: u32,
    /// Concise English explanation.
    pub message: String,
}

/// Kind of autocomplete candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompletionKind {
    /// Built-in spreadsheet function.
    Function,
    /// Live worksheet qualifier (`Sheet2!` or `'Sheet 2'!`).
    Sheet,
    /// Workbook- or worksheet-scoped defined named range.
    NamedRange,
}

/// Autocomplete suggestion item for the formula bar popover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItem {
    /// Display name (`"VLOOKUP"` or `"Rates"`).
    pub label: String,
    /// Text inserted over `[replace_start_utf16, replace_end_utf16)` (`"VLOOKUP("` or `"'Sheet 2'!"`).
    pub insert_text: String,
    /// UTF-16 start offset of prefix to replace.
    pub replace_start_utf16: u32,
    /// UTF-16 end offset of prefix to replace.
    pub replace_end_utf16: u32,
    /// Candidate kind.
    pub kind: CompletionKind,
    /// Concise parameter signature (`"(lookup_value, table_array, ...)"` or `"Worksheet"`).
    pub signature: String,
    /// One-sentence description.
    pub summary: String,
}

/// Active function call parameter tooltip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelp {
    /// Uppercase function name (`"VLOOKUP"`).
    pub name: String,
    /// Parameter labels in order.
    pub params: Vec<String>,
    /// 0-based index into `params` for the argument currently being edited.
    pub active_param: u32,
    /// One-sentence summary of the function.
    pub summary: String,
}

/// Resolved cell/range reference highlighted simultaneously in the formula bar
/// and on the grid canvas (`14-m1b.md` 6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormulaRefHighlight {
    /// Inclusive UTF-16 start offset in `source`.
    pub start_utf16: u32,
    /// Exclusive UTF-16 end offset in `source`.
    pub end_utf16: u32,
    /// Palette index in `0..8`.
    pub color_index: u8,
    /// Target `SheetId`.
    pub sheet: u32,
    /// Inclusive 0-based top visible row on `sheet`.
    pub first_row: u32,
    /// Inclusive 0-based bottom visible row on `sheet`.
    pub last_row: u32,
    /// Inclusive 0-based left visible column on `sheet`.
    pub first_col: u32,
    /// Inclusive 0-based right visible column on `sheet`.
    pub last_col: u32,
}

/// Complete single-pass language service output for a formula draft.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormulaAnalysis {
    /// Whether `source` begins with `'='` (formula editing mode).
    pub is_formula: bool,
    /// Highlighted token spans in ascending UTF-16 order.
    pub tokens: Vec<SyntaxTokenSpan>,
    /// Error-recovering syntax and semantic diagnostics.
    pub diagnostics: Vec<FormulaDiagnostic>,
    /// Prefix-matched function and live-worksheet completions at `cursor_utf16`.
    pub completions: Vec<CompletionItem>,
    /// Active enclosing function call signature, if any.
    pub signature: Option<SignatureHelp>,
    /// Colored live sheet references (`0..7`).
    pub references: Vec<FormulaRefHighlight>,
    /// `true` when clicking/dragging cells on the grid should insert or replace
    /// a reference at `cursor_utf16` (`14-m1b.md` 6.1).
    pub can_insert_ref: bool,
    /// When `cursor_utf16` sits inside or at the end of an existing reference
    /// token, gives `(start_utf16, end_utf16)` to replace on point-and-click.
    pub active_ref_span: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
enum RawKind {
    Equals,
    Number { digits_only: bool },
    StringLit,
    SheetPrefix { name: String, terminated: bool },
    ErrorLit,
    Ident,
    Colon,
    Comma,
    Semicolon,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Hash,
    Operator,
    Unknown,
}

#[derive(Clone, Debug)]
struct RawTok {
    start_byte: usize,
    end_byte: usize,
    start_utf16: u32,
    end_utf16: u32,
    kind: RawKind,
}

impl RawTok {
    fn slice<'a>(&self, src: &'a str) -> &'a str {
        src.get(self.start_byte..self.end_byte).unwrap_or("")
    }
}

/// Analyses `source` at cursor position `cursor_utf16` against `workbook` and
/// `host_sheet`, never panicking on arbitrary or incomplete input.
#[must_use]
pub fn analyze_formula(
    source: &str,
    cursor_utf16: u32,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> FormulaAnalysis {
    analyze_formula_with_spills(source, cursor_utf16, workbook, host_sheet, None)
}

/// Spill-aware variant of [`analyze_formula`] that expands `A1#` spill-range
/// highlights to the live materialized footprint in `spills`.
#[must_use]
pub fn analyze_formula_with_spills(
    source: &str,
    cursor_utf16: u32,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    spills: Option<&crate::spill::SpillRegistry>,
) -> FormulaAnalysis {
    let Some(after_eq) = source.strip_prefix('=') else {
        return FormulaAnalysis::default();
    };
    let total_utf16 = count_utf16(source);
    let cursor = cursor_utf16.min(total_utf16);

    let mut diagnostics = Vec::new();
    if after_eq.chars().count() > MAX_FORMULA_CHARS {
        diagnostics.push(FormulaDiagnostic {
            start_utf16: 0,
            end_utf16: total_utf16,
            message: format!("Formula exceeds maximum length of {MAX_FORMULA_CHARS} characters."),
        });
    }

    let raw_tokens = scan_tolerant(source, &mut diagnostics);
    if raw_tokens.len() == 1 {
        // Bare `"="` -> valid in-progress formula waiting for an expression!
        diagnostics.push(FormulaDiagnostic {
            start_utf16: 0,
            end_utf16: 1,
            message: String::from("Expected expression after '='."),
        });
        let completions = build_completions(source, &raw_tokens, cursor, workbook);
        return FormulaAnalysis {
            is_formula: true,
            tokens: alloc::vec![SyntaxTokenSpan {
                start_utf16: 0,
                end_utf16: 1,
                kind: SyntaxTokenKind::Equals,
                color_index: None,
                matched_bracket: false,
            }],
            diagnostics,
            completions,
            signature: None,
            references: Vec::new(),
            can_insert_ref: cursor >= 1,
            active_ref_span: None,
        };
    }

    // Group reference token sequences (`[SheetPrefix?][Coord](:[Coord])?` or `A1#`) when
    // NOT immediately followed by `(`.
    let groups = find_ref_token_groups(source, &raw_tokens);
    let (mut spans, mut references) =
        classify_tokens_and_refs(source, &raw_tokens, workbook, host_sheet, &mut diagnostics);

    enrich_named_ranges_and_spills(
        source,
        &raw_tokens,
        &groups,
        workbook,
        host_sheet,
        spills,
        &mut spans,
        &mut references,
    );

    // Match parentheses, report unclosed/unmatched parens, check function names &
    // arities, and compute active signature help!
    let signature =
        analyze_calls_and_parens(source, &raw_tokens, &mut spans, cursor, &mut diagnostics);

    // Diagnose incomplete binary operators or trailing `:`, `,` at the end.
    diagnose_incomplete_operators(source, &raw_tokens, &mut diagnostics);

    let mut completions = build_completions(source, &raw_tokens, cursor, workbook);
    append_named_range_completions(
        source,
        &raw_tokens,
        cursor,
        workbook,
        host_sheet,
        &mut completions,
    );
    let (can_insert_ref, active_ref_span) =
        compute_ref_insertion_state(&raw_tokens, &spans, cursor);

    FormulaAnalysis {
        is_formula: true,
        tokens: spans,
        diagnostics,
        completions,
        signature,
        references,
        can_insert_ref,
        active_ref_span,
    }
}

/// Cycles `$` anchor locks on the cell or range reference containing (or
/// immediately preceding) `cursor_utf16` following Excel's `F4` order:
/// `A1 -> $A$1 -> A$1 -> $A1 -> A1` (and symmetrically for `A1:B2`, `A:C`,
/// `1:5`), preserving any worksheet qualifier (`'Sheet 2'!`).
///
/// Returns `(updated_formula, new_sel_start_utf16, new_sel_end_utf16)` when a
/// reference was found at `cursor_utf16`.
#[must_use]
pub fn toggle_reference_dollar(source: &str, cursor_utf16: u32) -> Option<(String, u32, u32)> {
    if !source.starts_with('=') {
        return None;
    }
    let mut dummy_diags = Vec::new();
    let raw_tokens = scan_tolerant(source, &mut dummy_diags);
    let groups = find_ref_token_groups(source, &raw_tokens);

    let target = groups
        .into_iter()
        .find(|g| cursor_utf16 >= g.start_utf16 && cursor_utf16 <= g.end_utf16)?;

    let prefix_slice = match target.sheet_tok {
        Some(st_idx) => raw_tokens.get(st_idx)?.slice(source),
        None => "",
    };
    let first_slice = raw_tokens.get(target.first_coord_tok)?.slice(source);
    let toggled_first = cycle_single_coord(first_slice)?;
    let spill_suffix = if target.has_spill_hash { "#" } else { "" };
    let replacement = if let Some(second_idx) = target.second_coord_tok {
        let second_slice = raw_tokens.get(second_idx)?.slice(source);
        let toggled_second = cycle_single_coord(second_slice)?;
        format!("{prefix_slice}{toggled_first}:{toggled_second}{spill_suffix}")
    } else {
        format!("{prefix_slice}{toggled_first}{spill_suffix}")
    };

    let before = source.get(..target.start_byte)?;
    let after = source.get(target.end_byte..)?;
    let mut out = String::with_capacity(
        before
            .len()
            .saturating_add(replacement.len())
            .saturating_add(after.len()),
    );
    out.push_str(before);
    out.push_str(&replacement);
    out.push_str(after);

    let repl_utf16 = count_utf16(&replacement);
    let new_end = target.start_utf16.saturating_add(repl_utf16);
    Some((out, target.start_utf16, new_end))
}

/// Formats a point-and-click cell or rectangular range selection on
/// `target_sheet` for insertion into a formula hosted on `host_sheet`
/// (e.g. `"B2"`, `"B2:D10"`, `"Rates!B2"`, `"'Sheet 2'!B2:D10"`).
#[must_use]
pub fn format_sheet_range_ref(
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    target_sheet: SheetId,
    r1: u32,
    c1: u32,
    r2: u32,
    c2: u32,
) -> String {
    let mut out = String::new();
    if target_sheet != host_sheet
        && let Some(sh) = workbook.sheet(target_sheet)
    {
        write_sheet_prefix(Some(sh.name()), &mut out);
    }
    let top = r1.min(r2);
    let bottom = r1.max(r2);
    let left = c1.min(c2);
    let right = c1.max(c2);
    CellAddr {
        col: left,
        row: top,
        abs_col: false,
        abs_row: false,
    }
    .write_a1(&mut out);
    if top != bottom || left != right {
        out.push(':');
        CellAddr {
            col: right,
            row: bottom,
            abs_col: false,
            abs_row: false,
        }
        .write_a1(&mut out);
    }
    out
}

// ------------------------------------------------------------ tolerant scanner

fn count_utf16(s: &str) -> u32 {
    let mut n: u32 = 0;
    for ch in s.chars() {
        let w = if ch.len_utf16() == 2 { 2 } else { 1 };
        n = n.saturating_add(w);
    }
    n
}

fn build_byte_to_utf16_map(s: &str) -> Vec<u32> {
    let mut map = alloc::vec![0_u32; s.len().saturating_add(1)];
    let mut u16_pos: u32 = 0;
    for (byte_idx, ch) in s.char_indices() {
        if let Some(slot) = map.get_mut(byte_idx) {
            *slot = u16_pos;
        }
        let w = if ch.len_utf16() == 2 { 2 } else { 1 };
        u16_pos = u16_pos.saturating_add(w);
        let next_b = byte_idx.saturating_add(ch.len_utf8());
        if let Some(slot) = map.get_mut(next_b) {
            *slot = u16_pos;
        }
    }
    map
}

#[allow(clippy::too_many_lines)]
fn scan_tolerant(source: &str, diagnostics: &mut Vec<FormulaDiagnostic>) -> Vec<RawTok> {
    let u16_map = build_byte_to_utf16_map(source);
    let u16_at = |b: usize| -> u32 { u16_map.get(b).copied().unwrap_or(0) };

    let mut tokens = Vec::new();
    // Leading `'='` is guaranteed by caller.
    tokens.push(RawTok {
        start_byte: 0,
        end_byte: 1,
        start_utf16: 0,
        end_utf16: 1,
        kind: RawKind::Equals,
    });

    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut pos = 1_usize;

    while pos < len {
        let Some(&b) = bytes.get(pos) else {
            break;
        };
        if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
            pos = pos.saturating_add(1);
            continue;
        }

        let start = pos;
        match b {
            b':' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::Colon,
                });
            }
            b',' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::Comma,
                });
            }
            b';' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::Semicolon,
                });
            }
            b'(' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::LParen,
                });
            }
            b')' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::RParen,
                });
            }
            b'{' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::LBrace,
                });
            }
            b'}' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::RBrace,
                });
            }
            b'+' | b'-' | b'*' | b'/' | b'^' | b'%' | b'&' | b'@' | b'=' => {
                pos = pos.saturating_add(1);
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::Operator,
                });
            }
            b'<' => {
                let next = bytes.get(pos.saturating_add(1)).copied();
                pos = if next == Some(b'>') || next == Some(b'=') {
                    pos.saturating_add(2)
                } else {
                    pos.saturating_add(1)
                };
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::Operator,
                });
            }
            b'>' => {
                pos = if bytes.get(pos.saturating_add(1)) == Some(&b'=') {
                    pos.saturating_add(2)
                } else {
                    pos.saturating_add(1)
                };
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: u16_at(start),
                    end_utf16: u16_at(pos),
                    kind: RawKind::Operator,
                });
            }
            b'"' => {
                let (end_p, terminated) = scan_string_tolerant(source, start);
                pos = end_p;
                let s_u16 = u16_at(start);
                let e_u16 = u16_at(pos);
                if !terminated {
                    diagnostics.push(FormulaDiagnostic {
                        start_utf16: s_u16,
                        end_utf16: e_u16,
                        message: String::from(
                            "Unterminated string literal (missing closing '\"').",
                        ),
                    });
                }
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: s_u16,
                    end_utf16: e_u16,
                    kind: RawKind::StringLit,
                });
            }
            b'\'' => {
                let (name, end_p, terminated) = scan_quoted_sheet_tolerant(source, start);
                pos = end_p;
                let s_u16 = u16_at(start);
                let e_u16 = u16_at(pos);
                if !terminated {
                    diagnostics.push(FormulaDiagnostic {
                        start_utf16: s_u16,
                        end_utf16: e_u16,
                        message: String::from(
                            "Unterminated worksheet reference (expected closing \"'!\" ).",
                        ),
                    });
                }
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: s_u16,
                    end_utf16: e_u16,
                    kind: RawKind::SheetPrefix { name, terminated },
                });
            }
            b'#' => {
                if !bytes
                    .get(start.saturating_add(1))
                    .is_some_and(u8::is_ascii_alphabetic)
                {
                    pos = start.saturating_add(1);
                    tokens.push(RawTok {
                        start_byte: start,
                        end_byte: pos,
                        start_utf16: u16_at(start),
                        end_utf16: u16_at(pos),
                        kind: RawKind::Hash,
                    });
                    continue;
                }
                let mut end_p = start.saturating_add(1);
                while let Some(&ch) = bytes.get(end_p) {
                    if ch.is_ascii_alphanumeric() || ch == b'/' || ch == b'!' || ch == b'?' {
                        if ch == b'/'
                            && end_p.saturating_sub(start) == 4
                            && source
                                .get(start..end_p)
                                .is_some_and(|s| s.eq_ignore_ascii_case("#N/A"))
                        {
                            break;
                        }
                        end_p = end_p.saturating_add(1);
                        if ch == b'!' || ch == b'?' {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                pos = end_p;
                let slice = source.get(start..pos).unwrap_or("");
                let valid = CalcError::parse_literal(slice).is_some();
                let s_u16 = u16_at(start);
                let e_u16 = u16_at(pos);
                if !valid {
                    diagnostics.push(FormulaDiagnostic {
                        start_utf16: s_u16,
                        end_utf16: e_u16,
                        message: format!("Unrecognised error literal '{slice}'."),
                    });
                }
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: s_u16,
                    end_utf16: e_u16,
                    kind: RawKind::ErrorLit,
                });
            }
            b'0'..=b'9' | b'.' => {
                let (end_p, valid, digits_only) = scan_number_tolerant(source, start);
                pos = end_p;
                let s_u16 = u16_at(start);
                let e_u16 = u16_at(pos);
                if !valid {
                    diagnostics.push(FormulaDiagnostic {
                        start_utf16: s_u16,
                        end_utf16: e_u16,
                        message: String::from("Invalid numeric literal."),
                    });
                }
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: s_u16,
                    end_utf16: e_u16,
                    kind: if valid {
                        RawKind::Number { digits_only }
                    } else {
                        RawKind::Unknown
                    },
                });
            }
            b'$' | b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let starts_dollar = b == b'$';
                let mut end_p = start;
                while let Some(&ch) = bytes.get(end_p) {
                    if ch.is_ascii_alphanumeric() || ch == b'$' || ch == b'_' || ch == b'.' {
                        end_p = end_p.saturating_add(1);
                    } else {
                        break;
                    }
                }
                let slice = source.get(start..end_p).unwrap_or("");
                if !starts_dollar && bytes.get(end_p) == Some(&b'!') {
                    let bang_end = end_p.saturating_add(1);
                    pos = bang_end;
                    tokens.push(RawTok {
                        start_byte: start,
                        end_byte: bang_end,
                        start_utf16: u16_at(start),
                        end_utf16: u16_at(bang_end),
                        kind: RawKind::SheetPrefix {
                            name: String::from(slice),
                            terminated: true,
                        },
                    });
                } else {
                    pos = end_p;
                    tokens.push(RawTok {
                        start_byte: start,
                        end_byte: pos,
                        start_utf16: u16_at(start),
                        end_utf16: u16_at(pos),
                        kind: RawKind::Ident,
                    });
                }
            }
            _ => {
                let ch_len = source
                    .get(start..)
                    .and_then(|s| s.chars().next())
                    .map_or(1, char::len_utf8);
                pos = start.saturating_add(ch_len);
                let s_u16 = u16_at(start);
                let e_u16 = u16_at(pos);
                diagnostics.push(FormulaDiagnostic {
                    start_utf16: s_u16,
                    end_utf16: e_u16,
                    message: String::from("Unexpected character in formula."),
                });
                tokens.push(RawTok {
                    start_byte: start,
                    end_byte: pos,
                    start_utf16: s_u16,
                    end_utf16: e_u16,
                    kind: RawKind::Unknown,
                });
            }
        }
    }

    tokens
}

fn scan_string_tolerant(source: &str, start_byte: usize) -> (usize, bool) {
    let Some(rest) = source.get(start_byte.saturating_add(1)..) else {
        return (source.len(), false);
    };
    let mut chars = rest.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        if ch == '"' {
            if let Some(&(_, '"')) = chars.peek() {
                let _ = chars.next();
            } else {
                return (
                    start_byte
                        .saturating_add(1)
                        .saturating_add(offset)
                        .saturating_add(1),
                    true,
                );
            }
        }
    }
    (source.len(), false)
}

fn scan_quoted_sheet_tolerant(source: &str, start_byte: usize) -> (String, usize, bool) {
    let Some(rest) = source.get(start_byte.saturating_add(1)..) else {
        return (String::new(), source.len(), false);
    };
    let mut name = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        if ch == '\'' {
            if let Some(&(_, '\'')) = chars.peek() {
                let _ = chars.next();
                name.push('\'');
            } else {
                let after_quote = start_byte
                    .saturating_add(1)
                    .saturating_add(offset)
                    .saturating_add(1);
                if source.as_bytes().get(after_quote) == Some(&b'!') && !name.is_empty() {
                    return (name, after_quote.saturating_add(1), true);
                }
                return (name, after_quote, false);
            }
        } else {
            name.push(ch);
        }
    }
    (name, source.len(), false)
}

fn scan_number_tolerant(source: &str, start_byte: usize) -> (usize, bool, bool) {
    let bytes = source.as_bytes();
    let mut pos = start_byte;
    let mut int_digits = 0usize;
    while let Some(&b) = bytes.get(pos) {
        if b.is_ascii_digit() {
            int_digits = int_digits.saturating_add(1);
            pos = pos.saturating_add(1);
        } else {
            break;
        }
    }
    let mut has_dot = false;
    let mut frac_digits = 0usize;
    if bytes.get(pos) == Some(&b'.') {
        has_dot = true;
        pos = pos.saturating_add(1);
        while let Some(&b) = bytes.get(pos) {
            if b.is_ascii_digit() {
                frac_digits = frac_digits.saturating_add(1);
                pos = pos.saturating_add(1);
            } else {
                break;
            }
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return (pos, false, false);
    }
    let mut has_exp = false;
    if let Some(&e_ch) = bytes.get(pos)
        && (e_ch == b'e' || e_ch == b'E')
    {
        has_exp = true;
        pos = pos.saturating_add(1);
        if let Some(&sign) = bytes.get(pos)
            && (sign == b'+' || sign == b'-')
        {
            pos = pos.saturating_add(1);
        }
        let exp_start = pos;
        while let Some(&b) = bytes.get(pos) {
            if b.is_ascii_digit() {
                pos = pos.saturating_add(1);
            } else {
                break;
            }
        }
        if pos == exp_start {
            return (pos, false, false);
        }
    }
    let valid = source
        .get(start_byte..pos)
        .and_then(|s| s.parse::<f64>().ok())
        .is_some_and(f64::is_finite);
    (pos, valid, !has_dot && !has_exp)
}

// ------------------------------------------------ references & token spans

struct RefTokenGroup {
    start_tok: usize,
    end_tok_excl: usize,
    sheet_tok: Option<usize>,
    first_coord_tok: usize,
    second_coord_tok: Option<usize>,
    has_spill_hash: bool,
    start_byte: usize,
    end_byte: usize,
    start_utf16: u32,
    end_utf16: u32,
}

fn is_coord_token(source: &str, tok: &RawTok) -> bool {
    let s = tok.slice(source);
    match tok.kind {
        RawKind::Ident => {
            parse_cell_addr(s).is_some()
                || parse_col_only(s).is_some()
                || parse_row_only(s).is_some()
        }
        RawKind::Number { digits_only: true } => parse_row_only(s).is_some(),
        _ => false,
    }
}

fn find_ref_token_groups(source: &str, raw_tokens: &[RawTok]) -> Vec<RefTokenGroup> {
    let mut groups = Vec::new();
    let mut i = 1_usize; // skip leading `=`
    while i < raw_tokens.len() {
        let Some(tok) = raw_tokens.get(i) else {
            break;
        };
        let (sheet_tok, coord1_idx) = match &tok.kind {
            RawKind::SheetPrefix {
                terminated: true, ..
            } => (Some(i), i.saturating_add(1)),
            _ => (None, i),
        };

        let Some(coord1) = raw_tokens.get(coord1_idx) else {
            i = i.saturating_add(1);
            continue;
        };
        // An identifier followed immediately by `(` is a function call, not a reference!
        let after_coord1 = raw_tokens.get(coord1_idx.saturating_add(1));
        if matches!(after_coord1.map(|t| &t.kind), Some(RawKind::LParen)) {
            i = coord1_idx.saturating_add(1);
            continue;
        }

        if !is_coord_token(source, coord1) {
            i = i.saturating_add(1);
            continue;
        }

        let c1_str = coord1.slice(source);
        // Bare `A` or `10` without `:` is NOT a cell reference unless paired with `:`!
        let c1_is_cell = parse_cell_addr(c1_str).is_some();

        let has_colon_second = matches!(after_coord1.map(|t| &t.kind), Some(RawKind::Colon))
            && raw_tokens
                .get(coord1_idx.saturating_add(2))
                .is_some_and(|t2| {
                    !matches!(
                        raw_tokens
                            .get(coord1_idx.saturating_add(3))
                            .map(|t3| &t3.kind),
                        Some(RawKind::LParen)
                    ) && is_coord_token(source, t2)
                });

        if has_colon_second {
            let c2_idx = coord1_idx.saturating_add(2);
            if let Some(end_t) = raw_tokens.get(c2_idx) {
                groups.push(RefTokenGroup {
                    start_tok: i,
                    end_tok_excl: c2_idx.saturating_add(1),
                    sheet_tok,
                    first_coord_tok: coord1_idx,
                    second_coord_tok: Some(c2_idx),
                    has_spill_hash: false,
                    start_byte: tok.start_byte,
                    end_byte: end_t.end_byte,
                    start_utf16: tok.start_utf16,
                    end_utf16: end_t.end_utf16,
                });
                i = c2_idx.saturating_add(1);
                continue;
            }
        } else if c1_is_cell {
            let hash_opt = raw_tokens
                .get(coord1_idx.saturating_add(1))
                .filter(|ht| matches!(ht.kind, RawKind::Hash) && ht.start_byte == coord1.end_byte);
            let (end_tok_excl, end_byte, end_utf16, has_spill_hash) = match hash_opt {
                Some(ht) => (
                    coord1_idx.saturating_add(2),
                    ht.end_byte,
                    ht.end_utf16,
                    true,
                ),
                None => (
                    coord1_idx.saturating_add(1),
                    coord1.end_byte,
                    coord1.end_utf16,
                    false,
                ),
            };
            groups.push(RefTokenGroup {
                start_tok: i,
                end_tok_excl,
                sheet_tok,
                first_coord_tok: coord1_idx,
                second_coord_tok: None,
                has_spill_hash,
                start_byte: tok.start_byte,
                end_byte,
                start_utf16: tok.start_utf16,
                end_utf16,
            });
            i = end_tok_excl;
            continue;
        }

        i = i.saturating_add(1);
    }
    groups
}

fn resolve_live_sheet_by_name(workbook: &Workbook<BoundFormula>, name: &str) -> Option<SheetId> {
    workbook
        .sheets()
        .iter()
        .find(|s| !s.is_deleted() && s.name().eq_ignore_ascii_case(name))
        .map(reticula_core::sheet::Sheet::id)
}

#[allow(clippy::too_many_lines)]
fn classify_tokens_and_refs(
    source: &str,
    raw_tokens: &[RawTok],
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    diagnostics: &mut Vec<FormulaDiagnostic>,
) -> (Vec<SyntaxTokenSpan>, Vec<FormulaRefHighlight>) {
    let groups = find_ref_token_groups(source, raw_tokens);
    let mut spans = Vec::with_capacity(raw_tokens.len());
    let mut refs = Vec::new();
    let mut color_counter: u8 = 0;

    let mut group_cursor = 0usize;
    let mut idx = 0usize;
    while idx < raw_tokens.len() {
        if let Some(g) = groups.get(group_cursor)
            && idx == g.start_tok
        {
            let target_sheet_opt = match g.sheet_tok.and_then(|si| raw_tokens.get(si)) {
                None => Some(host_sheet),
                Some(st) => match &st.kind {
                    RawKind::SheetPrefix { name, .. } => {
                        let found = resolve_live_sheet_by_name(workbook, name);
                        if found.is_none() {
                            diagnostics.push(FormulaDiagnostic {
                                start_utf16: st.start_utf16,
                                end_utf16: st.end_utf16,
                                message: format!("Unknown worksheet '{name}'."),
                            });
                        }
                        found
                    }
                    _ => None,
                },
            };

            let resolved_rect = target_sheet_opt.and_then(|sid| {
                let sheet = workbook.sheet(sid)?;
                let max_r = sheet.rows().saturating_sub(1);
                let max_c = sheet.cols().saturating_sub(1);
                let s1 = raw_tokens.get(g.first_coord_tok)?.slice(source);
                if let Some(c2_idx) = g.second_coord_tok {
                    let s2 = raw_tokens.get(c2_idx)?.slice(source);
                    if let (Some(a), Some(b)) = (parse_cell_addr(s1), parse_cell_addr(s2)) {
                        let r1 = a.row.min(b.row);
                        let r2 = a.row.max(b.row);
                        let c1 = a.col.min(b.col);
                        let c2 = a.col.max(b.col);
                        if r2 < sheet.rows() && c2 < sheet.cols() {
                            return Some((sid, r1, r2, c1, c2));
                        }
                        return None;
                    }
                    if let (Some((c1, _)), Some((c2, _))) = (parse_col_only(s1), parse_col_only(s2))
                    {
                        let lo = c1.min(c2);
                        let hi = c1.max(c2);
                        if hi < sheet.cols() && sheet.rows() > 0 {
                            return Some((sid, 0, max_r, lo, hi));
                        }
                        return None;
                    }
                    if let (Some((r1, _)), Some((r2, _))) = (parse_row_only(s1), parse_row_only(s2))
                    {
                        let lo = r1.min(r2);
                        let hi = r1.max(r2);
                        if hi < sheet.rows() && sheet.cols() > 0 {
                            return Some((sid, lo, hi, 0, max_c));
                        }
                        return None;
                    }
                    diagnostics.push(FormulaDiagnostic {
                        start_utf16: g.start_utf16,
                        end_utf16: g.end_utf16,
                        message: String::from("Invalid range endpoints around ':'."),
                    });
                    None
                } else {
                    let a = parse_cell_addr(s1)?;
                    if a.row < sheet.rows() && a.col < sheet.cols() {
                        Some((sid, a.row, a.row, a.col, a.col))
                    } else {
                        None
                    }
                }
            });

            let assigned_color = resolved_rect.map(|(sid, r1, r2, c1, c2)| {
                let c = color_counter % REF_COLOR_PALETTE_LEN;
                color_counter = color_counter.wrapping_add(1);
                refs.push(FormulaRefHighlight {
                    start_utf16: g.start_utf16,
                    end_utf16: g.end_utf16,
                    color_index: c,
                    sheet: sid.0,
                    first_row: r1,
                    last_row: r2,
                    first_col: c1,
                    last_col: c2,
                });
                c
            });

            spans.push(SyntaxTokenSpan {
                start_utf16: g.start_utf16,
                end_utf16: g.end_utf16,
                kind: SyntaxTokenKind::Reference,
                color_index: assigned_color,
                matched_bracket: false,
            });
            idx = g.end_tok_excl;
            group_cursor = group_cursor.saturating_add(1);
            continue;
        }

        while groups.get(group_cursor).is_some_and(|g| g.start_tok < idx) {
            group_cursor = group_cursor.saturating_add(1);
        }

        let Some(tok) = raw_tokens.get(idx) else {
            break;
        };
        let next_is_lparen = matches!(
            raw_tokens.get(idx.saturating_add(1)).map(|t| &t.kind),
            Some(RawKind::LParen)
        );

        let kind = match &tok.kind {
            RawKind::Equals => SyntaxTokenKind::Equals,
            RawKind::Number { .. } => SyntaxTokenKind::Number,
            RawKind::StringLit => SyntaxTokenKind::String,
            RawKind::SheetPrefix { name, terminated } => {
                if *terminated && resolve_live_sheet_by_name(workbook, name).is_none() {
                    diagnostics.push(FormulaDiagnostic {
                        start_utf16: tok.start_utf16,
                        end_utf16: tok.end_utf16,
                        message: format!("Unknown worksheet '{name}'."),
                    });
                }
                SyntaxTokenKind::SheetRef
            }
            RawKind::ErrorLit => SyntaxTokenKind::Error,
            RawKind::Ident => {
                let s = tok.slice(source);
                if next_is_lparen {
                    SyntaxTokenKind::Function
                } else if s.eq_ignore_ascii_case("TRUE") || s.eq_ignore_ascii_case("FALSE") {
                    SyntaxTokenKind::Boolean
                } else {
                    SyntaxTokenKind::Unknown
                }
            }
            RawKind::LParen | RawKind::RParen => SyntaxTokenKind::Paren,
            RawKind::Colon
            | RawKind::Comma
            | RawKind::Semicolon
            | RawKind::LBrace
            | RawKind::RBrace => SyntaxTokenKind::Punctuation,
            RawKind::Operator | RawKind::Hash => SyntaxTokenKind::Operator,
            RawKind::Unknown => SyntaxTokenKind::Unknown,
        };

        spans.push(SyntaxTokenSpan {
            start_utf16: tok.start_utf16,
            end_utf16: tok.end_utf16,
            kind,
            color_index: None,
            matched_bracket: false,
        });
        idx = idx.saturating_add(1);
    }

    (spans, refs)
}

// ---------------------------------------- calls, parens & signature help

struct CallFrame {
    fn_tok_idx: Option<usize>,
    open_tok_idx: usize,
    close_tok_idx: Option<usize>,
    comma_offsets_utf16: Vec<u32>,
    has_non_comma_token: bool,
}

fn analyze_calls_and_parens(
    source: &str,
    raw_tokens: &[RawTok],
    spans: &mut [SyntaxTokenSpan],
    cursor_utf16: u32,
    diagnostics: &mut Vec<FormulaDiagnostic>,
) -> Option<SignatureHelp> {
    let mut stack: Vec<usize> = Vec::new();
    let mut frames: Vec<CallFrame> = Vec::new();

    for (idx, tok) in raw_tokens.iter().enumerate() {
        match tok.kind {
            RawKind::LParen => {
                let fn_tok_idx = idx.checked_sub(1).filter(|&prev| {
                    raw_tokens
                        .get(prev)
                        .is_some_and(|pt| matches!(pt.kind, RawKind::Ident))
                });
                let frame_idx = frames.len();
                frames.push(CallFrame {
                    fn_tok_idx,
                    open_tok_idx: idx,
                    close_tok_idx: None,
                    comma_offsets_utf16: Vec::new(),
                    has_non_comma_token: false,
                });
                stack.push(frame_idx);
            }
            RawKind::RParen => {
                if let Some(frame_idx) = stack.pop() {
                    if let Some(frame) = frames.get_mut(frame_idx) {
                        frame.close_tok_idx = Some(idx);
                    }
                } else {
                    diagnostics.push(FormulaDiagnostic {
                        start_utf16: tok.start_utf16,
                        end_utf16: tok.end_utf16,
                        message: String::from("Unmatched closing ')'."),
                    });
                }
            }
            RawKind::Comma => {
                if let Some(&top_idx) = stack.last()
                    && let Some(frame) = frames.get_mut(top_idx)
                {
                    frame.comma_offsets_utf16.push(tok.start_utf16);
                }
            }
            _ => {
                if let Some(&top_idx) = stack.last()
                    && let Some(frame) = frames.get_mut(top_idx)
                {
                    frame.has_non_comma_token = true;
                }
            }
        }
    }

    // Report unclosed `(`.
    for &unclosed_idx in &stack {
        if let Some(frame) = frames.get(unclosed_idx)
            && let Some(open_tok) = raw_tokens.get(frame.open_tok_idx)
        {
            diagnostics.push(FormulaDiagnostic {
                start_utf16: open_tok.start_utf16,
                end_utf16: open_tok.end_utf16,
                message: String::from("Unclosed '(' (missing ')')."),
            });
        }
    }

    // Highlight innermost matched bracket pair adjacent to or enclosing `cursor_utf16`.
    let mut best_pair: Option<(u32, u32)> = None;
    for frame in &frames {
        let (Some(close_idx), Some(open_tok)) =
            (frame.close_tok_idx, raw_tokens.get(frame.open_tok_idx))
        else {
            continue;
        };
        let Some(close_tok) = raw_tokens.get(close_idx) else {
            continue;
        };
        if cursor_utf16 >= open_tok.start_utf16 && cursor_utf16 <= close_tok.end_utf16 {
            best_pair = Some((open_tok.start_utf16, close_tok.start_utf16));
        }
    }
    if let Some((open_u16, close_u16)) = best_pair {
        for sp in spans.iter_mut() {
            if sp.kind == SyntaxTokenKind::Paren
                && (sp.start_utf16 == open_u16 || sp.start_utf16 == close_u16)
            {
                sp.matched_bracket = true;
            }
        }
    }

    // Validate function names & argument counts, and pick innermost active SignatureHelp.
    let mut active_sig: Option<SignatureHelp> = None;
    for frame in &frames {
        let Some(fn_idx) = frame.fn_tok_idx else {
            continue;
        };
        let (Some(fn_tok), Some(open_tok)) =
            (raw_tokens.get(fn_idx), raw_tokens.get(frame.open_tok_idx))
        else {
            continue;
        };
        let upper_name = fn_tok.slice(source).to_ascii_uppercase();
        let Some(meta) = lookup_function_meta(&upper_name) else {
            diagnostics.push(FormulaDiagnostic {
                start_utf16: fn_tok.start_utf16,
                end_utf16: fn_tok.end_utf16,
                message: format!("Unknown function '{upper_name}'."),
            });
            continue;
        };

        let arg_count = if frame.comma_offsets_utf16.is_empty() && !frame.has_non_comma_token {
            0
        } else {
            frame.comma_offsets_utf16.len().saturating_add(1)
        };

        let close_tok = frame.close_tok_idx.and_then(|ci| raw_tokens.get(ci));
        if let Some(ct) = close_tok {
            if arg_count < meta.min_args || arg_count > meta.max_args {
                let msg = format_arity_error(meta, arg_count);
                diagnostics.push(FormulaDiagnostic {
                    start_utf16: fn_tok.start_utf16,
                    end_utf16: ct.end_utf16,
                    message: msg,
                });
            }
        } else if arg_count > meta.max_args {
            let msg = format_arity_error(meta, arg_count);
            diagnostics.push(FormulaDiagnostic {
                start_utf16: fn_tok.start_utf16,
                end_utf16: open_tok.end_utf16,
                message: msg,
            });
        }

        // Signature help applies when cursor is strictly inside `( ... )` (or unclosed `(`..EOF).
        let in_call = cursor_utf16 >= open_tok.end_utf16
            && close_tok.is_none_or(|ct| cursor_utf16 <= ct.start_utf16);
        if in_call {
            let commas_before = frame
                .comma_offsets_utf16
                .iter()
                .filter(|&&c_u16| c_u16 < cursor_utf16)
                .count();
            let clamped = if meta.params.is_empty() {
                0
            } else {
                u32::try_from(commas_before.min(meta.params.len().saturating_sub(1))).unwrap_or(0)
            };
            active_sig = Some(SignatureHelp {
                name: String::from(meta.name),
                params: meta.params.iter().map(|&p| String::from(p)).collect(),
                active_param: clamped,
                summary: String::from(meta.summary),
            });
        }
    }

    active_sig
}

fn format_arity_error(meta: &FunctionMeta, got: usize) -> String {
    if meta.min_args == meta.max_args {
        let plural = if meta.min_args == 1 { "" } else { "s" };
        format!(
            "{} expects {} argument{plural}, got {got}.",
            meta.name, meta.min_args
        )
    } else if meta.max_args == MAX_FUNCTION_ARGS {
        format!(
            "{} expects at least {} argument(s), got {got}.",
            meta.name, meta.min_args
        )
    } else {
        format!(
            "{} expects {} to {} arguments, got {got}.",
            meta.name, meta.min_args, meta.max_args
        )
    }
}

fn diagnose_incomplete_operators(
    source: &str,
    raw_tokens: &[RawTok],
    diagnostics: &mut Vec<FormulaDiagnostic>,
) {
    let Some(last) = raw_tokens.last() else {
        return;
    };
    match &last.kind {
        RawKind::Colon => {
            diagnostics.push(FormulaDiagnostic {
                start_utf16: last.start_utf16,
                end_utf16: last.end_utf16,
                message: String::from("Expected cell or row/column coordinate after ':'."),
            });
        }
        RawKind::Comma | RawKind::Semicolon => {
            diagnostics.push(FormulaDiagnostic {
                start_utf16: last.start_utf16,
                end_utf16: last.end_utf16,
                message: String::from("Expected expression after separator."),
            });
        }
        RawKind::Operator if last.slice(source) != "%" => {
            diagnostics.push(FormulaDiagnostic {
                start_utf16: last.start_utf16,
                end_utf16: last.end_utf16,
                message: String::from("Expected operand after operator."),
            });
        }
        _ => {}
    }
    for pair in raw_tokens.windows(2) {
        if let [a, b] = pair
            && matches!(a.kind, RawKind::Colon)
            && matches!(
                b.kind,
                RawKind::RParen | RawKind::Comma | RawKind::Operator | RawKind::Colon
            )
        {
            diagnostics.push(FormulaDiagnostic {
                start_utf16: a.start_utf16,
                end_utf16: b.end_utf16,
                message: String::from("Incomplete range reference around ':'."),
            });
        }
    }
}

// ------------------------------------------------------------ completions

fn build_completions(
    source: &str,
    raw_tokens: &[RawTok],
    cursor_utf16: u32,
    workbook: &Workbook<BoundFormula>,
) -> Vec<CompletionItem> {
    // Find token whose span contains `cursor_utf16` (`start < cursor <= end`).
    let tok = raw_tokens
        .iter()
        .enumerate()
        .find(|(_, t)| cursor_utf16 > t.start_utf16 && cursor_utf16 <= t.end_utf16);

    let Some((idx, active_tok)) = tok else {
        return Vec::new();
    };

    match &active_tok.kind {
        RawKind::SheetPrefix {
            name,
            terminated: false,
        } => {
            let prefix_lower = name.to_ascii_lowercase();
            let mut items = Vec::new();
            for sheet in workbook.sheets() {
                if sheet.is_deleted() {
                    continue;
                }
                if sheet.name().to_ascii_lowercase().starts_with(&prefix_lower) {
                    let mut formatted = String::new();
                    write_sheet_prefix(Some(sheet.name()), &mut formatted);
                    items.push(CompletionItem {
                        label: String::from(sheet.name()),
                        insert_text: formatted,
                        replace_start_utf16: active_tok.start_utf16,
                        replace_end_utf16: active_tok.end_utf16,
                        kind: CompletionKind::Sheet,
                        signature: String::from("Worksheet"),
                        summary: String::from("Reference cells on this worksheet."),
                    });
                }
            }
            items
        }
        RawKind::Ident => {
            // Do not pop completions right after `Sheet1!` (where user is typing a cell address)
            // or if `(` already follows this identifier.
            if idx
                .checked_sub(1)
                .and_then(|pi| raw_tokens.get(pi))
                .is_some_and(|pt| matches!(pt.kind, RawKind::SheetPrefix { .. } | RawKind::Colon))
            {
                return Vec::new();
            }
            if raw_tokens
                .get(idx.saturating_add(1))
                .is_some_and(|nt| matches!(nt.kind, RawKind::LParen))
            {
                return Vec::new();
            }

            let raw_ident = active_tok.slice(source);
            // Skip `$`-prefixed or digit-containing coordinates like `A1`, `$B2`.
            if raw_ident.contains('$') || raw_ident.bytes().any(|b| b.is_ascii_digit()) {
                return Vec::new();
            }
            let prefix_upper = raw_ident.to_ascii_uppercase();
            if prefix_upper.is_empty() {
                return Vec::new();
            }

            let mut out = Vec::new();
            // 1. Prefix-matched built-in functions.
            for meta in &FUNCTION_CATALOG {
                if meta.name.starts_with(&prefix_upper) {
                    out.push(make_fn_completion(meta, active_tok));
                    if out.len() >= MAX_COMPLETIONS {
                        return out;
                    }
                }
            }
            // 2. Live worksheets matching prefix.
            for sheet in workbook.sheets() {
                if sheet.is_deleted() {
                    continue;
                }
                if sheet.name().to_ascii_uppercase().starts_with(&prefix_upper) {
                    let mut formatted = String::new();
                    write_sheet_prefix(Some(sheet.name()), &mut formatted);
                    out.push(CompletionItem {
                        label: String::from(sheet.name()),
                        insert_text: formatted,
                        replace_start_utf16: active_tok.start_utf16,
                        replace_end_utf16: active_tok.end_utf16,
                        kind: CompletionKind::Sheet,
                        signature: String::from("Worksheet"),
                        summary: String::from("Reference cells on this worksheet."),
                    });
                    if out.len() >= MAX_COMPLETIONS {
                        return out;
                    }
                }
            }
            // 3. Substring-matched functions if room remains.
            if prefix_upper.len() >= 2 {
                for meta in &FUNCTION_CATALOG {
                    if !meta.name.starts_with(&prefix_upper) && meta.name.contains(&prefix_upper) {
                        out.push(make_fn_completion(meta, active_tok));
                        if out.len() >= MAX_COMPLETIONS {
                            break;
                        }
                    }
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

fn make_fn_completion(meta: &FunctionMeta, tok: &RawTok) -> CompletionItem {
    let mut sig = String::from("(");
    for (i, &p) in meta.params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(p);
    }
    sig.push(')');
    CompletionItem {
        label: String::from(meta.name),
        insert_text: format!("{}(", meta.name),
        replace_start_utf16: tok.start_utf16,
        replace_end_utf16: tok.end_utf16,
        kind: CompletionKind::Function,
        signature: sig,
        summary: String::from(meta.summary),
    }
}

#[allow(clippy::too_many_arguments)]
fn enrich_named_ranges_and_spills(
    source: &str,
    raw_tokens: &[RawTok],
    groups: &[RefTokenGroup],
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    spills: Option<&crate::spill::SpillRegistry>,
    spans: &mut [SyntaxTokenSpan],
    references: &mut Vec<FormulaRefHighlight>,
) {
    // 1. Expand `A1#` spill highlights to the active spill footprint and color the `#` token.
    for g in groups {
        if !g.has_spill_hash {
            continue;
        }
        let Some(ref_hl) = references
            .iter_mut()
            .find(|r| r.start_utf16 == g.start_utf16)
        else {
            continue;
        };
        if let Some(spills_reg) = spills
            && let Some(sheet) = workbook.live_sheet(SheetId(ref_hl.sheet))
        {
            let target_sheet = SheetId(ref_hl.sheet);
            let r_id = reticula_core::id::RowId(sheet.row_id(ref_hl.first_row));
            let c_id = reticula_core::id::ColId(sheet.col_id(ref_hl.first_col));
            if let Some((rows, cols)) = spills_reg.spill_shape((target_sheet, r_id, c_id)) {
                ref_hl.last_row = ref_hl
                    .first_row
                    .saturating_add(rows.saturating_sub(1))
                    .min(sheet.rows().saturating_sub(1));
                ref_hl.last_col = ref_hl
                    .first_col
                    .saturating_add(cols.saturating_sub(1))
                    .min(sheet.cols().saturating_sub(1));
            }
        }
        ref_hl.end_utf16 = g.end_utf16;
        for sp in spans.iter_mut() {
            if sp.start_utf16 >= g.start_utf16 && sp.end_utf16 <= g.end_utf16 {
                if sp.kind != SyntaxTokenKind::SheetRef {
                    sp.kind = SyntaxTokenKind::Reference;
                }
                sp.color_index = Some(ref_hl.color_index);
            }
        }
    }

    // 2. Classify defined named range identifiers (`TaxRate` or `'Sheet 2'!TaxRate`) and
    //    emit live FormulaRefHighlight spans for direct cell/range named targets.
    if workbook.named_ranges().is_empty() {
        return;
    }
    let mut color_counter = u8::try_from(references.len()).unwrap_or(0) % REF_COLOR_PALETTE_LEN;
    for (idx, tok) in raw_tokens.iter().enumerate() {
        if !matches!(tok.kind, RawKind::Ident) {
            continue;
        }
        let next_is_lparen = raw_tokens
            .get(idx.saturating_add(1))
            .is_some_and(|nt| matches!(nt.kind, RawKind::LParen));
        if next_is_lparen {
            continue;
        }
        let Some(span_idx) = spans
            .iter()
            .position(|sp| sp.start_utf16 == tok.start_utf16 && sp.end_utf16 == tok.end_utf16)
        else {
            continue;
        };
        let already_classified = spans.get(span_idx).is_none_or(|sp| {
            sp.kind == SyntaxTokenKind::Reference || sp.kind == SyntaxTokenKind::Boolean
        });
        if already_classified {
            continue;
        }

        let ident = tok.slice(source);
        let prev_sheet = idx
            .checked_sub(1)
            .and_then(|pi| raw_tokens.get(pi))
            .and_then(|pt| match &pt.kind {
                RawKind::SheetPrefix {
                    name,
                    terminated: true,
                } => resolve_live_sheet_by_name(workbook, name),
                _ => None,
            });
        let resolved_def = if let Some(scoped_sid) = prev_sheet {
            workbook
                .named_ranges()
                .get(&reticula_core::ops::NamedRangeKey::new(
                    Some(scoped_sid),
                    ident,
                ))
                .map(|def| (scoped_sid, def))
        } else {
            workbook
                .named_ranges()
                .get(&reticula_core::ops::NamedRangeKey::new(
                    Some(host_sheet),
                    ident,
                ))
                .map(|def| (host_sheet, def))
                .or_else(|| {
                    workbook
                        .named_ranges()
                        .get(&reticula_core::ops::NamedRangeKey::new(None, ident))
                        .map(|def| (host_sheet, def))
                })
        };

        let Some((target_host, def)) = resolved_def else {
            continue;
        };
        if let Some(sp) = spans.get_mut(span_idx) {
            sp.kind = SyntaxTokenKind::Reference;
        }

        let resolved_rect = resolve_named_target_rect(workbook, target_host, &def.formula.root);
        if let Some((sid, r1, r2, c1, c2)) = resolved_rect {
            let color_index = color_counter;
            color_counter = color_counter.wrapping_add(1) % REF_COLOR_PALETTE_LEN;
            if let Some(sp) = spans.get_mut(span_idx) {
                sp.color_index = Some(color_index);
            }
            let hl_start_utf16 = if prev_sheet.is_some()
                && let Some(pt_tok) = idx.checked_sub(1).and_then(|pi| raw_tokens.get(pi))
                && let Some(pt_span) = spans.iter_mut().find(|sp| {
                    sp.start_utf16 == pt_tok.start_utf16 && sp.end_utf16 == pt_tok.end_utf16
                }) {
                pt_span.kind = SyntaxTokenKind::SheetRef;
                pt_span.start_utf16
            } else {
                tok.start_utf16
            };
            references.push(FormulaRefHighlight {
                start_utf16: hl_start_utf16,
                end_utf16: tok.end_utf16,
                color_index,
                sheet: sid.0,
                first_row: r1,
                last_row: r2,
                first_col: c1,
                last_col: c2,
            });
        }
    }
}

fn resolve_named_target_rect(
    workbook: &Workbook<BoundFormula>,
    fallback_sheet: SheetId,
    expr: &crate::binder::BoundExpr,
) -> Option<(SheetId, u32, u32, u32, u32)> {
    let mut cur = expr;
    while let crate::binder::BoundExpr::Paren(inner) = cur {
        cur = inner;
    }
    let crate::binder::BoundExpr::Reference(r) = cur else {
        return None;
    };
    match r {
        crate::binder::BoundRef::Cell {
            sheet, row, col, ..
        } => {
            let sid = sheet.unwrap_or(fallback_sheet);
            let s = workbook.live_sheet(sid)?;
            let r_pos = s.row_axis().id_to_pos(row.0)?;
            let c_pos = s.col_axis().id_to_pos(col.0)?;
            Some((sid, r_pos, r_pos, c_pos, c_pos))
        }
        crate::binder::BoundRef::Area {
            sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            ..
        } => {
            let sid = sheet.unwrap_or(fallback_sheet);
            let s = workbook.live_sheet(sid)?;
            let r1 = s.row_axis().id_to_pos(first_row.0)?;
            let r2 = s.row_axis().id_to_pos(last_row.0)?;
            let c1 = s.col_axis().id_to_pos(first_col.0)?;
            let c2 = s.col_axis().id_to_pos(last_col.0)?;
            Some((sid, r1.min(r2), r1.max(r2), c1.min(c2), c1.max(c2)))
        }
        _ => None,
    }
}

fn append_named_range_completions(
    source: &str,
    raw_tokens: &[RawTok],
    cursor_utf16: u32,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    out: &mut Vec<CompletionItem>,
) {
    if workbook.named_ranges().is_empty() || out.len() >= MAX_COMPLETIONS {
        return;
    }
    let Some((tok_idx, active_tok)) = raw_tokens
        .iter()
        .enumerate()
        .find(|(_, t)| t.start_utf16 < cursor_utf16 && cursor_utf16 <= t.end_utf16)
    else {
        return;
    };
    if !matches!(active_tok.kind, RawKind::Ident) {
        return;
    }
    let prefix = active_tok.slice(source);
    let prefix_upper = prefix.to_ascii_uppercase();
    if prefix_upper.is_empty() {
        return;
    }

    let prev_sheet = tok_idx
        .checked_sub(1)
        .and_then(|pi| raw_tokens.get(pi))
        .and_then(|pt| match &pt.kind {
            RawKind::SheetPrefix {
                name,
                terminated: true,
            } => resolve_live_sheet_by_name(workbook, name),
            _ => None,
        });

    let mut seen_names: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
    // Prioritize sheet-scoped matches on target sheet, then workbook-scoped when unqualified.
    let target_scope = prev_sheet.unwrap_or(host_sheet);
    for pass_workbook_wide in [false, true] {
        if pass_workbook_wide && prev_sheet.is_some() {
            break;
        }
        for (nkey, def) in workbook.named_ranges() {
            if def.hidden {
                continue;
            }
            let scope_matches = if pass_workbook_wide {
                nkey.scope.is_none()
            } else {
                nkey.scope == Some(target_scope)
            };
            if !scope_matches {
                continue;
            }
            if !nkey
                .canonical_name
                .to_ascii_uppercase()
                .starts_with(&prefix_upper)
            {
                continue;
            }
            if !seen_names.insert(nkey.canonical_name.clone()) {
                continue;
            }
            let sig = def
                .formula
                .unparse(workbook, nkey.scope.unwrap_or(host_sheet));
            out.push(CompletionItem {
                label: def.name.clone(),
                insert_text: def.name.clone(),
                replace_start_utf16: active_tok.start_utf16,
                replace_end_utf16: active_tok.end_utf16,
                kind: CompletionKind::NamedRange,
                signature: sig,
                summary: def
                    .comment
                    .clone()
                    .unwrap_or_else(|| String::from("Named range")),
            });
            if out.len() >= MAX_COMPLETIONS {
                return;
            }
        }
    }
}

// ------------------------------------------------ point-and-click & F4

fn compute_ref_insertion_state(
    raw_tokens: &[RawTok],
    spans: &[SyntaxTokenSpan],
    cursor_utf16: u32,
) -> (bool, Option<(u32, u32)>) {
    // 1. If cursor sits inside or right at the end of an existing `Reference` span,
    //    point-and-click replaces that reference span!
    if let Some(ref_span) = spans.iter().find(|sp| {
        sp.kind == SyntaxTokenKind::Reference
            && cursor_utf16 >= sp.start_utf16
            && cursor_utf16 <= sp.end_utf16
    }) {
        return (true, Some((ref_span.start_utf16, ref_span.end_utf16)));
    }

    // 2. Otherwise check the last raw token ending at or before `cursor_utf16`:
    //    if it expects an expression (`=`, `(`, `,`, `;`, `{`, `:`, or operator),
    //    point-and-click inserts a fresh reference at `cursor_utf16`!
    let prev = raw_tokens.iter().rfind(|t| t.end_utf16 <= cursor_utf16);
    let can_insert = matches!(
        prev.map(|t| &t.kind),
        Some(
            RawKind::Equals
                | RawKind::LParen
                | RawKind::Comma
                | RawKind::Semicolon
                | RawKind::LBrace
                | RawKind::Colon
                | RawKind::Operator
        )
    );
    (can_insert, None)
}

fn cycle_single_coord(coord: &str) -> Option<String> {
    if let Some(addr) = parse_cell_addr(coord) {
        // Excel F4 cycle: `B2 -> $B$2 -> B$2 -> $B2 -> B2`
        let (next_abs_col, next_abs_row) = match (addr.abs_col, addr.abs_row) {
            (false, false) => (true, true),
            (true, true) => (false, true),
            (false, true) => (true, false),
            (true, false) => (false, false),
        };
        let mut out = String::new();
        CellAddr {
            col: addr.col,
            row: addr.row,
            abs_col: next_abs_col,
            abs_row: next_abs_row,
        }
        .write_a1(&mut out);
        return Some(out);
    }
    if let Some((col, abs)) = parse_col_only(coord) {
        let mut out = String::new();
        if !abs {
            out.push('$');
        }
        write_col_letters(col, &mut out);
        return Some(out);
    }
    if let Some((row, abs)) = parse_row_only(coord) {
        let one_based = u64::from(row).saturating_add(1);
        return Some(if !abs {
            format!("${one_based}")
        } else {
            format!("{one_based}")
        });
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;

    use reticula_core::id::SheetId;
    use reticula_core::ops::Op;

    use super::{
        CompletionKind, SyntaxTokenKind, analyze_formula, format_sheet_range_ref,
        toggle_reference_dollar,
    };
    use crate::engine::FormulaEngine;

    #[test]
    fn tokens_refs_signature_completions_and_tolerant_diagnostics_work() {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        let s1 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: s1,
            name: String::from("Sheet 2"),
            after: Some(s0),
        })
        .unwrap();

        // 1. Valid multi-reference cross-sheet formula with cursor on second argument of VLOOKUP.
        let src = "=VLOOKUP(A2, 'Sheet 2'!$B$2:D10, 2, FALSE)";
        let cursor = 16; // inside `'Sheet 2'!$B$2:D10`
        let res = analyze_formula(src, cursor, eng.workbook(), s0);
        assert!(res.is_formula);
        assert!(res.diagnostics.is_empty(), "diags: {:?}", res.diagnostics);
        assert_eq!(res.references.len(), 2);
        assert_eq!(res.references[0].color_index, 0);
        assert_eq!(res.references[0].sheet, 0);
        assert_eq!(res.references[1].color_index, 1);
        assert_eq!(res.references[1].sheet, s1.0);
        assert_eq!(res.references[1].first_row, 1);
        assert_eq!(res.references[1].last_row, 9);
        assert_eq!(res.references[1].first_col, 1);
        assert_eq!(res.references[1].last_col, 3);
        assert!(res.can_insert_ref);
        assert!(res.active_ref_span.is_some());

        let sig = res.signature.expect("signature help present");
        assert_eq!(sig.name, "VLOOKUP");
        assert_eq!(sig.active_param, 1);

        // Matched brackets highlighted when cursor is inside `(...)`.
        let matched_parens = res.tokens.iter().filter(|t| t.matched_bracket).count();
        assert_eq!(matched_parens, 2);

        // 2. Incomplete formula `=SUM(A1:` does not panic and emits recoverable diagnostics!
        let inc = "=SUM(A1:";
        let res_inc = analyze_formula(inc, u32::try_from(inc.len()).unwrap(), eng.workbook(), s0);
        assert!(!res_inc.diagnostics.is_empty());
        assert!(res_inc.can_insert_ref);
        assert_eq!(res_inc.signature.unwrap().name, "SUM");

        // 3. Unknown function + wrong arity diagnostics.
        let bad = "=NO_SUCH_FN(1)+ROUND(1.5)";
        let res_bad = analyze_formula(bad, 0, eng.workbook(), s0);
        assert!(
            res_bad
                .diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown function 'NO_SUCH_FN'"))
        );
        assert!(
            res_bad
                .diagnostics
                .iter()
                .any(|d| d.message.contains("ROUND expects 2 arguments, got 1"))
        );
        assert!(
            res_bad
                .tokens
                .iter()
                .any(|t| t.kind == SyntaxTokenKind::Function)
        );

        // 4. Autocomplete for `"=VL"` and `"=Sh"`.
        let comp_fn = analyze_formula("=VL", 3, eng.workbook(), s0);
        assert_eq!(comp_fn.completions[0].label, "VLOOKUP");
        assert_eq!(comp_fn.completions[0].insert_text, "VLOOKUP(");
        assert_eq!(comp_fn.completions[0].kind, CompletionKind::Function);

        let comp_sh = analyze_formula("=Sh", 3, eng.workbook(), s0);
        assert!(
            comp_sh
                .completions
                .iter()
                .any(|c| c.kind == CompletionKind::Sheet && c.insert_text == "'Sheet 2'!")
        );
    }

    #[test]
    fn f4_toggle_dollar_cycles_all_four_states_and_preserves_sheet_prefix() {
        let s0 = "='Sheet 2'!B2+10";
        let (s1, a1, b1) = toggle_reference_dollar(s0, 12).unwrap();
        assert_eq!(s1, "='Sheet 2'!$B$2+10");
        let (s2, _, _) = toggle_reference_dollar(&s1, b1).unwrap();
        assert_eq!(s2, "='Sheet 2'!B$2+10");
        let (s3, _, _) = toggle_reference_dollar(&s2, a1 + 2).unwrap();
        assert_eq!(s3, "='Sheet 2'!$B2+10");
        let (s4, _, _) = toggle_reference_dollar(&s3, a1 + 2).unwrap();
        assert_eq!(s4, s0);

        // Range cycling: `A1:C3 -> $A$1:$C$3 -> A$1:C$3 -> $A1:$C3 -> A1:C3`
        let (r1, _, end1) = toggle_reference_dollar("=SUM(A1:C3)", 7).unwrap();
        assert_eq!(r1, "=SUM($A$1:$C$3)");
        let (r2, _, end2) = toggle_reference_dollar(&r1, end1).unwrap();
        assert_eq!(r2, "=SUM(A$1:C$3)");
        let (r3, _, _) = toggle_reference_dollar(&r2, end2).unwrap();
        assert_eq!(r3, "=SUM($A1:$C3)");

        // Cross-sheet range formatter quotes spaces automatically.
        let mut eng = FormulaEngine::new();
        let sh2 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: sh2,
            name: String::from("Q4 Sales"),
            after: Some(SheetId(0)),
        })
        .unwrap();
        assert_eq!(
            format_sheet_range_ref(eng.workbook(), SheetId(0), sh2, 1, 1, 9, 3),
            "'Q4 Sales'!B2:D10"
        );
        assert_eq!(
            format_sheet_range_ref(eng.workbook(), SheetId(0), SheetId(0), 4, 2, 4, 2),
            "C5"
        );
    }

    #[test]
    fn named_range_completions_highlights_and_spill_ref_work() {
        use alloc::vec;

        use reticula_core::id::{ColId, RowId};
        use reticula_core::ops::FormulaMode;

        use super::analyze_formula_with_spills;
        use crate::binder::BoundFormula;

        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        let rate_f =
            BoundFormula::parse_and_bind("=B2:C3", FormulaMode::Dynamic, eng.workbook(), s0)
                .unwrap();
        let hidden_f =
            BoundFormula::parse_and_bind("=A1", FormulaMode::Dynamic, eng.workbook(), s0).unwrap();
        eng.apply_txn(vec![
            Op::DefineNamedRange {
                scope: None,
                name: String::from("TaxRate"),
                formula: rate_f,
                comment: Some(String::from("VAT matrix")),
                hidden: false,
            },
            Op::DefineNamedRange {
                scope: None,
                name: String::from("_xlnm.Print_Area"),
                formula: hidden_f,
                comment: None,
                hidden: true,
            },
        ])
        .unwrap();

        // Active spill at A1 (3 rows x 2 cols)
        eng.edit_cell_raw(s0, RowId(0), ColId(0), "=SEQUENCE(3,2)")
            .unwrap();
        eng.recalc_all();

        let analysis = analyze_formula_with_spills(
            "=SUM(A1#)+TaxRate",
            17,
            eng.workbook(),
            s0,
            Some(eng.spills()),
        );
        assert!(
            analysis.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            analysis.diagnostics
        );
        assert_eq!(analysis.references.len(), 2);
        assert_eq!(analysis.references[0].first_row, 0);
        assert_eq!(analysis.references[0].last_row, 2);
        assert_eq!(analysis.references[0].first_col, 0);
        assert_eq!(analysis.references[0].last_col, 1);
        assert_eq!(analysis.references[1].first_row, 1);
        assert_eq!(analysis.references[1].last_row, 2);
        assert_eq!(analysis.references[1].first_col, 1);
        assert_eq!(analysis.references[1].last_col, 2);

        // Autocomplete finds visible NamedRange `TaxRate` and hides `_xlnm.Print_Area`.
        let comp = analyze_formula("=Tax", 4, eng.workbook(), s0);
        assert!(
            comp.completions
                .iter()
                .any(|c| c.kind == CompletionKind::NamedRange && c.label == "TaxRate")
        );
        let hidden_comp = analyze_formula("=_xlnm", 6, eng.workbook(), s0);
        assert!(hidden_comp.completions.is_empty());

        // F4 dollar toggle preserves trailing `#` on spill ref.
        let (toggled, _, _) = toggle_reference_dollar("=SUM(A1#)", 6).unwrap();
        assert_eq!(toggled, "=SUM($A$1#)");
    }
}

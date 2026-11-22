//! Workbook search and batch find/replace (`17-m3a.md` Step 4.3).
//!
//! Performs sparse cell scans via [`Sheet::for_each_non_empty_cell`] and uses
//! a bounded-step `#![no_std]` Thompson NFA regex matcher so pathological patterns
//! can neither overflow the stack nor hang the worker.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::input::{InferredInput, infer_input_for_cell};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, FormulaMode, Op};
use reticula_core::sheet::Sheet;
use reticula_core::value::Value;
use serde::{Deserialize, Serialize};

use crate::binder::BoundFormula;
use crate::engine::FormulaEngine;
use crate::filter::read_formatted_cell;

/// Maximum per-cell NFA execution steps before aborting safely.
pub const MAX_REGEX_STEPS: usize = 25_000;
/// Maximum compiled NFA instructions allowed per pattern.
const MAX_NFA_STATES: usize = 1024;
/// Maximum character length of a regex query.
const MAX_REGEX_LEN: usize = 512;
/// Maximum single quantifier upper bound in `{n,m}` expansion.
const MAX_QUANTIFIER_REPEAT: usize = 64;
/// Maximum replacement edits emitted in a single transaction batch.
pub const MAX_REPLACE_BATCH: usize = 10_000;

/// Search target scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchScope {
    /// Restrict scan to the currently active worksheet.
    ActiveSheet,
    /// Scan all non-deleted worksheets in tab order.
    WholeWorkbook,
}

/// Cell text source inspected during search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchIn {
    /// Match against formatted display values (including spill shadow values).
    DisplayValues,
    /// Match against unparsed formula source (`=SUM(A1:A10)`).
    Formulas,
}

/// Options controlling find & replace matching.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindOptions {
    /// Query substring or regular expression.
    pub query: String,
    /// Active sheet vs whole workbook.
    pub scope: SearchScope,
    /// Formatted values vs formula expressions.
    pub search_in: SearchIn,
    /// Case-sensitive comparison.
    pub case_sensitive: bool,
    /// Match entire cell text rather than any substring.
    pub whole_cell: bool,
    /// Treat `query` as a regular expression.
    pub use_regex: bool,
    /// Skip rows currently hidden by an active AutoFilter.
    pub skip_hidden: bool,
}

/// Single matched cell returned by [`find_all`] or [`find_next`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    /// Worksheet identifier.
    pub sheet: SheetId,
    /// Worksheet display name.
    pub sheet_name: String,
    /// 0-based physical row index on `sheet`.
    pub row_pos: u32,
    /// 0-based physical column index on `sheet`.
    pub col_pos: u32,
    /// Live row identity.
    pub row_id: RowId,
    /// Live column identity.
    pub col_id: ColId,
    /// Matched display text or unparsed formula string.
    pub text: String,
    /// Whether the cell holds a formula.
    pub is_formula: bool,
}

/// Planned atomic batch replacement produced by [`plan_replace_all`].
#[derive(Clone, Debug, PartialEq)]
pub struct ReplacePlan {
    /// Per-sheet `Op::SetCells` batches (capped at `<= 10,000` edits total).
    pub ops: Vec<Op<BoundFormula>>,
    /// Number of valid cell edits generated.
    pub replaced_cells: u32,
    /// Formula replacements skipped because the resulting formula failed syntax checks.
    pub skipped_invalid_formulas: u32,
    /// `true` when additional matching cells remained beyond the batch cap.
    pub truncated: bool,
}

/// Bounded-search error conditions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// Query string was empty.
    EmptyQuery,
    /// Regular expression syntax or complexity check failed.
    InvalidRegex(String),
    /// Per-cell regex NFA step budget exceeded.
    StepBudgetExceeded,
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuery => f.write_str("Search query cannot be empty"),
            Self::InvalidRegex(msg) => f.write_str(msg),
            Self::StepBudgetExceeded => {
                f.write_str("Regular expression exceeded per-cell evaluation step limit")
            }
        }
    }
}

/// Result of [`find_all`] carrying bounded row-major hits and truncation flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindAllResult {
    /// Matched cells ordered by `(tab_order, row_pos, col_pos)`.
    pub matches: Vec<SearchMatch>,
    /// `true` if more matches existed beyond `max_results`.
    pub truncated: bool,
}

/// Finds up to `max_results` cells matching `opts` in row-major order.
pub fn find_all(
    engine: &FormulaEngine,
    active_sheet: SheetId,
    opts: &FindOptions,
    max_results: usize,
) -> Result<FindAllResult, SearchError> {
    let matcher = Matcher::compile(opts)?;
    let cap = max_results.max(1);
    let mut top_k: BTreeMap<(usize, u32, u32), SearchMatch> = BTreeMap::new();
    let mut truncated = false;

    visit_candidate_cells(engine, active_sheet, opts, &matcher, |key, candidate| {
        if top_k.len() < cap {
            top_k.insert(key, candidate);
        } else {
            truncated = true;
            if let Some(last_key) = top_k.keys().next_back().copied()
                && key < last_key
            {
                top_k.remove(&last_key);
                top_k.insert(key, candidate);
            }
        }
    })?;

    Ok(FindAllResult {
        matches: top_k.into_values().collect(),
        truncated,
    })
}

/// Finds the next (or previous if `backwards` is `true`) matching cell from
/// `(active_sheet, from_row, from_col)`, wrapping cleanly across sheets/rows.
pub fn find_next(
    engine: &FormulaEngine,
    active_sheet: SheetId,
    from_row: u32,
    from_col: u32,
    backwards: bool,
    opts: &FindOptions,
) -> Result<Option<SearchMatch>, SearchError> {
    let matcher = Matcher::compile(opts)?;
    let sheets = collect_target_sheets(engine, active_sheet, opts.scope);
    let active_tab_idx = sheets
        .iter()
        .position(|s| s.id() == active_sheet)
        .unwrap_or(0);
    let cursor = (active_tab_idx, from_row, from_col);

    let mut best_rel: Option<((usize, u32, u32), SearchMatch)> = None;
    let mut best_wrap: Option<((usize, u32, u32), SearchMatch)> = None;

    visit_candidate_cells(engine, active_sheet, opts, &matcher, |key, candidate| {
        if backwards {
            if key < cursor && best_rel.as_ref().is_none_or(|(cur_k, _)| key > *cur_k) {
                best_rel = Some((key, candidate.clone()));
            }
            if best_wrap.as_ref().is_none_or(|(cur_k, _)| key > *cur_k) {
                best_wrap = Some((key, candidate));
            }
        } else {
            if key > cursor && best_rel.as_ref().is_none_or(|(cur_k, _)| key < *cur_k) {
                best_rel = Some((key, candidate.clone()));
            }
            if best_wrap.as_ref().is_none_or(|(cur_k, _)| key < *cur_k) {
                best_wrap = Some((key, candidate));
            }
        }
    })?;

    Ok(best_rel.or(best_wrap).map(|(_, m)| m))
}

/// Plans an atomic batch Replace All (`<= max_edits.min(10_000)` edits).
///
/// Formula replacements are parsed & bound before inclusion; syntactically
/// invalid replacements increment `skipped_invalid_formulas` and are skipped.
pub fn plan_replace_all(
    engine: &FormulaEngine,
    active_sheet: SheetId,
    opts: &FindOptions,
    replacement: &str,
    max_edits: usize,
) -> Result<ReplacePlan, SearchError> {
    let cap = max_edits.clamp(1, MAX_REPLACE_BATCH);
    let matcher = Matcher::compile(opts)?;
    let found = find_all(engine, active_sheet, opts, cap.saturating_add(1))?;
    let mut truncated = found.truncated || found.matches.len() > cap;

    let wb = engine.workbook();
    let mut per_sheet: BTreeMap<SheetId, Vec<CellEdit<BoundFormula>>> = BTreeMap::new();
    let mut replaced_cells: u32 = 0;
    let mut skipped_invalid_formulas: u32 = 0;

    for m in found.matches.into_iter().take(cap) {
        let Some(sheet) = wb.live_sheet(m.sheet) else {
            continue;
        };
        let is_shadow = !m.is_formula
            && engine
                .spills()
                .anchor_of_cell((m.sheet, m.row_id, m.col_id))
                .is_some();
        if is_shadow {
            continue;
        }
        if opts.search_in == SearchIn::DisplayValues && m.is_formula {
            continue;
        }

        let new_raw = matcher.replace_in(&m.text, replacement)?;
        if new_raw == m.text {
            continue;
        }

        if m.is_formula || new_raw.starts_with('=') {
            let Ok(formula) =
                BoundFormula::parse_and_bind(&new_raw, FormulaMode::Dynamic, wb, m.sheet)
            else {
                skipped_invalid_formulas = skipped_invalid_formulas.saturating_add(1);
                continue;
            };
            per_sheet.entry(m.sheet).or_default().push(CellEdit {
                row: m.row_id,
                col: m.col_id,
                input: CellInput::Formula {
                    formula: BoundFormula {
                        mode: FormulaMode::Dynamic,
                        root: formula.root,
                    },
                    cached: None,
                },
                format: FormatPatch::Keep,
            });
            replaced_cells = replaced_cells.saturating_add(1);
        } else {
            let existing_fmt = sheet.effective_format_by_id(m.row_id, m.col_id);
            let edit = match infer_input_for_cell(&new_raw, existing_fmt) {
                Ok(InferredInput::Empty) => CellEdit {
                    row: m.row_id,
                    col: m.col_id,
                    input: CellInput::Empty,
                    format: FormatPatch::Keep,
                },
                Ok(InferredInput::Scalar {
                    value,
                    suggested_format,
                }) => CellEdit {
                    row: m.row_id,
                    col: m.col_id,
                    input: CellInput::Scalar(value),
                    format: suggested_format.map_or(FormatPatch::Keep, FormatPatch::Set),
                },
                Ok(InferredInput::Formula(raw)) => {
                    let Ok(formula) =
                        BoundFormula::parse_and_bind(&raw.source, raw.mode, wb, m.sheet)
                    else {
                        skipped_invalid_formulas = skipped_invalid_formulas.saturating_add(1);
                        continue;
                    };
                    CellEdit {
                        row: m.row_id,
                        col: m.col_id,
                        input: CellInput::Formula {
                            formula: BoundFormula {
                                mode: raw.mode,
                                root: formula.root,
                            },
                            cached: None,
                        },
                        format: FormatPatch::Keep,
                    }
                }
                Err(_) => {
                    skipped_invalid_formulas = skipped_invalid_formulas.saturating_add(1);
                    continue;
                }
            };
            per_sheet.entry(m.sheet).or_default().push(edit);
            replaced_cells = replaced_cells.saturating_add(1);
        }
    }

    if replaced_cells as usize >= cap && found.truncated {
        truncated = true;
    }

    let ops = per_sheet
        .into_iter()
        .filter(|(_, edits)| !edits.is_empty())
        .map(|(sheet, edits)| Op::SetCells { sheet, edits })
        .collect();

    Ok(ReplacePlan {
        ops,
        replaced_cells,
        skipped_invalid_formulas,
        truncated,
    })
}

fn collect_target_sheets(
    engine: &FormulaEngine,
    active_sheet: SheetId,
    scope: SearchScope,
) -> Vec<&Sheet> {
    let wb = engine.workbook();
    match scope {
        SearchScope::ActiveSheet => wb.live_sheet(active_sheet).into_iter().collect(),
        SearchScope::WholeWorkbook => wb.sheets().iter().filter(|s| !s.is_deleted()).collect(),
    }
}

fn visit_candidate_cells(
    engine: &FormulaEngine,
    active_sheet: SheetId,
    opts: &FindOptions,
    matcher: &Matcher,
    mut on_match: impl FnMut((usize, u32, u32), SearchMatch),
) -> Result<(), SearchError> {
    let wb = engine.workbook();
    let sheets = collect_target_sheets(engine, active_sheet, opts.scope);
    let mut buf = String::new();
    let mut step_err: Option<SearchError> = None;

    match opts.search_in {
        SearchIn::Formulas => {
            let mut sheet_tab: BTreeMap<SheetId, (usize, &Sheet)> = BTreeMap::new();
            for (tab_idx, s) in sheets.into_iter().enumerate() {
                sheet_tab.insert(s.id(), (tab_idx, s));
            }
            for (&(sid, row_id, col_id), formula) in wb.formulas() {
                let Some(&(tab_idx, sheet)) = sheet_tab.get(&sid) else {
                    continue;
                };
                if opts.skip_hidden && engine.is_row_hidden(sid, row_id) {
                    continue;
                }
                let (Some(row_pos), Some(col_pos)) = (
                    sheet.row_axis().id_to_pos(row_id.0),
                    sheet.col_axis().id_to_pos(col_id.0),
                ) else {
                    continue;
                };
                let text = formula.unparse(wb, sid);
                match matcher.matches(&text) {
                    Ok(true) => on_match(
                        (tab_idx, row_pos, col_pos),
                        SearchMatch {
                            sheet: sid,
                            sheet_name: String::from(sheet.name()),
                            row_pos,
                            col_pos,
                            row_id,
                            col_id,
                            text,
                            is_formula: true,
                        },
                    ),
                    Ok(false) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        SearchIn::DisplayValues => {
            let formulas = wb.formulas();
            let spills = engine.spills();
            for (tab_idx, sheet) in sheets.into_iter().enumerate() {
                let sid = sheet.id();
                let sheet_name = String::from(sheet.name());
                sheet.for_each_non_empty_cell(|row_pos, col_pos, row_id, col_id, _| {
                    if step_err.is_some() {
                        return;
                    }
                    if opts.skip_hidden && engine.is_row_hidden(sid, row_id) {
                        return;
                    }
                    let is_blank =
                        read_formatted_cell(sheet, Some(spills), row_id, col_id, &mut buf);
                    if is_blank && buf.is_empty() {
                        return;
                    }
                    match matcher.matches(&buf) {
                        Ok(true) => on_match(
                            (tab_idx, row_pos, col_pos),
                            SearchMatch {
                                sheet: sid,
                                sheet_name: sheet_name.clone(),
                                row_pos,
                                col_pos,
                                row_id,
                                col_id,
                                text: buf.clone(),
                                is_formula: formulas.contains_key(&(sid, row_id, col_id)),
                            },
                        ),
                        Ok(false) => {}
                        Err(e) => step_err = Some(e),
                    }
                });
                if let Some(e) = step_err {
                    return Err(e);
                }

                for anchor in spills.anchors_on_sheet(sid) {
                    let Some(entry) = spills.active_spill(anchor) else {
                        continue;
                    };
                    for (dr, &shadow_rid) in entry.rows.iter().enumerate() {
                        if opts.skip_hidden && engine.is_row_hidden(sid, shadow_rid) {
                            continue;
                        }
                        let Some(shadow_r_pos) = sheet.row_axis().id_to_pos(shadow_rid.0) else {
                            continue;
                        };
                        for (dc, &shadow_cid) in entry.cols.iter().enumerate() {
                            if dr == 0 && dc == 0 {
                                continue;
                            }
                            if sheet.value_by_id(shadow_rid, shadow_cid) != Value::Empty {
                                continue;
                            }
                            let Some(shadow_c_pos) = sheet.col_axis().id_to_pos(shadow_cid.0)
                            else {
                                continue;
                            };
                            let is_blank = read_formatted_cell(
                                sheet,
                                Some(spills),
                                shadow_rid,
                                shadow_cid,
                                &mut buf,
                            );
                            if !is_blank && matcher.matches(&buf)? {
                                on_match(
                                    (tab_idx, shadow_r_pos, shadow_c_pos),
                                    SearchMatch {
                                        sheet: sid,
                                        sheet_name: sheet_name.clone(),
                                        row_pos: shadow_r_pos,
                                        col_pos: shadow_c_pos,
                                        row_id: shadow_rid,
                                        col_id: shadow_cid,
                                        text: buf.clone(),
                                        is_formula: false,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ------------------------------------------------ Plain & Bounded Thompson NFA Matcher

enum Matcher {
    Plain {
        needle: String,
        needle_lower: String,
        case_sensitive: bool,
        whole_cell: bool,
    },
    Regex {
        nfa: RegexNfa,
        whole_cell: bool,
    },
}

impl Matcher {
    fn compile(opts: &FindOptions) -> Result<Self, SearchError> {
        if opts.query.is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        if !opts.use_regex {
            return Ok(Self::Plain {
                needle: opts.query.clone(),
                needle_lower: opts.query.to_lowercase(),
                case_sensitive: opts.case_sensitive,
                whole_cell: opts.whole_cell,
            });
        }
        let nfa = RegexNfa::compile(&opts.query, opts.case_sensitive)?;
        Ok(Self::Regex {
            nfa,
            whole_cell: opts.whole_cell,
        })
    }

    fn matches(&self, haystack: &str) -> Result<bool, SearchError> {
        match self {
            Self::Plain {
                needle,
                needle_lower,
                case_sensitive,
                whole_cell,
            } => {
                if *case_sensitive {
                    if *whole_cell {
                        Ok(haystack == needle)
                    } else {
                        Ok(haystack.contains(needle.as_str()))
                    }
                } else {
                    let hay_lower = haystack.to_lowercase();
                    if *whole_cell {
                        Ok(hay_lower == *needle_lower)
                    } else {
                        Ok(hay_lower.contains(needle_lower.as_str()))
                    }
                }
            }
            Self::Regex { nfa, whole_cell } => nfa.is_match(haystack, *whole_cell),
        }
    }

    fn replace_in(&self, text: &str, replacement: &str) -> Result<String, SearchError> {
        match self {
            Self::Plain {
                needle,
                needle_lower,
                case_sensitive,
                whole_cell,
            } => {
                if *whole_cell {
                    return Ok(if self.matches(text)? {
                        String::from(replacement)
                    } else {
                        String::from(text)
                    });
                }
                if *case_sensitive {
                    return Ok(text.replace(needle.as_str(), replacement));
                }
                replace_plain_case_insensitive(text, needle_lower, replacement)
            }
            Self::Regex { nfa, whole_cell } => nfa.replace_all(text, replacement, *whole_cell),
        }
    }
}

fn replace_plain_case_insensitive(
    text: &str,
    needle_lower: &str,
    replacement: &str,
) -> Result<String, SearchError> {
    if needle_lower.is_empty() {
        return Ok(String::from(text));
    }
    let hay_chars: Vec<char> = text.chars().collect();
    let needle_chars: Vec<char> = needle_lower.chars().collect();
    if hay_chars.len() < needle_chars.len() {
        return Ok(String::from(text));
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < hay_chars.len() {
        if i.saturating_add(needle_chars.len()) <= hay_chars.len() {
            let mut matched = true;
            for (k, &nc) in needle_chars.iter().enumerate() {
                if let Some(&hc) = hay_chars.get(i.saturating_add(k))
                    && !char_eq_case_insensitive(hc, nc)
                {
                    matched = false;
                    break;
                }
            }
            if matched {
                out.push_str(replacement);
                i = i.saturating_add(needle_chars.len());
                continue;
            }
        }
        if let Some(&ch) = hay_chars.get(i) {
            out.push(ch);
        }
        i = i.saturating_add(1);
    }
    Ok(out)
}

#[derive(Clone, Debug)]
enum CharPred {
    Any,
    Literal(char),
    Digit,
    NonDigit,
    Word,
    NonWord,
    Space,
    NonSpace,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
        specials: Vec<CharPred>,
    },
}

impl CharPred {
    fn matches(&self, ch: char, case_sensitive: bool) -> bool {
        match self {
            Self::Any => ch != '\n',
            Self::Literal(target) => {
                if case_sensitive {
                    ch == *target
                } else {
                    char_eq_case_insensitive(ch, *target)
                }
            }
            Self::Digit => ch.is_ascii_digit(),
            Self::NonDigit => !ch.is_ascii_digit(),
            Self::Word => ch.is_ascii_alphanumeric() || ch == '_',
            Self::NonWord => !(ch.is_ascii_alphanumeric() || ch == '_'),
            Self::Space => ch.is_whitespace(),
            Self::NonSpace => !ch.is_whitespace(),
            Self::Class {
                negated,
                ranges,
                specials,
            } => {
                let mut hit = false;
                for &(lo, hi) in ranges {
                    if case_sensitive {
                        if ch >= lo && ch <= hi {
                            hit = true;
                            break;
                        }
                    } else if char_in_range_ci(ch, lo, hi) {
                        hit = true;
                        break;
                    }
                }
                if !hit {
                    for sp in specials {
                        if sp.matches(ch, case_sensitive) {
                            hit = true;
                            break;
                        }
                    }
                }
                if *negated { !hit } else { hit }
            }
        }
    }
}

fn char_eq_case_insensitive(a: char, b: char) -> bool {
    if a == b {
        return true;
    }
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(&b);
    }
    a.to_lowercase().eq(b.to_lowercase())
}

fn char_in_range_ci(ch: char, lo: char, hi: char) -> bool {
    if ch >= lo && ch <= hi {
        return true;
    }
    if ch.is_ascii() && lo.is_ascii() && hi.is_ascii() {
        let cu = ch.to_ascii_lowercase();
        let lu = lo.to_ascii_lowercase();
        let hu = hi.to_ascii_lowercase();
        return cu >= lu && cu <= hu;
    }
    false
}

#[derive(Clone, Debug)]
enum Ast {
    Empty,
    Pred(CharPred),
    AnchorStart,
    AnchorEnd,
    Seq(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        node: alloc::boxed::Box<Ast>,
        min: usize,
        max: Option<usize>,
    },
}

struct RegexParser {
    chars: Vec<char>,
    pos: usize,
}

impl RegexParser {
    fn parse(pattern: &str) -> Result<Ast, SearchError> {
        let chars: Vec<char> = pattern.chars().collect();
        if chars.len() > MAX_REGEX_LEN {
            return Err(SearchError::InvalidRegex(format!(
                "Regular expression exceeds {MAX_REGEX_LEN} characters"
            )));
        }
        let mut parser = Self { chars, pos: 0 };
        let ast = parser.parse_alt(0)?;
        if parser.pos < parser.chars.len() {
            return Err(SearchError::InvalidRegex(String::from(
                "Unexpected closing parenthesis ')'",
            )));
        }
        Ok(ast)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos = self.pos.saturating_add(1);
        Some(ch)
    }

    fn parse_alt(&mut self, depth: usize) -> Result<Ast, SearchError> {
        if depth > 64 {
            return Err(SearchError::InvalidRegex(String::from(
                "Regular expression nesting too deep",
            )));
        }
        let mut branches = Vec::new();
        branches.push(self.parse_seq(depth)?);
        while self.peek() == Some('|') {
            self.next();
            branches.push(self.parse_seq(depth)?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap_or(Ast::Empty))
        } else {
            Ok(Ast::Alt(branches))
        }
    }

    fn parse_seq(&mut self, depth: usize) -> Result<Ast, SearchError> {
        let mut items = Vec::new();
        while let Some(ch) = self.peek() {
            if ch == ')' || ch == '|' {
                break;
            }
            let atom = self.parse_atom(depth)?;
            let node = self.parse_quantifier(atom)?;
            items.push(node);
        }
        if items.is_empty() {
            Ok(Ast::Empty)
        } else if items.len() == 1 {
            Ok(items.pop().unwrap_or(Ast::Empty))
        } else {
            Ok(Ast::Seq(items))
        }
    }

    fn parse_atom(&mut self, depth: usize) -> Result<Ast, SearchError> {
        let Some(ch) = self.next() else {
            return Err(SearchError::InvalidRegex(String::from(
                "Unexpected end of regular expression",
            )));
        };
        match ch {
            '*' | '+' | '?' => Err(SearchError::InvalidRegex(format!(
                "Quantifier '{ch}' without preceding atom"
            ))),
            '^' => Ok(Ast::AnchorStart),
            '$' => Ok(Ast::AnchorEnd),
            '.' => Ok(Ast::Pred(CharPred::Any)),
            '(' => {
                let inner = self.parse_alt(depth.saturating_add(1))?;
                if self.next() != Some(')') {
                    return Err(SearchError::InvalidRegex(String::from(
                        "Unclosed group '('",
                    )));
                }
                Ok(inner)
            }
            '[' => self.parse_char_class(),
            '\\' => Ok(Ast::Pred(self.parse_escape()?)),
            other => Ok(Ast::Pred(CharPred::Literal(other))),
        }
    }

    fn parse_escape(&mut self) -> Result<CharPred, SearchError> {
        let Some(esc) = self.next() else {
            return Err(SearchError::InvalidRegex(String::from(
                "Trailing backslash '\\' in regular expression",
            )));
        };
        Ok(match esc {
            'd' => CharPred::Digit,
            'D' => CharPred::NonDigit,
            'w' => CharPred::Word,
            'W' => CharPred::NonWord,
            's' => CharPred::Space,
            'S' => CharPred::NonSpace,
            'n' => CharPred::Literal('\n'),
            't' => CharPred::Literal('\t'),
            'r' => CharPred::Literal('\r'),
            other => CharPred::Literal(other),
        })
    }

    fn parse_char_class(&mut self) -> Result<Ast, SearchError> {
        let mut negated = false;
        if self.peek() == Some('^') {
            self.next();
            negated = true;
        }
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut specials: Vec<CharPred> = Vec::new();
        let mut first = true;

        while let Some(ch) = self.next() {
            if ch == ']' && !first {
                return Ok(Ast::Pred(CharPred::Class {
                    negated,
                    ranges,
                    specials,
                }));
            }
            first = false;
            if ch == '\\' {
                match self.parse_escape()? {
                    CharPred::Literal(lit) => {
                        self.finish_class_char(lit, &mut ranges)?;
                    }
                    sp => specials.push(sp),
                }
            } else {
                self.finish_class_char(ch, &mut ranges)?;
            }
        }
        Err(SearchError::InvalidRegex(String::from(
            "Unclosed character class '['",
        )))
    }

    fn finish_class_char(
        &mut self,
        start_ch: char,
        ranges: &mut Vec<(char, char)>,
    ) -> Result<(), SearchError> {
        if self.peek() == Some('-') && self.chars.get(self.pos.saturating_add(1)) != Some(&']') {
            self.next();
            let Some(end_raw) = self.next() else {
                return Err(SearchError::InvalidRegex(String::from(
                    "Unclosed character range",
                )));
            };
            let end_ch = if end_raw == '\\' {
                match self.parse_escape()? {
                    CharPred::Literal(lit) => lit,
                    _ => {
                        return Err(SearchError::InvalidRegex(String::from(
                            "Invalid character class range endpoint",
                        )));
                    }
                }
            } else {
                end_raw
            };
            if end_ch < start_ch {
                return Err(SearchError::InvalidRegex(String::from(
                    "Character class range out of order",
                )));
            }
            ranges.push((start_ch, end_ch));
        } else {
            ranges.push((start_ch, start_ch));
        }
        Ok(())
    }

    fn parse_quantifier(&mut self, atom: Ast) -> Result<Ast, SearchError> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.next();
                (0usize, None)
            }
            Some('+') => {
                self.next();
                (1usize, None)
            }
            Some('?') => {
                self.next();
                (0usize, Some(1usize))
            }
            Some('{') => {
                let saved = self.pos;
                if let Some((min, max)) = self.try_parse_braced_quant()? {
                    (min, max)
                } else {
                    self.pos = saved;
                    return Ok(atom);
                }
            }
            _ => return Ok(atom),
        };
        if self.peek() == Some('?') {
            self.next();
        }
        Ok(Ast::Repeat {
            node: alloc::boxed::Box::new(atom),
            min,
            max,
        })
    }

    fn try_parse_braced_quant(&mut self) -> Result<Option<(usize, Option<usize>)>, SearchError> {
        self.next(); // consume '{'
        let min = self.read_digits();
        let Some(min) = min else {
            return Ok(None);
        };
        if min > MAX_QUANTIFIER_REPEAT {
            return Err(SearchError::InvalidRegex(format!(
                "Quantifier repeat count exceeds {MAX_QUANTIFIER_REPEAT}"
            )));
        }
        match self.peek() {
            Some('}') => {
                self.next();
                Ok(Some((min, Some(min))))
            }
            Some(',') => {
                self.next();
                if self.peek() == Some('}') {
                    self.next();
                    return Ok(Some((min, None)));
                }
                let Some(max) = self.read_digits() else {
                    return Ok(None);
                };
                if max > MAX_QUANTIFIER_REPEAT {
                    return Err(SearchError::InvalidRegex(format!(
                        "Quantifier repeat count exceeds {MAX_QUANTIFIER_REPEAT}"
                    )));
                }
                if max < min {
                    return Err(SearchError::InvalidRegex(String::from(
                        "Quantifier min exceeds max in {m,n}",
                    )));
                }
                if self.next() != Some('}') {
                    return Ok(None);
                }
                Ok(Some((min, Some(max))))
            }
            _ => Ok(None),
        }
    }

    fn read_digits(&mut self) -> Option<usize> {
        let mut val: usize = 0;
        let mut any = false;
        while let Some(ch) = self.peek()
            && ch.is_ascii_digit()
        {
            any = true;
            self.next();
            val = val
                .saturating_mul(10)
                .saturating_add(usize::from(ch as u8).saturating_sub(usize::from(b'0')));
        }
        any.then_some(val)
    }
}

#[derive(Clone, Debug)]
enum Inst {
    Match,
    Pred { pred: CharPred, next: usize },
    AnchorStart { next: usize },
    AnchorEnd { next: usize },
    Split { a: usize, b: usize },
    Jump { next: usize },
}

struct NfaCompiler {
    insts: Vec<Inst>,
}

impl NfaCompiler {
    fn emit(&mut self, inst: Inst) -> Result<usize, SearchError> {
        if self.insts.len() >= MAX_NFA_STATES {
            return Err(SearchError::InvalidRegex(String::from(
                "Regular expression exceeds NFA state budget",
            )));
        }
        let idx = self.insts.len();
        self.insts.push(inst);
        Ok(idx)
    }

    fn compile_node(&mut self, node: &Ast, next: usize) -> Result<usize, SearchError> {
        match node {
            Ast::Empty => Ok(next),
            Ast::Pred(pred) => self.emit(Inst::Pred {
                pred: pred.clone(),
                next,
            }),
            Ast::AnchorStart => self.emit(Inst::AnchorStart { next }),
            Ast::AnchorEnd => self.emit(Inst::AnchorEnd { next }),
            Ast::Seq(items) => {
                let mut cur = next;
                for item in items.iter().rev() {
                    cur = self.compile_node(item, cur)?;
                }
                Ok(cur)
            }
            Ast::Alt(branches) => {
                let mut iter = branches.iter().rev();
                let Some(last) = iter.next() else {
                    return Ok(next);
                };
                let mut cur = self.compile_node(last, next)?;
                for branch in iter {
                    let b_entry = self.compile_node(branch, next)?;
                    cur = self.emit(Inst::Split { a: b_entry, b: cur })?;
                }
                Ok(cur)
            }
            Ast::Repeat {
                node: inner,
                min,
                max,
            } => {
                let mut cur = next;
                match max {
                    None => {
                        let loop_jump = self.emit(Inst::Jump { next: 0 })?;
                        let body = self.compile_node(inner, loop_jump)?;
                        let split = self.emit(Inst::Split { a: body, b: cur })?;
                        if let Some(Inst::Jump { next: jn }) = self.insts.get_mut(loop_jump) {
                            *jn = split;
                        }
                        cur = split;
                    }
                    Some(max_val) => {
                        let optional = max_val.saturating_sub(*min);
                        for _ in 0..optional {
                            let body = self.compile_node(inner, cur)?;
                            cur = self.emit(Inst::Split { a: body, b: cur })?;
                        }
                    }
                }
                for _ in 0..*min {
                    cur = self.compile_node(inner, cur)?;
                }
                Ok(cur)
            }
        }
    }
}

struct RegexNfa {
    insts: Vec<Inst>,
    start: usize,
    case_sensitive: bool,
}

impl RegexNfa {
    fn compile(pattern: &str, case_sensitive: bool) -> Result<Self, SearchError> {
        let ast = RegexParser::parse(pattern)?;
        let mut compiler = NfaCompiler { insts: Vec::new() };
        let match_state = compiler.emit(Inst::Match)?;
        let start = compiler.compile_node(&ast, match_state)?;
        Ok(Self {
            insts: compiler.insts,
            start,
            case_sensitive,
        })
    }

    fn is_match(&self, haystack: &str, whole_cell: bool) -> Result<bool, SearchError> {
        let chars: Vec<char> = haystack.chars().collect();
        if whole_cell {
            return Ok(self.run_from(&chars, 0, true)?.is_some());
        }
        for start_pos in 0..=chars.len() {
            if self.run_from(&chars, start_pos, false)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn replace_all(
        &self,
        haystack: &str,
        replacement: &str,
        whole_cell: bool,
    ) -> Result<String, SearchError> {
        let chars: Vec<char> = haystack.chars().collect();
        if whole_cell {
            return Ok(if self.run_from(&chars, 0, true)?.is_some() {
                String::from(replacement)
            } else {
                String::from(haystack)
            });
        }
        let mut out = String::with_capacity(haystack.len());
        let mut pos = 0usize;
        while pos <= chars.len() {
            if let Some(end) = self.run_from(&chars, pos, false)? {
                out.push_str(replacement);
                if end > pos {
                    pos = end;
                    continue;
                }
                if pos == chars.len() {
                    break;
                }
            }
            if let Some(&ch) = chars.get(pos) {
                out.push(ch);
            }
            pos = pos.saturating_add(1);
        }
        Ok(out)
    }

    fn run_from(
        &self,
        chars: &[char],
        start_pos: usize,
        require_end: bool,
    ) -> Result<Option<usize>, SearchError> {
        let n_states = self.insts.len();
        let mut gen_id: u32 = 1;
        let mut seen = alloc::vec![0u32; n_states];
        let mut cur_states = Vec::new();
        let mut next_states = Vec::new();
        let mut steps_left = MAX_REGEX_STEPS;
        let mut best_end: Option<usize> = None;

        self.add_state(
            self.start,
            start_pos,
            chars.len(),
            &mut cur_states,
            &mut seen,
            gen_id,
            &mut steps_left,
        )?;
        if cur_states
            .iter()
            .any(|&s| matches!(self.insts.get(s), Some(Inst::Match)))
            && (!require_end || start_pos == chars.len())
        {
            best_end = Some(start_pos);
        }

        for pos in start_pos..chars.len() {
            let Some(&ch) = chars.get(pos) else {
                break;
            };
            if cur_states.is_empty() {
                break;
            }
            gen_id = gen_id.saturating_add(1);
            next_states.clear();
            let next_pos = pos.saturating_add(1);
            for &sid in &cur_states {
                if let Some(Inst::Pred { pred, next }) = self.insts.get(sid)
                    && pred.matches(ch, self.case_sensitive)
                {
                    self.add_state(
                        *next,
                        next_pos,
                        chars.len(),
                        &mut next_states,
                        &mut seen,
                        gen_id,
                        &mut steps_left,
                    )?;
                }
            }
            core::mem::swap(&mut cur_states, &mut next_states);
            if cur_states
                .iter()
                .any(|&s| matches!(self.insts.get(s), Some(Inst::Match)))
                && (!require_end || next_pos == chars.len())
            {
                best_end = Some(next_pos);
            }
        }

        Ok(best_end)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_state(
        &self,
        initial_sid: usize,
        pos: usize,
        len: usize,
        out: &mut Vec<usize>,
        seen: &mut [u32],
        gen_id: u32,
        steps_left: &mut usize,
    ) -> Result<(), SearchError> {
        let mut stack = alloc::vec![initial_sid];
        while let Some(sid) = stack.pop() {
            if *steps_left == 0 {
                return Err(SearchError::StepBudgetExceeded);
            }
            *steps_left = steps_left.saturating_sub(1);
            let Some(slot) = seen.get_mut(sid) else {
                continue;
            };
            if *slot == gen_id {
                continue;
            }
            *slot = gen_id;
            match self.insts.get(sid) {
                Some(Inst::Split { a, b }) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Some(Inst::Jump { next }) => {
                    stack.push(*next);
                }
                Some(Inst::AnchorStart { next }) => {
                    if pos == 0 {
                        stack.push(*next);
                    }
                }
                Some(Inst::AnchorEnd { next }) => {
                    if pos == len {
                        stack.push(*next);
                    }
                }
                Some(Inst::Match | Inst::Pred { .. }) => {
                    out.push(sid);
                }
                None => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use alloc::vec;

    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};

    use super::*;

    #[test]
    fn find_all_find_next_and_replace_handle_regex_formulas_and_hidden_rows() {
        let mut engine = FormulaEngine::new();
        engine
            .apply_op(Op::SetCells {
                sheet: SheetId(0),
                edits: vec![
                    CellEdit {
                        row: RowId(0),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Text(String::from("Item"))),
                        format: FormatPatch::Keep,
                    },
                    CellEdit {
                        row: RowId(1),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Text(String::from("order-101"))),
                        format: FormatPatch::Keep,
                    },
                    CellEdit {
                        row: RowId(2),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Text(String::from("order-202"))),
                        format: FormatPatch::Keep,
                    },
                    CellEdit {
                        row: RowId(3),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Text(String::from("misc"))),
                        format: FormatPatch::Keep,
                    },
                ],
            })
            .unwrap();
        engine
            .edit_cell_raw(SheetId(0), RowId(1), ColId(1), "=10+5")
            .unwrap();
        engine
            .edit_cell_raw(SheetId(0), RowId(2), ColId(1), "=10+8")
            .unwrap();
        engine.recalc_all();

        // Filter hides row 2 ("order-202").
        assert!(engine.set_filter_table(SheetId(0), 0, 3, 0, 1));
        assert!(engine.set_column_filter(
            SheetId(0),
            0,
            Some(crate::filter::ColFilterCriterion::TextContains {
                needle: String::from("101"),
                case_sensitive: false,
            })
        ));

        let opts = FindOptions {
            query: String::from(r"^order-\d{3}$"),
            scope: SearchScope::ActiveSheet,
            search_in: SearchIn::DisplayValues,
            case_sensitive: true,
            whole_cell: true,
            use_regex: true,
            skip_hidden: true,
        };
        let res = find_all(&engine, SheetId(0), &opts, 100).unwrap();
        assert_eq!(res.matches.len(), 1);
        assert_eq!(res.matches[0].text, "order-101");

        // Replace inside formulas with valid syntax AND verify invalid replacement skips cleanly.
        let formula_opts = FindOptions {
            query: String::from("10"),
            scope: SearchScope::ActiveSheet,
            search_in: SearchIn::Formulas,
            case_sensitive: false,
            whole_cell: false,
            use_regex: false,
            skip_hidden: false,
        };
        let bad_plan =
            plan_replace_all(&engine, SheetId(0), &formula_opts, "(((invalid", 100).unwrap();
        assert_eq!(bad_plan.replaced_cells, 0);
        assert_eq!(bad_plan.skipped_invalid_formulas, 2);

        let good_plan = plan_replace_all(&engine, SheetId(0), &formula_opts, "20", 100).unwrap();
        assert_eq!(good_plan.replaced_cells, 2);
        assert_eq!(good_plan.skipped_invalid_formulas, 0);
        engine.apply_txn(good_plan.ops).unwrap();
        engine.recalc_all();
        assert_eq!(
            engine.workbook().sheet(SheetId(0)).unwrap().value(1, 1),
            Value::number(25.0)
        );
        assert_eq!(
            engine.workbook().sheet(SheetId(0)).unwrap().value(2, 1),
            Value::number(28.0)
        );
    }

    #[test]
    fn regex_rejects_oversized_or_malformed_and_handles_complex_patterns_safely() {
        assert!(matches!(
            Matcher::compile(&FindOptions {
                query: String::from("(abc"),
                scope: SearchScope::ActiveSheet,
                search_in: SearchIn::DisplayValues,
                case_sensitive: false,
                whole_cell: false,
                use_regex: true,
                skip_hidden: false,
            }),
            Err(SearchError::InvalidRegex(_))
        ));

        let evil = Matcher::compile(&FindOptions {
            query: String::from("(a+)+b"),
            scope: SearchScope::ActiveSheet,
            search_in: SearchIn::DisplayValues,
            case_sensitive: true,
            whole_cell: true,
            use_regex: true,
            skip_hidden: false,
        })
        .unwrap();
        let long_a = "a".repeat(200);
        assert_eq!(evil.matches(&long_a), Ok(false));
    }
}

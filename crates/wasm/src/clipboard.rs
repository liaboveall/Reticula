//! Fill-handle series/formula planner and TSV + HTML `<table>` clipboard
//! interop (`14-m1b.md` 5.2 & 6.3).

use reticula_core::csv::write_escaped_field;
use reticula_core::digest::Digest64;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::input::{InferredInput, infer_input_for_cell};
use reticula_core::ops::{
    CellInput, FormatPatch, FormatTarget, MAX_BULK_CELLS_PER_OP, MAX_EDITS_PER_OP, Op, ScalarValue,
    Workbook,
};
use reticula_core::sheet::Sheet;
use reticula_core::style::{
    BorderWeight, CellStyle, ColorPatch, ColorValue, HorizontalAlign, StylePatch,
};
use reticula_core::value::Value;
use reticula_formula::binder::{
    BoundFormula, FillTargetRect, FormulaShiftSpec, build_fill_plan, shift_formula_for_cell,
    templatize_formula,
};
use reticula_formula::determinism;

use crate::FillDirection;

/// Maximum cells copied to the system clipboard in a single `Copy` / `Cut`
/// action (`200,000` cells keeps TSV + HTML strings bounded to a few MiB).
pub const MAX_CLIPBOARD_CELLS: usize = 200_000;

/// Most `Op::FillFormula` one copy fill is made of. Applying and undoing each
/// takes time that grows with the formulas on the sheet.
const MAX_FILL_FORMULA_OPS: usize = 128;

/// 0-based visible rectangle on a worksheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectPos {
    pub first_row: u32,
    pub row_count: u32,
    pub first_col: u32,
    pub col_count: u32,
}

impl RectPos {
    #[must_use]
    pub const fn end_row(self) -> u32 {
        self.first_row.saturating_add(self.row_count)
    }

    #[must_use]
    pub const fn end_col(self) -> u32 {
        self.first_col.saturating_add(self.col_count)
    }
}

#[derive(Clone, Debug)]
struct CopiedCell {
    input: CellInput<BoundFormula>,
    explicit_format: Option<NumberFormat>,
    effective_format: NumberFormat,
    style_patch: StylePatch,
}

/// In-memory clipboard snapshot captured on `Ctrl+C` / `Ctrl+X` so pasting back
/// inside Reticula shifts bound formula ASTs and preserves styles cleanly.
#[derive(Clone, Debug)]
pub struct InternalClipboard {
    pub src_sheet: SheetId,
    pub src_rect: RectPos,
    pub src_row_runs: Vec<IdRun>,
    pub src_col_runs: Vec<IdRun>,
    pub cut: bool,
    pub fingerprint: String,
    /// Column-major `[col_offset][row_offset]` grid.
    cells: Vec<Vec<CopiedCell>>,
}

/// Output of copying/cutting a rectangular selection.
#[derive(Clone, Debug)]
pub struct CopyPayload {
    pub tsv: String,
    pub html: String,
    pub fingerprint: String,
}

/// Computes a 16-hex normalized TSV fingerprint invariant to `\r\n` vs `\n` and
/// trailing newline differences added by OS clipboards.
#[must_use]
pub fn tsv_fingerprint(tsv: &str) -> String {
    let mut d = Digest64::new();
    let norm = normalize_clipboard_newlines(tsv);
    let trimmed = norm.strip_suffix('\n').unwrap_or(&norm);
    d.write_bytes(trimmed.as_bytes());
    let hex = determinism::to_hex(d.finish());
    String::from_utf8_lossy(&hex).into_owned()
}

fn normalize_clipboard_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return String::from(s);
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'\r') {
            out.push('\n');
            if bytes.get(i.saturating_add(1)) == Some(&b'\n') {
                i = i.saturating_add(2);
            } else {
                i = i.saturating_add(1);
            }
        } else if let Some(rest) = s.get(i..)
            && let Some(ch) = rest.chars().next()
        {
            out.push(ch);
            i = i.saturating_add(ch.len_utf8());
        } else {
            break;
        }
    }
    out
}

// ------------------------------------------------------------------ fill handle

#[derive(Clone, Debug)]
enum StripModel {
    /// All cells in the source strip share the exact same relative-ID formula
    /// template as `(src_row_pos, src_col_pos, formula)`.
    UniformFormula {
        src_row_pos: u32,
        src_col_pos: u32,
        formula: BoundFormula,
        format: Option<NumberFormat>,
    },
    /// Linear numeric or date progression `start + offset * step`.
    Arithmetic {
        start: f64,
        step: f64,
        format: Option<NumberFormat>,
    },
    /// Text with integer suffix `prefix + format(start + offset * step, pad_width) + suffix`.
    TextSeries {
        prefix: String,
        suffix: String,
        start: i64,
        step: i64,
        pad_width: usize,
        format: Option<NumberFormat>,
    },
    /// Cyclic repeat of the source strip (values or non-uniform formulas).
    Cyclic { items: Vec<(u32, u32, CopiedCell)> },
}

/// Scans left or right adjacent column to find the end row (`inclusive`) for a
/// double-click auto-fill down gesture (`14-m1b.md` 6.3).
#[must_use]
pub fn find_autofill_end_row(
    workbook: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    src: RectPos,
) -> Option<u32> {
    let sheet = workbook.sheet(sheet_id)?;
    if src.row_count == 0 || src.col_count == 0 {
        return None;
    }
    let start_scan = src.end_row();
    if start_scan >= sheet.rows() {
        return None;
    }

    let mut candidates = Vec::with_capacity(2);
    if src.first_col > 0 {
        candidates.push(src.first_col.saturating_sub(1));
    }
    if src.end_col() < sheet.cols() {
        candidates.push(src.end_col());
    }

    for adj_col in candidates {
        if !is_pos_occupied(workbook, sheet, sheet_id, start_scan, adj_col) {
            continue;
        }
        let mut r = start_scan;
        while r.saturating_add(1) < sheet.rows()
            && is_pos_occupied(workbook, sheet, sheet_id, r.saturating_add(1), adj_col)
        {
            r = r.saturating_add(1);
        }
        return Some(r);
    }
    None
}

fn is_pos_occupied(
    workbook: &Workbook<BoundFormula>,
    sheet: &Sheet,
    sheet_id: SheetId,
    row_pos: u32,
    col_pos: u32,
) -> bool {
    let (Some(r_id), Some(c_id)) = (
        sheet.row_axis().pos_to_id(row_pos),
        sheet.col_axis().pos_to_id(col_pos),
    ) else {
        return false;
    };
    !matches!(sheet.value_by_id(RowId(r_id), ColId(c_id)), Value::Empty)
        || workbook
            .formula(sheet_id, RowId(r_id), ColId(c_id))
            .is_some()
}

/// Builds atomic forward operations (`FillFormula` / `SetBlock` / `SetFormat`)
/// to fill from `src` across `dst` (where `dst` is the union bounding box
/// extending `src` vertically OR horizontally).
pub fn build_fill_ops(
    workbook: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    src: RectPos,
    dst: RectPos,
) -> Result<Vec<Op<BoundFormula>>, String> {
    let sheet = workbook
        .sheet(sheet_id)
        .ok_or_else(|| String::from("Target sheet does not exist."))?;
    if src.row_count == 0 || src.col_count == 0 || dst.row_count == 0 || dst.col_count == 0 {
        return Ok(Vec::new());
    }
    if src == dst {
        return Ok(Vec::new());
    }
    if src.end_row() > sheet.rows()
        || src.end_col() > sheet.cols()
        || dst.end_row() > sheet.rows()
        || dst.end_col() > sheet.cols()
    {
        return Err(String::from("Fill range exceeds worksheet dimensions."));
    }

    let vertical = dst.first_col == src.first_col && dst.col_count == src.col_count;
    let horizontal = dst.first_row == src.first_row && dst.row_count == src.row_count;
    if !vertical && !horizontal {
        return Err(String::from(
            "Fill range must align with source rows or source columns.",
        ));
    }

    if vertical {
        // Compute target vertical sub-range (`above` if dragged up, `below` if dragged down).
        let mut target_spans: Vec<(u32, u32)> = Vec::new();
        if dst.first_row < src.first_row {
            target_spans.push((dst.first_row, src.first_row.saturating_sub(dst.first_row)));
        }
        if dst.end_row() > src.end_row() {
            target_spans.push((src.end_row(), dst.end_row().saturating_sub(src.end_row())));
        }
        let total_ext_cells: usize = target_spans.iter().fold(0usize, |acc, &(_, rc)| {
            acc.saturating_add((rc as usize).saturating_mul(src.col_count as usize))
        });
        if total_ext_cells == 0 {
            return Ok(Vec::new());
        }
        if total_ext_cells > MAX_BULK_CELLS_PER_OP {
            return Err(String::from(
                "Fill exceeds 2,000,000-cell transaction limit.",
            ));
        }

        let mut ops = Vec::new();
        for &(ext_first_row, ext_row_count) in &target_spans {
            build_vertical_fill_ops(
                workbook,
                sheet,
                sheet_id,
                src,
                ext_first_row,
                ext_row_count,
                &mut ops,
            )?;
        }
        Ok(ops)
    } else {
        let mut target_spans: Vec<(u32, u32)> = Vec::new();
        if dst.first_col < src.first_col {
            target_spans.push((dst.first_col, src.first_col.saturating_sub(dst.first_col)));
        }
        if dst.end_col() > src.end_col() {
            target_spans.push((src.end_col(), dst.end_col().saturating_sub(src.end_col())));
        }
        let total_ext_cells: usize = target_spans.iter().fold(0usize, |acc, &(_, cc)| {
            acc.saturating_add((cc as usize).saturating_mul(src.row_count as usize))
        });
        if total_ext_cells == 0 {
            return Ok(Vec::new());
        }
        if total_ext_cells > MAX_BULK_CELLS_PER_OP {
            return Err(String::from(
                "Fill exceeds 2,000,000-cell transaction limit.",
            ));
        }

        let mut ops = Vec::new();
        for &(ext_first_col, ext_col_count) in &target_spans {
            build_horizontal_fill_ops(
                workbook,
                sheet,
                sheet_id,
                src,
                ext_first_col,
                ext_col_count,
                &mut ops,
            )?;
        }
        Ok(ops)
    }
}

fn build_vertical_fill_ops(
    workbook: &Workbook<BoundFormula>,
    sheet: &Sheet,
    sheet_id: SheetId,
    src: RectPos,
    ext_first_row: u32,
    ext_row_count: u32,
    ops: &mut Vec<Op<BoundFormula>>,
) -> Result<(), String> {
    let row_runs = axis_runs_for_span(sheet.row_axis(), ext_first_row, ext_row_count);
    let mut block_cols: Vec<ColId> = Vec::new();
    let mut block_data: Vec<Vec<CellInput<BoundFormula>>> = Vec::new();
    let mut col_formats: Vec<(IdRun, NumberFormat)> = Vec::new();

    for c_off in 0..src.col_count {
        let col_pos = src.first_col.saturating_add(c_off);
        let Some(col_raw) = sheet.col_axis().pos_to_id(col_pos) else {
            continue;
        };
        let mut strip = Vec::with_capacity(src.row_count as usize);
        for r_off in 0..src.row_count {
            let r_pos = src.first_row.saturating_add(r_off);
            strip.push((
                r_pos,
                col_pos,
                read_copied_cell(workbook, sheet, sheet_id, r_pos, col_pos),
            ));
        }
        let model = classify_strip(&strip);
        match model {
            StripModel::UniformFormula {
                src_row_pos,
                src_col_pos,
                formula,
                format,
            } => {
                let fill_ops = build_fill_plan(
                    &formula,
                    workbook,
                    sheet_id,
                    src_row_pos,
                    src_col_pos,
                    FillTargetRect {
                        dst_sheet: sheet_id,
                        first_row_pos: ext_first_row,
                        row_count: ext_row_count,
                        first_col_pos: col_pos,
                        col_count: 1,
                    },
                );
                ops.extend(fill_ops);
                if let Some(fmt) = format {
                    col_formats.push((
                        IdRun {
                            first_id: col_raw,
                            len: 1,
                        },
                        fmt,
                    ));
                }
            }
            other => {
                let mut col_vec = Vec::with_capacity(ext_row_count as usize);
                let mut uniform_fmt: Option<NumberFormat> = None;
                for r_idx in 0..ext_row_count {
                    let dst_r_pos = ext_first_row.saturating_add(r_idx);
                    let offset = i64::from(dst_r_pos).saturating_sub(i64::from(src.first_row));
                    let (cell_in, fmt_opt) =
                        eval_strip_model(&other, workbook, sheet_id, dst_r_pos, col_pos, offset);
                    if r_idx == 0 {
                        uniform_fmt = fmt_opt;
                    } else if uniform_fmt != fmt_opt {
                        uniform_fmt = None;
                    }
                    col_vec.push(cell_in);
                }
                block_cols.push(ColId(col_raw));
                block_data.push(col_vec);
                if let Some(fmt) = uniform_fmt {
                    col_formats.push((
                        IdRun {
                            first_id: col_raw,
                            len: 1,
                        },
                        fmt,
                    ));
                }
            }
        }
    }

    if !block_cols.is_empty() {
        ops.push(Op::SetBlock {
            sheet: sheet_id,
            rows: row_runs.clone(),
            cols: block_cols,
            columns_data: block_data,
        });
    }
    for (c_run, fmt) in col_formats {
        ops.push(Op::SetFormat {
            sheet: sheet_id,
            target: FormatTarget::Cells {
                rows: row_runs.clone(),
                cols: vec![c_run],
            },
            number_format: FormatPatch::Set(fmt),
            style: StylePatch::default(),
        });
    }
    Ok(())
}

fn build_horizontal_fill_ops(
    workbook: &Workbook<BoundFormula>,
    sheet: &Sheet,
    sheet_id: SheetId,
    src: RectPos,
    ext_first_col: u32,
    ext_col_count: u32,
    ops: &mut Vec<Op<BoundFormula>>,
) -> Result<(), String> {
    // Check if every row strip is a uniform formula first so pure formula
    // horizontal fills also use `Op::FillFormula`.
    let mut row_models = Vec::with_capacity(src.row_count as usize);
    for r_off in 0..src.row_count {
        let row_pos = src.first_row.saturating_add(r_off);
        let mut strip = Vec::with_capacity(src.col_count as usize);
        for c_off in 0..src.col_count {
            let c_pos = src.first_col.saturating_add(c_off);
            strip.push((
                row_pos,
                c_pos,
                read_copied_cell(workbook, sheet, sheet_id, row_pos, c_pos),
            ));
        }
        row_models.push((row_pos, classify_strip(&strip)));
    }

    let mut non_formula_rows: Vec<(u32, u64, StripModel)> = Vec::new();
    let col_runs = axis_runs_for_span(sheet.col_axis(), ext_first_col, ext_col_count);

    for (row_pos, model) in row_models {
        let Some(row_raw) = sheet.row_axis().pos_to_id(row_pos) else {
            continue;
        };
        match model {
            StripModel::UniformFormula {
                src_row_pos,
                src_col_pos,
                formula,
                format,
            } => {
                let fill_ops = build_fill_plan(
                    &formula,
                    workbook,
                    sheet_id,
                    src_row_pos,
                    src_col_pos,
                    FillTargetRect {
                        dst_sheet: sheet_id,
                        first_row_pos: row_pos,
                        row_count: 1,
                        first_col_pos: ext_first_col,
                        col_count: ext_col_count,
                    },
                );
                ops.extend(fill_ops);
                if let Some(fmt) = format {
                    ops.push(Op::SetFormat {
                        sheet: sheet_id,
                        target: FormatTarget::Cells {
                            rows: vec![IdRun {
                                first_id: row_raw,
                                len: 1,
                            }],
                            cols: col_runs.clone(),
                        },
                        number_format: FormatPatch::Set(fmt),
                        style: StylePatch::default(),
                    });
                }
            }
            other => {
                non_formula_rows.push((row_pos, row_raw, other));
            }
        }
    }

    if !non_formula_rows.is_empty() {
        let row_runs_nf = coalesce_raw_ids(non_formula_rows.iter().map(|&(_, r_id, _)| r_id));
        let mut block_cols = Vec::with_capacity(ext_col_count as usize);
        let mut block_data = Vec::with_capacity(ext_col_count as usize);
        let mut format_edits: Vec<(u64, NumberFormat)> = Vec::new();

        for c_idx in 0..ext_col_count {
            let dst_c_pos = ext_first_col.saturating_add(c_idx);
            let Some(col_raw) = sheet.col_axis().pos_to_id(dst_c_pos) else {
                continue;
            };
            let offset = i64::from(dst_c_pos).saturating_sub(i64::from(src.first_col));
            let mut col_vec = Vec::with_capacity(non_formula_rows.len());
            for &(r_pos, r_raw, ref model) in &non_formula_rows {
                let (cell_in, fmt_opt) =
                    eval_strip_model(model, workbook, sheet_id, r_pos, dst_c_pos, offset);
                if c_idx == 0
                    && let Some(fmt) = fmt_opt
                {
                    format_edits.push((r_raw, fmt));
                }
                col_vec.push(cell_in);
            }
            block_cols.push(ColId(col_raw));
            block_data.push(col_vec);
        }

        if !block_cols.is_empty() {
            ops.push(Op::SetBlock {
                sheet: sheet_id,
                rows: row_runs_nf,
                cols: block_cols,
                columns_data: block_data,
            });
        }
        for (r_raw, fmt) in format_edits {
            ops.push(Op::SetFormat {
                sheet: sheet_id,
                target: FormatTarget::Cells {
                    rows: vec![IdRun {
                        first_id: r_raw,
                        len: 1,
                    }],
                    cols: col_runs.clone(),
                },
                number_format: FormatPatch::Set(fmt),
                style: StylePatch::default(),
            });
        }
    }
    Ok(())
}

fn classify_strip(strip: &[(u32, u32, CopiedCell)]) -> StripModel {
    let Some((first_r, first_c, first_cell)) = strip.first() else {
        return StripModel::Cyclic { items: Vec::new() };
    };

    // 1. Uniform formula check.
    if let CellInput::Formula {
        formula: ref first_f,
        ..
    } = first_cell.input
    {
        let first_tpl = templatize_formula(
            first_f,
            RowId(u64::from(*first_r)),
            ColId(u64::from(*first_c)),
        );
        let all_same_tpl = strip.iter().skip(1).all(|(r, c, cell)| {
            if let CellInput::Formula { ref formula, .. } = cell.input {
                templatize_formula(formula, RowId(u64::from(*r)), ColId(u64::from(*c))) == first_tpl
            } else {
                false
            }
        });
        if all_same_tpl {
            return StripModel::UniformFormula {
                src_row_pos: *first_r,
                src_col_pos: *first_c,
                formula: first_f.clone(),
                format: first_cell.explicit_format,
            };
        }
        return StripModel::Cyclic {
            items: strip.to_vec(),
        };
    }

    // Reject series recognition if any cell in the strip holds a formula.
    if strip
        .iter()
        .any(|(_, _, c)| matches!(c.input, CellInput::Formula { .. }))
    {
        return StripModel::Cyclic {
            items: strip.to_vec(),
        };
    }

    // 2. Single cell: Date (+1 day) or text-with-integer-suffix (+1).
    if strip.len() == 1 {
        match &first_cell.input {
            CellInput::Scalar(ScalarValue::Number(n))
                if is_date_like_format(first_cell.effective_format) =>
            {
                return StripModel::Arithmetic {
                    start: *n,
                    step: 1.0,
                    format: Some(first_cell.effective_format),
                };
            }
            CellInput::Scalar(ScalarValue::Text(s)) => {
                if let Some((prefix, num, pad_width, suffix)) = split_text_integer_suffix(s) {
                    return StripModel::TextSeries {
                        prefix,
                        suffix,
                        start: num,
                        step: 1,
                        pad_width,
                        format: first_cell.explicit_format,
                    };
                }
            }
            _ => {}
        }
        return StripModel::Cyclic {
            items: strip.to_vec(),
        };
    }

    // 3. >= 2 numeric/date cells with constant arithmetic step!
    let mut nums = Vec::with_capacity(strip.len());
    for (_, _, cell) in strip {
        if let CellInput::Scalar(ScalarValue::Number(n)) = cell.input {
            nums.push(n);
        } else {
            nums.clear();
            break;
        }
    }
    if nums.len() == strip.len()
        && let (Some(&n0), Some(&n1)) = (nums.first(), nums.get(1))
    {
        let step = n1 - n0;
        let tol = 1e-9 * (1.0 + step.abs() + n0.abs());
        let is_arith = nums.windows(2).all(|w| {
            w.first()
                .zip(w.get(1))
                .is_some_and(|(&a, &b)| ((b - a) - step).abs() <= tol)
        });
        if is_arith {
            return StripModel::Arithmetic {
                start: n0,
                step,
                format: non_general_opt(first_cell.effective_format),
            };
        }
    }

    // 4. >= 2 text cells with matching `(prefix, suffix)` and constant integer step!
    let mut parsed_texts = Vec::with_capacity(strip.len());
    for (_, _, cell) in strip {
        if let CellInput::Scalar(ScalarValue::Text(ref s)) = cell.input
            && let Some(parts) = split_text_integer_suffix(s)
        {
            parsed_texts.push(parts);
        } else {
            parsed_texts.clear();
            break;
        }
    }
    if parsed_texts.len() == strip.len()
        && let (Some((p0, v0, pad0, s0)), Some((_, v1, _, _))) =
            (parsed_texts.first(), parsed_texts.get(1))
    {
        let step = v1.saturating_sub(*v0);
        let consistent = parsed_texts.iter().all(|(p, _, _, s)| p == p0 && s == s0)
            && parsed_texts.windows(2).all(|w| {
                w.first()
                    .zip(w.get(1))
                    .is_some_and(|(a, b)| b.1.saturating_sub(a.1) == step)
            });
        if consistent {
            return StripModel::TextSeries {
                prefix: p0.clone(),
                suffix: s0.clone(),
                start: *v0,
                step,
                pad_width: *pad0,
                format: first_cell.explicit_format,
            };
        }
    }

    StripModel::Cyclic {
        items: strip.to_vec(),
    }
}

fn eval_strip_model(
    model: &StripModel,
    workbook: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    dst_row_pos: u32,
    dst_col_pos: u32,
    offset: i64,
) -> (CellInput<BoundFormula>, Option<NumberFormat>) {
    match model {
        StripModel::Arithmetic {
            start,
            step,
            format,
        } => {
            #[expect(
                clippy::cast_precision_loss,
                reason = "grid coordinates <= 2M fit in f64"
            )]
            let raw_val = *start + (offset as f64) * *step;
            // Snap subtle float drift (e.g. `0.1 + 2 * 0.1 = 0.30000000000000004`) to 15 digits.
            let clean = reticula_core::decimal15::digits15(raw_val)
                .and_then(|d| {
                    reticula_core::decimal15::round_decimal(
                        raw_val,
                        d.scale,
                        reticula_core::decimal15::RoundingMode::HalfAwayFromZero,
                    )
                })
                .unwrap_or(raw_val);
            (CellInput::Scalar(ScalarValue::Number(clean)), *format)
        }
        StripModel::TextSeries {
            prefix,
            suffix,
            start,
            step,
            pad_width,
            format,
        } => {
            let val = start.saturating_add(offset.saturating_mul(*step));
            let formatted_num = if *pad_width > 1 && val >= 0 {
                format!("{val:0>width$}", width = *pad_width)
            } else {
                format!("{val}")
            };
            (
                CellInput::Scalar(ScalarValue::Text(format!(
                    "{prefix}{formatted_num}{suffix}"
                ))),
                *format,
            )
        }
        StripModel::Cyclic { items } => {
            if items.is_empty() {
                return (CellInput::Empty, None);
            }
            let len_i64 = i64::try_from(items.len()).unwrap_or(1);
            let idx = usize::try_from(offset.rem_euclid(len_i64)).unwrap_or(0);
            let Some((src_r, src_c, item)) = items.get(idx) else {
                return (CellInput::Empty, None);
            };
            let input = match &item.input {
                CellInput::Formula { formula, .. } => CellInput::Formula {
                    formula: shift_formula_for_cell(
                        formula,
                        workbook,
                        FormulaShiftSpec {
                            src_sheet: sheet_id,
                            src_row_pos: *src_r,
                            src_col_pos: *src_c,
                            dst_sheet: sheet_id,
                            dst_row_pos,
                            dst_col_pos,
                        },
                    ),
                    cached: None,
                },
                other => other.clone(),
            };
            (input, item.explicit_format)
        }
        StripModel::UniformFormula {
            src_row_pos,
            src_col_pos,
            formula,
            format,
        } => (
            CellInput::Formula {
                formula: shift_formula_for_cell(
                    formula,
                    workbook,
                    FormulaShiftSpec {
                        src_sheet: sheet_id,
                        src_row_pos: *src_row_pos,
                        src_col_pos: *src_col_pos,
                        dst_sheet: sheet_id,
                        dst_row_pos,
                        dst_col_pos,
                    },
                ),
                cached: None,
            },
            *format,
        ),
    }
}

/// Extracts `(prefix, unsigned_int_value, zero_pad_width, suffix)` from the last
/// contiguous ASCII digit run in `s` (e.g. `"Item 007"` -> `("Item ", 7, 3, "")`,
/// `"第1季度"` -> `("第", 1, 1, "季度")`). Rejects strings that are pure digits
/// (`prefix` and `suffix` both empty) since numeric strings are already numbers
/// unless `@`-formatted.
fn split_text_integer_suffix(s: &str) -> Option<(String, i64, usize, String)> {
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    while end > 0
        && !bytes
            .get(end.saturating_sub(1))
            .is_some_and(u8::is_ascii_digit)
    {
        end = end.saturating_sub(1);
    }
    if end == 0 {
        return None;
    }
    let mut start = end;
    while start > 0
        && bytes
            .get(start.saturating_sub(1))
            .is_some_and(u8::is_ascii_digit)
    {
        start = start.saturating_sub(1);
    }
    let prefix = s.get(..start)?;
    let digits = s.get(start..end)?;
    let suffix = s.get(end..)?;
    if prefix.is_empty() && suffix.is_empty() {
        return None;
    }
    let val = digits.parse::<i64>().ok()?;
    let pad_width = if digits.starts_with('0') && digits.len() > 1 {
        digits.len()
    } else {
        1
    };
    Some((String::from(prefix), val, pad_width, String::from(suffix)))
}

const fn is_date_like_format(fmt: NumberFormat) -> bool {
    matches!(
        fmt,
        NumberFormat::Date
            | NumberFormat::DateSlash
            | NumberFormat::DateUs
            | NumberFormat::DateYearMonth
            | NumberFormat::DateTime { .. }
    )
}

const fn non_general_opt(fmt: NumberFormat) -> Option<NumberFormat> {
    if matches!(fmt, NumberFormat::General) {
        None
    } else {
        Some(fmt)
    }
}

// ------------------------------------------------------------------ copy fill

/// Number format and style that a copy fill gives to the cells it writes.
type FillLook = (Option<NumberFormat>, StylePatch);

/// Cells of one line of a copy fill whose formulas are written by the same operation.
struct FillGroup {
    /// Position of the first cell along the line.
    first: u32,
    len: u32,
    value: BoundFormula,
}

/// Adds the cell at `position` to the last group if it continues it.
fn extend_groups(
    groups: &mut Vec<FillGroup>,
    position: u32,
    value: &BoundFormula,
    continues: impl FnOnce(&FillGroup, &BoundFormula) -> bool,
) {
    if let Some(last) = groups.last_mut()
        && last.first.saturating_add(last.len) == position
        && continues(last, value)
    {
        last.len = last.len.saturating_add(1);
        return;
    }
    groups.push(FillGroup {
        first: position,
        len: 1,
        value: value.clone(),
    });
}

/// A cell of the line a copy fill copies.
struct FillSource {
    /// Identifier along the line.
    id: u64,
    row: u32,
    col: u32,
    input: CellInput<BoundFormula>,
}

impl FillSource {
    /// What the cell at a position holds after it was copied from this one.
    fn copied_to(
        &self,
        workbook: &Workbook<BoundFormula>,
        sheet: SheetId,
        row: u32,
        col: u32,
    ) -> CellInput<BoundFormula> {
        let CellInput::Formula { formula, .. } = &self.input else {
            return self.input.clone();
        };
        CellInput::Formula {
            formula: shift_formula_for_cell(
                formula,
                workbook,
                FormulaShiftSpec {
                    src_sheet: sheet,
                    src_row_pos: self.row,
                    src_col_pos: self.col,
                    dst_sheet: sheet,
                    dst_row_pos: row,
                    dst_col_pos: col,
                },
            ),
            cached: None,
        }
    }
}

/// Builds the operations that copy the first row of a range over the rest of
/// it, or its first column, the way pasting it there would: relative references
/// shift, and values, dates included, stay as they are.
///
/// The rows are `(first, count)` runs of adjacent rows, so that rows hidden by
/// a filter can be left out.
pub fn build_copy_fill_ops(
    workbook: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    row_runs: &[(u32, u32)],
    (first_col, col_count): (u32, u32),
    dir: FillDirection,
) -> Result<Vec<Op<BoundFormula>>, String> {
    let sheet = workbook
        .sheet(sheet_id)
        .ok_or_else(|| String::from("Target sheet does not exist."))?;
    let total_rows = row_runs
        .iter()
        .fold(0_u32, |sum, &(_, count)| sum.saturating_add(count));
    if row_runs
        .iter()
        .any(|&(first, count)| first.saturating_add(count) > sheet.rows())
        || first_col.saturating_add(col_count) > sheet.cols()
    {
        return Err(String::from("Fill range exceeds worksheet dimensions."));
    }
    if (total_rows as usize).saturating_mul(col_count as usize) > MAX_BULK_CELLS_PER_OP {
        return Err(String::from(
            "Fill exceeds 2,000,000-cell transaction limit.",
        ));
    }
    match dir {
        FillDirection::Down => {
            let Some((&(first_row, first_count), rest)) = row_runs.split_first() else {
                return Ok(Vec::new());
            };
            let mut targets = Vec::with_capacity(row_runs.len());
            if first_count > 1 {
                targets.push((first_row.saturating_add(1), first_count.saturating_sub(1)));
            }
            targets.extend_from_slice(rest);
            let source = (first_col..first_col.saturating_add(col_count))
                .map(|col| (first_row, col))
                .collect::<Vec<_>>();
            Ok(copy_fill_ops(
                workbook, sheet, sheet_id, &source, &targets, dir,
            ))
        }
        FillDirection::Right => {
            let source = row_runs
                .iter()
                .flat_map(|&(first, count)| first..first.saturating_add(count))
                .map(|row| (row, first_col))
                .collect::<Vec<_>>();
            let targets = [(first_col.saturating_add(1), col_count.saturating_sub(1))];
            Ok(copy_fill_ops(
                workbook, sheet, sheet_id, &source, &targets, dir,
            ))
        }
    }
}

/// `source` is the line of cells to copy, and `targets` the `(first, count)`
/// runs across it to copy each of them to.
fn copy_fill_ops(
    workbook: &Workbook<BoundFormula>,
    sheet: &Sheet,
    sheet_id: SheetId,
    source: &[(u32, u32)],
    targets: &[(u32, u32)],
    dir: FillDirection,
) -> Vec<Op<BoundFormula>> {
    let across = targets.iter().fold(0_usize, |sum, &(_, count)| {
        sum.saturating_add(count as usize)
    });
    if across == 0 || source.is_empty() {
        return Vec::new();
    }
    let (line_axis, target_axis) = match dir {
        FillDirection::Down => (sheet.col_axis(), sheet.row_axis()),
        FillDirection::Right => (sheet.row_axis(), sheet.col_axis()),
    };
    let target_ids = targets
        .iter()
        .flat_map(|&(first, count)| axis_runs_for_span(target_axis, first, count))
        .collect::<Vec<_>>();

    let mut block: Vec<FillSource> = Vec::new();
    let mut formula_cells: Vec<FillSource> = Vec::new();
    let mut formulas: Vec<FillGroup> = Vec::new();
    let mut formats: Vec<(FillLook, Vec<u64>)> = Vec::new();
    for &(row, col) in source {
        let position = match dir {
            FillDirection::Down => col,
            FillDirection::Right => row,
        };
        let Some(id) = line_axis.pos_to_id(position) else {
            continue;
        };
        let cell = read_copied_cell(workbook, sheet, sheet_id, row, col);
        if let CellInput::Formula { formula, .. } = &cell.input {
            // A cell that holds what filling would have put there can be
            // filled together with the cells before it.
            extend_groups(&mut formulas, position, formula, |group, formula| {
                let (src_row_pos, src_col_pos) = match dir {
                    FillDirection::Down => (row, group.first),
                    FillDirection::Right => (group.first, col),
                };
                shift_formula_for_cell(
                    &group.value,
                    workbook,
                    FormulaShiftSpec {
                        src_sheet: sheet_id,
                        src_row_pos,
                        src_col_pos,
                        dst_sheet: sheet_id,
                        dst_row_pos: row,
                        dst_col_pos: col,
                    },
                ) == *formula
            });
        }
        if cell.explicit_format.is_some() || !cell.style_patch.is_empty() {
            let look = (cell.explicit_format, cell.style_patch);
            match formats.iter_mut().find(|(other, _)| *other == look) {
                Some((_, ids)) => ids.push(id),
                None => formats.push((look, vec![id])),
            }
        }
        let line = FillSource {
            id,
            row,
            col,
            input: cell.input,
        };
        if matches!(line.input, CellInput::Formula { .. }) {
            formula_cells.push(line);
        } else {
            block.push(line);
        }
    }
    // Each operation costs time of its own: many formulas that have nothing in
    // common are written cell by cell, as pasting writes them.
    if formulas.len().saturating_mul(targets.len()) > MAX_FILL_FORMULA_OPS {
        formulas.clear();
        block.append(&mut formula_cells);
    }

    let mut ops = Vec::new();
    if !block.is_empty() {
        let positions = targets
            .iter()
            .flat_map(|&(first, count)| first..first.saturating_add(count));
        ops.push(match dir {
            FillDirection::Down => Op::SetBlock {
                sheet: sheet_id,
                rows: target_ids.clone(),
                cols: block.iter().map(|line| ColId(line.id)).collect(),
                columns_data: block
                    .iter()
                    .map(|line| {
                        positions
                            .clone()
                            .map(|row| line.copied_to(workbook, sheet_id, row, line.col))
                            .collect()
                    })
                    .collect(),
            },
            FillDirection::Right => {
                block.sort_unstable_by_key(|line| line.row);
                Op::SetBlock {
                    sheet: sheet_id,
                    rows: coalesce_raw_ids(block.iter().map(|line| line.id)),
                    cols: target_ids
                        .iter()
                        .flat_map(|run| run.iter())
                        .map(ColId)
                        .collect(),
                    columns_data: positions
                        .map(|col| {
                            block
                                .iter()
                                .map(|line| line.copied_to(workbook, sheet_id, line.row, col))
                                .collect()
                        })
                        .collect(),
                }
            }
        });
    }
    let (first_row, first_col) = source.first().copied().unwrap_or_default();
    for group in &formulas {
        for &(first, count) in targets {
            let (src, rect) = match dir {
                FillDirection::Down => (
                    (first_row, group.first),
                    RectPos {
                        first_row: first,
                        row_count: count,
                        first_col: group.first,
                        col_count: group.len,
                    },
                ),
                FillDirection::Right => (
                    (group.first, first_col),
                    RectPos {
                        first_row: group.first,
                        row_count: group.len,
                        first_col: first,
                        col_count: count,
                    },
                ),
            };
            ops.extend(build_fill_plan(
                &group.value,
                workbook,
                sheet_id,
                src.0,
                src.1,
                FillTargetRect {
                    dst_sheet: sheet_id,
                    first_row_pos: rect.first_row,
                    row_count: rect.row_count,
                    first_col_pos: rect.first_col,
                    col_count: rect.col_count,
                },
            ));
        }
    }
    for ((number_format, style), ids) in formats {
        let line_ids = coalesce_raw_ids(ids.into_iter());
        let (rows, cols) = match dir {
            FillDirection::Down => (target_ids.clone(), line_ids),
            FillDirection::Right => (line_ids, target_ids.clone()),
        };
        ops.push(Op::SetFormat {
            sheet: sheet_id,
            target: FormatTarget::Cells { rows, cols },
            number_format: number_format.map_or(FormatPatch::Keep, FormatPatch::Set),
            style,
        });
    }
    ops
}

// ------------------------------------------------------------------ clipboard

/// Captures `rect` on `sheet_id` into `(CopyPayload, InternalClipboard)`.
pub fn copy_range(
    workbook: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    rect: RectPos,
    cut: bool,
) -> Result<(CopyPayload, InternalClipboard), String> {
    let sheet = workbook
        .sheet(sheet_id)
        .ok_or_else(|| String::from("Target sheet does not exist."))?;
    if rect.row_count == 0 || rect.col_count == 0 {
        return Err(String::from("Select at least one cell to copy."));
    }
    if rect.end_row() > sheet.rows() || rect.end_col() > sheet.cols() {
        return Err(String::from("Copy range exceeds worksheet dimensions."));
    }
    let total = (rect.row_count as usize).saturating_mul(rect.col_count as usize);
    if total > MAX_CLIPBOARD_CELLS {
        return Err(format!(
            "Clipboard copy is capped at {MAX_CLIPBOARD_CELLS} cells (use Export CSV for larger ranges)."
        ));
    }

    let mut cells: Vec<Vec<CopiedCell>> = Vec::with_capacity(rect.col_count as usize);
    for c_off in 0..rect.col_count {
        let c_pos = rect.first_col.saturating_add(c_off);
        let mut col_vec = Vec::with_capacity(rect.row_count as usize);
        for r_off in 0..rect.row_count {
            let r_pos = rect.first_row.saturating_add(r_off);
            col_vec.push(read_copied_cell(workbook, sheet, sheet_id, r_pos, c_pos));
        }
        cells.push(col_vec);
    }

    let mut tsv = String::new();
    let mut html = String::from(
        "<table data-reticula=\"1\" style=\"border-collapse:collapse;font-family:sans-serif;font-size:13px\"><tbody>",
    );
    let mut cell_text = String::new();

    for r_off in 0..rect.row_count {
        let r_pos = rect.first_row.saturating_add(r_off);
        if r_off > 0 {
            tsv.push('\n');
        }
        html.push_str("<tr>");
        for c_off in 0..rect.col_count {
            let c_pos = rect.first_col.saturating_add(c_off);
            if c_off > 0 {
                tsv.push('\t');
            }
            cell_text.clear();
            sheet.write_text(r_pos, c_pos, &mut cell_text);
            write_escaped_field(&cell_text, b'\t', &mut tsv);

            let style = sheet
                .row_axis()
                .pos_to_id(r_pos)
                .zip(sheet.col_axis().pos_to_id(c_pos))
                .map_or_else(CellStyle::default, |(r_id, c_id)| {
                    sheet.effective_style_by_id(RowId(r_id), ColId(c_id))
                });
            html.push_str("<td");
            write_inline_style_attr(&style, &mut html);
            html.push('>');
            write_html_escaped(&cell_text, &mut html);
            html.push_str("</td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table>");

    let fingerprint = tsv_fingerprint(&tsv);
    let internal = InternalClipboard {
        src_sheet: sheet_id,
        src_rect: rect,
        src_row_runs: axis_runs_for_span(sheet.row_axis(), rect.first_row, rect.row_count),
        src_col_runs: axis_runs_for_span(sheet.col_axis(), rect.first_col, rect.col_count),
        cut,
        fingerprint: fingerprint.clone(),
        cells,
    };

    Ok((
        CopyPayload {
            tsv,
            html,
            fingerprint,
        },
        internal,
    ))
}

/// Builds atomic forward operations for pasting `(tsv, html)` into `dest` on
/// `dst_sheet`. Consumes `cut` on `internal` if an internal cut was pasted.
pub fn build_paste_ops(
    workbook: &Workbook<BoundFormula>,
    dst_sheet: SheetId,
    dest: RectPos,
    tsv: &str,
    html: Option<&str>,
    internal: &mut Option<InternalClipboard>,
) -> Result<(Vec<Op<BoundFormula>>, RectPos), String> {
    let sheet = workbook
        .sheet(dst_sheet)
        .ok_or_else(|| String::from("Target sheet does not exist."))?;

    if !tsv.is_empty()
        && let Some(clip) = internal.as_mut()
        && clip.fingerprint == tsv_fingerprint(tsv)
    {
        return build_internal_paste_ops(workbook, sheet, dst_sheet, dest, clip);
    }

    // External clipboard: prefer HTML `<table>` when present, else parse TSV.
    if let Some(raw_html) = html
        && let Some(parsed) = parse_html_table(raw_html)
        && !parsed.is_empty()
    {
        return build_external_grid_ops(
            workbook,
            sheet,
            dst_sheet,
            dest.first_row,
            dest.first_col,
            &parsed,
        );
    }

    let tsv_rows = parse_tsv_grid(tsv);
    if tsv_rows.is_empty() {
        return Ok((Vec::new(), dest));
    }
    let grid: Vec<Vec<ExternalCell>> = tsv_rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|text| ExternalCell {
                    text,
                    style: StylePatch::default(),
                })
                .collect()
        })
        .collect();
    build_external_grid_ops(
        workbook,
        sheet,
        dst_sheet,
        dest.first_row,
        dest.first_col,
        &grid,
    )
}

fn build_internal_paste_ops(
    workbook: &Workbook<BoundFormula>,
    dst_sheet_ref: &Sheet,
    dst_sheet: SheetId,
    dest: RectPos,
    clip: &mut InternalClipboard,
) -> Result<(Vec<Op<BoundFormula>>, RectPos), String> {
    let src_r = clip.src_rect.row_count.max(1);
    let src_c = clip.src_rect.col_count.max(1);

    // Tile if user selected an exact multiple of `src_r × src_c` (e.g. `1 × 1`
    // copied onto `100 × 1`); otherwise paste `1 ×` starting at `(dest.first_row, dest.first_col)`.
    let tile_rows = if dest.row_count >= src_r && dest.row_count.is_multiple_of(src_r) && !clip.cut
    {
        dest.row_count
    } else {
        src_r
    };
    let tile_cols = if dest.col_count >= src_c && dest.col_count.is_multiple_of(src_c) && !clip.cut
    {
        dest.col_count
    } else {
        src_c
    };

    let applied_rect = RectPos {
        first_row: dest.first_row,
        row_count: tile_rows,
        first_col: dest.first_col,
        col_count: tile_cols,
    };
    if applied_rect.end_row() > dst_sheet_ref.rows()
        || applied_rect.end_col() > dst_sheet_ref.cols()
    {
        return Err(String::from("Paste target exceeds worksheet bounds."));
    }
    let total_cells = (tile_rows as usize).saturating_mul(tile_cols as usize);
    if total_cells > MAX_BULK_CELLS_PER_OP {
        return Err(String::from(
            "Paste exceeds 2,000,000-cell transaction limit.",
        ));
    }

    let mut ops = Vec::new();
    let is_cut = core::mem::replace(&mut clip.cut, false);

    if is_cut
        && tile_rows == src_r
        && tile_cols == src_c
        && let Some(move_ops) =
            try_build_move_cells_op(workbook, clip, dst_sheet, dst_sheet_ref, applied_rect)
    {
        return Ok((move_ops, applied_rect));
    }

    // If cutting and MoveCells fallback applied (e.g. source rows/cols deleted or split),
    // clear source first so overlapping pastes overwrite cleanly.
    if is_cut {
        ops.push(Op::ClearRange {
            sheet: clip.src_sheet,
            rows: clip.src_row_runs.clone(),
            cols: clip.src_col_runs.clone(),
        });
    }

    // Fast constant-size `FillFormula` path when pasting a single copied formula
    // across a multi-cell destination (`14-m1b.md` 5.2)!
    if src_r == 1
        && src_c == 1
        && total_cells > 1
        && let Some(first_col) = clip.cells.first()
        && let Some(cell0) = first_col.first()
        && let CellInput::Formula { ref formula, .. } = cell0.input
    {
        ops.extend(build_fill_plan(
            formula,
            workbook,
            clip.src_sheet,
            clip.src_rect.first_row,
            clip.src_rect.first_col,
            FillTargetRect {
                dst_sheet,
                first_row_pos: applied_rect.first_row,
                row_count: applied_rect.row_count,
                first_col_pos: applied_rect.first_col,
                col_count: applied_rect.col_count,
            },
        ));
        if cell0.explicit_format.is_some() || !cell0.style_patch.is_empty() {
            ops.push(Op::SetFormat {
                sheet: dst_sheet,
                target: FormatTarget::Cells {
                    rows: axis_runs_for_span(
                        dst_sheet_ref.row_axis(),
                        applied_rect.first_row,
                        applied_rect.row_count,
                    ),
                    cols: axis_runs_for_span(
                        dst_sheet_ref.col_axis(),
                        applied_rect.first_col,
                        applied_rect.col_count,
                    ),
                },
                number_format: cell0
                    .explicit_format
                    .map_or(FormatPatch::Keep, FormatPatch::Set),
                style: cell0.style_patch,
            });
        }
        return Ok((ops, applied_rect));
    }

    let dst_row_runs = axis_runs_for_span(
        dst_sheet_ref.row_axis(),
        applied_rect.first_row,
        applied_rect.row_count,
    );
    let mut block_cols = Vec::with_capacity(tile_cols as usize);
    let mut block_data = Vec::with_capacity(tile_cols as usize);
    let mut format_ops = Vec::new();

    for dc in 0..tile_cols {
        let dst_col_pos = applied_rect.first_col.saturating_add(dc);
        let Some(dst_col_raw) = dst_sheet_ref.col_axis().pos_to_id(dst_col_pos) else {
            continue;
        };
        let sc = (dc % src_c) as usize;
        let Some(src_col_vec) = clip.cells.get(sc) else {
            continue;
        };

        let mut col_inputs = Vec::with_capacity(tile_rows as usize);
        for dr in 0..tile_rows {
            let dst_row_pos = applied_rect.first_row.saturating_add(dr);
            let sr = (dr % src_r) as usize;
            let Some(copied) = src_col_vec.get(sr) else {
                col_inputs.push(CellInput::Empty);
                continue;
            };
            let src_row_pos = clip.src_rect.first_row.saturating_add(dr % src_r);
            let src_col_pos = clip.src_rect.first_col.saturating_add(dc % src_c);

            let input = match &copied.input {
                CellInput::Formula { formula, .. } => CellInput::Formula {
                    formula: shift_formula_for_cell(
                        formula,
                        workbook,
                        FormulaShiftSpec {
                            src_sheet: clip.src_sheet,
                            src_row_pos,
                            src_col_pos,
                            dst_sheet,
                            dst_row_pos,
                            dst_col_pos,
                        },
                    ),
                    cached: None,
                },
                other => other.clone(),
            };
            col_inputs.push(input);

            if (copied.explicit_format.is_some() || !copied.style_patch.is_empty())
                && let Some(dst_row_raw) = dst_sheet_ref.row_axis().pos_to_id(dst_row_pos)
            {
                format_ops.push(Op::SetFormat {
                    sheet: dst_sheet,
                    target: FormatTarget::Cells {
                        rows: vec![IdRun {
                            first_id: dst_row_raw,
                            len: 1,
                        }],
                        cols: vec![IdRun {
                            first_id: dst_col_raw,
                            len: 1,
                        }],
                    },
                    number_format: copied
                        .explicit_format
                        .map_or(FormatPatch::Keep, FormatPatch::Set),
                    style: copied.style_patch,
                });
            }
        }
        block_cols.push(ColId(dst_col_raw));
        block_data.push(col_inputs);
    }

    if !block_cols.is_empty() {
        ops.push(Op::SetBlock {
            sheet: dst_sheet,
            rows: dst_row_runs,
            cols: block_cols,
            columns_data: block_data,
        });
    }
    ops.extend(format_ops);
    Ok((ops, applied_rect))
}

fn try_build_move_cells_op(
    workbook: &Workbook<BoundFormula>,
    clip: &InternalClipboard,
    dst_sheet: SheetId,
    dst_sheet_ref: &Sheet,
    applied_rect: RectPos,
) -> Option<Vec<Op<BoundFormula>>> {
    let src_sheet_ref = workbook.live_sheet(clip.src_sheet)?;

    let mut src_rows = Vec::with_capacity(clip.src_rect.row_count as usize);
    let mut prev_r_pos: Option<u32> = None;
    for run in &clip.src_row_runs {
        for raw_r in run.iter() {
            let pos = src_sheet_ref.row_axis().id_to_pos(raw_r)?;
            if let Some(prev) = prev_r_pos
                && pos != prev.saturating_add(1)
            {
                return None;
            }
            prev_r_pos = Some(pos);
            src_rows.push(RowId(raw_r));
        }
    }
    if src_rows.len() != clip.src_rect.row_count as usize || src_rows.is_empty() {
        return None;
    }

    let mut src_cols = Vec::with_capacity(clip.src_rect.col_count as usize);
    let mut prev_c_pos: Option<u32> = None;
    for run in &clip.src_col_runs {
        for raw_c in run.iter() {
            let pos = src_sheet_ref.col_axis().id_to_pos(raw_c)?;
            if let Some(prev) = prev_c_pos
                && pos != prev.saturating_add(1)
            {
                return None;
            }
            prev_c_pos = Some(pos);
            src_cols.push(ColId(raw_c));
        }
    }
    if src_cols.len() != clip.src_rect.col_count as usize || src_cols.is_empty() {
        return None;
    }

    let mut dst_rows = Vec::with_capacity(applied_rect.row_count as usize);
    dst_sheet_ref.row_axis().for_each_live_positions(
        applied_rect.first_row,
        applied_rect.row_count,
        |run| {
            for raw_r in run.iter() {
                dst_rows.push(RowId(raw_r));
            }
        },
    );
    let mut dst_cols = Vec::with_capacity(applied_rect.col_count as usize);
    dst_sheet_ref.col_axis().for_each_live_positions(
        applied_rect.first_col,
        applied_rect.col_count,
        |run| {
            for raw_c in run.iter() {
                dst_cols.push(ColId(raw_c));
            }
        },
    );

    if dst_rows.len() != src_rows.len() || dst_cols.len() != src_cols.len() {
        return None;
    }
    if clip.src_sheet == dst_sheet && src_rows == dst_rows && src_cols == dst_cols {
        return Some(Vec::new());
    }

    Some(vec![Op::MoveCells {
        src_sheet: clip.src_sheet,
        src_rows,
        src_cols,
        dst_sheet,
        dst_rows,
        dst_cols,
    }])
}

#[derive(Clone, Debug)]
struct ExternalCell {
    text: String,
    style: StylePatch,
}

fn build_external_grid_ops(
    workbook: &Workbook<BoundFormula>,
    sheet: &Sheet,
    dst_sheet: SheetId,
    first_row: u32,
    first_col: u32,
    rows_grid: &[Vec<ExternalCell>],
) -> Result<(Vec<Op<BoundFormula>>, RectPos), String> {
    let row_count = u32::try_from(rows_grid.len()).unwrap_or(0);
    let col_count = rows_grid
        .iter()
        .map(|r| u32::try_from(r.len()).unwrap_or(0))
        .max()
        .unwrap_or(0);
    if row_count == 0 || col_count == 0 {
        return Ok((
            Vec::new(),
            RectPos {
                first_row,
                row_count: 1,
                first_col,
                col_count: 1,
            },
        ));
    }
    let rect = RectPos {
        first_row,
        row_count,
        first_col,
        col_count,
    };
    if rect.end_row() > sheet.rows() || rect.end_col() > sheet.cols() {
        return Err(String::from("Pasted table exceeds worksheet bounds."));
    }
    let total = (row_count as usize).saturating_mul(col_count as usize);
    if total > MAX_BULK_CELLS_PER_OP {
        return Err(String::from(
            "Paste exceeds 2,000,000-cell transaction limit.",
        ));
    }

    let dst_row_runs = axis_runs_for_span(sheet.row_axis(), first_row, row_count);
    let mut block_cols = Vec::with_capacity(col_count as usize);
    let mut block_data = Vec::with_capacity(col_count as usize);
    let mut format_ops = Vec::new();

    for c_off in 0..col_count {
        let c_pos = first_col.saturating_add(c_off);
        let Some(c_raw) = sheet.col_axis().pos_to_id(c_pos) else {
            continue;
        };
        let mut col_vec = Vec::with_capacity(row_count as usize);
        for r_off in 0..row_count {
            let r_pos = first_row.saturating_add(r_off);
            let r_raw = sheet.row_axis().pos_to_id(r_pos).unwrap_or(0);
            let ext_opt = rows_grid
                .get(r_off as usize)
                .and_then(|r| r.get(c_off as usize));
            let Some(ext) = ext_opt else {
                col_vec.push(CellInput::Empty);
                continue;
            };

            let cur_fmt = sheet.effective_format_by_id(RowId(r_raw), ColId(c_raw));
            let (input, suggested_fmt) = match infer_input_for_cell(&ext.text, cur_fmt) {
                Ok(InferredInput::Empty) => (CellInput::Empty, None),
                Ok(InferredInput::Scalar {
                    value,
                    suggested_format,
                }) => (CellInput::Scalar(value), suggested_format),
                Ok(InferredInput::Formula(raw_f)) => {
                    match BoundFormula::parse_and_bind(
                        &raw_f.source,
                        raw_f.mode,
                        workbook,
                        dst_sheet,
                    ) {
                        Ok(bound) => (
                            CellInput::Formula {
                                formula: bound,
                                cached: None,
                            },
                            None,
                        ),
                        Err(_) => (CellInput::Scalar(ScalarValue::Text(ext.text.clone())), None),
                    }
                }
                Err(_) => {
                    let truncated: String = ext.text.chars().take(32_767).collect();
                    (CellInput::Scalar(ScalarValue::Text(truncated)), None)
                }
            };
            col_vec.push(input);

            let nf_patch = match suggested_fmt {
                Some(fmt)
                    if sheet
                        .cell_format_by_id(RowId(r_raw), ColId(c_raw))
                        .is_none() =>
                {
                    FormatPatch::Set(fmt)
                }
                _ => FormatPatch::Keep,
            };
            if !matches!(nf_patch, FormatPatch::Keep) || !ext.style.is_empty() {
                format_ops.push(Op::SetFormat {
                    sheet: dst_sheet,
                    target: FormatTarget::Cells {
                        rows: vec![IdRun {
                            first_id: r_raw,
                            len: 1,
                        }],
                        cols: vec![IdRun {
                            first_id: c_raw,
                            len: 1,
                        }],
                    },
                    number_format: nf_patch,
                    style: ext.style,
                });
            }
        }
        block_cols.push(ColId(c_raw));
        block_data.push(col_vec);
    }

    let mut ops = Vec::with_capacity(1 + format_ops.len());
    if !block_cols.is_empty() {
        ops.push(Op::SetBlock {
            sheet: dst_sheet,
            rows: dst_row_runs,
            cols: block_cols,
            columns_data: block_data,
        });
    }
    ops.extend(format_ops);
    Ok((ops, rect))
}

// ------------------------------------------------ TSV & HTML table parsers

fn parse_tsv_grid(raw_tsv: &str) -> Vec<Vec<String>> {
    let norm = normalize_clipboard_newlines(raw_tsv);
    let text = norm.strip_suffix('\n').unwrap_or(&norm);
    if text.is_empty() {
        return Vec::new();
    }

    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur_row: Vec<String> = Vec::new();

    while pos < len {
        let mut field = String::new();
        if bytes.get(pos) == Some(&b'"') {
            pos = pos.saturating_add(1);
            while pos < len {
                let start = pos;
                while let Some(&b) = bytes.get(pos) {
                    if b == b'"' {
                        break;
                    }
                    pos = pos.saturating_add(1);
                }
                if let Some(piece) = text.get(start..pos) {
                    field.push_str(piece);
                }
                if bytes.get(pos) == Some(&b'"') {
                    if bytes.get(pos.saturating_add(1)) == Some(&b'"') {
                        field.push('"');
                        pos = pos.saturating_add(2);
                    } else {
                        pos = pos.saturating_add(1);
                        break;
                    }
                }
            }
            while let Some(&b) = bytes.get(pos) {
                if b == b'\t' || b == b'\n' {
                    break;
                }
                pos = pos.saturating_add(1);
            }
        } else {
            let start = pos;
            while let Some(&b) = bytes.get(pos) {
                if b == b'\t' || b == b'\n' {
                    break;
                }
                pos = pos.saturating_add(1);
            }
            if let Some(piece) = text.get(start..pos) {
                field.push_str(piece);
            }
        }
        cur_row.push(field);

        match bytes.get(pos).copied() {
            Some(b'\t') => {
                pos = pos.saturating_add(1);
                if pos == len {
                    cur_row.push(String::new());
                }
            }
            Some(b'\n') => {
                pos = pos.saturating_add(1);
                rows.push(core::mem::take(&mut cur_row));
                if pos == len {
                    rows.push(vec![String::new()]);
                }
            }
            _ => {}
        }
    }
    if !cur_row.is_empty() {
        rows.push(cur_row);
    }
    rows
}

fn parse_html_table(html: &str) -> Option<Vec<Vec<ExternalCell>>> {
    let lower = html.to_ascii_lowercase();
    let table_start = lower.find("<table")?;
    let table_end = lower
        .get(table_start..)?
        .find("</table>")
        .map_or(html.len(), |i| {
            table_start.saturating_add(i).saturating_add(8)
        });
    let slice = html.get(table_start..table_end)?;
    let lower_slice = lower.get(table_start..table_end)?;

    let mut grid: Vec<Vec<Option<ExternalCell>>> = Vec::new();
    let mut row_idx = 0usize;
    let mut cursor = 0usize;

    while let Some(tr_rel) = lower_slice.get(cursor..)?.find("<tr") {
        let tr_open = cursor.saturating_add(tr_rel);
        let Some(tr_tag_end_rel) = slice.get(tr_open..)?.find('>') else {
            break;
        };
        let tr_body_start = tr_open.saturating_add(tr_tag_end_rel).saturating_add(1);
        let tr_close = lower_slice
            .get(tr_body_start..)?
            .find("</tr")
            .map_or(slice.len(), |i| tr_body_start.saturating_add(i));

        if grid.len() <= row_idx {
            grid.push(Vec::new());
        }

        let mut cell_cursor = tr_body_start;
        let mut col_idx = 0usize;
        while cell_cursor < tr_close {
            let rem_lower = lower_slice.get(cell_cursor..tr_close).unwrap_or("");
            let next_td = rem_lower.find("<td");
            let next_th = rem_lower.find("<th");
            let Some(cell_rel) = (match (next_td, next_th) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }) else {
                break;
            };
            let cell_open = cell_cursor.saturating_add(cell_rel);
            let is_th = lower_slice.get(cell_open..cell_open.saturating_add(3)) == Some("<th");
            let Some(tag_end_rel) = slice.get(cell_open..tr_close)?.find('>') else {
                break;
            };
            let attrs = slice
                .get(cell_open.saturating_add(3)..cell_open.saturating_add(tag_end_rel))
                .unwrap_or("");
            let content_start = cell_open.saturating_add(tag_end_rel).saturating_add(1);
            let close_pat = if is_th { "</th" } else { "</td" };
            let content_end = lower_slice
                .get(content_start..tr_close)?
                .find(close_pat)
                .map_or(tr_close, |i| content_start.saturating_add(i));

            let raw_inner = slice.get(content_start..content_end).unwrap_or("");
            let text = strip_tags_and_decode_entities(raw_inner);
            let mut style = parse_inline_css_attr(attrs);
            if is_th && style.bold.is_none() {
                style.bold = Some(true);
            }
            let colspan = parse_span_attr(attrs, "colspan").clamp(1, 256);
            let rowspan = parse_span_attr(attrs, "rowspan").clamp(1, 1024);

            // Advance `col_idx` past any slot occupied by an earlier `rowspan`.
            while grid
                .get(row_idx)
                .and_then(|r| r.get(col_idx))
                .is_some_and(Option::is_some)
            {
                col_idx = col_idx.saturating_add(1);
            }

            for dr in 0..rowspan {
                let target_r = row_idx.saturating_add(dr);
                while grid.len() <= target_r {
                    grid.push(Vec::new());
                }
                if let Some(r_vec) = grid.get_mut(target_r) {
                    let needed = col_idx.saturating_add(colspan);
                    if r_vec.len() < needed {
                        r_vec.resize(needed, None);
                    }
                    for dc in 0..colspan {
                        if let Some(slot) = r_vec.get_mut(col_idx.saturating_add(dc)) {
                            *slot = Some(if dr == 0 && dc == 0 {
                                ExternalCell {
                                    text: text.clone(),
                                    style,
                                }
                            } else {
                                ExternalCell {
                                    text: String::new(),
                                    style,
                                }
                            });
                        }
                    }
                }
            }

            col_idx = col_idx.saturating_add(colspan);
            cell_cursor = content_end.saturating_add(5).min(tr_close);
        }

        row_idx = row_idx.saturating_add(1);
        cursor = tr_close.saturating_add(5).min(slice.len());
    }

    if grid.is_empty() {
        return None;
    }
    Some(
        grid.into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|opt| {
                        opt.unwrap_or(ExternalCell {
                            text: String::new(),
                            style: StylePatch::default(),
                        })
                    })
                    .collect()
            })
            .collect(),
    )
}

fn strip_tags_and_decode_entities(inner: &str) -> String {
    let mut stripped = String::with_capacity(inner.len());
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'<')
            && let Some(end_rel) = inner.get(i..).and_then(|s| s.find('>'))
        {
            let tag = inner
                .get(i.saturating_add(1)..i.saturating_add(end_rel))
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if tag.starts_with("br") {
                stripped.push('\n');
            }
            i = i.saturating_add(end_rel).saturating_add(1);
            continue;
        }
        if let Some(rest) = inner.get(i..)
            && let Some(ch) = rest.chars().next()
        {
            stripped.push(ch);
            i = i.saturating_add(ch.len_utf8());
        } else {
            break;
        }
    }
    let decoded = decode_html_entities(&stripped);
    decoded.trim_matches(['\r', '\n']).trim().to_owned()
}

fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return String::from(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        if let Some(prefix) = rest.get(..amp) {
            out.push_str(prefix);
        }
        let after_amp = rest.get(amp.saturating_add(1)..).unwrap_or("");
        if let Some(semi) = after_amp.find(';')
            && semi <= 12
        {
            let ent = after_amp.get(..semi).unwrap_or("");
            let decoded_ch = match ent {
                "amp" | "AMP" => Some('&'),
                "lt" | "LT" => Some('<'),
                "gt" | "GT" => Some('>'),
                "quot" | "QUOT" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" | "NBSP" => Some(' '),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => ent
                    .get(2..)
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .and_then(char::from_u32),
                _ if ent.starts_with('#') => ent
                    .get(1..)
                    .and_then(|d| d.parse::<u32>().ok())
                    .and_then(char::from_u32),
                _ => None,
            };
            if let Some(ch) = decoded_ch {
                out.push(ch);
                rest = after_amp.get(semi.saturating_add(1)..).unwrap_or("");
                continue;
            }
        }
        out.push('&');
        rest = after_amp;
    }
    out.push_str(rest);
    out
}

fn extract_attr_value<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let lower = attrs.to_ascii_lowercase();
    let pat = format!("{name}=");
    let idx = lower.find(&pat)?;
    let val_start = idx.saturating_add(pat.len());
    let rem = attrs.get(val_start..)?.trim_start();
    if let Some(stripped) = rem.strip_prefix('"') {
        let end = stripped.find('"')?;
        stripped.get(..end)
    } else if let Some(stripped) = rem.strip_prefix('\'') {
        let end = stripped.find('\'')?;
        stripped.get(..end)
    } else {
        let end = rem
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(rem.len());
        rem.get(..end)
    }
}

fn parse_span_attr(attrs: &str, name: &str) -> usize {
    extract_attr_value(attrs, name)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
}

fn parse_inline_css_attr(attrs: &str) -> StylePatch {
    let Some(css) = extract_attr_value(attrs, "style") else {
        return StylePatch::default();
    };
    let mut patch = StylePatch::default();
    for decl in css.split(';') {
        let Some((raw_prop, raw_val)) = decl.split_once(':') else {
            continue;
        };
        let prop = raw_prop.trim().to_ascii_lowercase();
        let val = raw_val.trim().to_ascii_lowercase();
        match prop.as_str() {
            "font-weight" => {
                if val.contains("bold") || val.parse::<u16>().is_ok_and(|w| w >= 600) {
                    patch.bold = Some(true);
                }
            }
            "font-style" if val.contains("italic") => {
                patch.italic = Some(true);
            }
            "text-decoration" | "text-decoration-line" if val.contains("line-through") => {
                patch.strikethrough = Some(true);
            }
            "text-align" => {
                patch.align = match val.as_str() {
                    "left" | "start" => Some(HorizontalAlign::Left),
                    "center" => Some(HorizontalAlign::Center),
                    "right" | "end" => Some(HorizontalAlign::Right),
                    _ => patch.align,
                };
            }
            "color" => {
                if let Some(rgb) = parse_css_rgb(&val) {
                    patch.text_color = ColorPatch::Set(ColorValue::Rgb(rgb));
                }
            }
            "background-color" | "background" => {
                if let Some(rgb) = parse_css_rgb(&val) {
                    patch.fill_color = ColorPatch::Set(ColorValue::Rgb(rgb));
                }
            }
            _ => {}
        }
    }
    patch
}

fn parse_css_rgb(val: &str) -> Option<u32> {
    let token = val.split_whitespace().next()?;
    if let Some(hex) = token.strip_prefix('#') {
        if hex.len() == 6 {
            return u32::from_str_radix(hex, 16)
                .ok()
                .filter(|&v| v <= 0x00FF_FFFF);
        }
        if hex.len() == 3 {
            let r = u32::from_str_radix(hex.get(0..1)?, 16).ok()?;
            let g = u32::from_str_radix(hex.get(1..2)?, 16).ok()?;
            let b = u32::from_str_radix(hex.get(2..3)?, 16).ok()?;
            return Some(((r * 17) << 16) | ((g * 17) << 8) | (b * 17));
        }
    }
    if let Some(inner) = val.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let mut parts = inner.split(',').map(|p| p.trim().parse::<u32>().ok());
        let r = parts.next()??.min(255);
        let g = parts.next()??.min(255);
        let b = parts.next()??.min(255);
        return Some((r << 16) | (g << 8) | b);
    }
    None
}

// ------------------------------------------------ formatting & axis helpers

fn read_copied_cell(
    workbook: &Workbook<BoundFormula>,
    sheet: &Sheet,
    sheet_id: SheetId,
    r_pos: u32,
    c_pos: u32,
) -> CopiedCell {
    let (Some(r_raw), Some(c_raw)) = (
        sheet.row_axis().pos_to_id(r_pos),
        sheet.col_axis().pos_to_id(c_pos),
    ) else {
        return CopiedCell {
            input: CellInput::Empty,
            explicit_format: None,
            effective_format: NumberFormat::General,
            style_patch: StylePatch::default(),
        };
    };
    let row = RowId(r_raw);
    let col = ColId(c_raw);
    let snap = workbook.cell_snapshot(sheet_id, row, col);
    let effective_style = sheet.effective_style_by_id(row, col);
    let style_patch = style_to_patch(effective_style);
    CopiedCell {
        input: snap.as_ref().map_or(CellInput::Empty, |s| s.input.clone()),
        explicit_format: snap.and_then(|s| s.explicit_format),
        effective_format: sheet.effective_format_by_id(row, col),
        style_patch,
    }
}

fn style_to_patch(style: CellStyle) -> StylePatch {
    StylePatch {
        bold: style.bold.then_some(true),
        italic: style.italic.then_some(true),
        strikethrough: style.strikethrough.then_some(true),
        align: (!matches!(style.align, HorizontalAlign::Auto)).then_some(style.align),
        text_color: style.text_color.map_or(ColorPatch::Keep, ColorPatch::Set),
        fill_color: style.fill_color.map_or(ColorPatch::Keep, ColorPatch::Set),
        border_top: (!matches!(style.borders.top, BorderWeight::None)).then_some(style.borders.top),
        border_right: (!matches!(style.borders.right, BorderWeight::None))
            .then_some(style.borders.right),
        border_bottom: (!matches!(style.borders.bottom, BorderWeight::None))
            .then_some(style.borders.bottom),
        border_left: (!matches!(style.borders.left, BorderWeight::None))
            .then_some(style.borders.left),
    }
}

fn axis_runs_for_span(axis: &reticula_core::axis::Axis, first_pos: u32, count: u32) -> Vec<IdRun> {
    if count == 0 {
        return Vec::new();
    }
    let last_pos = first_pos.saturating_add(count.saturating_sub(1));
    let (Some(first_id), Some(last_id)) = (axis.pos_to_id(first_pos), axis.pos_to_id(last_pos))
    else {
        return Vec::new();
    };
    let mut runs = Vec::new();
    let _ = axis.for_each_live_in_range(first_id, last_id, |run| runs.push(run));
    runs
}

fn coalesce_raw_ids(iter: impl Iterator<Item = u64>) -> Vec<IdRun> {
    let mut runs: Vec<IdRun> = Vec::new();
    for id in iter {
        if let Some(last) = runs.last_mut()
            && last.first_id.checked_add(u64::from(last.len)) == Some(id)
            && let Some(nl) = last.len.checked_add(1)
        {
            last.len = nl;
            continue;
        }
        runs.push(IdRun {
            first_id: id,
            len: 1,
        });
    }
    runs
}

fn write_inline_style_attr(style: &CellStyle, out: &mut String) {
    let mut rules = String::new();
    if style.bold {
        rules.push_str("font-weight:bold;");
    }
    if style.italic {
        rules.push_str("font-style:italic;");
    }
    if style.strikethrough {
        rules.push_str("text-decoration:line-through;");
    }
    match style.align {
        HorizontalAlign::Auto => {}
        HorizontalAlign::Left => rules.push_str("text-align:left;"),
        HorizontalAlign::Center => rules.push_str("text-align:center;"),
        HorizontalAlign::Right => rules.push_str("text-align:right;"),
    }
    if let Some(tc) = style.text_color {
        let hex = palette_or_rgb_hex(tc, false);
        rules.push_str(&format!("color:#{hex:06x};"));
    }
    if let Some(fc) = style.fill_color {
        let hex = palette_or_rgb_hex(fc, true);
        rules.push_str(&format!("background-color:#{hex:06x};"));
    }
    let _ = MAX_EDITS_PER_OP;
    if !rules.is_empty() {
        out.push_str(" style=\"");
        out.push_str(&rules);
        out.push('"');
    }
}

const fn palette_or_rgb_hex(color: ColorValue, is_fill: bool) -> u32 {
    match color {
        ColorValue::Rgb(rgb) => rgb & 0x00FF_FFFF,
        ColorValue::Palette(slot) => {
            if is_fill {
                match slot % 8 {
                    0 => 0xF1F5F9,
                    1 => 0xFEE2E2,
                    2 => 0xFEF3C7,
                    3 => 0xDCFCE7,
                    4 => 0xDBEAFE,
                    5 => 0xE0E7FF,
                    6 => 0xFCE7F3,
                    _ => 0xCCFBF1,
                }
            } else {
                match slot % 8 {
                    0 => 0x334155,
                    1 => 0xDC2626,
                    2 => 0xD97706,
                    3 => 0x16A34A,
                    4 => 0x2563EB,
                    5 => 0x4F46E5,
                    6 => 0xDB2777,
                    _ => 0x0D9488,
                }
            }
        }
    }
}

fn write_html_escaped(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br>"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::value::Value;
    use reticula_formula::engine::FormulaEngine;
    use reticula_wire::v1::{WireOpV1, WireTxnV1};
    use reticula_wire::wal::encode_wal_record;

    use super::{
        RectPos, build_fill_ops, build_paste_ops, copy_range, find_autofill_end_row,
        tsv_fingerprint,
    };

    #[test]
    fn fill_1m_rows_on_sample_produces_constant_size_wire_op() {
        let eng = FormulaEngine::open_sample(20261117, 1_000_000);
        let s0 = SheetId(0);
        let ops_10 = build_fill_ops(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 1,
                row_count: 1,
                first_col: 12,
                col_count: 1,
            },
            RectPos {
                first_row: 1,
                row_count: 11,
                first_col: 12,
                col_count: 1,
            },
        )
        .unwrap();
        let ops_1m = build_fill_ops(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 1,
                row_count: 1,
                first_col: 12,
                col_count: 1,
            },
            RectPos {
                first_row: 1,
                row_count: 999_999,
                first_col: 12,
                col_count: 1,
            },
        )
        .unwrap();

        assert_eq!(ops_10.len(), 1);
        assert_eq!(ops_1m.len(), 1);
        let rec_10 = encode_wal_record(&reticula_wire::v1::WireWalRecordV1 {
            seq: 1,
            timestamp_ms: 0,
            label: String::from("Fill"),
            ops: ops_10
                .iter()
                .map(|op| WireOpV1::try_from(op).unwrap())
                .collect(),
        })
        .unwrap();
        let rec_1m = encode_wal_record(&reticula_wire::v1::WireWalRecordV1 {
            seq: 1,
            timestamp_ms: 0,
            label: String::from("Fill"),
            ops: ops_1m
                .iter()
                .map(|op| WireOpV1::try_from(op).unwrap())
                .collect(),
        })
        .unwrap();
        let _ = WireTxnV1 {
            label: String::from("Fill"),
            ops: ops_1m
                .iter()
                .map(|op| WireOpV1::try_from(op).unwrap())
                .collect(),
        };
        // Constant O(1) byte size (~104 bytes; only 2 bytes of LEB128 varint
        // difference between `len = 10` and `len = 999_998`)!
        assert!(rec_1m.len() < 150, "rec_1m.len()={}", rec_1m.len());
        assert!(rec_10.len().abs_diff(rec_1m.len()) <= 4);
    }

    #[test]
    fn series_fill_handles_arithmetic_dates_padded_text_and_autofill_end() {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        // Col A: 10, 25 -> 40, 55
        let _ = eng.edit_cell_raw(s0, RowId(0), ColId(0), "10").unwrap();
        let _ = eng.edit_cell_raw(s0, RowId(1), ColId(0), "25").unwrap();
        // Col B: single ISO date 2026-11-19 -> +1 day
        let _ = eng
            .edit_cell_raw(s0, RowId(0), ColId(1), "2026-11-19")
            .unwrap();
        // Col C: Order-008 -> Order-009, Order-010
        let _ = eng
            .edit_cell_raw(s0, RowId(0), ColId(2), "Order-008")
            .unwrap();

        let ops_a = build_fill_ops(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 0,
                row_count: 2,
                first_col: 0,
                col_count: 1,
            },
            RectPos {
                first_row: 0,
                row_count: 4,
                first_col: 0,
                col_count: 1,
            },
        )
        .unwrap();
        eng.apply_txn(ops_a).unwrap();
        assert_eq!(
            eng.workbook().sheet(s0).unwrap().value(2, 0),
            Value::Number(40.0)
        );
        assert_eq!(
            eng.workbook().sheet(s0).unwrap().value(3, 0),
            Value::Number(55.0)
        );

        // Double-click auto-fill on Col B (row 0) detects Col A ends at row 3!
        let end_r = find_autofill_end_row(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 0,
                row_count: 1,
                first_col: 1,
                col_count: 1,
            },
        );
        assert_eq!(end_r, Some(3));

        let ops_bc = build_fill_ops(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 0,
                row_count: 1,
                first_col: 1,
                col_count: 2,
            },
            RectPos {
                first_row: 0,
                row_count: 4,
                first_col: 1,
                col_count: 2,
            },
        )
        .unwrap();
        eng.apply_txn(ops_bc).unwrap();
        assert_eq!(
            eng.inspect_cell(s0, RowId(3), ColId(1)).unwrap().display,
            "2026-11-22"
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(2), ColId(2)).unwrap().display,
            "Order-010"
        );
    }

    #[test]
    fn clipboard_copy_cut_and_external_html_paste_round_trip() {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        let _ = eng.edit_cell_raw(s0, RowId(0), ColId(0), "100").unwrap();
        let _ = eng.edit_cell_raw(s0, RowId(0), ColId(1), "=A1*2").unwrap();
        eng.recalc_all();

        let (copied, clip) = copy_range(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 0,
                row_count: 1,
                first_col: 0,
                col_count: 2,
            },
            true,
        )
        .unwrap();
        let mut clip_opt = Some(clip.clone());
        // Simulate Windows CRLF normalization on system clipboard!
        let crlf_tsv = format!("{}\r\n", copied.tsv);
        assert_eq!(tsv_fingerprint(&crlf_tsv), clip.fingerprint);

        let (paste_ops, rect) = build_paste_ops(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 2,
                row_count: 1,
                first_col: 0,
                col_count: 1,
            },
            &crlf_tsv,
            Some(&copied.html),
            &mut clip_opt,
        )
        .unwrap();
        assert_eq!(rect.col_count, 2);
        eng.apply_txn(paste_ops).unwrap();
        eng.recalc_all();
        // Cut cleared original A1:B1 and shifted `=A1*2` -> `=A3*2` (= 200)!
        assert_eq!(eng.workbook().sheet(s0).unwrap().value(0, 0), Value::Empty);
        assert_eq!(
            eng.inspect_cell(s0, RowId(2), ColId(1)).unwrap().raw,
            "=A3*2"
        );
        assert_eq!(
            eng.workbook().sheet(s0).unwrap().value(2, 1),
            Value::Number(200.0)
        );

        // External Excel/Google Sheets HTML table with inline styles & entities.
        let ext_html = "<table><tr><td style=\"font-weight: 700; background-color: #fef3c7\">Revenue &amp; Tax</td><td style=\"color: rgb(220, 38, 38)\">12.5%</td></tr></table>";
        let (ext_ops, _) = build_paste_ops(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 5,
                row_count: 1,
                first_col: 0,
                col_count: 1,
            },
            "Revenue & Tax\t12.5%\r\n",
            Some(ext_html),
            &mut clip_opt,
        )
        .unwrap();
        eng.apply_txn(ext_ops).unwrap();
        assert_eq!(
            eng.inspect_cell(s0, RowId(5), ColId(0)).unwrap().display,
            "Revenue & Tax"
        );
        assert!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .effective_style_by_id(RowId(5), ColId(0))
                .bold
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(5), ColId(1)).unwrap().display,
            "13%"
        );
    }

    #[test]
    fn cut_paste_emits_move_cells_and_rewrites_same_and_cross_sheet_references() {
        use super::{FormatPatch, FormatTarget, IdRun, Op, StylePatch};

        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        let s1 = SheetId(1);
        eng.apply_op(Op::AddSheet {
            sheet: s1,
            name: String::from("Summary"),
            after: Some(s0),
        })
        .unwrap();

        eng.edit_cell_raw(s0, RowId(0), ColId(0), "10").unwrap();
        eng.edit_cell_raw(s0, RowId(1), ColId(0), "20").unwrap();
        eng.edit_cell_raw(s0, RowId(2), ColId(0), "30").unwrap();
        eng.apply_op(Op::SetFormat {
            sheet: s0,
            target: FormatTarget::Cells {
                rows: vec![IdRun::new(0, 3).unwrap()],
                cols: vec![IdRun::new(0, 1).unwrap()],
            },
            number_format: FormatPatch::Keep,
            style: StylePatch {
                bold: Some(true),
                ..StylePatch::default()
            },
        })
        .unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "=SUM(A1:A3)")
            .unwrap();
        eng.edit_cell_raw(s1, RowId(0), ColId(0), "=SUM(Sheet1!A1:A3)")
            .unwrap();
        eng.recalc_all();

        let (payload, cut_clip) = copy_range(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 0,
                row_count: 3,
                first_col: 0,
                col_count: 1,
            },
            true,
        )
        .unwrap();
        let mut clip_opt = Some(cut_clip);
        let (ops, _) = build_paste_ops(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 4,
                row_count: 1,
                first_col: 3,
                col_count: 1,
            },
            &payload.tsv,
            None,
            &mut clip_opt,
        )
        .unwrap();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], Op::MoveCells { .. }));

        let invs = eng.apply_txn(ops).unwrap();
        eng.recalc_all();

        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(0)).unwrap().display,
            ""
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(4), ColId(3)).unwrap().display,
            "10"
        );
        assert!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .effective_style_by_id(RowId(4), ColId(3))
                .bold
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(D5:D7)"
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(1)).unwrap().display,
            "60"
        );
        assert_eq!(
            eng.inspect_cell(s1, RowId(0), ColId(0)).unwrap().raw,
            "=SUM(Sheet1!D5:D7)"
        );

        // Cut-paste consumes `cut` flag, then cross-sheet MoveCells move works via undo + re-cut.
        for inv in invs.into_iter().rev() {
            eng.apply_inverse(inv).unwrap();
        }
        eng.recalc_all();
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A1:A3)"
        );
        assert_eq!(
            eng.inspect_cell(s1, RowId(0), ColId(0)).unwrap().raw,
            "=SUM(Sheet1!A1:A3)"
        );

        let (payload2, cut_clip2) = copy_range(
            eng.workbook(),
            s0,
            RectPos {
                first_row: 0,
                row_count: 3,
                first_col: 0,
                col_count: 1,
            },
            true,
        )
        .unwrap();
        let mut clip_opt2 = Some(cut_clip2);
        let (ops2, _) = build_paste_ops(
            eng.workbook(),
            s1,
            RectPos {
                first_row: 2,
                row_count: 1,
                first_col: 2,
                col_count: 1,
            },
            &payload2.tsv,
            None,
            &mut clip_opt2,
        )
        .unwrap();
        assert!(matches!(ops2.as_slice(), [Op::MoveCells { .. }]));
        eng.apply_txn(ops2).unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(Summary!C3:C5)"
        );
    }
}

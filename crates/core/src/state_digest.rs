//! Versioned 192-bit collaborative state digests (`15-m2a.md` 5.1 & `06-sync.md`).
//!
//! [`StateDigest`] (`STATE_DIGEST_VERSION = 1`) commits to:
//! - `content` (`u128` via `content_lo` + `content_hi`): Abelian ring sum over
//!   $\mathbb{Z}/2^{128}\mathbb{Z}$ of logical sheet metadata, coalesced axis
//!   identity intervals, linear tombstone counts, custom heights/widths,
//!   formats, style patches, scalar cell values (canonicalising `-0.0` to `+0.0`
//!   and hashing UTF-8 string bytes rather than `StringId`), and bound/raw
//!   formula inputs (**explicitly excluding cached formula evaluation
//!   results**).
//! - `order` (`u64`): deterministic sequence digest over workbook sheet tab
//!   order and each sheet's coalesced row/column axis order.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::axis::Axis;
use crate::digest::Digest64;
use crate::format::NumberFormat;
use crate::id::{ColId, IdRun, RowId, SheetId, id_counter, id_stream};
use crate::ops::FormulaPayload;
use crate::rng::mix64;
use crate::sheet::{DEFAULT_COL_WIDTH, DEFAULT_ROW_HEIGHT, Sheet};
use crate::style::StylePatch;
use crate::value::CalcError;

/// Frozen version of the collaborative state digest format (`15-m2a.md` 5.1).
pub const STATE_DIGEST_VERSION: u16 = 1;

const TAG_SHEET_META: u64 = 1;
const TAG_AXIS_INTERVAL: u64 = 2;
const TAG_AXIS_TOMB: u64 = 3;
const TAG_ROW_HEIGHT: u64 = 4;
const TAG_COL_WIDTH: u64 = 5;
const TAG_COL_FORMAT: u64 = 6;
const TAG_ROW_FORMAT: u64 = 7;
const TAG_CELL_FORMAT: u64 = 8;
const TAG_COL_STYLE: u64 = 9;
const TAG_ROW_STYLE: u64 = 10;
const TAG_CELL_STYLE: u64 = 11;
const TAG_SCALAR_CELL: u64 = 12;
const TAG_FORMULA_CELL: u64 = 13;

const LANE_LO_MASK: u64 = 0x517C_C1B7_2722_0A95;
const LANE_HI_MASK: u64 = 0x94D0_49BB_1331_11EB;

/// Axis kind discriminator used in domain-separated element hashes.
pub const AXIS_KIND_ROW: u8 = 0;
/// Axis kind discriminator used in domain-separated element hashes.
pub const AXIS_KIND_COL: u8 = 1;

/// 192-bit versioned workbook state verification digest (`15-m2a.md` 5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StateDigest {
    /// Digest format version ([`STATE_DIGEST_VERSION`]).
    pub version: u16,
    /// Lower 64 bits of the 128-bit logical content/membership digest.
    pub content_lo: u64,
    /// Upper 64 bits of the 128-bit logical content/membership digest.
    pub content_hi: u64,
    /// 64-bit ordered digest over sheet tab order and row/column axis order.
    pub order: u64,
}

impl StateDigest {
    /// Constructs a v1 [`StateDigest`] from 128-bit content sum and 64-bit order.
    #[must_use]
    pub const fn from_parts(content: u128, order: u64) -> Self {
        Self {
            version: STATE_DIGEST_VERSION,
            content_lo: (content & 0xFFFF_FFFF_FFFF_FFFF) as u64,
            content_hi: (content >> 64) as u64,
            order,
        }
    }

    /// Reconstructs the 128-bit `content` value from `(content_lo, content_hi)`.
    #[must_use]
    pub const fn content(self) -> u128 {
        ((self.content_hi as u128) << 64) | (self.content_lo as u128)
    }
}

/// Independent dual-lane 128-bit hasher built exclusively on `Digest64`/`mix64`.
#[derive(Clone, Debug)]
struct Digest128 {
    lo: Digest64,
    hi: Digest64,
}

impl Digest128 {
    fn new(tag: u64) -> Self {
        let mut lo = Digest64::new();
        lo.write_u64(tag ^ LANE_LO_MASK);
        let mut hi = Digest64::new();
        hi.write_u64(tag.rotate_left(32) ^ LANE_HI_MASK);
        Self { lo, hi }
    }

    fn write_u8(&mut self, v: u8) {
        self.write_u64(u64::from(v));
    }

    fn write_u16(&mut self, v: u16) {
        self.write_u64(u64::from(v));
    }

    fn write_u32(&mut self, v: u32) {
        self.lo.write_u32(v);
        self.hi.write_u32(v);
    }

    fn write_u64(&mut self, v: u64) {
        self.lo.write_u64(v);
        self.hi.write_u64(v);
    }

    fn write_f64(&mut self, v: f64) {
        self.lo.write_f64(v);
        self.hi.write_f64(v);
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.lo.write_bytes(bytes);
        self.hi.write_bytes(bytes);
    }

    fn feed_digest64(&mut self, f: impl Fn(&mut Digest64)) {
        f(&mut self.lo);
        f(&mut self.hi);
    }

    fn finish(&self) -> u128 {
        let l = self.lo.finish();
        let h = mix64(self.hi.finish() ^ l.rotate_left(17));
        (u128::from(h) << 64) | u128::from(l)
    }
}

/// Canonical borrowed view of a non-empty scalar value for hashing without
/// cloning heap strings.
#[derive(Clone, Copy, Debug)]
pub enum CanonicalScalar<'a> {
    /// Finite IEEE 754 number (`-0.0` is canonicalised to `+0.0`).
    Number(f64),
    /// UTF-8 string content.
    Text(&'a str),
    /// Logical boolean.
    Bool(bool),
    /// Formula or domain error.
    Error(CalcError),
}

/// Computes the 128-bit hash of one sheet's metadata (`id`, `name`,
/// `deleted_count`, `frozen_rows`, `frozen_cols`).
#[must_use]
pub fn hash_sheet_meta(
    sheet_id: SheetId,
    name: &str,
    deleted_count: u32,
    frozen_rows: u32,
    frozen_cols: u32,
) -> u128 {
    let mut d = Digest128::new(TAG_SHEET_META);
    d.write_u32(sheet_id.0);
    d.write_bytes(name.as_bytes());
    d.write_u32(deleted_count);
    d.write_u32(frozen_rows);
    d.write_u32(frozen_cols);
    d.finish()
}

/// Computes the 128-bit hash of one coalesced contiguous `IdRun` on an axis.
#[must_use]
fn hash_axis_interval(sheet_id: SheetId, axis_kind: u8, first_id: u64, len: u32) -> u128 {
    let mut d = Digest128::new(TAG_AXIS_INTERVAL);
    d.write_u32(sheet_id.0);
    d.write_u8(axis_kind);
    d.write_u64(first_id);
    d.write_u32(len);
    d.finish()
}

/// Computes the 128-bit unit weight of one axis identity's tombstone count.
#[must_use]
pub fn hash_axis_tombstone(sheet_id: SheetId, axis_kind: u8, raw_id: u64) -> u128 {
    let mut d = Digest128::new(TAG_AXIS_TOMB);
    d.write_u32(sheet_id.0);
    d.write_u8(axis_kind);
    d.write_u64(raw_id);
    d.finish()
}

/// Computes the permutation-invariant 128-bit digest of an [`Axis`] in
/// `O(runs log runs)` time (coalescing sorted runs so splits or moves leave the
/// identity set invariant, plus linear weights for all tombstoned IDs).
#[must_use]
pub fn axis_content_hash(sheet_id: SheetId, axis_kind: u8, axis: &Axis) -> u128 {
    let mut runs: Vec<IdRun> = axis.runs().iter().map(|r| r.ids).collect();
    runs.sort_unstable_by_key(|r| r.first_id);

    let mut coalesced: Vec<IdRun> = Vec::with_capacity(runs.len());
    for run in runs {
        if let Some(last) = coalesced.last_mut()
            && id_stream(last.first_id) == id_stream(run.first_id)
            && id_counter(last.first_id).checked_add(last.len) == Some(id_counter(run.first_id))
        {
            last.len = last.len.saturating_add(run.ids_len());
        } else {
            coalesced.push(run);
        }
    }

    let mut acc = 0_u128;
    for r in coalesced {
        acc = acc.wrapping_add(hash_axis_interval(sheet_id, axis_kind, r.first_id, r.len));
    }

    for run in axis.runs() {
        if let Some(counts) = &run.tombstones {
            for (idx, &cnt) in counts.iter().enumerate() {
                if cnt > 0
                    && let Ok(off) = u32::try_from(idx)
                    && let Some(raw_id) = run.ids.id_at(off)
                {
                    let w = hash_axis_tombstone(sheet_id, axis_kind, raw_id);
                    acc = acc.wrapping_add(u128::from(cnt).wrapping_mul(w));
                }
            }
        }
    }
    acc
}

trait IdRunExt {
    fn ids_len(self) -> u32;
}

impl IdRunExt for IdRun {
    fn ids_len(self) -> u32 {
        self.len
    }
}

/// Computes the optional custom row height hash (`None` / [`DEFAULT_ROW_HEIGHT`] -> `0`).
#[must_use]
pub fn hash_row_height(sheet_id: SheetId, row: RowId, height: Option<u16>) -> u128 {
    let Some(px) = height.filter(|&h| h != DEFAULT_ROW_HEIGHT) else {
        return 0;
    };
    let mut d = Digest128::new(TAG_ROW_HEIGHT);
    d.write_u32(sheet_id.0);
    d.write_u64(row.0);
    d.write_u16(px);
    d.finish()
}

/// Computes the optional custom column width hash (`None` / [`DEFAULT_COL_WIDTH`] -> `0`).
#[must_use]
pub fn hash_col_width(sheet_id: SheetId, col: ColId, width: Option<u16>) -> u128 {
    let Some(px) = width.filter(|&w| w != DEFAULT_COL_WIDTH) else {
        return 0;
    };
    let mut d = Digest128::new(TAG_COL_WIDTH);
    d.write_u32(sheet_id.0);
    d.write_u64(col.0);
    d.write_u16(px);
    d.finish()
}

/// Computes column number-format hash (`General` hashes to `0` since it is default).
#[must_use]
pub fn hash_col_format(sheet_id: SheetId, col: ColId, fmt: NumberFormat) -> u128 {
    let norm = fmt.normalize();
    if norm == NumberFormat::General {
        return 0;
    }
    let mut d = Digest128::new(TAG_COL_FORMAT);
    d.write_u32(sheet_id.0);
    d.write_u64(col.0);
    d.feed_digest64(|h| norm.write_digest(h));
    d.finish()
}

/// Computes explicit row number-format hash (`None` -> `0`).
#[must_use]
pub fn hash_row_format(sheet_id: SheetId, row: RowId, fmt: Option<NumberFormat>) -> u128 {
    let Some(f) = fmt else {
        return 0;
    };
    let norm = f.normalize();
    let mut d = Digest128::new(TAG_ROW_FORMAT);
    d.write_u32(sheet_id.0);
    d.write_u64(row.0);
    d.feed_digest64(|h| norm.write_digest(h));
    d.finish()
}

/// Computes explicit per-cell number-format hash (`None` -> `0`).
#[must_use]
pub fn hash_cell_format(
    sheet_id: SheetId,
    row: RowId,
    col: ColId,
    fmt: Option<NumberFormat>,
) -> u128 {
    let Some(f) = fmt else {
        return 0;
    };
    let norm = f.normalize();
    let mut d = Digest128::new(TAG_CELL_FORMAT);
    d.write_u32(sheet_id.0);
    d.write_u64(row.0);
    d.write_u64(col.0);
    d.feed_digest64(|h| norm.write_digest(h));
    d.finish()
}

/// Computes whole-column [`StylePatch`] hash (empty patch hashes to `0`).
#[must_use]
pub fn hash_col_style(sheet_id: SheetId, col: ColId, patch: StylePatch) -> u128 {
    if patch.is_empty() {
        return 0;
    }
    let mut d = Digest128::new(TAG_COL_STYLE);
    d.write_u32(sheet_id.0);
    d.write_u64(col.0);
    d.feed_digest64(|h| patch.write_digest(h));
    d.finish()
}

/// Computes whole-row [`StylePatch`] hash (empty patch hashes to `0`).
#[must_use]
pub fn hash_row_style(sheet_id: SheetId, row: RowId, patch: StylePatch) -> u128 {
    if patch.is_empty() {
        return 0;
    }
    let mut d = Digest128::new(TAG_ROW_STYLE);
    d.write_u32(sheet_id.0);
    d.write_u64(row.0);
    d.feed_digest64(|h| patch.write_digest(h));
    d.finish()
}

/// Computes per-cell [`StylePatch`] hash (empty patch hashes to `0`).
#[must_use]
pub fn hash_cell_style(sheet_id: SheetId, row: RowId, col: ColId, patch: StylePatch) -> u128 {
    if patch.is_empty() {
        return 0;
    }
    let mut d = Digest128::new(TAG_CELL_STYLE);
    d.write_u32(sheet_id.0);
    d.write_u64(row.0);
    d.write_u64(col.0);
    d.feed_digest64(|h| patch.write_digest(h));
    d.finish()
}

/// Computes the 128-bit hash of a non-formula scalar cell.
#[must_use]
pub fn hash_scalar_cell(
    sheet_id: SheetId,
    row: RowId,
    col: ColId,
    scalar: CanonicalScalar<'_>,
) -> u128 {
    let mut d = Digest128::new(TAG_SCALAR_CELL);
    d.write_u32(sheet_id.0);
    d.write_u64(row.0);
    d.write_u64(col.0);
    match scalar {
        CanonicalScalar::Number(n) => {
            let canon = if n == 0.0 { 0.0 } else { n };
            d.write_u8(1);
            d.write_f64(canon);
        }
        CanonicalScalar::Text(s) => {
            d.write_u8(2);
            d.write_bytes(s.as_bytes());
        }
        CanonicalScalar::Bool(flag) => {
            d.write_u8(3);
            d.write_u8(u8::from(flag));
        }
        CanonicalScalar::Error(err) => {
            d.write_u8(4);
            d.write_u8(err as u8);
        }
    }
    d.finish()
}

/// Computes the 128-bit hash of a formula cell (covering only formula payload
/// and mode via [`FormulaPayload::write_digest`], ignoring cached evaluation).
#[must_use]
pub fn hash_formula_cell<F: FormulaPayload>(
    sheet_id: SheetId,
    row: RowId,
    col: ColId,
    formula: &F,
) -> u128 {
    let mut lo = Digest64::new();
    lo.write_u64(TAG_FORMULA_CELL ^ LANE_LO_MASK);
    lo.write_u32(sheet_id.0);
    lo.write_u64(row.0);
    lo.write_u64(col.0);
    formula.write_digest(&mut lo);

    let mut hi = Digest64::new();
    hi.write_u64(TAG_FORMULA_CELL.rotate_left(32) ^ LANE_HI_MASK);
    hi.write_u32(sheet_id.0);
    hi.write_u64(row.0);
    hi.write_u64(col.0);
    formula.write_digest(&mut hi);

    let l = lo.finish();
    let h = mix64(hi.finish() ^ l.rotate_left(17));
    (u128::from(h) << 64) | u128::from(l)
}

/// Computes the full content digest contribution (`u128`) and populated cell
/// count (`u64`) of one standalone [`Sheet`] (excluding formulas, which are
/// indexed workbook-wide — pass a check helper for whether a cell has a formula).
pub(crate) fn sheet_content_and_scalar_count<F: FnMut(RowId, ColId) -> bool>(
    sheet: &Sheet,
    mut has_formula: F,
) -> (u128, u64) {
    let sheet_id = sheet.id();
    let mut acc = 0_u128;
    let mut scalar_count = 0_u64;

    acc = acc.wrapping_add(hash_sheet_meta(
        sheet_id,
        sheet.name(),
        sheet.deleted_count(),
        sheet.frozen_rows(),
        sheet.frozen_cols(),
    ));
    acc = acc.wrapping_add(axis_content_hash(sheet_id, AXIS_KIND_ROW, sheet.row_axis()));
    acc = acc.wrapping_add(axis_content_hash(sheet_id, AXIS_KIND_COL, sheet.col_axis()));

    for (&row, &px) in sheet.row_heights() {
        acc = acc.wrapping_add(hash_row_height(sheet_id, row, Some(px)));
    }
    for (&col, &px) in sheet.col_widths() {
        acc = acc.wrapping_add(hash_col_width(sheet_id, col, Some(px)));
    }
    for (slot_idx, &fmt) in sheet.col_formats().iter().enumerate() {
        if fmt != NumberFormat::General
            && let Ok(slot_u32) = u32::try_from(slot_idx)
            && let Some(raw_col_id) = sheet.col_slots().slot_to_id(slot_u32)
        {
            acc = acc.wrapping_add(hash_col_format(sheet_id, ColId(raw_col_id), fmt));
        }
    }
    for (&row, &fmt) in sheet.row_formats() {
        acc = acc.wrapping_add(hash_row_format(sheet_id, row, Some(fmt)));
    }
    for (&(row, col), &fmt) in sheet.cell_formats() {
        acc = acc.wrapping_add(hash_cell_format(sheet_id, row, col, Some(fmt)));
    }
    for (&col, &patch) in sheet.col_styles() {
        acc = acc.wrapping_add(hash_col_style(sheet_id, col, patch));
    }
    for (&row, &patch) in sheet.row_styles() {
        acc = acc.wrapping_add(hash_row_style(sheet_id, row, patch));
    }
    for (&(row, col), &patch) in sheet.cell_styles() {
        acc = acc.wrapping_add(hash_cell_style(sheet_id, row, col, patch));
    }

    for (col_slot_idx, column) in sheet.raw_columns().iter().enumerate() {
        let Ok(col_slot) = u32::try_from(col_slot_idx) else {
            continue;
        };
        let Some(raw_col_id) = sheet.col_slots().slot_to_id(col_slot) else {
            continue;
        };
        let col = ColId(raw_col_id);
        column.for_each_non_empty(|row_slot, val| {
            let Some(raw_row_id) = sheet.row_slots().slot_to_id(row_slot) else {
                return;
            };
            let row = RowId(raw_row_id);
            if has_formula(row, col) {
                return;
            }
            let scalar = match val {
                crate::value::Value::Empty => return,
                crate::value::Value::Number(n) => CanonicalScalar::Number(n),
                crate::value::Value::Text(id) => CanonicalScalar::Text(sheet.strings().get(id)),
                crate::value::Value::Bool(flag) => CanonicalScalar::Bool(flag),
                crate::value::Value::Error(err) => CanonicalScalar::Error(err),
                crate::value::Value::Array(_) => CanonicalScalar::Error(CalcError::Spill),
            };
            acc = acc.wrapping_add(hash_scalar_cell(sheet_id, row, col, scalar));
            scalar_count = scalar_count.saturating_add(1);
        });
    }

    (acc, scalar_count)
}

/// Computes the 64-bit order digest covering worksheet tab sequence and each
/// sheet's coalesced row and column axis identity sequences.
#[must_use]
pub fn compute_order_digest(sheets: &[Sheet]) -> u64 {
    let mut d = Digest64::new();
    d.write_u32(u32::try_from(sheets.len()).unwrap_or(u32::MAX));
    for sheet in sheets {
        d.write_u32(sheet.id().0);
        write_coalesced_axis_order(sheet.row_axis(), &mut d);
        write_coalesced_axis_order(sheet.col_axis(), &mut d);
    }
    d.finish()
}

fn write_coalesced_axis_order(axis: &Axis, d: &mut Digest64) {
    let mut coalesced: Vec<IdRun> = Vec::new();
    for run in axis.runs() {
        if let Some(last) = coalesced.last_mut()
            && id_stream(last.first_id) == id_stream(run.ids.first_id)
            && id_counter(last.first_id).checked_add(last.len) == Some(id_counter(run.ids.first_id))
        {
            last.len = last.len.saturating_add(run.ids.len);
        } else {
            coalesced.push(run.ids);
        }
    }
    d.write_u32(u32::try_from(coalesced.len()).unwrap_or(u32::MAX));
    for r in coalesced {
        d.write_u64(r.first_id);
        d.write_u32(r.len);
    }
}

//! Segmented binary snapshot container v0 (`RTCLSNAP`).
//!
//! Layout (`04-engine.md` & `13-m1a.md` 5.7):
//! - 8-byte magic `RTCLSNAP`
//! - format version `u32` little-endian (`0`)
//! - kernel version string (`u16` byte length + UTF-8 bytes)
//! - directory entry count (`u16` little-endian)
//! - directory entries: `(section_kind: u16, offset: u64, length: u64)`
//! - LZ4-compressed section payloads (`Strings`, `Axes`, `Columns`, `Formulas`,
//!   optional `SeedOrigin`), each prefixed by a bounded `u32` uncompressed byte
//!   length so untrusted or fuzzed inputs can never trigger an unbounded
//!   allocation.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::KERNEL_VERSION;
use crate::axis::AxisRun;
use crate::format::NumberFormat;
use crate::formula_store::FormulaStoreBuilder;
use crate::id::{ColId, IdAllocator, RowId, SheetId};
use crate::ops::{
    FormulaPayload, MAX_CELL_TEXT_UTF16, MAX_SHEETS, MAX_WORKBOOK_BYTES, OpError, SeedOrigin,
    Workbook,
};
use crate::sheet::Sheet;
use crate::store::{Block, Column, SlotMap};
use crate::strings::StringTable;
use crate::value::Value;

/// 8-byte snapshot file magic.
pub const SNAPSHOT_MAGIC: &[u8; 8] = b"RTCLSNAP";

/// Snapshot container format version for M1a.
pub const SNAPSHOT_VERSION: u32 = 0;

const SECTION_STRINGS: u16 = 1;
const SECTION_AXES: u16 = 2;
const SECTION_COLUMNS: u16 = 3;
const SECTION_FORMULAS: u16 = 4;
const SECTION_SEED_ORIGIN: u16 = 5;

const MAX_KERNEL_VERSION_LEN: usize = 64;
const MAX_DIRECTORY_SECTIONS: usize = 16;
/// Hard ceiling on a single decompressed section (`512 MiB`) and expansion
/// ratio guard so malformed inputs cannot cause out-of-memory traps.
const MAX_SECTION_UNCOMPRESSED: usize = 536_870_912;

/// Errors returned by snapshot encoding or decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// Missing or mismatched `RTCLSNAP` magic header.
    BadMagic,
    /// Snapshot version is not supported by this build.
    UnsupportedVersion,
    /// Truncated header, directory entry, or section slice.
    Truncated,
    /// Directory contains duplicate or out-of-bounds section ranges.
    InvalidDirectory,
    /// Required section (`Strings`, `Axes`, `Columns`, `Formulas`) is missing.
    MissingSection,
    /// LZ4 compression or bounded decompression failed.
    Compression,
    /// `postcard` serialisation or deserialisation failed.
    Codec,
    /// Decoded content violated a structural, string, NaN-boxing, or workbook
    /// limit invariant.
    Validation,
}

#[derive(Serialize, Deserialize)]
struct StringsSection {
    sheets: Vec<SheetStringsWire>,
}

#[derive(Serialize, Deserialize)]
struct SheetStringsWire {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

#[derive(Serialize, Deserialize)]
struct AxesSection {
    allocator_epoch: u8,
    allocator_replica: u32,
    allocator_next_counter: u32,
    next_sheet_id: u32,
    sheets: Vec<SheetAxesWire>,
}

#[derive(Serialize, Deserialize)]
struct SheetAxesWire {
    id: SheetId,
    name: String,
    row_runs: Vec<AxisRun>,
    col_runs: Vec<AxisRun>,
    row_slot_base: u32,
    row_slot_extra: Vec<u64>,
    col_slot_base: u32,
    col_slot_extra: Vec<u64>,
    col_formats: Vec<NumberFormat>,
    cell_formats: Vec<(RowId, ColId, NumberFormat)>,
}

#[derive(Serialize, Deserialize)]
struct ColumnsSection {
    sheets: Vec<Vec<CompressedColumnWire>>,
}

#[derive(Serialize, Deserialize)]
struct CompressedColumnWire {
    len: u32,
    lz4_blocks: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct FormulasSection<F> {
    entries: Vec<(SheetId, RowId, ColId, F)>,
}

/// Encodes `workbook` into a self-contained `RTCLSNAP` v0 byte buffer.
pub fn encode_snapshot<F>(workbook: &Workbook<F>) -> Result<Vec<u8>, SnapshotError>
where
    F: FormulaPayload + Serialize,
{
    let strings_sec = StringsSection {
        sheets: workbook
            .sheets()
            .iter()
            .map(|sheet| SheetStringsWire {
                bytes: sheet.strings().raw_bytes().to_vec(),
                ends: sheet.strings().raw_ends().to_vec(),
            })
            .collect(),
    };

    let axes_sec = AxesSection {
        allocator_epoch: workbook.allocator().epoch(),
        allocator_replica: workbook.allocator().replica(),
        allocator_next_counter: workbook.allocator().next_counter(),
        next_sheet_id: workbook.next_sheet_id(),
        sheets: workbook
            .sheets()
            .iter()
            .map(|sheet| SheetAxesWire {
                id: sheet.id(),
                name: String::from(sheet.name()),
                row_runs: sheet.row_axis().to_runs_vec(),
                col_runs: sheet.col_axis().to_runs_vec(),
                row_slot_base: sheet.row_slots().base_len(),
                row_slot_extra: sheet.row_slots().extra_ids().to_vec(),
                col_slot_base: sheet.col_slots().base_len(),
                col_slot_extra: sheet.col_slots().extra_ids().to_vec(),
                col_formats: sheet.col_formats().to_vec(),
                cell_formats: sheet
                    .cell_formats()
                    .iter()
                    .map(|(&(r, c), &fmt)| (r, c, fmt))
                    .collect(),
            })
            .collect(),
    };

    let mut cols_sheets = Vec::with_capacity(workbook.sheets().len());
    for sheet in workbook.sheets() {
        let mut encoded_cols = Vec::with_capacity(sheet.raw_columns().len());
        for col in sheet.raw_columns() {
            let raw_blocks =
                postcard::to_allocvec(col.blocks()).map_err(|_| SnapshotError::Codec)?;
            let lz4_blocks = compress_bounded(&raw_blocks)?;
            encoded_cols.push(CompressedColumnWire {
                len: col.len(),
                lz4_blocks,
            });
        }
        cols_sheets.push(encoded_cols);
    }
    let columns_sec = ColumnsSection {
        sheets: cols_sheets,
    };

    let formulas_sec = FormulasSection {
        entries: workbook
            .formulas()
            .iter()
            .map(|((s, r, c), f)| (s, r, c, f.into_owned()))
            .collect(),
    };

    let mut sections: Vec<(u16, Vec<u8>)> = Vec::with_capacity(5);
    sections.push((SECTION_STRINGS, encode_section(&strings_sec)?));
    sections.push((SECTION_AXES, encode_section(&axes_sec)?));
    sections.push((SECTION_COLUMNS, encode_section(&columns_sec)?));
    sections.push((SECTION_FORMULAS, encode_section(&formulas_sec)?));
    if let Some(origin) = workbook.seed_origin() {
        sections.push((SECTION_SEED_ORIGIN, encode_section(&origin)?));
    }

    let kernel_bytes = KERNEL_VERSION.as_bytes();
    let kernel_len = u16::try_from(kernel_bytes.len()).map_err(|_| SnapshotError::Codec)?;
    let section_count = u16::try_from(sections.len()).map_err(|_| SnapshotError::Codec)?;

    // Header size: 8 (magic) + 4 (version) + 2 + kernel_len + 2 (count) + count * 18.
    let dir_bytes = sections.len().saturating_mul(18);
    let header_len = 16_usize
        .saturating_add(kernel_bytes.len())
        .saturating_add(dir_bytes);
    let payload_len = sections
        .iter()
        .fold(0usize, |acc, (_, data)| acc.saturating_add(data.len()));

    let mut out = Vec::with_capacity(header_len.saturating_add(payload_len));
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    out.extend_from_slice(&kernel_len.to_le_bytes());
    out.extend_from_slice(kernel_bytes);
    out.extend_from_slice(&section_count.to_le_bytes());

    let mut running_offset = u64::try_from(header_len).map_err(|_| SnapshotError::Codec)?;
    for (kind, data) in &sections {
        let len_u64 = u64::try_from(data.len()).map_err(|_| SnapshotError::Codec)?;
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&running_offset.to_le_bytes());
        out.extend_from_slice(&len_u64.to_le_bytes());
        running_offset = running_offset
            .checked_add(len_u64)
            .ok_or(SnapshotError::Codec)?;
    }

    for (_, data) in sections {
        out.extend_from_slice(&data);
    }

    Ok(out)
}

/// Decodes and validates a `Workbook<F>` (plus recorded kernel version string)
/// from an `RTCLSNAP` v0 byte slice.
pub fn decode_snapshot<F>(bytes: &[u8]) -> Result<(Workbook<F>, String), SnapshotError>
where
    F: FormulaPayload + DeserializeOwned,
{
    let magic = bytes.get(0..8).ok_or(SnapshotError::Truncated)?;
    if magic != SNAPSHOT_MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = read_u32_le(bytes, 8)?;
    if version != SNAPSHOT_VERSION {
        return Err(SnapshotError::UnsupportedVersion);
    }
    let kernel_len = usize::from(read_u16_le(bytes, 12)?);
    if kernel_len > MAX_KERNEL_VERSION_LEN {
        return Err(SnapshotError::InvalidDirectory);
    }
    let kernel_end = 14_usize.saturating_add(kernel_len);
    let kernel_slice = bytes.get(14..kernel_end).ok_or(SnapshotError::Truncated)?;
    let kernel_ver = core::str::from_utf8(kernel_slice)
        .map(String::from)
        .map_err(|_| SnapshotError::Codec)?;

    let section_count = usize::from(read_u16_le(bytes, kernel_end)?);
    if section_count == 0 || section_count > MAX_DIRECTORY_SECTIONS {
        return Err(SnapshotError::InvalidDirectory);
    }

    let dir_start = kernel_end.saturating_add(2);
    let dir_end = dir_start.saturating_add(section_count.saturating_mul(18));
    if dir_end > bytes.len() {
        return Err(SnapshotError::Truncated);
    }

    let mut dir: BTreeMap<u16, &[u8]> = BTreeMap::new();
    for idx in 0..section_count {
        let pos = dir_start.saturating_add(idx.saturating_mul(18));
        let kind = read_u16_le(bytes, pos)?;
        let offset = usize::try_from(read_u64_le(bytes, pos.saturating_add(2))?)
            .map_err(|_| SnapshotError::InvalidDirectory)?;
        let length = usize::try_from(read_u64_le(bytes, pos.saturating_add(10))?)
            .map_err(|_| SnapshotError::InvalidDirectory)?;
        let end = offset
            .checked_add(length)
            .ok_or(SnapshotError::InvalidDirectory)?;
        if offset < dir_end || end > bytes.len() {
            return Err(SnapshotError::InvalidDirectory);
        }
        let slice = bytes
            .get(offset..end)
            .ok_or(SnapshotError::InvalidDirectory)?;
        if dir.insert(kind, slice).is_some() {
            return Err(SnapshotError::InvalidDirectory);
        }
    }

    let strings_sec: StringsSection = decode_section(
        dir.get(&SECTION_STRINGS)
            .copied()
            .ok_or(SnapshotError::MissingSection)?,
    )?;
    let axes_sec: AxesSection = decode_section(
        dir.get(&SECTION_AXES)
            .copied()
            .ok_or(SnapshotError::MissingSection)?,
    )?;
    let columns_sec: ColumnsSection = decode_section(
        dir.get(&SECTION_COLUMNS)
            .copied()
            .ok_or(SnapshotError::MissingSection)?,
    )?;
    let formulas_sec: FormulasSection<F> = decode_section(
        dir.get(&SECTION_FORMULAS)
            .copied()
            .ok_or(SnapshotError::MissingSection)?,
    )?;
    let seed_origin: Option<SeedOrigin> = match dir.get(&SECTION_SEED_ORIGIN) {
        Some(raw) => Some(decode_section(raw)?),
        None => None,
    };

    let sheet_count = axes_sec.sheets.len();
    if sheet_count == 0
        || sheet_count > MAX_SHEETS
        || strings_sec.sheets.len() != sheet_count
        || columns_sec.sheets.len() != sheet_count
    {
        return Err(SnapshotError::Validation);
    }

    let allocator = IdAllocator::new(
        axes_sec.allocator_epoch,
        axes_sec.allocator_replica,
        axes_sec.allocator_next_counter,
    )
    .ok_or(SnapshotError::Validation)?;

    let mut sheets = Vec::with_capacity(sheet_count);
    let mut running_bytes = 0usize;

    for ((axes_wire, strings_wire), cols_wire) in axes_sec
        .sheets
        .into_iter()
        .zip(strings_sec.sheets)
        .zip(columns_sec.sheets)
    {
        let strings =
            StringTable::from_raw(strings_wire.bytes, strings_wire.ends, MAX_CELL_TEXT_UTF16)
                .ok_or(SnapshotError::Validation)?;
        let string_bound = u32::try_from(strings.len()).map_err(|_| SnapshotError::Validation)?;

        let row_slots = SlotMap::from_parts(axes_wire.row_slot_base, axes_wire.row_slot_extra)
            .ok_or(SnapshotError::Validation)?;
        let col_slots = SlotMap::from_parts(axes_wire.col_slot_base, axes_wire.col_slot_extra)
            .ok_or(SnapshotError::Validation)?;

        if cols_wire.len() > col_slots.slot_bound() as usize
            || axes_wire.col_formats.len() > col_slots.slot_bound() as usize
        {
            return Err(SnapshotError::Validation);
        }

        let mut columns = Vec::with_capacity(cols_wire.len());
        for col_wire in cols_wire {
            if col_wire.len > row_slots.slot_bound() {
                return Err(SnapshotError::Validation);
            }
            let raw_blocks = decompress_bounded(&col_wire.lz4_blocks)?;
            let blocks: Vec<Block> =
                postcard::from_bytes(&raw_blocks).map_err(|_| SnapshotError::Codec)?;
            let col = Column::try_from_blocks(blocks, col_wire.len, |bits| {
                validate_cell_word(bits, string_bound)
            })
            .ok_or(SnapshotError::Validation)?;
            columns.push(col);
        }

        let mut cell_formats = BTreeMap::new();
        for (r, c, fmt) in axes_wire.cell_formats {
            if cell_formats.insert((r, c), fmt).is_some() {
                return Err(SnapshotError::Validation);
            }
        }

        let sheet = Sheet::from_parts(
            axes_wire.id,
            axes_wire.name,
            axes_wire.row_runs,
            axes_wire.col_runs,
            row_slots,
            col_slots,
            columns,
            axes_wire.col_formats,
            cell_formats,
            strings,
        )
        .ok_or(SnapshotError::Validation)?;

        // Every cell format override must reference existing row/col IDs.
        for &(r, c) in sheet.cell_formats().keys() {
            if !sheet.row_axis().contains_id(r.0) || !sheet.col_axis().contains_id(c.0) {
                return Err(SnapshotError::Validation);
            }
        }

        running_bytes = running_bytes.saturating_add(sheet.estimated_bytes());
        if running_bytes > MAX_WORKBOOK_BYTES {
            return Err(SnapshotError::Validation);
        }
        sheets.push(sheet);
    }

    let mut formulas: FormulaStoreBuilder<F> = FormulaStoreBuilder::new();
    for (s, r, c, f) in formulas_sec.entries {
        if formulas.insert_unique((s, r, c), f).is_err() {
            return Err(SnapshotError::Validation);
        }
    }

    let workbook = Workbook::from_parts(
        sheets,
        formulas.finish(),
        allocator,
        axes_sec.next_sheet_id,
        seed_origin,
    )
    .map_err(|_: OpError| SnapshotError::Validation)?;

    Ok((workbook, kernel_ver))
}

fn validate_cell_word(bits: u64, string_bound: u32) -> bool {
    let Some(val) = Value::try_unpack(bits) else {
        return false;
    };
    match val {
        Value::Empty | Value::Number(_) | Value::Bool(_) | Value::Error(_) => true,
        Value::Text(id) => id < string_bound,
        // Transient array handles are never persisted inside column blocks.
        Value::Array(_) => false,
    }
}

fn encode_section<T: Serialize>(value: &T) -> Result<Vec<u8>, SnapshotError> {
    let raw = postcard::to_allocvec(value).map_err(|_| SnapshotError::Codec)?;
    compress_bounded(&raw)
}

fn decode_section<T: DeserializeOwned>(compressed: &[u8]) -> Result<T, SnapshotError> {
    let raw = decompress_bounded(compressed)?;
    postcard::from_bytes(&raw).map_err(|_| SnapshotError::Codec)
}

fn compress_bounded(raw: &[u8]) -> Result<Vec<u8>, SnapshotError> {
    let raw_len = u32::try_from(raw.len()).map_err(|_| SnapshotError::Compression)?;
    let lz4 = lz4_flex::block::compress(raw);
    let mut out = Vec::with_capacity(4_usize.saturating_add(lz4.len()));
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&lz4);
    Ok(out)
}

fn decompress_bounded(input: &[u8]) -> Result<Vec<u8>, SnapshotError> {
    let expected_len = read_u32_le(input, 0)? as usize;
    if expected_len > MAX_SECTION_UNCOMPRESSED {
        return Err(SnapshotError::Validation);
    }
    let lz4_body = input.get(4..).ok_or(SnapshotError::Truncated)?;
    if expected_len == 0 {
        return if lz4_body.is_empty() {
            Ok(Vec::new())
        } else {
            let mut scratch = Vec::new();
            lz4_flex::block::decompress_into(lz4_body, &mut scratch)
                .map_err(|_| SnapshotError::Compression)?;
            Ok(scratch)
        };
    }
    // Guard against synthetic tiny payloads claiming gigantic output before
    // LZ4's own framing fails: LZ4's theoretical maximum compression ratio is
    // ~255x, so allow `compressed_len * 260 + 4096`.
    let max_plausible = lz4_body.len().saturating_mul(260).saturating_add(4096);
    if expected_len > max_plausible {
        return Err(SnapshotError::Compression);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(expected_len)
        .map_err(|_| SnapshotError::Validation)?;
    out.resize(expected_len, 0);
    let written = lz4_flex::block::decompress_into(lz4_body, &mut out)
        .map_err(|_| SnapshotError::Compression)?;
    if written != expected_len {
        return Err(SnapshotError::Compression);
    }
    Ok(out)
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, SnapshotError> {
    let slice = bytes
        .get(offset..offset.saturating_add(2))
        .ok_or(SnapshotError::Truncated)?;
    let arr: [u8; 2] = slice.try_into().map_err(|_| SnapshotError::Truncated)?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, SnapshotError> {
    let slice = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or(SnapshotError::Truncated)?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| SnapshotError::Truncated)?;
    Ok(u32::from_le_bytes(arr))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Result<u64, SnapshotError> {
    let slice = bytes
        .get(offset..offset.saturating_add(8))
        .ok_or(SnapshotError::Truncated)?;
    let arr: [u8; 8] = slice.try_into().map_err(|_| SnapshotError::Truncated)?;
    Ok(u64::from_le_bytes(arr))
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
    use alloc::vec;

    use super::{SnapshotError, compress_bounded, decode_snapshot, encode_snapshot};
    use crate::KERNEL_VERSION;
    use crate::format::NumberFormat;
    use crate::id::{ColId, IdRun, RowId, SheetId};
    use crate::ops::{
        CellEdit, CellInput, FormatPatch, FormulaMode, Op, RawFormula, ScalarValue, SeedOrigin,
        Workbook,
    };
    use crate::sample;
    use crate::store::Block;
    use crate::value::CalcError;

    #[test]
    fn workbook_with_edits_tombstones_and_formulas_round_trips_bit_for_bit() {
        let seeded = sample::generate(42, 500);
        let mut wb: Workbook = Workbook::from_sheet(
            seeded,
            Some(SeedOrigin {
                seed: 42,
                rows: 500,
                generator_version: 1,
            }),
        );
        let sheet = SheetId(0);
        let inserted = wb.allocator_mut().alloc_run(3).unwrap();
        wb.apply(Op::InsertRows {
            sheet,
            after: Some(RowId(0)),
            ids: inserted,
        })
        .unwrap();
        wb.apply(Op::DeleteRows {
            sheet,
            ids: vec![IdRun::new(2, 2).unwrap()],
        })
        .unwrap();
        wb.apply(Op::SetCells {
            sheet,
            edits: vec![
                CellEdit {
                    row: RowId(inserted.first_id),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Text(String::from("Inserted Row"))),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(inserted.first_id),
                    col: ColId(1),
                    input: CellInput::Scalar(ScalarValue::Bool(true)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(inserted.first_id),
                    col: ColId(2),
                    input: CellInput::Scalar(ScalarValue::Error(CalcError::Div0)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(inserted.first_id),
                    col: ColId(3),
                    input: CellInput::Formula {
                        formula: RawFormula {
                            source: String::from("=SUM(M2:M10)"),
                            mode: FormulaMode::Dynamic,
                        },
                        cached: Some(ScalarValue::Number(1234.56)),
                    },
                    format: FormatPatch::Set(NumberFormat::Fixed2),
                },
            ],
        })
        .unwrap();

        let expected_digest = wb.logical_digest();
        let bytes = encode_snapshot(&wb).unwrap();
        let (decoded, ver): (Workbook, String) = decode_snapshot(&bytes).unwrap();
        assert_eq!(ver, KERNEL_VERSION);
        assert_eq!(decoded.logical_digest(), expected_digest);
    }

    #[test]
    fn corrupted_magic_truncation_and_forged_nan_bits_are_rejected() {
        let wb: Workbook = Workbook::new();
        let bytes = encode_snapshot(&wb).unwrap();

        // Bad magic.
        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert_eq!(
            decode_snapshot::<RawFormula>(&bad_magic).unwrap_err(),
            SnapshotError::BadMagic
        );

        // Truncated input at every prefix below full length fails cleanly without panicking.
        for cut in [0, 3, 8, 12, 18, bytes.len() / 2, bytes.len() - 1] {
            assert!(decode_snapshot::<RawFormula>(&bytes[..cut]).is_err());
        }

        // Foreign NaN word (`0x7FF8_0002_0000_0000`) in a Sparse block must be rejected.
        let bad_blocks = vec![Block::Sparse(vec![(0, 0x7FF8_0002_0000_0000)].into())];
        let raw_bad = postcard::to_allocvec(&bad_blocks).unwrap();
        let compressed_bad = compress_bounded(&raw_bad).unwrap();
        assert!(!super::validate_cell_word(0x7FF8_0002_0000_0000, 10));
        assert!(!super::validate_cell_word(
            crate::value::Value::Text(10).pack(),
            10
        ));
        assert!(!compressed_bad.is_empty());
    }
}

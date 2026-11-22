//! `RTCLSNAP` v1 segmented snapshot container with Copy-on-Write time-sliced
//! streaming (`SnapshotEncoder`), `ReplicaWatermarks` (`D-026`), `SeedOrigin`
//! stub snapshots (`14-m1b.md` 7.1), and backward-compatible `v0` decoding
//! (`14-m1b.md` 5.5).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Bound::{Excluded, Unbounded};

use reticula_core::KERNEL_VERSION;
use reticula_core::axis::AxisRun;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdAllocator, RowId, SheetId};
use reticula_core::ops::{
    MAX_CELL_TEXT_UTF16, MAX_NAMED_RANGE_COMMENT_CHARS, MAX_NAMED_RANGES, MAX_SHEETS,
    MAX_WORKBOOK_BYTES, NamedRangeDef, NamedRangeKey, OpError, ScalarValue, SeedOrigin, Workbook,
    WorkbookCellKey, is_valid_named_range_name,
};
use reticula_core::sheet::{Sheet, SheetExtras};
use reticula_core::snapshot::{
    SNAPSHOT_MAGIC, SNAPSHOT_VERSION as SNAPSHOT_VERSION_V0, SnapshotError,
    decode_snapshot as decode_snapshot_v0,
};
use reticula_core::store::{BLOCK_ROWS, Block, Column, SlotMap};
use reticula_core::strings::StringTable;
use reticula_core::style::{CellStyle, StylePatch, StyleTable};
use reticula_core::value::Value;
use reticula_formula::ast::{Literal, MAX_FUNCTION_ARGS, MAX_NESTING_DEPTH};
use reticula_formula::binder::{BoundExpr, BoundFormula};
use reticula_formula::engine::FormulaEngine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::convert_v3::formula_uses_v3;
use crate::v1::{
    WireAxesMetricsSectionV1, WireAxisRunV1, WireCellStyleV1, WireNumberFormatV1,
    WireReplicaWatermarkV1, WireSeedOriginV1, WireSheetAxesMetricsV1, WireSheetStringsV1,
    WireSheetStylesV1, WireStringsSectionV1, WireStylesSectionV1, WireWatermarksSectionV1,
};
use crate::v3::{
    WireBoundFormulaV3, WireCommittedPermuteV3, WireFormulaEntryV3, WireNamedRangeEntryV1,
    WireNamedRangesSectionV1, WirePermuteLogSectionV1,
};

/// `RTCLSNAP` container format version introduced in M1b.
pub const SNAPSHOT_VERSION_V1: u32 = 1;

const SECTION_STRINGS_V1: u16 = 1;
const SECTION_AXES_METRICS_V1: u16 = 2;
const SECTION_STYLES_V1: u16 = 3;
const SECTION_COLUMNS_V1: u16 = 4;
const SECTION_FORMULAS_V1: u16 = 5;
const SECTION_SEED_ORIGIN_V1: u16 = 6;
const SECTION_WATERMARKS_V1: u16 = 7;
const SECTION_SEED_STUB_V1: u16 = 8;
const SECTION_FORMULAS_V3: u16 = 9;
const SECTION_NAMED_RANGES_V1: u16 = 10;
const SECTION_PERMUTE_LOG_V1: u16 = 11;

const BLOCK_TAG_EMPTY: u8 = 0;
const BLOCK_TAG_SPARSE: u8 = 1;
const BLOCK_TAG_DENSE: u8 = 2;

/// Maximum `4096`-row blocks accepted in a single decoded column LZ4 chunk (`2 MiB` raw).
pub const BLOCKS_PER_COLUMN_CHUNK: usize = 64;

/// Number of `4096`-row blocks packed into each newly encoded column LZ4 chunk
/// (`512 KiB` raw), keeping background CoW snapshot slices well within a `16 ms`
/// frame budget even during 50M-cell saves (`14-m1b.md` 8.3 B11/B12).
const ENCODE_BLOCKS_PER_COLUMN_CHUNK: usize = 16;

/// Maximum formula entries packed into a single formula LZ4 chunk on decode.
const FORMULAS_PER_CHUNK: usize = 4_096;

/// Formula entries packed per newly encoded formula LZ4 chunk.
const ENCODE_FORMULAS_PER_CHUNK: usize = 2_048;

const MAX_KERNEL_VERSION_LEN: usize = 64;
const MAX_DIRECTORY_SECTIONS: usize = 16;
const MAX_SECTION_UNCOMPRESSED: usize = 268_435_456; // 256 MiB per individual compressed frame

/// Result of decoding an `RTCLSNAP` v0 or v1 snapshot.
#[derive(Debug)]
pub struct DecodedSnapshot {
    /// Reconstructed formula engine with dependency graph ready (no redundant recalc).
    pub engine: FormulaEngine,
    /// Kernel version recorded in the snapshot header.
    pub kernel_version: String,
    /// Snapshot container version (`0` or `1`).
    pub format_version: u32,
    /// Per-replica deduplication high-water marks (`D-026`).
    pub watermarks: Vec<WireReplicaWatermarkV1>,
    /// Persisted E16 committed permutation mappings up to snapshot sequence.
    pub permutes: Vec<WireCommittedPermuteV3>,
}

/// Output of one time-sliced call to [`SnapshotEncoder::encode_step`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotStep {
    /// Internal scan step completed (e.g. string reference marking); write nothing,
    /// call `encode_step` again on the next slice.
    Progress,
    /// Write `bytes` at `offset` in the target snapshot file.
    Chunk {
        /// Byte offset from start of snapshot file.
        offset: u64,
        /// Payload bytes to write.
        bytes: Vec<u8>,
    },
    /// All sections written; write `header_bytes` at `offset = 0`, truncate file
    /// to `total_len`, and `flush()`.
    Finished {
        /// Finalised header + section directory to write at byte offset `0`.
        header_bytes: Vec<u8>,
        /// Exact total byte length of the completed snapshot file.
        total_len: u64,
    },
}

struct SheetEncodeState {
    orig_strings: StringTable,
    used_strings: Vec<bool>,
    used_string_count: usize,
    string_remap: Option<Vec<u32>>,
    compacted_strings: Option<WireSheetStringsV1>,
    columns: Vec<Column>,
}

enum EncodePhase {
    ScanStrings {
        sheet_idx: usize,
        col_idx: usize,
        block_idx: usize,
    },
    EmitMetadata,
    EmitColumnsHeader,
    EmitColumnSheetHeader {
        sheet_idx: usize,
    },
    EmitColumnStream {
        sheet_idx: usize,
        col_idx: usize,
        block_idx: usize,
    },
    EmitFormulas,
    EmitTailSections,
    Done,
}

/// Copy-on-Write incremental `RTCLSNAP` v1 encoder (`14-m1b.md` 2.3 & 5.5).
///
/// Forking a `SnapshotEncoder` clones `Arc`-backed column block pointers in
/// `< 0.2 ms` without copying cell arrays or formula ASTs. Subsequent calls to
/// [`Self::encode_step`] compress small block batches so main-thread input
/// probes stay well inside a `16 ms` frame budget even on 50M-cell workbooks.
pub struct SnapshotEncoder {
    header_len: usize,
    write_offset: u64,
    directory: Vec<(u16, u64, u64)>,
    sheets: Vec<SheetEncodeState>,
    axes_metrics: WireAxesMetricsSectionV1,
    styles: WireStylesSectionV1,
    seed_origin: Option<WireSeedOriginV1>,
    watermarks: WireWatermarksSectionV1,
    named_ranges: Vec<(Option<SheetId>, NamedRangeDef<BoundFormula>)>,
    permutes: WirePermuteLogSectionV1,
    has_v3_formulas: bool,
    /// MVCC before-image of any formula mutated on the live workbook after
    /// `SnapshotEncoder::new` was called (`None` value means formula did not
    /// exist at snapshot start).
    formula_before_image: BTreeMap<WorkbookCellKey, Option<BoundFormula>>,
    formulas_collected: Option<Vec<WireFormulaEntryV3>>,
    formula_cursor: Option<WorkbookCellKey>,
    formula_chunks: Vec<Vec<u8>>,
    total_formulas_encoded: u32,
    columns_section_start: u64,
    phase: EncodePhase,
    scratch: Vec<u8>,
}

impl SnapshotEncoder {
    /// Creates a point-in-time CoW encoder over `workbook`.
    #[must_use]
    pub fn new(workbook: &Workbook<BoundFormula>, watermarks: Vec<WireReplicaWatermarkV1>) -> Self {
        Self::new_with_permutes(workbook, watermarks, Vec::new())
    }

    /// Creates a point-in-time CoW encoder over `workbook` including persisted
    /// E16 committed permutation history.
    #[must_use]
    pub fn new_with_permutes(
        workbook: &Workbook<BoundFormula>,
        watermarks: Vec<WireReplicaWatermarkV1>,
        permutes: Vec<WireCommittedPermuteV3>,
    ) -> Self {
        let mut sheets = Vec::with_capacity(workbook.sheets().len());
        let mut axes_sheets = Vec::with_capacity(workbook.sheets().len());
        let mut style_sheets = Vec::with_capacity(workbook.sheets().len());
        let mut any_strings = false;

        for sheet in workbook.sheets() {
            let str_len = sheet.strings().len();
            if str_len > 0 {
                any_strings = true;
            }
            sheets.push(SheetEncodeState {
                orig_strings: sheet.strings().clone(),
                used_strings: vec![false; str_len],
                used_string_count: 0,
                string_remap: None,
                compacted_strings: None,
                columns: sheet.raw_columns().to_vec(),
            });

            axes_sheets.push(WireSheetAxesMetricsV1 {
                id: sheet.id().0,
                name: String::from(sheet.name()),
                deleted_count: sheet.deleted_count(),
                frozen_rows: sheet.frozen_rows(),
                frozen_cols: sheet.frozen_cols(),
                row_runs: sheet
                    .row_axis()
                    .runs()
                    .iter()
                    .map(WireAxisRunV1::from)
                    .collect(),
                col_runs: sheet
                    .col_axis()
                    .runs()
                    .iter()
                    .map(WireAxisRunV1::from)
                    .collect(),
                row_slot_base: sheet.row_slots().base_len(),
                row_slot_extra: sheet.row_slots().extra_ids().to_vec(),
                col_slot_base: sheet.col_slots().base_len(),
                col_slot_extra: sheet.col_slots().extra_ids().to_vec(),
                row_heights: sheet
                    .row_heights()
                    .iter()
                    .map(|(&r, &px)| (r.0, px))
                    .collect(),
                col_widths: sheet
                    .col_widths()
                    .iter()
                    .map(|(&c, &px)| (c.0, px))
                    .collect(),
                col_formats: sheet
                    .col_formats()
                    .iter()
                    .copied()
                    .map(WireNumberFormatV1::from)
                    .collect(),
                row_formats: sheet
                    .row_formats()
                    .iter()
                    .map(|(&r, &fmt)| (r.0, fmt.into()))
                    .collect(),
                cell_formats: sheet
                    .cell_formats()
                    .iter()
                    .map(|(&(r, c), &fmt)| (r.0, c.0, fmt.into()))
                    .collect(),
            });

            style_sheets.push(WireSheetStylesV1 {
                table: sheet
                    .styles()
                    .entries()
                    .iter()
                    .copied()
                    .map(WireCellStyleV1::from)
                    .collect(),
                col_styles: sheet
                    .col_styles()
                    .iter()
                    .map(|(&c, &p)| (c.0, p.into()))
                    .collect(),
                row_styles: sheet
                    .row_styles()
                    .iter()
                    .map(|(&r, &p)| (r.0, p.into()))
                    .collect(),
                cell_styles: sheet
                    .cell_styles()
                    .iter()
                    .map(|(&(r, c), &p)| (r.0, c.0, p.into()))
                    .collect(),
            });
        }

        let named_ranges: Vec<(Option<SheetId>, NamedRangeDef<BoundFormula>)> = workbook
            .named_ranges()
            .iter()
            .map(|(k, v)| (k.scope, v.clone()))
            .collect();
        let has_v3_formulas = workbook.formulas().values().any(formula_uses_v3);
        let seed_origin = workbook.seed_origin().map(WireSeedOriginV1::from);
        let section_count: usize = 6_usize
            .saturating_add(usize::from(seed_origin.is_some()))
            .saturating_add(usize::from(!named_ranges.is_empty()))
            .saturating_add(usize::from(!permutes.is_empty()));
        let header_len = compute_header_len(KERNEL_VERSION.len(), section_count);

        let initial_phase = if any_strings {
            EncodePhase::ScanStrings {
                sheet_idx: 0,
                col_idx: 0,
                block_idx: 0,
            }
        } else {
            for st in &mut sheets {
                st.compacted_strings = Some(WireSheetStringsV1 {
                    bytes: Vec::new(),
                    ends: Vec::new(),
                });
            }
            EncodePhase::EmitMetadata
        };

        Self {
            header_len,
            write_offset: header_len as u64,
            directory: Vec::with_capacity(section_count),
            sheets,
            axes_metrics: WireAxesMetricsSectionV1 {
                allocator_epoch: workbook.allocator().epoch(),
                allocator_replica: workbook.allocator().replica(),
                allocator_next_counter: workbook.allocator().next_counter(),
                next_sheet_id: workbook.next_sheet_id(),
                sheets: axes_sheets,
            },
            styles: WireStylesSectionV1 {
                sheets: style_sheets,
            },
            seed_origin,
            watermarks: WireWatermarksSectionV1 {
                entries: watermarks,
            },
            named_ranges,
            permutes: WirePermuteLogSectionV1 { entries: permutes },
            has_v3_formulas,
            formula_before_image: BTreeMap::new(),
            formulas_collected: None,
            formula_cursor: None,
            formula_chunks: Vec::new(),
            total_formulas_encoded: 0,
            columns_section_start: 0,
            phase: initial_phase,
            scratch: Vec::new(),
        }
    }

    /// Records the pre-edit formula (`Some(prev)` or `None`) of `key` before the
    /// live workbook mutates `key` while this snapshot is still in flight.
    /// Subsequent edits to the same `key` during the same snapshot are ignored
    /// so the initial point-in-time value is preserved.
    pub fn record_formula_before_edit(
        &mut self,
        key: WorkbookCellKey,
        prior: Option<BoundFormula>,
    ) {
        self.formula_before_image.entry(key).or_insert(prior);
    }

    /// Whether encoding has finished completely.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        matches!(self.phase, EncodePhase::Done)
    }

    /// Advances the snapshot state machine by at most `max_blocks` column blocks
    /// (clamped to `>= 1`).
    pub fn encode_step(
        &mut self,
        live_workbook: &Workbook<BoundFormula>,
        max_blocks: usize,
    ) -> Result<SnapshotStep, SnapshotError> {
        let budget = max_blocks.max(1);
        match self.phase {
            EncodePhase::ScanStrings {
                mut sheet_idx,
                mut col_idx,
                mut block_idx,
            } => {
                // Scanning packed words is ~8x lighter than LZ4 compression.
                let mut remaining = budget.saturating_mul(8);
                while remaining > 0 && sheet_idx < self.sheets.len() {
                    let Some(st) = self.sheets.get_mut(sheet_idx) else {
                        break;
                    };
                    if st.orig_strings.is_empty() || col_idx >= st.columns.len() {
                        finalize_sheet_strings(st);
                        sheet_idx = sheet_idx.saturating_add(1);
                        col_idx = 0;
                        block_idx = 0;
                        continue;
                    }
                    let Some(col) = st.columns.get(col_idx) else {
                        col_idx = col_idx.saturating_add(1);
                        block_idx = 0;
                        continue;
                    };
                    let blocks = col.blocks();
                    if block_idx >= blocks.len() {
                        col_idx = col_idx.saturating_add(1);
                        block_idx = 0;
                        continue;
                    }
                    if let Some(blk) = blocks.get(block_idx) {
                        mark_block_strings(blk, &mut st.used_strings, &mut st.used_string_count);
                    }
                    block_idx = block_idx.saturating_add(1);
                    remaining = remaining.saturating_sub(1);
                }
                if sheet_idx >= self.sheets.len() {
                    self.phase = EncodePhase::EmitMetadata;
                } else {
                    self.phase = EncodePhase::ScanStrings {
                        sheet_idx,
                        col_idx,
                        block_idx,
                    };
                }
                Ok(SnapshotStep::Progress)
            }

            EncodePhase::EmitMetadata => {
                let mut strings_sheets = Vec::with_capacity(self.sheets.len());
                for st in &mut self.sheets {
                    if st.compacted_strings.is_none() {
                        finalize_sheet_strings(st);
                    }
                    let ws = st
                        .compacted_strings
                        .take()
                        .ok_or(SnapshotError::Validation)?;
                    strings_sheets.push(ws);
                }
                let strings_sec = WireStringsSectionV1 {
                    sheets: strings_sheets,
                };

                let strings_bytes = encode_compressed_section(&strings_sec)?;
                let axes_bytes = encode_compressed_section(&self.axes_metrics)?;
                let styles_bytes = encode_compressed_section(&self.styles)?;

                let chunk_start = self.write_offset;
                let mut combined = Vec::with_capacity(
                    strings_bytes
                        .len()
                        .saturating_add(axes_bytes.len())
                        .saturating_add(styles_bytes.len()),
                );

                self.push_dir_and_append(SECTION_STRINGS_V1, &strings_bytes, &mut combined)?;
                self.push_dir_and_append(SECTION_AXES_METRICS_V1, &axes_bytes, &mut combined)?;
                self.push_dir_and_append(SECTION_STYLES_V1, &styles_bytes, &mut combined)?;

                self.phase = EncodePhase::EmitColumnsHeader;
                Ok(SnapshotStep::Chunk {
                    offset: chunk_start,
                    bytes: combined,
                })
            }

            EncodePhase::EmitColumnsHeader => {
                self.columns_section_start = self.write_offset;
                let sheet_count =
                    u32::try_from(self.sheets.len()).map_err(|_| SnapshotError::Codec)?;
                let bytes = sheet_count.to_le_bytes().to_vec();
                let offset = self.write_offset;
                self.write_offset = self
                    .write_offset
                    .checked_add(4)
                    .ok_or(SnapshotError::Codec)?;
                self.phase = EncodePhase::EmitColumnSheetHeader { sheet_idx: 0 };
                Ok(SnapshotStep::Chunk { offset, bytes })
            }

            EncodePhase::EmitColumnSheetHeader { sheet_idx } => {
                if sheet_idx >= self.sheets.len() {
                    let sec_len = self
                        .write_offset
                        .checked_sub(self.columns_section_start)
                        .ok_or(SnapshotError::Codec)?;
                    self.directory
                        .push((SECTION_COLUMNS_V1, self.columns_section_start, sec_len));
                    self.phase = EncodePhase::EmitFormulas;
                    return self.encode_step(live_workbook, budget);
                }
                let col_count = self.sheets.get(sheet_idx).map_or(0, |st| st.columns.len());
                let col_count_u32 = u32::try_from(col_count).map_err(|_| SnapshotError::Codec)?;
                let offset = self.write_offset;
                self.write_offset = self
                    .write_offset
                    .checked_add(4)
                    .ok_or(SnapshotError::Codec)?;
                self.phase = EncodePhase::EmitColumnStream {
                    sheet_idx,
                    col_idx: 0,
                    block_idx: 0,
                };
                Ok(SnapshotStep::Chunk {
                    offset,
                    bytes: col_count_u32.to_le_bytes().to_vec(),
                })
            }

            EncodePhase::EmitColumnStream {
                sheet_idx,
                mut col_idx,
                mut block_idx,
            } => {
                let mut blocks_left = budget;
                let mut out = Vec::new();
                let start_offset = self.write_offset;

                while blocks_left > 0 && sheet_idx < self.sheets.len() {
                    let Some(st) = self.sheets.get(sheet_idx) else {
                        break;
                    };
                    if col_idx >= st.columns.len() {
                        break;
                    }
                    let Some(col) = st.columns.get(col_idx) else {
                        break;
                    };
                    let blocks = col.blocks();
                    if block_idx == 0 {
                        let col_len = col.len();
                        let total_blocks =
                            u32::try_from(blocks.len()).map_err(|_| SnapshotError::Codec)?;
                        let chunk_count =
                            u32::try_from(blocks.len().div_ceil(ENCODE_BLOCKS_PER_COLUMN_CHUNK))
                                .map_err(|_| SnapshotError::Codec)?;
                        out.extend_from_slice(&col_len.to_le_bytes());
                        out.extend_from_slice(&total_blocks.to_le_bytes());
                        out.extend_from_slice(&chunk_count.to_le_bytes());
                    }
                    if blocks.is_empty() {
                        col_idx = col_idx.saturating_add(1);
                        block_idx = 0;
                        continue;
                    }

                    // Always align chunk boundaries on multiples of `ENCODE_BLOCKS_PER_COLUMN_CHUNK`
                    // so chunk_count matches exact framing regardless of `max_blocks`!
                    let rem_in_col = blocks.len().saturating_sub(block_idx);
                    let take = rem_in_col.min(ENCODE_BLOCKS_PER_COLUMN_CHUNK);
                    let slice = blocks
                        .get(block_idx..block_idx.saturating_add(take))
                        .ok_or(SnapshotError::Codec)?;
                    encode_block_chunk(
                        slice,
                        st.string_remap.as_deref(),
                        &mut self.scratch,
                        &mut out,
                    )?;

                    block_idx = block_idx.saturating_add(take);
                    blocks_left = blocks_left.saturating_sub(take);
                    if block_idx >= blocks.len() {
                        col_idx = col_idx.saturating_add(1);
                        block_idx = 0;
                    }
                }

                let advanced_sheet = self
                    .sheets
                    .get(sheet_idx)
                    .is_none_or(|st| col_idx >= st.columns.len());
                if advanced_sheet {
                    self.phase = EncodePhase::EmitColumnSheetHeader {
                        sheet_idx: sheet_idx.saturating_add(1),
                    };
                } else {
                    self.phase = EncodePhase::EmitColumnStream {
                        sheet_idx,
                        col_idx,
                        block_idx,
                    };
                }

                if out.is_empty() {
                    return self.encode_step(live_workbook, budget);
                }

                let len_u64 = u64::try_from(out.len()).map_err(|_| SnapshotError::Codec)?;
                self.write_offset = self
                    .write_offset
                    .checked_add(len_u64)
                    .ok_or(SnapshotError::Codec)?;
                Ok(SnapshotStep::Chunk {
                    offset: start_offset,
                    bytes: out,
                })
            }

            EncodePhase::EmitFormulas => {
                // Collect up to `ENCODE_FORMULAS_PER_CHUNK` point-in-time formulas per step!
                let batch = self.next_formula_batch(live_workbook, ENCODE_FORMULAS_PER_CHUNK)?;
                if !batch.is_empty() {
                    let count_u32 = u32::try_from(batch.len()).map_err(|_| SnapshotError::Codec)?;
                    self.total_formulas_encoded = self
                        .total_formulas_encoded
                        .checked_add(count_u32)
                        .ok_or(SnapshotError::Codec)?;
                    let raw = postcard::to_allocvec(&batch).map_err(|_| SnapshotError::Codec)?;
                    let raw_len = u32::try_from(raw.len()).map_err(|_| SnapshotError::Codec)?;
                    let lz4 = lz4_flex::block::compress(&raw);
                    let comp_len = u32::try_from(lz4.len()).map_err(|_| SnapshotError::Codec)?;
                    let mut frame = Vec::with_capacity(8_usize.saturating_add(lz4.len()));
                    frame.extend_from_slice(&raw_len.to_le_bytes());
                    frame.extend_from_slice(&comp_len.to_le_bytes());
                    frame.extend_from_slice(&lz4);
                    self.formula_chunks.push(frame);
                    return Ok(SnapshotStep::Progress);
                }

                // All formula chunks compressed — emit `SECTION_FORMULAS_V1` (or `V3`).
                let sec_start = self.write_offset;
                let chunk_count =
                    u32::try_from(self.formula_chunks.len()).map_err(|_| SnapshotError::Codec)?;
                let body_bytes: usize = self
                    .formula_chunks
                    .iter()
                    .fold(0usize, |acc, f| acc.saturating_add(f.len()));
                let mut out = Vec::with_capacity(8_usize.saturating_add(body_bytes));
                out.extend_from_slice(&self.total_formulas_encoded.to_le_bytes());
                out.extend_from_slice(&chunk_count.to_le_bytes());
                for frame in core::mem::take(&mut self.formula_chunks) {
                    out.extend_from_slice(&frame);
                }
                let len_u64 = u64::try_from(out.len()).map_err(|_| SnapshotError::Codec)?;
                let formula_sec_id = if self.has_v3_formulas {
                    SECTION_FORMULAS_V3
                } else {
                    SECTION_FORMULAS_V1
                };
                self.directory.push((formula_sec_id, sec_start, len_u64));
                self.write_offset = self
                    .write_offset
                    .checked_add(len_u64)
                    .ok_or(SnapshotError::Codec)?;
                self.phase = EncodePhase::EmitTailSections;
                Ok(SnapshotStep::Chunk {
                    offset: sec_start,
                    bytes: out,
                })
            }

            EncodePhase::EmitTailSections => {
                let start = self.write_offset;
                let mut out = Vec::new();
                if let Some(origin) = &self.seed_origin {
                    let origin_bytes = encode_compressed_section(origin)?;
                    self.push_dir_and_append(SECTION_SEED_ORIGIN_V1, &origin_bytes, &mut out)?;
                }
                let wm_bytes = encode_compressed_section(&self.watermarks)?;
                self.push_dir_and_append(SECTION_WATERMARKS_V1, &wm_bytes, &mut out)?;
                if !self.named_ranges.is_empty() {
                    let mut entries = Vec::with_capacity(self.named_ranges.len());
                    for (scope, def) in &self.named_ranges {
                        entries.push(WireNamedRangeEntryV1 {
                            scope: scope.map(|s| s.0),
                            name: def.name.clone(),
                            formula: WireBoundFormulaV3::try_from(&def.formula)
                                .map_err(|_| SnapshotError::Codec)?,
                            comment: def.comment.clone(),
                            hidden: def.hidden,
                        });
                    }
                    let nr_sec = WireNamedRangesSectionV1 { entries };
                    let nr_bytes = encode_compressed_section(&nr_sec)?;
                    self.push_dir_and_append(SECTION_NAMED_RANGES_V1, &nr_bytes, &mut out)?;
                }
                if !self.permutes.entries.is_empty() {
                    let perm_bytes = encode_compressed_section(&self.permutes)?;
                    self.push_dir_and_append(SECTION_PERMUTE_LOG_V1, &perm_bytes, &mut out)?;
                }

                self.phase = EncodePhase::Done;
                Ok(SnapshotStep::Chunk {
                    offset: start,
                    bytes: out,
                })
            }

            EncodePhase::Done => {
                let header_bytes = build_header_bytes(SNAPSHOT_VERSION_V1, &self.directory)?;
                if header_bytes.len() != self.header_len {
                    return Err(SnapshotError::InvalidDirectory);
                }
                Ok(SnapshotStep::Finished {
                    header_bytes,
                    total_len: self.write_offset,
                })
            }
        }
    }

    fn push_dir_and_append(
        &mut self,
        kind: u16,
        payload: &[u8],
        combined: &mut Vec<u8>,
    ) -> Result<(), SnapshotError> {
        let len_u64 = u64::try_from(payload.len()).map_err(|_| SnapshotError::Codec)?;
        let offset = self.write_offset;
        self.write_offset = self
            .write_offset
            .checked_add(len_u64)
            .ok_or(SnapshotError::Codec)?;
        self.directory.push((kind, offset, len_u64));
        combined.extend_from_slice(payload);
        Ok(())
    }

    fn next_formula_batch(
        &mut self,
        live_workbook: &Workbook<BoundFormula>,
        limit: usize,
    ) -> Result<Vec<WireFormulaEntryV3>, SnapshotError> {
        if self.formula_before_image.is_empty() {
            // Fast path (no formulas edited while snapshot in flight): stream
            // directly from `live_workbook.formulas()` with zero allocations!
            let iter = match self.formula_cursor {
                None => live_workbook.formulas().range(..),
                Some(last) => live_workbook.formulas().range((Excluded(last), Unbounded)),
            };
            let mut batch = Vec::new();
            for (&key, f) in iter.take(limit) {
                if batch.is_empty() {
                    batch.reserve(limit.min(live_workbook.formulas().len()));
                }
                self.formula_cursor = Some(key);
                self.has_v3_formulas |= formula_uses_v3(f);
                batch.push(WireFormulaEntryV3 {
                    sheet: key.0.0,
                    row: key.1.0,
                    col: key.2.0,
                    formula: WireBoundFormulaV3::try_from(f).map_err(|_| SnapshotError::Codec)?,
                });
            }
            return Ok(batch);
        }

        // Merge live `formulas()` with `formula_before_image` in sorted key order.
        if self.formulas_collected.is_none() {
            let mut merged: BTreeMap<WorkbookCellKey, (WireBoundFormulaV3, bool)> = BTreeMap::new();
            for (&k, f) in live_workbook.formulas() {
                merged.insert(
                    k,
                    (
                        WireBoundFormulaV3::try_from(f).map_err(|_| SnapshotError::Codec)?,
                        formula_uses_v3(f),
                    ),
                );
            }
            for (&k, prior) in &self.formula_before_image {
                match prior {
                    Some(f) => {
                        merged.insert(
                            k,
                            (
                                WireBoundFormulaV3::try_from(f)
                                    .map_err(|_| SnapshotError::Codec)?,
                                formula_uses_v3(f),
                            ),
                        );
                    }
                    None => {
                        merged.remove(&k);
                    }
                }
            }
            self.has_v3_formulas = merged.values().any(|&(_, is_v3)| is_v3);
            self.formulas_collected = Some(
                merged
                    .into_iter()
                    .map(|((s, r, c), (formula, _))| WireFormulaEntryV3 {
                        sheet: s.0,
                        row: r.0,
                        col: c.0,
                        formula,
                    })
                    .collect(),
            );
        }
        let Some(all) = &mut self.formulas_collected else {
            return Ok(Vec::new());
        };
        if all.is_empty() {
            return Ok(Vec::new());
        }
        let take = all.len().min(limit);
        Ok(all.drain(..take).collect())
    }
}

/// Encodes `workbook` into a complete in-memory `RTCLSNAP` buffer.
pub fn encode_snapshot_v1(
    workbook: &Workbook<BoundFormula>,
    watermarks: &[WireReplicaWatermarkV1],
) -> Result<Vec<u8>, SnapshotError> {
    encode_snapshot_with_permutes(workbook, watermarks, &[])
}

/// Encodes `workbook` together with E16 committed permutation history into an `RTCLSNAP` buffer.
pub fn encode_snapshot_with_permutes(
    workbook: &Workbook<BoundFormula>,
    watermarks: &[WireReplicaWatermarkV1],
    permutes: &[WireCommittedPermuteV3],
) -> Result<Vec<u8>, SnapshotError> {
    let mut enc =
        SnapshotEncoder::new_with_permutes(workbook, watermarks.to_vec(), permutes.to_vec());
    let mut out = vec![0_u8; enc.header_len];
    loop {
        match enc.encode_step(workbook, BLOCKS_PER_COLUMN_CHUNK)? {
            SnapshotStep::Progress => {}
            SnapshotStep::Chunk { offset, bytes } => {
                let pos = usize::try_from(offset).map_err(|_| SnapshotError::Codec)?;
                if pos != out.len() {
                    return Err(SnapshotError::InvalidDirectory);
                }
                out.extend_from_slice(&bytes);
            }
            SnapshotStep::Finished {
                header_bytes,
                total_len,
            } => {
                let expected = usize::try_from(total_len).map_err(|_| SnapshotError::Codec)?;
                if out.len() != expected || header_bytes.len() > out.len() {
                    return Err(SnapshotError::InvalidDirectory);
                }
                if let Some(prefix) = out.get_mut(..header_bytes.len()) {
                    prefix.copy_from_slice(&header_bytes);
                }
                break;
            }
        }
    }
    Ok(out)
}

/// Encodes an ~120-byte `SeedOrigin` stub snapshot (`14-m1b.md` 7.1) for
/// instant persistence of the default 1M-row sample without writing 160 MB of
/// deterministic base columns.
pub fn encode_seed_stub_v1(
    origin: SeedOrigin,
    watermarks: &[WireReplicaWatermarkV1],
) -> Result<Vec<u8>, SnapshotError> {
    let wire_origin = WireSeedOriginV1::from(origin);
    let wire_wm = WireWatermarksSectionV1 {
        entries: watermarks.to_vec(),
    };
    let origin_bytes = encode_compressed_section(&wire_origin)?;
    let wm_bytes = encode_compressed_section(&wire_wm)?;

    let header_len = compute_header_len(KERNEL_VERSION.len(), 2);
    let o1 = u64::try_from(header_len).map_err(|_| SnapshotError::Codec)?;
    let l1 = u64::try_from(origin_bytes.len()).map_err(|_| SnapshotError::Codec)?;
    let o2 = o1.checked_add(l1).ok_or(SnapshotError::Codec)?;
    let l2 = u64::try_from(wm_bytes.len()).map_err(|_| SnapshotError::Codec)?;

    let directory = vec![
        (SECTION_SEED_STUB_V1, o1, l1),
        (SECTION_WATERMARKS_V1, o2, l2),
    ];
    let mut out = build_header_bytes(SNAPSHOT_VERSION_V1, &directory)?;
    out.extend_from_slice(&origin_bytes);
    out.extend_from_slice(&wm_bytes);
    Ok(out)
}

/// Decodes and validates either an `RTCLSNAP` v1 snapshot (full or `SeedOrigin`
/// stub) or a legacy M1a `RTCLSNAP` v0 snapshot into a ready [`DecodedSnapshot`].
pub fn decode_snapshot_any(bytes: &[u8]) -> Result<DecodedSnapshot, SnapshotError> {
    decode_snapshot_any_with_limits(bytes, usize::MAX, MAX_WORKBOOK_BYTES)
}

/// Decodes and validates a full `RTCLSNAP` v0 or v1 snapshot directly into a
/// raw [`Workbook`] without instantiating [`FormulaEngine`], rejecting
/// `SeedOrigin` stub snapshots with [`SnapshotError::UnsupportedVersion`].
pub fn decode_snapshot_workbook_only(
    bytes: &[u8],
) -> Result<Workbook<BoundFormula>, SnapshotError> {
    let (payload, _, _, _, _) =
        decode_snapshot_raw_with_limits(bytes, usize::MAX, MAX_WORKBOOK_BYTES)?;
    match payload {
        RawSnapshotPayload::Workbook(wb) => Ok(wb),
        RawSnapshotPayload::SeedStub(_) => Err(SnapshotError::UnsupportedVersion),
    }
}

/// Decodes and validates a snapshot for collaborative import, enforcing cell
/// and estimated memory limits before allocating or expanding `SeedOrigin` stubs.
pub fn decode_snapshot_for_collab(
    bytes: &[u8],
    max_cells: usize,
    max_bytes: usize,
) -> Result<DecodedSnapshot, SnapshotError> {
    decode_snapshot_any_with_limits(bytes, max_cells, max_bytes.min(MAX_WORKBOOK_BYTES))
}

enum RawSnapshotPayload {
    Workbook(Workbook<BoundFormula>),
    SeedStub(WireSeedOriginV1),
}

fn decode_snapshot_any_with_limits(
    bytes: &[u8],
    max_cells: usize,
    max_bytes: usize,
) -> Result<DecodedSnapshot, SnapshotError> {
    let (payload, kernel_version, format_version, watermarks, permutes) =
        decode_snapshot_raw_with_limits(bytes, max_cells, max_bytes)?;
    let engine = match payload {
        RawSnapshotPayload::Workbook(wb) => FormulaEngine::from_workbook_clean(wb),
        RawSnapshotPayload::SeedStub(wire_origin) => {
            let engine = FormulaEngine::open_sample(wire_origin.seed, wire_origin.rows);
            if engine.workbook().populated_cell_count() > max_cells as u64
                || engine.workbook().estimated_bytes() > max_bytes
            {
                return Err(SnapshotError::Validation);
            }
            engine
        }
    };
    Ok(DecodedSnapshot {
        engine,
        kernel_version,
        format_version,
        watermarks,
        permutes,
    })
}

#[expect(
    clippy::too_many_lines,
    clippy::type_complexity,
    reason = "sequential snapshot container validation and pre-allocation checks"
)]
fn decode_snapshot_raw_with_limits(
    bytes: &[u8],
    max_cells: usize,
    max_bytes: usize,
) -> Result<
    (
        RawSnapshotPayload,
        String,
        u32,
        Vec<WireReplicaWatermarkV1>,
        Vec<WireCommittedPermuteV3>,
    ),
    SnapshotError,
> {
    let section_cap = max_bytes
        .saturating_mul(2)
        .clamp(1024 * 1024, MAX_SECTION_UNCOMPRESSED);
    let magic = bytes.get(0..8).ok_or(SnapshotError::Truncated)?;
    if magic != SNAPSHOT_MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = read_u32_le(bytes, 8)?;
    if version == SNAPSHOT_VERSION_V0 {
        if bytes.len() > max_bytes.saturating_mul(2).max(1024 * 1024) {
            return Err(SnapshotError::Validation);
        }
        let (wb, kernel_version): (Workbook<BoundFormula>, String) = decode_snapshot_v0(bytes)?;
        if wb.populated_cell_count() > max_cells as u64 || wb.estimated_bytes() > max_bytes {
            return Err(SnapshotError::Validation);
        }
        return Ok((
            RawSnapshotPayload::Workbook(wb),
            kernel_version,
            SNAPSHOT_VERSION_V0,
            Vec::new(),
            Vec::new(),
        ));
    }
    if version != SNAPSHOT_VERSION_V1 {
        return Err(SnapshotError::UnsupportedVersion);
    }

    let kernel_len = usize::from(read_u16_le(bytes, 12)?);
    if kernel_len > MAX_KERNEL_VERSION_LEN {
        return Err(SnapshotError::InvalidDirectory);
    }
    let kernel_end = 14_usize.saturating_add(kernel_len);
    let kernel_slice = bytes.get(14..kernel_end).ok_or(SnapshotError::Truncated)?;
    let kernel_version = core::str::from_utf8(kernel_slice)
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

    let watermarks = match dir.get(&SECTION_WATERMARKS_V1) {
        Some(raw) => {
            decode_compressed_section::<WireWatermarksSectionV1>(raw, section_cap)?.entries
        }
        None => Vec::new(),
    };

    // Fast path: `SeedOrigin` stub snapshot (~120 bytes).
    if let Some(stub_raw) = dir.get(&SECTION_SEED_STUB_V1) {
        if dir.contains_key(&SECTION_COLUMNS_V1) {
            return Err(SnapshotError::InvalidDirectory);
        }
        let wire_origin: WireSeedOriginV1 = decode_compressed_section(stub_raw, section_cap)?;
        if wire_origin.rows > reticula_core::axis::MAX_ROW_AXIS_LEN {
            return Err(SnapshotError::Validation);
        }
        let min_rows = wire_origin.rows as usize;
        // Sample rows populate at least 18 columns and allocate >=160 B/row across 20 columns.
        if min_rows.saturating_mul(18) > max_cells || min_rows.saturating_mul(160) > max_bytes {
            return Err(SnapshotError::Validation);
        }
        return Ok((
            RawSnapshotPayload::SeedStub(wire_origin),
            kernel_version,
            SNAPSHOT_VERSION_V1,
            watermarks,
            Vec::new(),
        ));
    }

    let strings_sec: WireStringsSectionV1 = decode_compressed_section(
        dir.get(&SECTION_STRINGS_V1)
            .copied()
            .ok_or(SnapshotError::MissingSection)?,
        section_cap,
    )?;
    let axes_sec: WireAxesMetricsSectionV1 = decode_compressed_section(
        dir.get(&SECTION_AXES_METRICS_V1)
            .copied()
            .ok_or(SnapshotError::MissingSection)?,
        section_cap,
    )?;
    let styles_sec: WireStylesSectionV1 = decode_compressed_section(
        dir.get(&SECTION_STYLES_V1)
            .copied()
            .ok_or(SnapshotError::MissingSection)?,
        section_cap,
    )?;
    let cols_slice = dir
        .get(&SECTION_COLUMNS_V1)
        .copied()
        .ok_or(SnapshotError::MissingSection)?;
    let (formulas_slice, allow_v3_formulas) = match (
        dir.get(&SECTION_FORMULAS_V1).copied(),
        dir.get(&SECTION_FORMULAS_V3).copied(),
    ) {
        (Some(slice), None) => (slice, false),
        (None, Some(slice)) => (slice, true),
        (Some(_), Some(_)) => return Err(SnapshotError::InvalidDirectory),
        (None, None) => return Err(SnapshotError::MissingSection),
    };
    let seed_origin: Option<SeedOrigin> = match dir.get(&SECTION_SEED_ORIGIN_V1) {
        Some(raw) => Some(decode_compressed_section::<WireSeedOriginV1>(raw, section_cap)?.into()),
        None => None,
    };
    let named_ranges_wire: Option<WireNamedRangesSectionV1> =
        match dir.get(&SECTION_NAMED_RANGES_V1) {
            Some(raw) => Some(decode_compressed_section(raw, section_cap)?),
            None => None,
        };
    let permutes: Vec<WireCommittedPermuteV3> = match dir.get(&SECTION_PERMUTE_LOG_V1) {
        Some(raw) => {
            decode_compressed_section::<WirePermuteLogSectionV1>(raw, section_cap)?.entries
        }
        None => Vec::new(),
    };

    // Pre-warm linear memory in one contiguous step so decoding large
    // snapshots on WebAssembly does not invoke `memory.grow` thousands of times.
    let formula_count_hint = read_u32_le(formulas_slice, 0).unwrap_or(0) as usize;
    if formula_count_hint > max_cells {
        return Err(SnapshotError::Validation);
    }
    let reserve_hint = cols_slice
        .len()
        .saturating_mul(5)
        .saturating_add(formula_count_hint.saturating_mul(600))
        .min(max_bytes)
        .min(768 * 1024 * 1024);
    if reserve_hint >= 4 * 1024 * 1024 {
        let _ = reticula_core::memory::try_reserve_heap(reserve_hint);
    }

    let sheet_count = axes_sec.sheets.len();
    if sheet_count == 0
        || sheet_count > MAX_SHEETS
        || strings_sec.sheets.len() != sheet_count
        || styles_sec.sheets.len() != sheet_count
    {
        return Err(SnapshotError::Validation);
    }

    let allocator = IdAllocator::new(
        axes_sec.allocator_epoch,
        axes_sec.allocator_replica,
        axes_sec.allocator_next_counter,
    )
    .ok_or(SnapshotError::Validation)?;

    let cols_sheet_count =
        usize::try_from(read_u32_le(cols_slice, 0)?).map_err(|_| SnapshotError::Validation)?;
    if cols_sheet_count != sheet_count {
        return Err(SnapshotError::Validation);
    }
    let mut cols_cursor = 4_usize;
    let mut decode_scratch = Vec::new();

    let mut sheets = Vec::with_capacity(sheet_count);
    let mut running_bytes = 0usize;
    let mut running_cells = 0usize;

    for ((axes_w, strings_w), styles_w) in axes_sec
        .sheets
        .into_iter()
        .zip(strings_sec.sheets)
        .zip(styles_sec.sheets)
    {
        let strings = StringTable::from_raw(strings_w.bytes, strings_w.ends, MAX_CELL_TEXT_UTF16)
            .ok_or(SnapshotError::Validation)?;
        let string_bound = u32::try_from(strings.len()).map_err(|_| SnapshotError::Validation)?;

        let row_slots = SlotMap::from_parts(axes_w.row_slot_base, axes_w.row_slot_extra)
            .ok_or(SnapshotError::Validation)?;
        let col_slots = SlotMap::from_parts(axes_w.col_slot_base, axes_w.col_slot_extra)
            .ok_or(SnapshotError::Validation)?;

        let col_count = usize::try_from(read_u32_le(cols_slice, cols_cursor)?)
            .map_err(|_| SnapshotError::Validation)?;
        cols_cursor = cols_cursor.saturating_add(4);

        if col_count > col_slots.slot_bound() as usize
            || axes_w.col_formats.len() > col_slots.slot_bound() as usize
        {
            return Err(SnapshotError::Validation);
        }

        let mut columns = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            let col = decode_one_column(
                cols_slice,
                &mut cols_cursor,
                row_slots.slot_bound(),
                string_bound,
                &mut decode_scratch,
                section_cap,
                &mut running_cells,
                max_cells,
            )?;
            columns.push(col);
        }

        let row_runs: Vec<AxisRun> = axes_w
            .row_runs
            .into_iter()
            .map(AxisRun::try_from)
            .collect::<Result<_, _>>()
            .map_err(|_| SnapshotError::Validation)?;
        let col_runs: Vec<AxisRun> = axes_w
            .col_runs
            .into_iter()
            .map(AxisRun::try_from)
            .collect::<Result<_, _>>()
            .map_err(|_| SnapshotError::Validation)?;

        let col_formats: Vec<NumberFormat> = axes_w
            .col_formats
            .into_iter()
            .map(NumberFormat::try_from)
            .collect::<Result<_, _>>()
            .map_err(|_| SnapshotError::Validation)?;

        let mut row_formats = BTreeMap::new();
        for (r, wf) in axes_w.row_formats {
            let fmt = NumberFormat::try_from(wf).map_err(|_| SnapshotError::Validation)?;
            if row_formats.insert(RowId(r), fmt).is_some() {
                return Err(SnapshotError::Validation);
            }
        }

        let mut cell_formats = BTreeMap::new();
        for (r, c, wf) in axes_w.cell_formats {
            let fmt = NumberFormat::try_from(wf).map_err(|_| SnapshotError::Validation)?;
            if cell_formats.insert((RowId(r), ColId(c)), fmt).is_some() {
                return Err(SnapshotError::Validation);
            }
        }

        let mut row_heights = BTreeMap::new();
        for (r, px) in axes_w.row_heights {
            if row_heights.insert(RowId(r), px).is_some() {
                return Err(SnapshotError::Validation);
            }
        }
        let mut col_widths = BTreeMap::new();
        for (c, px) in axes_w.col_widths {
            if col_widths.insert(ColId(c), px).is_some() {
                return Err(SnapshotError::Validation);
            }
        }

        let style_entries: Vec<CellStyle> = styles_w
            .table
            .into_iter()
            .map(CellStyle::try_from)
            .collect::<Result<_, _>>()
            .map_err(|_| SnapshotError::Validation)?;
        let style_table =
            StyleTable::from_entries(style_entries).ok_or(SnapshotError::Validation)?;

        let mut col_styles = BTreeMap::new();
        for (c, wp) in styles_w.col_styles {
            let patch = StylePatch::try_from(wp).map_err(|_| SnapshotError::Validation)?;
            if col_styles.insert(ColId(c), patch).is_some() {
                return Err(SnapshotError::Validation);
            }
        }
        let mut row_styles = BTreeMap::new();
        for (r, wp) in styles_w.row_styles {
            let patch = StylePatch::try_from(wp).map_err(|_| SnapshotError::Validation)?;
            if row_styles.insert(RowId(r), patch).is_some() {
                return Err(SnapshotError::Validation);
            }
        }
        let mut cell_styles = BTreeMap::new();
        for (r, c, wp) in styles_w.cell_styles {
            let patch = StylePatch::try_from(wp).map_err(|_| SnapshotError::Validation)?;
            if cell_styles.insert((RowId(r), ColId(c)), patch).is_some() {
                return Err(SnapshotError::Validation);
            }
        }

        let extras = SheetExtras {
            deleted_count: axes_w.deleted_count,
            frozen_rows: axes_w.frozen_rows,
            frozen_cols: axes_w.frozen_cols,
            row_heights,
            col_widths,
            row_formats,
            col_styles,
            row_styles,
            cell_styles,
            styles: style_table,
        };

        let sheet = Sheet::from_parts_with_extras(
            SheetId(axes_w.id),
            axes_w.name,
            row_runs,
            col_runs,
            row_slots,
            col_slots,
            columns,
            col_formats,
            cell_formats,
            strings,
            extras,
        )
        .ok_or(SnapshotError::Validation)?;

        for &(r, c) in sheet.cell_formats().keys() {
            if !sheet.row_axis().contains_id(r.0) || !sheet.col_axis().contains_id(c.0) {
                return Err(SnapshotError::Validation);
            }
        }

        running_bytes = running_bytes.saturating_add(sheet.estimated_bytes());
        if running_bytes > max_bytes {
            return Err(SnapshotError::Validation);
        }
        sheets.push(sheet);
    }

    if cols_cursor != cols_slice.len() {
        return Err(SnapshotError::InvalidDirectory);
    }

    let formulas = decode_formulas_section(
        formulas_slice,
        &mut decode_scratch,
        running_bytes,
        max_bytes,
        section_cap,
        allow_v3_formulas,
    )?;

    let mut named_ranges: BTreeMap<NamedRangeKey, NamedRangeDef<BoundFormula>> = BTreeMap::new();
    if let Some(nr_sec) = named_ranges_wire {
        if nr_sec.entries.len() > MAX_NAMED_RANGES {
            return Err(SnapshotError::Validation);
        }
        for entry in nr_sec.entries {
            if !is_valid_named_range_name(&entry.name) {
                return Err(SnapshotError::Validation);
            }
            if let Some(ref comment) = entry.comment
                && comment.chars().count() > MAX_NAMED_RANGE_COMMENT_CHARS
            {
                return Err(SnapshotError::Validation);
            }
            let formula =
                BoundFormula::try_from(entry.formula).map_err(|_| SnapshotError::Validation)?;
            let scope = entry.scope.map(SheetId);
            let key = NamedRangeKey::new(scope, &entry.name);
            let def = NamedRangeDef {
                name: entry.name,
                formula,
                comment: entry.comment,
                hidden: entry.hidden,
            };
            if named_ranges.insert(key, def).is_some() {
                return Err(SnapshotError::Validation);
            }
        }
    }

    let workbook = Workbook::from_parts_with_names(
        sheets,
        formulas,
        named_ranges,
        allocator,
        axes_sec.next_sheet_id,
        seed_origin,
    )
    .map_err(|_: OpError| SnapshotError::Validation)?;

    if workbook.populated_cell_count() > max_cells as u64 || workbook.estimated_bytes() > max_bytes
    {
        return Err(SnapshotError::Validation);
    }

    Ok((
        RawSnapshotPayload::Workbook(workbook),
        kernel_version,
        SNAPSHOT_VERSION_V1,
        watermarks,
        permutes,
    ))
}

// ------------------------------------------------------------ internal helpers

fn compute_header_len(kernel_ver_len: usize, section_count: usize) -> usize {
    16_usize
        .saturating_add(kernel_ver_len)
        .saturating_add(section_count.saturating_mul(18))
}

fn build_header_bytes(
    version: u32,
    directory: &[(u16, u64, u64)],
) -> Result<Vec<u8>, SnapshotError> {
    let kernel_bytes = KERNEL_VERSION.as_bytes();
    let kernel_len = u16::try_from(kernel_bytes.len()).map_err(|_| SnapshotError::Codec)?;
    let section_count = u16::try_from(directory.len()).map_err(|_| SnapshotError::Codec)?;
    let total = compute_header_len(kernel_bytes.len(), directory.len());
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&kernel_len.to_le_bytes());
    out.extend_from_slice(kernel_bytes);
    out.extend_from_slice(&section_count.to_le_bytes());
    for &(kind, offset, length) in directory {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
    }
    Ok(out)
}

fn mark_block_strings(blk: &Block, used: &mut [bool], used_count: &mut usize) {
    match blk {
        Block::Empty => {}
        Block::Sparse(entries) => {
            for &(_, bits) in entries.iter() {
                if let Value::Text(id) = Value::unpack(bits)
                    && let Some(flag) = used.get_mut(id as usize)
                    && !*flag
                {
                    *flag = true;
                    *used_count = used_count.saturating_add(1);
                }
            }
        }
        Block::Dense(words) => {
            for &bits in words.iter() {
                if bits != Value::EMPTY_BITS
                    && let Value::Text(id) = Value::unpack(bits)
                    && let Some(flag) = used.get_mut(id as usize)
                    && !*flag
                {
                    *flag = true;
                    *used_count = used_count.saturating_add(1);
                }
            }
        }
    }
}

fn finalize_sheet_strings(st: &mut SheetEncodeState) {
    let total = st.orig_strings.len();
    if st.used_string_count == total {
        st.string_remap = None;
        st.compacted_strings = Some(WireSheetStringsV1 {
            bytes: st.orig_strings.raw_bytes().to_vec(),
            ends: st.orig_strings.raw_ends().to_vec(),
        });
        st.used_strings = Vec::new();
        return;
    }

    let mut new_table =
        StringTable::with_capacity(st.used_string_count, st.orig_strings.raw_bytes().len());
    let mut remap = vec![0_u32; total];
    for (old_idx, &is_used) in st.used_strings.iter().enumerate() {
        if is_used
            && let Ok(old_id) = u32::try_from(old_idx)
            && let Some(new_id) = new_table.intern(st.orig_strings.get(old_id))
            && let Some(slot) = remap.get_mut(old_idx)
        {
            *slot = new_id;
        }
    }
    st.string_remap = Some(remap);
    st.compacted_strings = Some(WireSheetStringsV1 {
        bytes: new_table.raw_bytes().to_vec(),
        ends: new_table.raw_ends().to_vec(),
    });
    st.used_strings = Vec::new();
}

#[inline]
fn remap_word_if_needed(bits: u64, remap: Option<&[u32]>) -> u64 {
    let Some(map) = remap else {
        return bits;
    };
    if bits == Value::EMPTY_BITS {
        return bits;
    }
    if let Value::Text(old_id) = Value::unpack(bits)
        && let Some(&new_id) = map.get(old_id as usize)
    {
        return Value::Text(new_id).pack();
    }
    bits
}

fn encode_block_chunk(
    blocks: &[Block],
    remap: Option<&[u32]>,
    scratch: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(), SnapshotError> {
    scratch.clear();
    let count_u16 = u16::try_from(blocks.len()).map_err(|_| SnapshotError::Codec)?;
    for blk in blocks {
        match blk {
            Block::Empty => scratch.push(BLOCK_TAG_EMPTY),
            Block::Sparse(entries) => {
                scratch.push(BLOCK_TAG_SPARSE);
                let len_u16 = u16::try_from(entries.len()).map_err(|_| SnapshotError::Codec)?;
                scratch.extend_from_slice(&len_u16.to_le_bytes());
                for &(idx, bits) in entries.iter() {
                    let mapped = remap_word_if_needed(bits, remap);
                    scratch.extend_from_slice(&idx.to_le_bytes());
                    scratch.extend_from_slice(&mapped.to_le_bytes());
                }
            }
            Block::Dense(words) => {
                scratch.push(BLOCK_TAG_DENSE);
                scratch.reserve((BLOCK_ROWS as usize).saturating_mul(8));
                if remap.is_none() {
                    for &bits in words.iter() {
                        scratch.extend_from_slice(&bits.to_le_bytes());
                    }
                } else {
                    for &bits in words.iter() {
                        let mapped = remap_word_if_needed(bits, remap);
                        scratch.extend_from_slice(&mapped.to_le_bytes());
                    }
                }
            }
        }
    }
    let raw_len = u32::try_from(scratch.len()).map_err(|_| SnapshotError::Compression)?;
    let lz4 = lz4_flex::block::compress(scratch);
    let comp_len = u32::try_from(lz4.len()).map_err(|_| SnapshotError::Compression)?;
    out.extend_from_slice(&count_u16.to_le_bytes());
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&comp_len.to_le_bytes());
    out.extend_from_slice(&lz4);
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "streaming column decode enforces cell limits incrementally"
)]
fn decode_one_column(
    bytes: &[u8],
    cursor: &mut usize,
    max_row_slot: u32,
    string_bound: u32,
    scratch: &mut Vec<u8>,
    section_cap: usize,
    running_cells: &mut usize,
    max_cells: usize,
) -> Result<Column, SnapshotError> {
    let col_len = read_u32_le(bytes, *cursor)?;
    let total_blocks = usize::try_from(read_u32_le(bytes, cursor.saturating_add(4))?)
        .map_err(|_| SnapshotError::Validation)?;
    let chunk_count = usize::try_from(read_u32_le(bytes, cursor.saturating_add(8))?)
        .map_err(|_| SnapshotError::Validation)?;
    *cursor = cursor.saturating_add(12);

    if col_len > max_row_slot {
        return Err(SnapshotError::Validation);
    }
    let max_blocks = (max_row_slot.div_ceil(BLOCK_ROWS)) as usize;
    if total_blocks > max_blocks || chunk_count > total_blocks {
        return Err(SnapshotError::Validation);
    }
    if total_blocks == 0 {
        if chunk_count != 0 {
            return Err(SnapshotError::Validation);
        }
        return Column::try_from_blocks(Vec::new(), col_len, |_| true)
            .ok_or(SnapshotError::Validation);
    }

    let mut blocks = Vec::with_capacity(total_blocks);
    for _ in 0..chunk_count {
        let block_count = usize::from(read_u16_le(bytes, *cursor)?);
        let raw_len = usize::try_from(read_u32_le(bytes, cursor.saturating_add(2))?)
            .map_err(|_| SnapshotError::Validation)?;
        let comp_len = usize::try_from(read_u32_le(bytes, cursor.saturating_add(6))?)
            .map_err(|_| SnapshotError::Validation)?;
        *cursor = cursor.saturating_add(10);

        if block_count == 0
            || block_count > BLOCKS_PER_COLUMN_CHUNK
            || blocks.len().saturating_add(block_count) > total_blocks
        {
            return Err(SnapshotError::Validation);
        }
        let comp_end = cursor
            .checked_add(comp_len)
            .ok_or(SnapshotError::Truncated)?;
        let lz4_slice = bytes
            .get(*cursor..comp_end)
            .ok_or(SnapshotError::Truncated)?;
        *cursor = comp_end;

        decompress_into_scratch(lz4_slice, raw_len, scratch, section_cap)?;
        let mut rc = 0_usize;
        for _ in 0..block_count {
            let tag = *scratch.get(rc).ok_or(SnapshotError::Codec)?;
            rc = rc.saturating_add(1);
            match tag {
                BLOCK_TAG_EMPTY => blocks.push(Block::Empty),
                BLOCK_TAG_SPARSE => {
                    let entry_count = usize::from(read_u16_le(scratch, rc)?);
                    rc = rc.saturating_add(2);
                    if entry_count > BLOCK_ROWS as usize {
                        return Err(SnapshotError::Validation);
                    }
                    let byte_span = entry_count.saturating_mul(10);
                    let end = rc.checked_add(byte_span).ok_or(SnapshotError::Codec)?;
                    let body = scratch.get(rc..end).ok_or(SnapshotError::Codec)?;
                    rc = end;
                    let mut entries = Vec::with_capacity(entry_count);
                    for item in body.as_chunks::<10>().0 {
                        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9] = *item;
                        let idx = u16::from_le_bytes([b0, b1]);
                        let bits = u64::from_le_bytes([b2, b3, b4, b5, b6, b7, b8, b9]);
                        if !validate_cell_word(bits, string_bound) {
                            return Err(SnapshotError::Validation);
                        }
                        if bits != Value::EMPTY_BITS {
                            *running_cells = running_cells.saturating_add(1);
                            if *running_cells > max_cells {
                                return Err(SnapshotError::Validation);
                            }
                        }
                        entries.push((idx, bits));
                    }
                    blocks.push(Block::Sparse(entries.into()));
                }
                BLOCK_TAG_DENSE => {
                    let byte_span = (BLOCK_ROWS as usize).saturating_mul(8);
                    let end = rc.checked_add(byte_span).ok_or(SnapshotError::Codec)?;
                    let body = scratch.get(rc..end).ok_or(SnapshotError::Codec)?;
                    rc = end;
                    let mut words = vec![0_u64; BLOCK_ROWS as usize];
                    for (dst, src) in words.iter_mut().zip(body.as_chunks::<8>().0) {
                        let bits = u64::from_le_bytes(*src);
                        if !validate_cell_word(bits, string_bound) {
                            return Err(SnapshotError::Validation);
                        }
                        if bits != Value::EMPTY_BITS {
                            *running_cells = running_cells.saturating_add(1);
                            if *running_cells > max_cells {
                                return Err(SnapshotError::Validation);
                            }
                        }
                        *dst = bits;
                    }
                    blocks.push(Block::Dense(alloc::sync::Arc::new(words)));
                }
                _ => return Err(SnapshotError::Codec),
            }
        }
        if rc != scratch.len() {
            return Err(SnapshotError::Codec);
        }
    }

    if blocks.len() != total_blocks {
        return Err(SnapshotError::Validation);
    }
    Column::try_from_blocks(blocks, col_len, |_| true).ok_or(SnapshotError::Validation)
}

/// Wire-compatible mirror of [`WireFormulaEntryV1`] that deserializes directly
/// into an in-memory [`BoundFormula`], avoiding allocating and immediately
/// dropping millions of intermediate `WireBoundExprV1` heap nodes on decode.
#[derive(Deserialize)]
struct DirectFormulaEntry {
    sheet: u32,
    row: u64,
    col: u64,
    formula: BoundFormula,
}

fn validate_and_normalize_direct_expr(
    expr: &mut BoundExpr,
    depth: usize,
    allow_v3: bool,
) -> Result<(), SnapshotError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(SnapshotError::Validation);
    }
    let next = depth.saturating_add(1);
    match expr {
        BoundExpr::Literal(lit) => validate_direct_literal(lit),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => {
            if *rows == 0 || *cols == 0 {
                return Err(SnapshotError::Validation);
            }
            let expected = (*rows as usize)
                .checked_mul(*cols as usize)
                .ok_or(SnapshotError::Validation)?;
            if expected != elements.len() {
                return Err(SnapshotError::Validation);
            }
            for item in elements.iter_mut() {
                validate_direct_literal(item)?;
            }
            Ok(())
        }
        BoundExpr::Reference(_) => Ok(()),
        BoundExpr::Name(n) => check_direct_text_len(n),
        BoundExpr::ScopedName {
            missing_sheet,
            name,
            ..
        } => {
            if !allow_v3 {
                return Err(SnapshotError::Codec);
            }
            if let Some(ms) = missing_sheet {
                check_direct_text_len(ms)?;
            }
            check_direct_text_len(name)
        }
        BoundExpr::SpillRef(inner) => {
            if !allow_v3 {
                return Err(SnapshotError::Codec);
            }
            validate_and_normalize_direct_expr(inner, next, allow_v3)
        }
        BoundExpr::Opaque { raw } => {
            if !allow_v3 {
                return Err(SnapshotError::Codec);
            }
            check_direct_text_len(raw)
        }
        BoundExpr::PreservedCached { raw, cached } => {
            if !allow_v3 {
                return Err(SnapshotError::Codec);
            }
            check_direct_text_len(raw)?;
            validate_direct_scalar(cached)
        }
        BoundExpr::MissingArg => {
            if !allow_v3 {
                return Err(SnapshotError::Codec);
            }
            Ok(())
        }
        BoundExpr::Paren(inner) | BoundExpr::Unary { expr: inner, .. } => {
            validate_and_normalize_direct_expr(inner, next, allow_v3)
        }
        BoundExpr::Binary { left, right, .. } => {
            validate_and_normalize_direct_expr(left, next, allow_v3)?;
            validate_and_normalize_direct_expr(right, next, allow_v3)
        }
        BoundExpr::Call { name, args } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(SnapshotError::Validation);
            }
            check_direct_text_len(name)?;
            if name.bytes().any(|b| b.is_ascii_lowercase()) {
                name.make_ascii_uppercase();
            }
            for arg in args.iter_mut() {
                validate_and_normalize_direct_expr(arg, next, allow_v3)?;
            }
            Ok(())
        }
    }
}

fn validate_direct_scalar(scalar: &mut ScalarValue) -> Result<(), SnapshotError> {
    match scalar {
        ScalarValue::Number(n) => {
            if !n.is_finite() {
                return Err(SnapshotError::Validation);
            }
            if *n == 0.0 {
                *n = 0.0;
            }
            Ok(())
        }
        ScalarValue::Text(s) => check_direct_text_len(s),
        ScalarValue::Bool(_) | ScalarValue::Error(_) => Ok(()),
    }
}

fn validate_direct_literal(lit: &mut Literal) -> Result<(), SnapshotError> {
    match lit {
        Literal::Number(n) => {
            if !n.is_finite() {
                return Err(SnapshotError::Validation);
            }
            if *n == 0.0 {
                *n = 0.0;
            }
            Ok(())
        }
        Literal::Text(s) => check_direct_text_len(s),
        Literal::Bool(_) | Literal::Error(_) => Ok(()),
    }
}

fn check_direct_text_len(s: &str) -> Result<(), SnapshotError> {
    if s.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
        return Err(SnapshotError::Validation);
    }
    Ok(())
}

fn decode_formulas_section(
    bytes: &[u8],
    scratch: &mut Vec<u8>,
    mut running_bytes: usize,
    max_bytes: usize,
    section_cap: usize,
    allow_v3: bool,
) -> Result<BTreeMap<WorkbookCellKey, BoundFormula>, SnapshotError> {
    let total_formulas =
        usize::try_from(read_u32_le(bytes, 0)?).map_err(|_| SnapshotError::Validation)?;
    let chunk_count =
        usize::try_from(read_u32_le(bytes, 4)?).map_err(|_| SnapshotError::Validation)?;
    let mut cursor = 8_usize;

    if total_formulas == 0 {
        if chunk_count != 0 || cursor != bytes.len() {
            return Err(SnapshotError::Validation);
        }
        return Ok(BTreeMap::new());
    }
    if chunk_count == 0 || chunk_count > total_formulas {
        return Err(SnapshotError::Validation);
    }

    let mut formulas: BTreeMap<WorkbookCellKey, BoundFormula> = BTreeMap::new();
    for _ in 0..chunk_count {
        let raw_len =
            usize::try_from(read_u32_le(bytes, cursor)?).map_err(|_| SnapshotError::Validation)?;
        let comp_len = usize::try_from(read_u32_le(bytes, cursor.saturating_add(4))?)
            .map_err(|_| SnapshotError::Validation)?;
        cursor = cursor.saturating_add(8);
        let comp_end = cursor
            .checked_add(comp_len)
            .ok_or(SnapshotError::Truncated)?;
        let lz4_slice = bytes
            .get(cursor..comp_end)
            .ok_or(SnapshotError::Truncated)?;
        cursor = comp_end;

        decompress_into_scratch(lz4_slice, raw_len, scratch, section_cap)?;
        let entries: Vec<DirectFormulaEntry> =
            postcard::from_bytes(scratch).map_err(|_| SnapshotError::Codec)?;
        if entries.is_empty() || entries.len() > FORMULAS_PER_CHUNK {
            return Err(SnapshotError::Validation);
        }
        for mut entry in entries {
            validate_and_normalize_direct_expr(&mut entry.formula.root, 0, allow_v3)?;
            use reticula_core::ops::FormulaPayload as _;
            entry
                .formula
                .validate()
                .map_err(|_| SnapshotError::Validation)?;
            running_bytes = running_bytes.saturating_add(entry.formula.estimated_bytes());
            if running_bytes > max_bytes {
                return Err(SnapshotError::Validation);
            }
            let key = (SheetId(entry.sheet), RowId(entry.row), ColId(entry.col));
            if formulas.insert(key, entry.formula).is_some() {
                return Err(SnapshotError::Validation);
            }
        }
    }

    if cursor != bytes.len() || formulas.len() != total_formulas {
        return Err(SnapshotError::Validation);
    }
    Ok(formulas)
}

#[inline]
fn validate_cell_word(bits: u64, string_bound: u32) -> bool {
    // Fast path: any finite IEEE 754 float (non-NaN, non-Inf) is valid in one mask check.
    const EXP_MASK: u64 = 0x7FF0_0000_0000_0000;
    if (bits & EXP_MASK) != EXP_MASK || bits == Value::EMPTY_BITS {
        return true;
    }
    let Some(val) = Value::try_unpack(bits) else {
        return false;
    };
    match val {
        Value::Empty | Value::Number(_) | Value::Bool(_) | Value::Error(_) => true,
        Value::Text(id) => id < string_bound,
        Value::Array(_) => false,
    }
}

fn encode_compressed_section<T: Serialize>(value: &T) -> Result<Vec<u8>, SnapshotError> {
    let raw = postcard::to_allocvec(value).map_err(|_| SnapshotError::Codec)?;
    let raw_len = u32::try_from(raw.len()).map_err(|_| SnapshotError::Compression)?;
    let lz4 = lz4_flex::block::compress(&raw);
    let mut out = Vec::with_capacity(4_usize.saturating_add(lz4.len()));
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&lz4);
    Ok(out)
}

fn decode_compressed_section<T: DeserializeOwned>(
    compressed: &[u8],
    section_cap: usize,
) -> Result<T, SnapshotError> {
    let expected_len =
        usize::try_from(read_u32_le(compressed, 0)?).map_err(|_| SnapshotError::Validation)?;
    let lz4_body = compressed.get(4..).ok_or(SnapshotError::Truncated)?;
    let mut scratch = Vec::new();
    decompress_into_scratch(lz4_body, expected_len, &mut scratch, section_cap)?;
    postcard::from_bytes(&scratch).map_err(|_| SnapshotError::Codec)
}

fn decompress_into_scratch(
    lz4_body: &[u8],
    expected_len: usize,
    scratch: &mut Vec<u8>,
    section_cap: usize,
) -> Result<(), SnapshotError> {
    if expected_len > section_cap {
        return Err(SnapshotError::Validation);
    }
    scratch.clear();
    if expected_len == 0 {
        if !lz4_body.is_empty() {
            lz4_flex::block::decompress_into(lz4_body, scratch)
                .map_err(|_| SnapshotError::Compression)?;
            if !scratch.is_empty() {
                return Err(SnapshotError::Compression);
            }
        }
        return Ok(());
    }
    let max_plausible = lz4_body.len().saturating_mul(260).saturating_add(4096);
    if expected_len > max_plausible {
        return Err(SnapshotError::Compression);
    }
    if scratch.capacity() < expected_len {
        scratch
            .try_reserve(expected_len.saturating_sub(scratch.len()))
            .map_err(|_| SnapshotError::Validation)?;
    }
    scratch.resize(expected_len, 0);
    let written = lz4_flex::block::decompress_into(lz4_body, scratch)
        .map_err(|_| SnapshotError::Compression)?;
    if written != expected_len {
        return Err(SnapshotError::Compression);
    }
    Ok(())
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

    use reticula_core::format::{CurrencySymbol, NumberFormat};
    use reticula_core::id::{ColId, IdRun, RowId, SheetId};
    use reticula_core::ops::{FormatPatch, FormatTarget, Op};
    use reticula_core::style::{BorderWeight, ColorPatch, ColorValue, HorizontalAlign, StylePatch};
    use reticula_formula::engine::FormulaEngine;

    use super::{
        SNAPSHOT_VERSION_V0, SNAPSHOT_VERSION_V1, SnapshotEncoder, SnapshotStep,
        decode_snapshot_any, encode_seed_stub_v1, encode_snapshot_v1,
    };
    use crate::v1::WireReplicaWatermarkV1;

    #[test]
    fn v1_snapshot_cow_streaming_isolates_concurrent_edits_and_compacts_strings() {
        let mut eng = FormulaEngine::open_sample(42, 300);
        let s0 = SheetId(0);
        let s1 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: s1,
            name: String::from("Summary"),
            after: Some(s0),
        })
        .unwrap();
        // Create an orphaned string by overwriting A2 twice.
        eng.edit_cell_raw(s1, RowId(0), ColId(0), "'orphan_string")
            .unwrap();
        eng.edit_cell_raw(s1, RowId(0), ColId(0), "'kept_string")
            .unwrap();
        eng.edit_cell_raw(s1, RowId(1), ColId(0), "=SUM(Sheet1!M2:M50)")
            .unwrap();
        eng.apply_op(Op::SetFreeze {
            sheet: s1,
            frozen_rows: 1,
            frozen_cols: 1,
        })
        .unwrap();
        eng.apply_op(Op::ResizeCols {
            sheet: s1,
            sizes: vec![(IdRun::new(0, 1).unwrap(), Some(180))],
        })
        .unwrap();
        eng.apply_op(Op::SetFormat {
            sheet: s1,
            target: FormatTarget::Cells {
                rows: vec![IdRun::new(1, 1).unwrap()],
                cols: vec![IdRun::new(0, 1).unwrap()],
            },
            number_format: FormatPatch::Set(NumberFormat::Currency {
                symbol: CurrencySymbol::Usd,
                decimals: 2,
            }),
            style: StylePatch {
                bold: Some(true),
                align: Some(HorizontalAlign::Right),
                fill_color: ColorPatch::Set(ColorValue::Palette(1)),
                border_bottom: Some(BorderWeight::Medium),
                ..StylePatch::default()
            },
        })
        .unwrap();
        eng.recalc_all();

        let snap_digest = eng.workbook().logical_digest();
        let watermarks = vec![WireReplicaWatermarkV1 {
            replica: 7,
            last_client_seq: 19,
            max_id_counter: 104,
        }];

        // Fork CoW encoder with tiny 1-block steps while concurrently mutating `eng`!
        let mut encoder = SnapshotEncoder::new(eng.workbook(), watermarks.clone());
        let mut streamed = vec![0_u8; encoder.header_len];

        // Mutate cells & formulas on the live engine mid-flight!
        let prior_f = eng.workbook().formula(s1, RowId(1), ColId(0)).cloned();
        encoder.record_formula_before_edit((s1, RowId(1), ColId(0)), prior_f);
        eng.edit_cell_raw(s1, RowId(1), ColId(0), "=999999")
            .unwrap();
        eng.edit_cell_raw(s0, RowId(1), ColId(9), "88888").unwrap();
        eng.recalc_all();
        assert_ne!(eng.workbook().logical_digest(), snap_digest);

        loop {
            match encoder.encode_step(eng.workbook(), 1).unwrap() {
                SnapshotStep::Progress => {}
                SnapshotStep::Chunk { offset, bytes } => {
                    assert_eq!(offset, streamed.len() as u64);
                    streamed.extend_from_slice(&bytes);
                }
                SnapshotStep::Finished {
                    header_bytes,
                    total_len,
                } => {
                    assert_eq!(total_len, streamed.len() as u64);
                    streamed[..header_bytes.len()].copy_from_slice(&header_bytes);
                    break;
                }
            }
        }

        let decoded = decode_snapshot_any(&streamed).unwrap();
        assert_eq!(decoded.format_version, SNAPSHOT_VERSION_V1);
        assert_eq!(decoded.watermarks, watermarks);
        assert_eq!(decoded.engine.workbook().logical_digest(), snap_digest);
        // Orphaned string was compacted out of `Summary`!
        assert_eq!(
            decoded.engine.workbook().sheet(s1).unwrap().strings().len(),
            1
        );
    }

    #[test]
    fn seed_stub_and_legacy_v0_snapshots_decode_cleanly() {
        let sample = FormulaEngine::open_sample(0xABCD_1234, 250);
        let origin = sample.workbook().seed_origin().unwrap();

        let stub_bytes = encode_seed_stub_v1(origin, &[]).unwrap();
        assert!(
            stub_bytes.len() <= 200,
            "seed stub must be tiny (~120B), got {}B",
            stub_bytes.len()
        );
        let from_stub = decode_snapshot_any(&stub_bytes).unwrap();
        assert_eq!(
            from_stub.engine.workbook().logical_digest(),
            sample.workbook().logical_digest()
        );

        // Legacy v0 snapshot still decodes via `decode_snapshot_any`.
        let v0_bytes = sample.encode_snapshot().unwrap();
        let from_v0 = decode_snapshot_any(&v0_bytes).unwrap();
        assert_eq!(from_v0.format_version, SNAPSHOT_VERSION_V0);
        assert_eq!(
            from_v0.engine.workbook().logical_digest(),
            sample.workbook().logical_digest()
        );

        // One-shot `encode_snapshot_v1` also round-trips identically.
        let v1_bytes = encode_snapshot_v1(sample.workbook(), &[]).unwrap();
        let from_v1 = decode_snapshot_any(&v1_bytes).unwrap();
        assert_eq!(from_v1.format_version, SNAPSHOT_VERSION_V1);
        assert_eq!(
            from_v1.engine.workbook().logical_digest(),
            sample.workbook().logical_digest()
        );
    }

    #[test]
    fn direct_formula_entry_matches_frozen_wire_v1_layout() {
        use alloc::boxed::Box;
        use alloc::vec::Vec;
        use reticula_core::ops::FormulaMode;
        use reticula_core::value::CalcError;
        use reticula_formula::ast::{BinaryOp, Literal, UnaryOp};
        use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};

        use super::{DirectFormulaEntry, validate_and_normalize_direct_expr};
        use crate::v1::{WireBoundFormulaV1, WireFormulaEntryV1};

        let samples: Vec<BoundFormula> = vec![
            BoundFormula {
                mode: FormulaMode::Dynamic,
                root: BoundExpr::Call {
                    name: String::from("SUM"),
                    args: vec![
                        BoundExpr::Literal(Literal::Number(12.5)),
                        BoundExpr::Literal(Literal::Text(String::from("hi"))),
                        BoundExpr::Literal(Literal::Bool(true)),
                        BoundExpr::Literal(Literal::Error(CalcError::Cycle)),
                        BoundExpr::ArrayLiteral {
                            rows: 1,
                            cols: 2,
                            elements: vec![Literal::Number(1.0), Literal::Error(CalcError::Div0)],
                        },
                        BoundExpr::Reference(BoundRef::Cell {
                            sheet: Some(SheetId(2)),
                            missing_sheet: Some(String::from("Gone")),
                            row: RowId(5),
                            col: ColId(3),
                            abs_col: true,
                            abs_row: false,
                        }),
                        BoundExpr::Reference(BoundRef::Area {
                            sheet: None,
                            missing_sheet: None,
                            first_row: RowId(1),
                            first_col: ColId(2),
                            last_row: RowId(9),
                            last_col: ColId(4),
                            abs_first_col: true,
                            abs_first_row: false,
                            abs_last_col: false,
                            abs_last_row: true,
                        }),
                        BoundExpr::Reference(BoundRef::ColRange {
                            sheet: None,
                            missing_sheet: None,
                            first_col: ColId(0),
                            last_col: ColId(3),
                            abs_first: true,
                            abs_last: false,
                        }),
                        BoundExpr::Reference(BoundRef::RowRange {
                            sheet: Some(SheetId(1)),
                            missing_sheet: None,
                            first_row: RowId(2),
                            last_row: RowId(8),
                            abs_first: false,
                            abs_last: true,
                        }),
                        BoundExpr::Reference(BoundRef::Invalid {
                            sheet_name: Some(String::from("Deleted")),
                        }),
                        BoundExpr::Name(String::from("MY_NAME")),
                        BoundExpr::Paren(Box::new(BoundExpr::Unary {
                            op: UnaryOp::Percent,
                            expr: Box::new(BoundExpr::Binary {
                                op: BinaryOp::Ge,
                                left: Box::new(BoundExpr::Literal(Literal::Number(2.0))),
                                right: Box::new(BoundExpr::Literal(Literal::Number(1.0))),
                            }),
                        })),
                    ],
                },
            },
            BoundFormula {
                mode: FormulaMode::Legacy,
                root: BoundExpr::Unary {
                    op: UnaryOp::ImplicitIntersection,
                    expr: Box::new(BoundExpr::Binary {
                        op: BinaryOp::Pow,
                        left: Box::new(BoundExpr::Literal(Literal::Error(CalcError::Spill))),
                        right: Box::new(BoundExpr::Literal(Literal::Bool(false))),
                    }),
                },
            },
        ];

        let wire_entries: Vec<WireFormulaEntryV1> = samples
            .iter()
            .enumerate()
            .map(|(i, f)| WireFormulaEntryV1 {
                sheet: u32::try_from(i).unwrap(),
                row: (i as u64) + 10,
                col: (i as u64) + 20,
                formula: WireBoundFormulaV1::try_from(f).unwrap(),
            })
            .collect();
        let encoded = postcard::to_allocvec(&wire_entries).unwrap();
        let mut decoded: Vec<DirectFormulaEntry> = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.len(), samples.len());
        for (idx, (direct, expected)) in decoded.iter_mut().zip(samples.iter()).enumerate() {
            validate_and_normalize_direct_expr(&mut direct.formula.root, 0, false).unwrap();
            assert_eq!(direct.sheet, u32::try_from(idx).unwrap());
            assert_eq!(direct.row, (idx as u64) + 10);
            assert_eq!(direct.col, (idx as u64) + 20);
            assert_eq!(&direct.formula, expected);
        }

        // Verify v3 WireFormulaEntryV3 shares exact binary layout on v1 formulas and
        // round-trips all 5 new v3 BoundExpr variants through DirectFormulaEntry!
        let v3_same_bytes = postcard::to_allocvec(
            &samples
                .iter()
                .enumerate()
                .map(|(i, f)| crate::v3::WireFormulaEntryV3 {
                    sheet: u32::try_from(i).unwrap(),
                    row: (i as u64) + 10,
                    col: (i as u64) + 20,
                    formula: crate::v3::WireBoundFormulaV3::try_from(f).unwrap(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(encoded, v3_same_bytes);

        let v3_sample = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Call {
                name: String::from("XLOOKUP"),
                args: vec![
                    BoundExpr::ScopedName {
                        sheet: Some(SheetId(0)),
                        missing_sheet: None,
                        name: String::from("Rate"),
                    },
                    BoundExpr::SpillRef(Box::new(BoundExpr::Reference(BoundRef::Cell {
                        sheet: None,
                        missing_sheet: None,
                        row: RowId(0),
                        col: ColId(0),
                        abs_col: false,
                        abs_row: false,
                    }))),
                    BoundExpr::Opaque {
                        raw: String::from("=_xlfn.FUTURE()"),
                    },
                    BoundExpr::PreservedCached {
                        raw: String::from("=LEGACY(2)"),
                        cached: reticula_core::ops::ScalarValue::Number(9.5),
                    },
                    BoundExpr::MissingArg,
                ],
            },
        };
        let v3_encoded = postcard::to_allocvec(&vec![crate::v3::WireFormulaEntryV3 {
            sheet: 0,
            row: 1,
            col: 2,
            formula: crate::v3::WireBoundFormulaV3::try_from(&v3_sample).unwrap(),
        }])
        .unwrap();
        let mut v3_direct: Vec<DirectFormulaEntry> = postcard::from_bytes(&v3_encoded).unwrap();
        assert_eq!(
            validate_and_normalize_direct_expr(&mut v3_direct[0].formula.root.clone(), 0, false),
            Err(crate::snapshot::SnapshotError::Codec)
        );
        validate_and_normalize_direct_expr(&mut v3_direct[0].formula.root, 0, true).unwrap();
        assert_eq!(v3_direct[0].formula, v3_sample);
    }

    #[test]
    fn m3a_sections_formulas_v3_named_ranges_and_permutes_round_trip() {
        use alloc::boxed::Box;
        use reticula_core::id::{ColId, RowId, SheetId};
        use reticula_core::ops::{CellEdit, CellInput, FormatPatch, FormulaMode, Op};
        use reticula_formula::ast::Literal;
        use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};

        use super::encode_snapshot_with_permutes;
        use crate::v3::WireCommittedPermuteV3;

        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        eng.edit_cell_raw(s0, RowId(0), ColId(0), "=SEQUENCE(3)")
            .unwrap();
        let f_spill = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Call {
                name: String::from("SUM"),
                args: vec![
                    BoundExpr::SpillRef(Box::new(BoundExpr::Reference(BoundRef::Cell {
                        sheet: None,
                        missing_sheet: None,
                        row: RowId(0),
                        col: ColId(0),
                        abs_col: false,
                        abs_row: false,
                    }))),
                    BoundExpr::MissingArg,
                ],
            },
        };
        eng.apply_op(Op::SetCells {
            sheet: s0,
            edits: vec![CellEdit {
                row: RowId(0),
                col: ColId(1),
                input: CellInput::Formula {
                    formula: f_spill,
                    cached: None,
                },
                format: FormatPatch::Keep,
            }],
        })
        .unwrap();
        eng.apply_op(Op::DefineNamedRange {
            scope: Some(s0),
            name: String::from("TaxRate"),
            formula: BoundFormula {
                mode: FormulaMode::Legacy,
                root: BoundExpr::Literal(Literal::Number(0.075)),
            },
            comment: Some(String::from("Local rate")),
            hidden: false,
        })
        .unwrap();

        let permutes = vec![WireCommittedPermuteV3 {
            seq: 5,
            replica: 1,
            client_seq: 3,
            sheet: 0,
            cols: vec![0, 1],
            row_pairs: vec![(0, 2), (2, 0)],
        }];
        let bytes = encode_snapshot_with_permutes(eng.workbook(), &[], &permutes).unwrap();
        let decoded = decode_snapshot_any(&bytes).unwrap();
        assert_eq!(
            decoded.engine.workbook().state_digest(),
            eng.workbook().state_digest()
        );
        assert_eq!(
            decoded.engine.workbook().logical_digest(),
            eng.workbook().logical_digest()
        );
        assert_eq!(decoded.permutes, permutes);
    }

    #[test]
    fn collab_prevalidation_rejects_oversized_seed_stub_and_snapshots() {
        use reticula_core::ops::SeedOrigin;

        use super::{SnapshotError, decode_snapshot_for_collab};

        let million_stub = encode_seed_stub_v1(
            SeedOrigin {
                seed: 42,
                rows: 1_000_000,
                generator_version: 1,
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            decode_snapshot_for_collab(&million_stub, 2_000_000, 64 * 1024 * 1024).err(),
            Some(SnapshotError::Validation)
        );

        let small_sample = FormulaEngine::open_sample(42, 250);
        let small_stub =
            encode_seed_stub_v1(small_sample.workbook().seed_origin().unwrap(), &[]).unwrap();
        assert!(decode_snapshot_for_collab(&small_stub, 2_000_000, 64 * 1024 * 1024).is_ok());

        let v1_bytes = encode_snapshot_v1(small_sample.workbook(), &[]).unwrap();
        assert_eq!(
            decode_snapshot_for_collab(&v1_bytes, 1_000, 64 * 1024 * 1024).err(),
            Some(SnapshotError::Validation)
        );
        assert_eq!(
            decode_snapshot_for_collab(&v1_bytes, 2_000_000, 1024).err(),
            Some(SnapshotError::Validation)
        );
        assert!(decode_snapshot_for_collab(&v1_bytes, 2_000_000, 64 * 1024 * 1024).is_ok());
    }
}

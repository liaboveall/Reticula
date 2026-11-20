//! `RTCLWAL1` CRC-32 framed write-ahead log encoder, corruption-classifying
//! scanner, and transaction replay engine (`14-m1b.md` 2.4 & 5.5).
//!
//! File layout:
//! - Bytes `0..8`: `b"RTCLWAL1"`
//! - Bytes `8..12`: `version: u32 LE` (`1`)
//! - Bytes `12..16`: `head_crc32: u32 LE` over bytes `0..12`
//! - Frame `0` (at offset `16`): `[len: u32 LE][crc32: u32 LE][postcard(WireWalMetaV1)]`
//! - Frames `1..`: `[len: u32 LE][crc32: u32 LE][postcard(WireWalRecordV1)]`
//!   where `crc32` covers the 4-byte `len` prefix followed by `payload`.

use alloc::vec::Vec;

use reticula_core::ops::Op;
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::convert_v1::WireConvertError;
use crate::crc32::{crc32, crc32_pair};
use crate::v1::{WireWalMetaV1, WireWalRecordV1};

/// 8-byte file magic at the start of every write-ahead log.
pub const WAL_MAGIC: &[u8; 8] = b"RTCLWAL1";

/// `RTCLWAL1` container format version introduced in M1b.
pub const WAL_VERSION_V1: u32 = 1;

/// Byte length of the fixed `RTCLWAL1` file header.
pub const WAL_HEADER_LEN: usize = 16;

/// Upper bound on a single framed WAL record payload (`64 MiB`).
pub const MAX_WAL_FRAME_PAYLOAD: usize = 67_108_864;

/// Encoding or replay error on `RTCLWAL1` records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalError {
    /// Serialised frame exceeds [`MAX_WAL_FRAME_PAYLOAD`] or failed `postcard` encoding.
    Encode,
    /// Wire operation failed static structural/numeric validation.
    InvalidWireOp(WireConvertError),
    /// Applying an operation to the engine failed.
    ApplyFailed {
        /// Sequence number of the failing WAL record.
        seq: u64,
    },
}

/// Tail status of a scanned WAL file (`14-m1b.md` 2.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalTailDisposition {
    /// Every byte in the file belongs to complete, valid frames.
    Clean,
    /// Trailing `0x00` preallocation padding begins at `valid_bytes`; safe to truncate.
    ZeroPaddedTail {
        /// Exact byte offset where valid frames end.
        valid_bytes: usize,
    },
    /// Crash left an incomplete final frame starting at `valid_bytes` without
    /// losing flushed records; safe to truncate to `valid_bytes`.
    TornFinalRecord {
        /// Exact byte offset where valid frames end.
        valid_bytes: usize,
    },
}

/// Validated prefix and tail disposition produced by [`scan_wal`].
#[derive(Clone, Debug, PartialEq)]
pub struct WalScan {
    /// Base snapshot sequence anchored by frame `0`.
    pub base_snapshot_seq: u64,
    /// Validated contiguous transaction records (`base_snapshot_seq + 1 ..= last_seq`).
    pub records: Vec<WireWalRecordV1>,
    /// Byte length of the valid prefix (`<= input.len()`).
    pub valid_bytes: usize,
    /// Classification of any trailing bytes beyond `valid_bytes`.
    pub tail: WalTailDisposition,
}

impl WalScan {
    /// Highest contiguous sequence number present (`base_snapshot_seq` if no
    /// transaction records were present).
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.records
            .last()
            .map_or(self.base_snapshot_seq, |r| r.seq)
    }
}

/// Unrecoverable WAL corruption requiring D-029 Safe Mode (`14-m1b.md` 2.4 & 7.3).
#[derive(Clone, Debug, PartialEq)]
pub enum WalCorruption {
    /// Magic mismatch, unsupported version, or corrupted 16-byte header CRC.
    InvalidHeader,
    /// Metadata frame missing, corrupted, or anchored to a different snapshot sequence.
    BaseSeqMismatch {
        /// Decoded `base_snapshot_seq` if readable.
        found: Option<u64>,
        /// Expected `base_snapshot_seq` from `state.json`.
        expected: u64,
    },
    /// Record sequence jumped or repeated instead of incrementing by `+1`.
    NonContiguousSeq {
        /// Byte offset of the out-of-sequence frame.
        offset: usize,
        /// Expected `seq`.
        expected_seq: u64,
        /// Actual `seq` decoded.
        found_seq: u64,
    },
    /// Mid-file CRC/codec corruption (non-zero bytes follow the bad frame) or
    /// committed records below `last_flushed_seq - 1` were lost.
    MidFileCorruption {
        /// Byte offset of the first corrupted frame.
        offset: usize,
        /// Highest valid contiguous sequence before `offset`.
        last_valid_seq: u64,
        /// Valid prefix records decoded prior to `offset` (usable in Safe Mode).
        salvageable_records: Vec<WireWalRecordV1>,
    },
}

/// Encodes the 16-byte `RTCLWAL1` header followed by the initial
/// [`WireWalMetaV1`] anchor frame.
pub fn encode_wal_init(base_snapshot_seq: u64) -> Result<Vec<u8>, WalError> {
    let meta = WireWalMetaV1 { base_snapshot_seq };
    let meta_frame = encode_frame(&meta)?;
    let mut out = Vec::with_capacity(WAL_HEADER_LEN.saturating_add(meta_frame.len()));
    out.extend_from_slice(WAL_MAGIC);
    out.extend_from_slice(&WAL_VERSION_V1.to_le_bytes());
    let head_crc = crc32(&out);
    out.extend_from_slice(&head_crc.to_le_bytes());
    out.extend_from_slice(&meta_frame);
    Ok(out)
}

/// Encodes a single [`WireWalRecordV1`] transaction into an 8-byte-prefixed
/// `[len: u32 LE][crc32: u32 LE][payload]` WAL frame ready to append.
pub fn encode_wal_record(record: &WireWalRecordV1) -> Result<Vec<u8>, WalError> {
    encode_frame(record)
}

/// Convenience helper encoding a complete standalone WAL file (`header + meta + records`).
pub fn encode_wal_complete(
    base_snapshot_seq: u64,
    records: &[WireWalRecordV1],
) -> Result<Vec<u8>, WalError> {
    let mut out = encode_wal_init(base_snapshot_seq)?;
    for rec in records {
        let frame = encode_wal_record(rec)?;
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

/// Scans `bytes` from byte `0`, strictly distinguishing recoverable crash
/// tails (`TornFinalRecord` / `ZeroPaddedTail`) from mid-file corruption
/// (`WalCorruption::MidFileCorruption`) per `14-m1b.md` 2.4.
pub fn scan_wal(
    bytes: &[u8],
    expected_base_seq: u64,
    last_flushed_seq: u64,
) -> Result<WalScan, WalCorruption> {
    if bytes.is_empty() {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(WalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::Clean,
            });
        }
        return Err(WalCorruption::MidFileCorruption {
            offset: 0,
            last_valid_seq: expected_base_seq,
            salvageable_records: Vec::new(),
        });
    }

    if all_zero(bytes) {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(WalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::ZeroPaddedTail { valid_bytes: 0 },
            });
        }
        return Err(WalCorruption::InvalidHeader);
    }

    if bytes.len() < WAL_HEADER_LEN {
        let magic_check_len = bytes.len().min(WAL_MAGIC.len());
        let prefix_matches = bytes
            .get(..magic_check_len)
            .zip(WAL_MAGIC.get(..magic_check_len))
            .is_some_and(|(a, b)| a == b);
        if prefix_matches && watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(WalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::TornFinalRecord { valid_bytes: 0 },
            });
        }
        return Err(WalCorruption::InvalidHeader);
    }

    if bytes.get(0..8) != Some(WAL_MAGIC.as_slice()) {
        return Err(WalCorruption::InvalidHeader);
    }
    let Some(version) = read_u32_le(bytes, 8) else {
        return Err(WalCorruption::InvalidHeader);
    };
    if version != WAL_VERSION_V1 {
        return Err(WalCorruption::InvalidHeader);
    }
    let Some(head_crc) = read_u32_le(bytes, 12) else {
        return Err(WalCorruption::InvalidHeader);
    };
    let Some(head_prefix) = bytes.get(0..12) else {
        return Err(WalCorruption::InvalidHeader);
    };
    if head_crc != crc32(head_prefix) {
        return Err(WalCorruption::InvalidHeader);
    }

    // Frame 0 at offset 16: metadata anchor `WireWalMetaV1`.
    let meta_offset = WAL_HEADER_LEN;
    if meta_offset == bytes.len() {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(WalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::TornFinalRecord { valid_bytes: 0 },
            });
        }
        return Err(WalCorruption::BaseSeqMismatch {
            found: None,
            expected: expected_base_seq,
        });
    }
    if bytes.get(meta_offset..).is_some_and(all_zero) {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(WalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::ZeroPaddedTail { valid_bytes: 0 },
            });
        }
        return Err(WalCorruption::BaseSeqMismatch {
            found: None,
            expected: expected_base_seq,
        });
    }

    let (meta, mut cursor): (WireWalMetaV1, usize) = match decode_frame_at(bytes, meta_offset) {
        FrameOutcome::Valid(m, next_pos) => (m, next_pos),
        FrameOutcome::TornTail => {
            if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
                return Ok(WalScan {
                    base_snapshot_seq: expected_base_seq,
                    records: Vec::new(),
                    valid_bytes: 0,
                    tail: WalTailDisposition::TornFinalRecord { valid_bytes: 0 },
                });
            }
            return Err(WalCorruption::BaseSeqMismatch {
                found: None,
                expected: expected_base_seq,
            });
        }
        FrameOutcome::MidCorrupt => {
            return Err(WalCorruption::BaseSeqMismatch {
                found: None,
                expected: expected_base_seq,
            });
        }
    };

    if meta.base_snapshot_seq > expected_base_seq {
        return Err(WalCorruption::BaseSeqMismatch {
            found: Some(meta.base_snapshot_seq),
            expected: expected_base_seq,
        });
    }

    let mut records = Vec::new();
    let mut last_seq = meta.base_snapshot_seq;

    while cursor < bytes.len() {
        let Some(rem) = bytes.get(cursor..) else {
            break;
        };
        if all_zero(rem) {
            if last_seq < expected_base_seq {
                return Err(WalCorruption::BaseSeqMismatch {
                    found: Some(meta.base_snapshot_seq),
                    expected: expected_base_seq,
                });
            }
            if watermark_ok(last_seq, expected_base_seq, last_flushed_seq) {
                return Ok(WalScan {
                    base_snapshot_seq: expected_base_seq,
                    records,
                    valid_bytes: cursor,
                    tail: WalTailDisposition::ZeroPaddedTail {
                        valid_bytes: cursor,
                    },
                });
            }
            return Err(WalCorruption::MidFileCorruption {
                offset: cursor,
                last_valid_seq: last_seq,
                salvageable_records: records,
            });
        }

        match decode_frame_at::<WireWalRecordV1>(bytes, cursor) {
            FrameOutcome::Valid(rec, next_pos) => {
                let Some(expected_seq) = last_seq.checked_add(1) else {
                    return Err(WalCorruption::MidFileCorruption {
                        offset: cursor,
                        last_valid_seq: last_seq,
                        salvageable_records: records,
                    });
                };
                if rec.seq != expected_seq {
                    return Err(WalCorruption::NonContiguousSeq {
                        offset: cursor,
                        expected_seq,
                        found_seq: rec.seq,
                    });
                }
                last_seq = rec.seq;
                if rec.seq > expected_base_seq {
                    records.push(rec);
                }
                cursor = next_pos;
            }
            FrameOutcome::TornTail => {
                if last_seq < expected_base_seq {
                    return Err(WalCorruption::BaseSeqMismatch {
                        found: Some(meta.base_snapshot_seq),
                        expected: expected_base_seq,
                    });
                }
                if watermark_ok(last_seq, expected_base_seq, last_flushed_seq) {
                    return Ok(WalScan {
                        base_snapshot_seq: expected_base_seq,
                        records,
                        valid_bytes: cursor,
                        tail: WalTailDisposition::TornFinalRecord {
                            valid_bytes: cursor,
                        },
                    });
                }
                return Err(WalCorruption::MidFileCorruption {
                    offset: cursor,
                    last_valid_seq: last_seq,
                    salvageable_records: records,
                });
            }
            FrameOutcome::MidCorrupt => {
                return Err(WalCorruption::MidFileCorruption {
                    offset: cursor,
                    last_valid_seq: last_seq,
                    salvageable_records: records,
                });
            }
        }
    }

    if last_seq < expected_base_seq {
        return Err(WalCorruption::BaseSeqMismatch {
            found: Some(meta.base_snapshot_seq),
            expected: expected_base_seq,
        });
    }

    if !watermark_ok(last_seq, expected_base_seq, last_flushed_seq) {
        return Err(WalCorruption::MidFileCorruption {
            offset: cursor,
            last_valid_seq: last_seq,
            salvageable_records: records,
        });
    }

    Ok(WalScan {
        base_snapshot_seq: expected_base_seq,
        records,
        valid_bytes: cursor,
        tail: WalTailDisposition::Clean,
    })
}

/// Applies `records` (optionally skipping the last `skip_tail` records for Safe
/// Mode recovery) onto `engine`, optionally running recalculation at the end.
pub fn replay_wal_records(
    engine: &mut FormulaEngine,
    records: &[WireWalRecordV1],
    skip_tail: usize,
    recalculate: bool,
) -> Result<usize, WalError> {
    let keep = records.len().saturating_sub(skip_tail);
    let slice = records.get(..keep).unwrap_or(&[]);
    for rec in slice {
        for wire_op in &rec.ops {
            let op =
                Op::<BoundFormula>::try_from(wire_op.clone()).map_err(WalError::InvalidWireOp)?;
            engine
                .apply_op(op)
                .map_err(|_| WalError::ApplyFailed { seq: rec.seq })?;
        }
    }
    if recalculate {
        engine.recalc_all();
    }
    Ok(slice.len())
}

// ------------------------------------------------------------ internal helpers

enum FrameOutcome<T> {
    Valid(T, usize),
    TornTail,
    MidCorrupt,
}

fn watermark_ok(last_read_seq: u64, expected_base_seq: u64, last_flushed_seq: u64) -> bool {
    last_read_seq >= expected_base_seq && last_read_seq >= last_flushed_seq.saturating_sub(1)
}

fn all_zero(slice: &[u8]) -> bool {
    slice.iter().all(|&b| b == 0)
}

fn encode_frame<T: Serialize>(val: &T) -> Result<Vec<u8>, WalError> {
    let payload = postcard::to_allocvec(val).map_err(|_| WalError::Encode)?;
    if payload.len() > MAX_WAL_FRAME_PAYLOAD {
        return Err(WalError::Encode);
    }
    let len_u32 = u32::try_from(payload.len()).map_err(|_| WalError::Encode)?;
    let len_bytes = len_u32.to_le_bytes();
    let crc = crc32_pair(&len_bytes, &payload);
    let mut frame = Vec::with_capacity(8_usize.saturating_add(payload.len()));
    frame.extend_from_slice(&len_bytes);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn decode_frame_at<T: DeserializeOwned>(bytes: &[u8], offset: usize) -> FrameOutcome<T> {
    let rem = bytes.len().saturating_sub(offset);
    if rem < 8 {
        return FrameOutcome::TornTail;
    }
    let Some(len_u32) = read_u32_le(bytes, offset) else {
        return FrameOutcome::TornTail;
    };
    let Some(expected_crc) = read_u32_le(bytes, offset.saturating_add(4)) else {
        return FrameOutcome::TornTail;
    };
    let payload_len = len_u32 as usize;
    if payload_len > MAX_WAL_FRAME_PAYLOAD {
        return FrameOutcome::MidCorrupt;
    }
    let Some(payload_start) = offset.checked_add(8) else {
        return FrameOutcome::MidCorrupt;
    };
    let Some(frame_end) = payload_start.checked_add(payload_len) else {
        return FrameOutcome::MidCorrupt;
    };
    if frame_end > bytes.len() {
        return FrameOutcome::TornTail;
    }
    let (Some(len_slice), Some(payload)) = (
        bytes.get(offset..offset.saturating_add(4)),
        bytes.get(payload_start..frame_end),
    ) else {
        return FrameOutcome::TornTail;
    };
    let trailing_empty_or_zero = bytes.get(frame_end..).is_none_or(all_zero);
    if crc32_pair(len_slice, payload) != expected_crc {
        return if trailing_empty_or_zero {
            FrameOutcome::TornTail
        } else {
            FrameOutcome::MidCorrupt
        };
    }
    match postcard::from_bytes::<T>(payload) {
        Ok(val) => FrameOutcome::Valid(val, frame_end),
        Err(_) => {
            if trailing_empty_or_zero {
                FrameOutcome::TornTail
            } else {
                FrameOutcome::MidCorrupt
            }
        }
    }
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.saturating_add(4))?;
    let arr: [u8; 4] = slice.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
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

    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
    use reticula_formula::binder::BoundFormula;
    use reticula_formula::engine::FormulaEngine;

    use super::{
        WalCorruption, WalTailDisposition, encode_wal_complete, encode_wal_record,
        replay_wal_records, scan_wal,
    };
    use crate::v1::{WireOpV1, WireWalRecordV1};

    fn make_record(seq: u64, row: u64, val: f64) -> WireWalRecordV1 {
        let op: Op<BoundFormula> = Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(row),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Number(val)),
                format: FormatPatch::Keep,
            }],
        };
        WireWalRecordV1 {
            seq,
            timestamp_ms: 1_760_000_000_000 + seq,
            label: String::from("Edit cell"),
            ops: vec![WireOpV1::try_from(&op).unwrap()],
        }
    }

    #[test]
    fn clean_torn_tail_zero_padded_and_mid_file_corruption_are_distinguished() {
        let recs = vec![
            make_record(11, 0, 100.0),
            make_record(12, 1, 200.0),
            make_record(13, 2, 300.0),
        ];
        let clean_bytes = encode_wal_complete(10, &recs).unwrap();

        // 1. Clean scan + replay.
        let scan = scan_wal(&clean_bytes, 10, 13).unwrap();
        assert_eq!(scan.tail, WalTailDisposition::Clean);
        assert_eq!(scan.records.len(), 3);
        let mut eng = FormulaEngine::new();
        replay_wal_records(&mut eng, &scan.records, 0, true).unwrap();
        assert_eq!(
            eng.workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(2), ColId(0)),
            reticula_core::value::Value::Number(300.0)
        );

        // 2. Append an unflushed torn 4th record (`seq = 14`, cut at every prefix):
        //    `last_flushed_seq = 13` -> every cut recovers all 3 committed records!
        let rec4_frame = encode_wal_record(&make_record(14, 3, 400.0)).unwrap();
        for cut in 1..rec4_frame.len() {
            let mut torn = clean_bytes.clone();
            torn.extend_from_slice(&rec4_frame[..cut]);
            let res = scan_wal(&torn, 10, 13).unwrap();
            assert_eq!(res.records.len(), 3);
            assert_eq!(res.valid_bytes, clean_bytes.len());
            assert!(matches!(
                res.tail,
                WalTailDisposition::TornFinalRecord { .. }
                    | WalTailDisposition::ZeroPaddedTail { .. }
            ));
        }

        // 3. Zero-padded preallocated tail -> `ZeroPaddedTail`.
        let mut padded = clean_bytes.clone();
        padded.extend_from_slice(&[0_u8; 4096]);
        let res_pad = scan_wal(&padded, 10, 13).unwrap();
        assert_eq!(res_pad.records.len(), 3);
        assert_eq!(
            res_pad.tail,
            WalTailDisposition::ZeroPaddedTail {
                valid_bytes: clean_bytes.len()
            }
        );

        // 4. Flip any byte in record 1 or record 2 (mid-file!) -> MUST be `MidFileCorruption`!
        let prefix_2 = encode_wal_complete(10, &recs[..2]).unwrap();
        let meta_only = encode_wal_complete(10, &[]).unwrap();
        for pos in meta_only.len()..prefix_2.len() {
            let mut corrupted = clean_bytes.clone();
            corrupted[pos] ^= 0x40;
            let err = scan_wal(&corrupted, 10, 13).unwrap_err();
            assert!(
                matches!(
                    err,
                    WalCorruption::MidFileCorruption { .. }
                        | WalCorruption::NonContiguousSeq { .. }
                ),
                "mid-file byte flip at {pos} was not rejected as corruption: {err:?}"
            );
        }

        // 5. Compaction crash window: `state.json` switched to a newer snapshot
        //    (`expected_base_seq = 12`), while `wal.log` still anchors at `10`
        //    and covers `11..=13`. Only `seq > 12` (`13`) is returned for replay.
        let overlap = scan_wal(&clean_bytes, 12, 13).unwrap();
        assert_eq!(overlap.base_snapshot_seq, 12);
        assert_eq!(overlap.records.len(), 1);
        assert_eq!(overlap.records[0].seq, 13);

        // If `wal.log` lags behind `expected_base_seq` (`14`), reject as mismatch.
        let lag = scan_wal(&clean_bytes, 14, 14).unwrap_err();
        assert!(matches!(
            lag,
            WalCorruption::BaseSeqMismatch {
                found: Some(10),
                expected: 14,
            }
        ));
    }
}

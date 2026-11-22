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
use crate::v2::{WirePendingTxnV2, WireWalRecordV2};
use crate::v3::{WirePendingTxnV3, WireWalRecordV3};

/// 8-byte file magic at the start of every `wal.log` v1 write-ahead log.
pub const WAL_MAGIC: &[u8; 8] = b"RTCLWAL1";

/// 8-byte file magic at the start of every `wal.log` v2 write-ahead log.
pub const WAL_MAGIC_V2: &[u8; 8] = b"RTCLWAL2";

/// 8-byte file magic at the start of every `wal.log` v3 write-ahead log.
pub const WAL_MAGIC_V3: &[u8; 8] = b"RTCLWAL3";

/// 8-byte file magic at the start of every collaborative `pending.wal` log.
pub const PENDING_WAL_MAGIC: &[u8; 8] = b"RTCLPWAL";

/// `RTCLWAL1` container format version introduced in M1b.
pub const WAL_VERSION_V1: u32 = 1;

/// `RTCLWAL2` container format version introduced in M2a.
pub const WAL_VERSION_V2: u32 = 2;

/// `RTCLWAL3` container format version introduced in M3a.
pub const WAL_VERSION_V3: u32 = 3;

/// `RTCLPWAL` pending-queue container format version introduced in M2a.
pub const PENDING_WAL_VERSION_V1: u32 = 1;

/// `RTCLPWAL` pending-queue container format version introduced in M3a (`WirePendingTxnV3`).
pub const PENDING_WAL_VERSION_V2: u32 = 2;

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

/// Result of scanning and validating `wal.log` v2 bytes up to a crash/clean boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct WalScanV2 {
    /// Base snapshot sequence anchored by frame `0`.
    pub base_snapshot_seq: u64,
    /// Validated contiguous transaction records (`base_snapshot_seq + 1 ..= last_seq`).
    pub records: Vec<WireWalRecordV2>,
    /// Byte length of the valid prefix (`<= input.len()`).
    pub valid_bytes: usize,
    /// Classification of any trailing bytes beyond `valid_bytes`.
    pub tail: WalTailDisposition,
}

impl WalScanV2 {
    /// Highest contiguous sequence number present (`base_snapshot_seq` if no
    /// transaction records were present).
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.records
            .last()
            .map_or(self.base_snapshot_seq, |r| r.seq)
    }
}

/// Unrecoverable corruption in a scanned `RTCLWAL2` (or upgraded) WAL file.
#[derive(Clone, Debug, PartialEq)]
pub enum WalCorruptionV2 {
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
        /// Found `seq`.
        found_seq: u64,
    },
    /// CRC mismatch or malformed payload followed by non-zero trailing bytes.
    MidFileCorruption {
        /// Byte offset of the corrupted frame.
        offset: usize,
        /// Highest valid sequence number before `offset`.
        last_valid_seq: u64,
        /// Already-validated records preceding `offset`.
        salvageable_records: Vec<WireWalRecordV2>,
    },
}

impl From<WalScan> for WalScanV2 {
    fn from(s: WalScan) -> Self {
        Self {
            base_snapshot_seq: s.base_snapshot_seq,
            records: s.records.into_iter().map(WireWalRecordV2::from).collect(),
            valid_bytes: s.valid_bytes,
            tail: s.tail,
        }
    }
}

impl From<WalCorruption> for WalCorruptionV2 {
    fn from(c: WalCorruption) -> Self {
        match c {
            WalCorruption::InvalidHeader => Self::InvalidHeader,
            WalCorruption::BaseSeqMismatch { found, expected } => {
                Self::BaseSeqMismatch { found, expected }
            }
            WalCorruption::NonContiguousSeq {
                offset,
                expected_seq,
                found_seq,
            } => Self::NonContiguousSeq {
                offset,
                expected_seq,
                found_seq,
            },
            WalCorruption::MidFileCorruption {
                offset,
                last_valid_seq,
                salvageable_records,
            } => Self::MidFileCorruption {
                offset,
                last_valid_seq,
                salvageable_records: salvageable_records
                    .into_iter()
                    .map(WireWalRecordV2::from)
                    .collect(),
            },
        }
    }
}

/// Result of scanning collaborative offline `pending.wal` (`RTCLPWAL`) bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingWalScan {
    /// Validated strictly increasing pending transactions.
    pub records: Vec<WirePendingTxnV2>,
    /// Byte length of the valid prefix (`<= input.len()`).
    pub valid_bytes: usize,
    /// Tail classification beyond `valid_bytes`.
    pub tail: WalTailDisposition,
}

/// Unrecoverable corruption in a scanned `pending.wal` (`RTCLPWAL`) file.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingWalCorruption {
    /// Magic mismatch, unsupported version, or corrupted 16-byte header CRC.
    InvalidHeader,
    /// Pending records did not strictly increase in `client_seq`.
    NonMonotonicClientSeq {
        /// Byte offset of the out-of-order frame.
        offset: usize,
        /// Previous valid `client_seq`.
        prev_client_seq: u64,
        /// Observed `client_seq` (`<= prev_client_seq`).
        found_client_seq: u64,
    },
    /// CRC mismatch or malformed payload followed by non-zero trailing bytes.
    MidFileCorruption {
        /// Byte offset of the corrupted frame.
        offset: usize,
        /// Already-validated records preceding `offset`.
        salvageable_records: Vec<WirePendingTxnV2>,
    },
}

/// Result of scanning and validating `wal.log` v3 (or upgraded `v1`/`v2`) bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct WalScanV3 {
    /// Base snapshot sequence anchored by frame `0`.
    pub base_snapshot_seq: u64,
    /// Validated contiguous transaction records (`base_snapshot_seq + 1 ..= last_seq`).
    pub records: Vec<WireWalRecordV3>,
    /// Byte length of the valid prefix (`<= input.len()`).
    pub valid_bytes: usize,
    /// Classification of any trailing bytes beyond `valid_bytes`.
    pub tail: WalTailDisposition,
}

impl WalScanV3 {
    /// Highest contiguous sequence number present (`base_snapshot_seq` if empty).
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.records
            .last()
            .map_or(self.base_snapshot_seq, |r| r.seq)
    }
}

/// Unrecoverable corruption in a scanned `RTCLWAL3` (or upgraded `v1`/`v2`) WAL file.
#[derive(Clone, Debug, PartialEq)]
pub enum WalCorruptionV3 {
    /// Magic mismatch, unsupported version, or corrupted 16-byte header CRC.
    InvalidHeader,
    /// Metadata frame missing, corrupted, or anchored to a different snapshot sequence.
    BaseSeqMismatch {
        /// Decoded `base_snapshot_seq` if readable.
        found: Option<u64>,
        /// Expected `base_snapshot_seq`.
        expected: u64,
    },
    /// Record sequence jumped or repeated instead of incrementing by `+1`.
    NonContiguousSeq {
        /// Byte offset of the out-of-sequence frame.
        offset: usize,
        /// Expected `seq`.
        expected_seq: u64,
        /// Found `seq`.
        found_seq: u64,
    },
    /// CRC mismatch or malformed payload followed by non-zero trailing bytes.
    MidFileCorruption {
        /// Byte offset of the corrupted frame.
        offset: usize,
        /// Highest valid sequence number before `offset`.
        last_valid_seq: u64,
        /// Already-validated records preceding `offset`.
        salvageable_records: Vec<WireWalRecordV3>,
    },
}

impl From<WalScan> for WalScanV3 {
    fn from(s: WalScan) -> Self {
        Self {
            base_snapshot_seq: s.base_snapshot_seq,
            records: s.records.into_iter().map(WireWalRecordV3::from).collect(),
            valid_bytes: s.valid_bytes,
            tail: s.tail,
        }
    }
}

impl From<WalScanV2> for WalScanV3 {
    fn from(s: WalScanV2) -> Self {
        Self {
            base_snapshot_seq: s.base_snapshot_seq,
            records: s.records.into_iter().map(WireWalRecordV3::from).collect(),
            valid_bytes: s.valid_bytes,
            tail: s.tail,
        }
    }
}

impl From<WalCorruption> for WalCorruptionV3 {
    fn from(c: WalCorruption) -> Self {
        Self::from(WalCorruptionV2::from(c))
    }
}

impl From<WalCorruptionV2> for WalCorruptionV3 {
    fn from(c: WalCorruptionV2) -> Self {
        match c {
            WalCorruptionV2::InvalidHeader => Self::InvalidHeader,
            WalCorruptionV2::BaseSeqMismatch { found, expected } => {
                Self::BaseSeqMismatch { found, expected }
            }
            WalCorruptionV2::NonContiguousSeq {
                offset,
                expected_seq,
                found_seq,
            } => Self::NonContiguousSeq {
                offset,
                expected_seq,
                found_seq,
            },
            WalCorruptionV2::MidFileCorruption {
                offset,
                last_valid_seq,
                salvageable_records,
            } => Self::MidFileCorruption {
                offset,
                last_valid_seq,
                salvageable_records: salvageable_records
                    .into_iter()
                    .map(WireWalRecordV3::from)
                    .collect(),
            },
        }
    }
}

/// Result of scanning collaborative offline `pending.wal` (`RTCLPWAL` v2 or upgraded v1).
#[derive(Clone, Debug, PartialEq)]
pub struct PendingWalScanV2 {
    /// Validated strictly increasing pending transactions.
    pub records: Vec<WirePendingTxnV3>,
    /// Byte length of the valid prefix (`<= input.len()`).
    pub valid_bytes: usize,
    /// Tail classification beyond `valid_bytes`.
    pub tail: WalTailDisposition,
}

/// Unrecoverable corruption in a scanned `pending.wal` (`RTCLPWAL` v2 or v1) file.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingWalCorruptionV2 {
    /// Magic mismatch, unsupported version, or corrupted 16-byte header CRC.
    InvalidHeader,
    /// Pending records did not strictly increase in `client_seq`.
    NonMonotonicClientSeq {
        /// Byte offset of the out-of-order frame.
        offset: usize,
        /// Previous valid `client_seq`.
        prev_client_seq: u64,
        /// Observed `client_seq` (`<= prev_client_seq`).
        found_client_seq: u64,
    },
    /// CRC mismatch or malformed payload followed by non-zero trailing bytes.
    MidFileCorruption {
        /// Byte offset of the corrupted frame.
        offset: usize,
        /// Already-validated records preceding `offset`.
        salvageable_records: Vec<WirePendingTxnV3>,
    },
}

impl From<PendingWalScan> for PendingWalScanV2 {
    fn from(s: PendingWalScan) -> Self {
        Self {
            records: s.records.into_iter().map(WirePendingTxnV3::from).collect(),
            valid_bytes: s.valid_bytes,
            tail: s.tail,
        }
    }
}

impl From<PendingWalCorruption> for PendingWalCorruptionV2 {
    fn from(c: PendingWalCorruption) -> Self {
        match c {
            PendingWalCorruption::InvalidHeader => Self::InvalidHeader,
            PendingWalCorruption::NonMonotonicClientSeq {
                offset,
                prev_client_seq,
                found_client_seq,
            } => Self::NonMonotonicClientSeq {
                offset,
                prev_client_seq,
                found_client_seq,
            },
            PendingWalCorruption::MidFileCorruption {
                offset,
                salvageable_records,
            } => Self::MidFileCorruption {
                offset,
                salvageable_records: salvageable_records
                    .into_iter()
                    .map(WirePendingTxnV3::from)
                    .collect(),
            },
        }
    }
}

/// Encodes the 16-byte `RTCLWAL1` header followed by the initial
/// [`WireWalMetaV1`] anchor frame.
pub fn encode_wal_init(base_snapshot_seq: u64) -> Result<Vec<u8>, WalError> {
    encode_sequenced_wal_init(WAL_MAGIC, WAL_VERSION_V1, base_snapshot_seq)
}

/// Encodes the 16-byte `RTCLWAL2` header followed by the initial
/// [`WireWalMetaV1`] anchor frame.
pub fn encode_wal_init_v2(base_snapshot_seq: u64) -> Result<Vec<u8>, WalError> {
    encode_sequenced_wal_init(WAL_MAGIC_V2, WAL_VERSION_V2, base_snapshot_seq)
}

/// Encodes the 16-byte `RTCLWAL3` header followed by the initial
/// [`WireWalMetaV1`] anchor frame.
pub fn encode_wal_init_v3(base_snapshot_seq: u64) -> Result<Vec<u8>, WalError> {
    encode_sequenced_wal_init(WAL_MAGIC_V3, WAL_VERSION_V3, base_snapshot_seq)
}

fn encode_sequenced_wal_init(
    magic: &[u8; 8],
    version: u32,
    base_snapshot_seq: u64,
) -> Result<Vec<u8>, WalError> {
    let meta = WireWalMetaV1 { base_snapshot_seq };
    let meta_frame = encode_frame(&meta)?;
    let mut out = Vec::with_capacity(WAL_HEADER_LEN.saturating_add(meta_frame.len()));
    append_log_header(&mut out, magic, version);
    out.extend_from_slice(&meta_frame);
    Ok(out)
}

fn append_log_header(out: &mut Vec<u8>, magic: &[u8; 8], version: u32) {
    let start = out.len();
    out.extend_from_slice(magic);
    out.extend_from_slice(&version.to_le_bytes());
    let head_crc = crc32(out.get(start..).unwrap_or(&[]));
    out.extend_from_slice(&head_crc.to_le_bytes());
}

/// Encodes a single [`WireWalRecordV1`] transaction into an 8-byte-prefixed
/// `[len: u32 LE][crc32: u32 LE][payload]` WAL frame ready to append.
pub fn encode_wal_record(record: &WireWalRecordV1) -> Result<Vec<u8>, WalError> {
    encode_frame(record)
}

/// Encodes a single [`WireWalRecordV2`] transaction frame ready to append.
pub fn encode_wal_record_v2(record: &WireWalRecordV2) -> Result<Vec<u8>, WalError> {
    encode_frame(record)
}

/// Encodes a single [`WireWalRecordV3`] transaction frame ready to append.
pub fn encode_wal_record_v3(record: &WireWalRecordV3) -> Result<Vec<u8>, WalError> {
    encode_frame(record)
}

/// Convenience helper encoding a complete standalone WAL v1 file (`header + meta + records`).
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

/// Convenience helper encoding a complete standalone WAL v2 file (`header + meta + records`).
pub fn encode_wal_complete_v2(
    base_snapshot_seq: u64,
    records: &[WireWalRecordV2],
) -> Result<Vec<u8>, WalError> {
    let mut out = encode_wal_init_v2(base_snapshot_seq)?;
    for rec in records {
        let frame = encode_wal_record_v2(rec)?;
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

/// Convenience helper encoding a complete standalone WAL v3 file (`header + meta + records`).
pub fn encode_wal_complete_v3(
    base_snapshot_seq: u64,
    records: &[WireWalRecordV3],
) -> Result<Vec<u8>, WalError> {
    let mut out = encode_wal_init_v3(base_snapshot_seq)?;
    for rec in records {
        let frame = encode_wal_record_v3(rec)?;
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

/// Encodes the initial 16-byte `pending.wal` (`RTCLPWAL`) header (v1).
#[must_use]
pub fn encode_pending_wal_init() -> Vec<u8> {
    let mut out = Vec::with_capacity(WAL_HEADER_LEN);
    append_log_header(&mut out, PENDING_WAL_MAGIC, PENDING_WAL_VERSION_V1);
    out
}

/// Encodes one [`WirePendingTxnV2`] frame for appending to `pending.wal` (v1).
pub fn encode_pending_wal_record(record: &WirePendingTxnV2) -> Result<Vec<u8>, WalError> {
    encode_frame(record)
}

/// Encodes a complete standalone `pending.wal` v1 (`header + records`).
pub fn encode_pending_wal_complete(records: &[WirePendingTxnV2]) -> Result<Vec<u8>, WalError> {
    let mut out = encode_pending_wal_init();
    for rec in records {
        let frame = encode_pending_wal_record(rec)?;
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

/// Encodes the initial 16-byte `pending.wal` (`RTCLPWAL` v2) header.
#[must_use]
pub fn encode_pending_wal_init_v2() -> Vec<u8> {
    let mut out = Vec::with_capacity(WAL_HEADER_LEN);
    append_log_header(&mut out, PENDING_WAL_MAGIC, PENDING_WAL_VERSION_V2);
    out
}

/// Encodes one [`WirePendingTxnV3`] frame for appending to `pending.wal` v2.
pub fn encode_pending_wal_record_v2(record: &WirePendingTxnV3) -> Result<Vec<u8>, WalError> {
    encode_frame(record)
}

/// Encodes a complete standalone `pending.wal` v2 (`header + records`).
pub fn encode_pending_wal_complete_v2(records: &[WirePendingTxnV3]) -> Result<Vec<u8>, WalError> {
    let mut out = encode_pending_wal_init_v2();
    for rec in records {
        let frame = encode_pending_wal_record_v2(rec)?;
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

/// Scans `RTCLWAL1` bytes from byte `0`, strictly distinguishing recoverable
/// crash tails from mid-file corruption per `14-m1b.md` 2.4.
pub fn scan_wal(
    bytes: &[u8],
    expected_base_seq: u64,
    last_flushed_seq: u64,
) -> Result<WalScan, WalCorruption> {
    let raw = scan_sequenced_wal_generic(
        bytes,
        WAL_MAGIC,
        WAL_VERSION_V1,
        expected_base_seq,
        last_flushed_seq,
        |r: &WireWalRecordV1| r.seq,
    )?;
    Ok(WalScan {
        base_snapshot_seq: raw.base_snapshot_seq,
        records: raw.records,
        valid_bytes: raw.valid_bytes,
        tail: raw.tail,
    })
}

/// Scans `RTCLWAL2` bytes from byte `0`, distinguishing recoverable crash tails
/// from mid-file corruption.
pub fn scan_wal_v2(
    bytes: &[u8],
    expected_base_seq: u64,
    last_flushed_seq: u64,
) -> Result<WalScanV2, WalCorruptionV2> {
    let raw = scan_sequenced_wal_generic(
        bytes,
        WAL_MAGIC_V2,
        WAL_VERSION_V2,
        expected_base_seq,
        last_flushed_seq,
        |r: &WireWalRecordV2| r.seq,
    )
    .map_err(map_raw_corruption_v2)?;
    Ok(WalScanV2 {
        base_snapshot_seq: raw.base_snapshot_seq,
        records: raw.records,
        valid_bytes: raw.valid_bytes,
        tail: raw.tail,
    })
}

/// Scans either `RTCLWAL1` (transparently upgraded to [`WireWalRecordV2`]) or
/// `RTCLWAL2` bytes cleanly. Empty / prefix bytes default cleanly via [`scan_wal`].
pub fn scan_wal_any(
    bytes: &[u8],
    expected_base_seq: u64,
    last_flushed_seq: u64,
) -> Result<WalScanV2, WalCorruptionV2> {
    if bytes.get(0..8) == Some(WAL_MAGIC_V2.as_slice())
        || (bytes.len() < WAL_HEADER_LEN
            && bytes
                .get(..bytes.len().min(WAL_MAGIC_V2.len()))
                .zip(WAL_MAGIC_V2.get(..bytes.len().min(WAL_MAGIC_V2.len())))
                .is_some_and(|(a, b)| a == b)
            && bytes.len() == WAL_MAGIC_V2.len())
    {
        return scan_wal_v2(bytes, expected_base_seq, last_flushed_seq);
    }
    if bytes.get(0..8) == Some(WAL_MAGIC.as_slice()) {
        return scan_wal(bytes, expected_base_seq, last_flushed_seq)
            .map(WalScanV2::from)
            .map_err(WalCorruptionV2::from);
    }
    scan_wal_v2(bytes, expected_base_seq, last_flushed_seq).or_else(|_| {
        scan_wal(bytes, expected_base_seq, last_flushed_seq)
            .map(WalScanV2::from)
            .map_err(WalCorruptionV2::from)
    })
}

/// Scans `RTCLWAL3` bytes from byte `0`, distinguishing recoverable crash tails
/// from mid-file corruption.
pub fn scan_wal_v3(
    bytes: &[u8],
    expected_base_seq: u64,
    last_flushed_seq: u64,
) -> Result<WalScanV3, WalCorruptionV3> {
    let raw = scan_sequenced_wal_generic(
        bytes,
        WAL_MAGIC_V3,
        WAL_VERSION_V3,
        expected_base_seq,
        last_flushed_seq,
        |r: &WireWalRecordV3| r.seq,
    )
    .map_err(map_raw_corruption_v3)?;
    Ok(WalScanV3 {
        base_snapshot_seq: raw.base_snapshot_seq,
        records: raw.records,
        valid_bytes: raw.valid_bytes,
        tail: raw.tail,
    })
}

/// Scans `RTCLWAL1`, `RTCLWAL2`, or `RTCLWAL3` bytes cleanly, transparently
/// upgrading `v1`/`v2` records into [`WalScanV3`].
pub fn scan_wal_any_v3(
    bytes: &[u8],
    expected_base_seq: u64,
    last_flushed_seq: u64,
) -> Result<WalScanV3, WalCorruptionV3> {
    match bytes.get(0..8) {
        Some(m) if m == WAL_MAGIC_V3 => scan_wal_v3(bytes, expected_base_seq, last_flushed_seq),
        Some(m) if m == WAL_MAGIC_V2 => scan_wal_v2(bytes, expected_base_seq, last_flushed_seq)
            .map(WalScanV3::from)
            .map_err(WalCorruptionV3::from),
        Some(m) if m == WAL_MAGIC => scan_wal(bytes, expected_base_seq, last_flushed_seq)
            .map(WalScanV3::from)
            .map_err(WalCorruptionV3::from),
        _ => scan_wal_v3(bytes, expected_base_seq, last_flushed_seq)
            .or_else(|_| {
                scan_wal_v2(bytes, expected_base_seq, last_flushed_seq)
                    .map(WalScanV3::from)
                    .map_err(WalCorruptionV3::from)
            })
            .or_else(|_| {
                scan_wal(bytes, expected_base_seq, last_flushed_seq)
                    .map(WalScanV3::from)
                    .map_err(WalCorruptionV3::from)
            }),
    }
}

fn map_raw_corruption_v3(c: WalCorruptionGeneric<WireWalRecordV3>) -> WalCorruptionV3 {
    match c {
        WalCorruptionGeneric::InvalidHeader => WalCorruptionV3::InvalidHeader,
        WalCorruptionGeneric::BaseSeqMismatch { found, expected } => {
            WalCorruptionV3::BaseSeqMismatch { found, expected }
        }
        WalCorruptionGeneric::NonContiguousSeq {
            offset,
            expected_seq,
            found_seq,
        } => WalCorruptionV3::NonContiguousSeq {
            offset,
            expected_seq,
            found_seq,
        },
        WalCorruptionGeneric::MidFileCorruption {
            offset,
            last_valid_seq,
            salvageable_records,
        } => WalCorruptionV3::MidFileCorruption {
            offset,
            last_valid_seq,
            salvageable_records,
        },
    }
}

fn map_raw_corruption_v2(c: WalCorruptionGeneric<WireWalRecordV2>) -> WalCorruptionV2 {
    match c {
        WalCorruptionGeneric::InvalidHeader => WalCorruptionV2::InvalidHeader,
        WalCorruptionGeneric::BaseSeqMismatch { found, expected } => {
            WalCorruptionV2::BaseSeqMismatch { found, expected }
        }
        WalCorruptionGeneric::NonContiguousSeq {
            offset,
            expected_seq,
            found_seq,
        } => WalCorruptionV2::NonContiguousSeq {
            offset,
            expected_seq,
            found_seq,
        },
        WalCorruptionGeneric::MidFileCorruption {
            offset,
            last_valid_seq,
            salvageable_records,
        } => WalCorruptionV2::MidFileCorruption {
            offset,
            last_valid_seq,
            salvageable_records,
        },
    }
}

type WalCorruptionGeneric<R> = RawWalCorruption<R>;

struct RawWalScan<R> {
    base_snapshot_seq: u64,
    records: Vec<R>,
    valid_bytes: usize,
    tail: WalTailDisposition,
}

enum RawWalCorruption<R> {
    InvalidHeader,
    BaseSeqMismatch {
        found: Option<u64>,
        expected: u64,
    },
    NonContiguousSeq {
        offset: usize,
        expected_seq: u64,
        found_seq: u64,
    },
    MidFileCorruption {
        offset: usize,
        last_valid_seq: u64,
        salvageable_records: Vec<R>,
    },
}

impl From<RawWalCorruption<WireWalRecordV1>> for WalCorruption {
    fn from(c: RawWalCorruption<WireWalRecordV1>) -> Self {
        match c {
            RawWalCorruption::InvalidHeader => Self::InvalidHeader,
            RawWalCorruption::BaseSeqMismatch { found, expected } => {
                Self::BaseSeqMismatch { found, expected }
            }
            RawWalCorruption::NonContiguousSeq {
                offset,
                expected_seq,
                found_seq,
            } => Self::NonContiguousSeq {
                offset,
                expected_seq,
                found_seq,
            },
            RawWalCorruption::MidFileCorruption {
                offset,
                last_valid_seq,
                salvageable_records,
            } => Self::MidFileCorruption {
                offset,
                last_valid_seq,
                salvageable_records,
            },
        }
    }
}

fn scan_sequenced_wal_generic<R: DeserializeOwned>(
    bytes: &[u8],
    magic: &[u8; 8],
    version: u32,
    expected_base_seq: u64,
    last_flushed_seq: u64,
    seq_of: fn(&R) -> u64,
) -> Result<RawWalScan<R>, RawWalCorruption<R>> {
    if bytes.is_empty() {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(RawWalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::Clean,
            });
        }
        return Err(RawWalCorruption::MidFileCorruption {
            offset: 0,
            last_valid_seq: expected_base_seq,
            salvageable_records: Vec::new(),
        });
    }

    if all_zero(bytes) {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(RawWalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::ZeroPaddedTail { valid_bytes: 0 },
            });
        }
        return Err(RawWalCorruption::InvalidHeader);
    }

    if bytes.len() < WAL_HEADER_LEN {
        let magic_check_len = bytes.len().min(magic.len());
        let prefix_matches = bytes
            .get(..magic_check_len)
            .zip(magic.get(..magic_check_len))
            .is_some_and(|(a, b)| a == b);
        if prefix_matches && watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(RawWalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::TornFinalRecord { valid_bytes: 0 },
            });
        }
        return Err(RawWalCorruption::InvalidHeader);
    }

    if !header_valid(bytes, magic, version) {
        return Err(RawWalCorruption::InvalidHeader);
    }

    let meta_offset = WAL_HEADER_LEN;
    if meta_offset == bytes.len() {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(RawWalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::TornFinalRecord { valid_bytes: 0 },
            });
        }
        return Err(RawWalCorruption::BaseSeqMismatch {
            found: None,
            expected: expected_base_seq,
        });
    }
    if bytes.get(meta_offset..).is_some_and(all_zero) {
        if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
            return Ok(RawWalScan {
                base_snapshot_seq: expected_base_seq,
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::ZeroPaddedTail { valid_bytes: 0 },
            });
        }
        return Err(RawWalCorruption::BaseSeqMismatch {
            found: None,
            expected: expected_base_seq,
        });
    }

    let (meta, mut cursor): (WireWalMetaV1, usize) = match decode_frame_at(bytes, meta_offset) {
        FrameOutcome::Valid(m, next_pos) => (m, next_pos),
        FrameOutcome::TornTail => {
            if watermark_ok(expected_base_seq, expected_base_seq, last_flushed_seq) {
                return Ok(RawWalScan {
                    base_snapshot_seq: expected_base_seq,
                    records: Vec::new(),
                    valid_bytes: 0,
                    tail: WalTailDisposition::TornFinalRecord { valid_bytes: 0 },
                });
            }
            return Err(RawWalCorruption::BaseSeqMismatch {
                found: None,
                expected: expected_base_seq,
            });
        }
        FrameOutcome::MidCorrupt => {
            return Err(RawWalCorruption::BaseSeqMismatch {
                found: None,
                expected: expected_base_seq,
            });
        }
    };

    if meta.base_snapshot_seq > expected_base_seq {
        return Err(RawWalCorruption::BaseSeqMismatch {
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
                return Err(RawWalCorruption::BaseSeqMismatch {
                    found: Some(meta.base_snapshot_seq),
                    expected: expected_base_seq,
                });
            }
            if watermark_ok(last_seq, expected_base_seq, last_flushed_seq) {
                return Ok(RawWalScan {
                    base_snapshot_seq: expected_base_seq,
                    records,
                    valid_bytes: cursor,
                    tail: WalTailDisposition::ZeroPaddedTail {
                        valid_bytes: cursor,
                    },
                });
            }
            return Err(RawWalCorruption::MidFileCorruption {
                offset: cursor,
                last_valid_seq: last_seq,
                salvageable_records: records,
            });
        }

        match decode_frame_at::<R>(bytes, cursor) {
            FrameOutcome::Valid(rec, next_pos) => {
                let rec_seq = seq_of(&rec);
                let Some(expected_seq) = last_seq.checked_add(1) else {
                    return Err(RawWalCorruption::MidFileCorruption {
                        offset: cursor,
                        last_valid_seq: last_seq,
                        salvageable_records: records,
                    });
                };
                if rec_seq != expected_seq {
                    return Err(RawWalCorruption::NonContiguousSeq {
                        offset: cursor,
                        expected_seq,
                        found_seq: rec_seq,
                    });
                }
                last_seq = rec_seq;
                if rec_seq > expected_base_seq {
                    records.push(rec);
                }
                cursor = next_pos;
            }
            FrameOutcome::TornTail => {
                if last_seq < expected_base_seq {
                    return Err(RawWalCorruption::BaseSeqMismatch {
                        found: Some(meta.base_snapshot_seq),
                        expected: expected_base_seq,
                    });
                }
                if watermark_ok(last_seq, expected_base_seq, last_flushed_seq) {
                    return Ok(RawWalScan {
                        base_snapshot_seq: expected_base_seq,
                        records,
                        valid_bytes: cursor,
                        tail: WalTailDisposition::TornFinalRecord {
                            valid_bytes: cursor,
                        },
                    });
                }
                return Err(RawWalCorruption::MidFileCorruption {
                    offset: cursor,
                    last_valid_seq: last_seq,
                    salvageable_records: records,
                });
            }
            FrameOutcome::MidCorrupt => {
                return Err(RawWalCorruption::MidFileCorruption {
                    offset: cursor,
                    last_valid_seq: last_seq,
                    salvageable_records: records,
                });
            }
        }
    }

    if last_seq < expected_base_seq {
        return Err(RawWalCorruption::BaseSeqMismatch {
            found: Some(meta.base_snapshot_seq),
            expected: expected_base_seq,
        });
    }

    if !watermark_ok(last_seq, expected_base_seq, last_flushed_seq) {
        return Err(RawWalCorruption::MidFileCorruption {
            offset: cursor,
            last_valid_seq: last_seq,
            salvageable_records: records,
        });
    }

    Ok(RawWalScan {
        base_snapshot_seq: expected_base_seq,
        records,
        valid_bytes: cursor,
        tail: WalTailDisposition::Clean,
    })
}

enum RawPendingCorruption<R> {
    InvalidHeader,
    NonMonotonicClientSeq {
        offset: usize,
        prev_client_seq: u64,
        found_client_seq: u64,
    },
    MidFileCorruption {
        offset: usize,
        salvageable_records: Vec<R>,
    },
}

struct RawPendingScan<R> {
    records: Vec<R>,
    valid_bytes: usize,
    tail: WalTailDisposition,
}

fn scan_pending_wal_generic<R: DeserializeOwned>(
    bytes: &[u8],
    version: u32,
    client_seq_of: fn(&R) -> u64,
) -> Result<RawPendingScan<R>, RawPendingCorruption<R>> {
    if bytes.is_empty() {
        return Ok(RawPendingScan {
            records: Vec::new(),
            valid_bytes: 0,
            tail: WalTailDisposition::Clean,
        });
    }
    if all_zero(bytes) {
        return Ok(RawPendingScan {
            records: Vec::new(),
            valid_bytes: 0,
            tail: WalTailDisposition::ZeroPaddedTail { valid_bytes: 0 },
        });
    }
    if bytes.len() < WAL_HEADER_LEN {
        let magic_check_len = bytes.len().min(PENDING_WAL_MAGIC.len());
        let prefix_matches = bytes
            .get(..magic_check_len)
            .zip(PENDING_WAL_MAGIC.get(..magic_check_len))
            .is_some_and(|(a, b)| a == b);
        if prefix_matches {
            return Ok(RawPendingScan {
                records: Vec::new(),
                valid_bytes: 0,
                tail: WalTailDisposition::TornFinalRecord { valid_bytes: 0 },
            });
        }
        return Err(RawPendingCorruption::InvalidHeader);
    }
    if !header_valid(bytes, PENDING_WAL_MAGIC, version) {
        return Err(RawPendingCorruption::InvalidHeader);
    }

    let mut records = Vec::new();
    let mut prev_seq: Option<u64> = None;
    let mut cursor = WAL_HEADER_LEN;

    while cursor < bytes.len() {
        let Some(rem) = bytes.get(cursor..) else {
            break;
        };
        if all_zero(rem) {
            return Ok(RawPendingScan {
                records,
                valid_bytes: cursor,
                tail: WalTailDisposition::ZeroPaddedTail {
                    valid_bytes: cursor,
                },
            });
        }
        match decode_frame_at::<R>(bytes, cursor) {
            FrameOutcome::Valid(rec, next_pos) => {
                let cseq = client_seq_of(&rec);
                if let Some(prev) = prev_seq
                    && cseq <= prev
                {
                    return Err(RawPendingCorruption::NonMonotonicClientSeq {
                        offset: cursor,
                        prev_client_seq: prev,
                        found_client_seq: cseq,
                    });
                }
                prev_seq = Some(cseq);
                records.push(rec);
                cursor = next_pos;
            }
            FrameOutcome::TornTail => {
                return Ok(RawPendingScan {
                    records,
                    valid_bytes: cursor,
                    tail: WalTailDisposition::TornFinalRecord {
                        valid_bytes: cursor,
                    },
                });
            }
            FrameOutcome::MidCorrupt => {
                return Err(RawPendingCorruption::MidFileCorruption {
                    offset: cursor,
                    salvageable_records: records,
                });
            }
        }
    }

    Ok(RawPendingScan {
        records,
        valid_bytes: cursor,
        tail: WalTailDisposition::Clean,
    })
}

/// Scans `RTCLPWAL` v1 (`pending.wal`) bytes from offset `0`, distinguishing
/// recoverable crash tails (`ZeroPaddedTail` / `TornFinalRecord`) from mid-file
/// corruption or non-monotonic `client_seq`.
pub fn scan_pending_wal(bytes: &[u8]) -> Result<PendingWalScan, PendingWalCorruption> {
    let raw = scan_pending_wal_generic(bytes, PENDING_WAL_VERSION_V1, |r: &WirePendingTxnV2| {
        r.client_seq
    })
    .map_err(|c| match c {
        RawPendingCorruption::InvalidHeader => PendingWalCorruption::InvalidHeader,
        RawPendingCorruption::NonMonotonicClientSeq {
            offset,
            prev_client_seq,
            found_client_seq,
        } => PendingWalCorruption::NonMonotonicClientSeq {
            offset,
            prev_client_seq,
            found_client_seq,
        },
        RawPendingCorruption::MidFileCorruption {
            offset,
            salvageable_records,
        } => PendingWalCorruption::MidFileCorruption {
            offset,
            salvageable_records,
        },
    })?;
    Ok(PendingWalScan {
        records: raw.records,
        valid_bytes: raw.valid_bytes,
        tail: raw.tail,
    })
}

/// Scans `RTCLPWAL` v2 (`pending.wal` for `WirePendingTxnV3`) bytes from offset `0`.
pub fn scan_pending_wal_v2(bytes: &[u8]) -> Result<PendingWalScanV2, PendingWalCorruptionV2> {
    let raw = scan_pending_wal_generic(bytes, PENDING_WAL_VERSION_V2, |r: &WirePendingTxnV3| {
        r.client_seq
    })
    .map_err(|c| match c {
        RawPendingCorruption::InvalidHeader => PendingWalCorruptionV2::InvalidHeader,
        RawPendingCorruption::NonMonotonicClientSeq {
            offset,
            prev_client_seq,
            found_client_seq,
        } => PendingWalCorruptionV2::NonMonotonicClientSeq {
            offset,
            prev_client_seq,
            found_client_seq,
        },
        RawPendingCorruption::MidFileCorruption {
            offset,
            salvageable_records,
        } => PendingWalCorruptionV2::MidFileCorruption {
            offset,
            salvageable_records,
        },
    })?;
    Ok(PendingWalScanV2 {
        records: raw.records,
        valid_bytes: raw.valid_bytes,
        tail: raw.tail,
    })
}

/// Scans `RTCLPWAL` v1 (upgraded to `WirePendingTxnV3`) or v2 bytes cleanly.
pub fn scan_pending_wal_any(bytes: &[u8]) -> Result<PendingWalScanV2, PendingWalCorruptionV2> {
    if bytes.len() >= WAL_HEADER_LEN
        && bytes.get(0..8) == Some(PENDING_WAL_MAGIC.as_slice())
        && read_u32_le(bytes, 8) == Some(PENDING_WAL_VERSION_V1)
    {
        return scan_pending_wal(bytes)
            .map(PendingWalScanV2::from)
            .map_err(PendingWalCorruptionV2::from);
    }
    scan_pending_wal_v2(bytes)
}

fn header_valid(bytes: &[u8], magic: &[u8; 8], expected_version: u32) -> bool {
    if bytes.get(0..8) != Some(magic.as_slice()) {
        return false;
    }
    let Some(version) = read_u32_le(bytes, 8) else {
        return false;
    };
    if version != expected_version {
        return false;
    }
    let Some(head_crc) = read_u32_le(bytes, 12) else {
        return false;
    };
    let Some(head_prefix) = bytes.get(0..12) else {
        return false;
    };
    head_crc == crc32(head_prefix)
}

fn replay_wal_records_generic<R, W>(
    engine: &mut FormulaEngine,
    records: &[R],
    skip_tail: usize,
    recalculate: bool,
    seq_of: fn(&R) -> u64,
    ops_of: fn(&R) -> &[W],
) -> Result<usize, WalError>
where
    W: Clone + TryInto<Op<BoundFormula>, Error = WireConvertError>,
{
    let keep = records.len().saturating_sub(skip_tail);
    let slice = records.get(..keep).unwrap_or(&[]);
    for rec in slice {
        let seq = seq_of(rec);
        for wire_op in ops_of(rec) {
            let op = wire_op
                .clone()
                .try_into()
                .map_err(WalError::InvalidWireOp)?;
            engine
                .apply_op(op)
                .map_err(|_| WalError::ApplyFailed { seq })?;
        }
    }
    if recalculate {
        engine.recalc_all();
    }
    Ok(slice.len())
}

/// Applies `RTCLWAL3` (or upgraded) `records` onto `engine`, optionally recalculating.
pub fn replay_wal_records_v3(
    engine: &mut FormulaEngine,
    records: &[WireWalRecordV3],
    skip_tail: usize,
    recalculate: bool,
) -> Result<usize, WalError> {
    replay_wal_records_generic(
        engine,
        records,
        skip_tail,
        recalculate,
        |r| r.seq,
        |r| &r.ops,
    )
}

/// Applies `RTCLWAL2` `records` (optionally skipping the last `skip_tail`
/// records) onto `engine`, optionally running recalculation at the end.
pub fn replay_wal_records_v2(
    engine: &mut FormulaEngine,
    records: &[WireWalRecordV2],
    skip_tail: usize,
    recalculate: bool,
) -> Result<usize, WalError> {
    replay_wal_records_generic(
        engine,
        records,
        skip_tail,
        recalculate,
        |r| r.seq,
        |r| &r.ops,
    )
}

/// Applies `records` (optionally skipping the last `skip_tail` records for Safe
/// Mode recovery) onto `engine`, optionally running recalculation at the end.
pub fn replay_wal_records(
    engine: &mut FormulaEngine,
    records: &[WireWalRecordV1],
    skip_tail: usize,
    recalculate: bool,
) -> Result<usize, WalError> {
    replay_wal_records_generic(
        engine,
        records,
        skip_tail,
        recalculate,
        |r| r.seq,
        |r| &r.ops,
    )
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

    #[test]
    fn wal_v2_any_and_pending_wal_round_trip_with_torn_and_corrupt_detection() {
        use super::{
            PendingWalCorruption, encode_pending_wal_complete, encode_pending_wal_record,
            encode_wal_complete_v2, encode_wal_record_v2, replay_wal_records_v2, scan_pending_wal,
            scan_wal_any, scan_wal_v2,
        };
        use crate::v2::{WireOpV2, WirePendingTxnV2, WireTxnV2, WireWalRecordV2};
        use reticula_core::ops::ConditionalCellEdit;

        // 1. RTCLWAL1 transparently upgraded through `scan_wal_any`.
        let v1_bytes = encode_wal_complete(0, &[make_record(1, 0, 55.0)]).unwrap();
        let upgraded = scan_wal_any(&v1_bytes, 0, 1).unwrap();
        assert_eq!(upgraded.records.len(), 1);
        let mut eng = FormulaEngine::new();
        replay_wal_records_v2(&mut eng, &upgraded.records, 0, true).unwrap();
        assert_eq!(
            eng.workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            reticula_core::value::Value::Number(55.0)
        );

        // 2. RTCLWAL2 with `SetCellsIf` + torn tail.
        let cond_op = Op::SetCellsIf {
            sheet: SheetId(0),
            edits: vec![ConditionalCellEdit {
                row: RowId(0),
                col: ColId(0),
                expected_input: CellInput::Scalar(ScalarValue::Number(55.0)),
                expected_format: None,
                new_input: CellInput::Scalar(ScalarValue::Number(77.0)),
                new_format: FormatPatch::Keep,
            }],
        };
        let rec1 = WireWalRecordV2 {
            seq: 1,
            timestamp_ms: 100,
            label: String::from("Undo conditional"),
            ops: vec![WireOpV2::try_from(&cond_op).unwrap()],
        };
        let wal2 = encode_wal_complete_v2(0, core::slice::from_ref(&rec1)).unwrap();
        let scan2 = scan_wal_v2(&wal2, 0, 1).unwrap();
        assert_eq!(scan2.records, vec![rec1]);
        let scan2_any = scan_wal_any(&wal2, 0, 1).unwrap();
        assert_eq!(scan2_any, scan2);
        replay_wal_records_v2(&mut eng, &scan2.records, 0, true).unwrap();
        assert_eq!(
            eng.workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            reticula_core::value::Value::Number(77.0)
        );

        // Torn tail on record 2
        let extra = encode_wal_record_v2(&WireWalRecordV2 {
            seq: 2,
            timestamp_ms: 200,
            label: String::from("Torn"),
            ops: vec![WireOpV2::try_from(&cond_op).unwrap()],
        })
        .unwrap();
        let mut torn_wal2 = wal2.clone();
        torn_wal2.extend_from_slice(&extra[..extra.len() / 2]);
        let torn_scan = scan_wal_v2(&torn_wal2, 0, 1).unwrap();
        assert!(matches!(
            torn_scan.tail,
            WalTailDisposition::TornFinalRecord { .. }
        ));
        assert_eq!(torn_scan.records.len(), 1);

        // 3. `pending.wal` (`RTCLPWAL`) round-trip, torn tail, zero pad & non-monotonic reject.
        let p1 = WirePendingTxnV2 {
            replica: 1,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 1000,
            txn: WireTxnV2 {
                label: String::from("Pending 1"),
                ops: vec![WireOpV2::try_from(&cond_op).unwrap()],
            },
        };
        let p2 = WirePendingTxnV2 {
            replica: 1,
            client_seq: 2,
            base_server_seq: 0,
            timestamp_ms: 1001,
            txn: WireTxnV2 {
                label: String::from("Pending 2"),
                ops: vec![],
            },
        };
        let pending_bytes = encode_pending_wal_complete(&[p1.clone(), p2.clone()]).unwrap();
        let pscan = scan_pending_wal(&pending_bytes).unwrap();
        assert_eq!(pscan.tail, WalTailDisposition::Clean);
        assert_eq!(pscan.records, vec![p1.clone(), p2.clone()]);

        let p3_frame = encode_pending_wal_record(&WirePendingTxnV2 {
            replica: 1,
            client_seq: 3,
            base_server_seq: 0,
            timestamp_ms: 1002,
            txn: WireTxnV2 {
                label: String::from("Torn pending"),
                ops: vec![],
            },
        })
        .unwrap();
        let mut torn_pending = pending_bytes.clone();
        torn_pending.extend_from_slice(&p3_frame[..5]);
        let p_torn_scan = scan_pending_wal(&torn_pending).unwrap();
        assert_eq!(p_torn_scan.records.len(), 2);
        assert!(matches!(
            p_torn_scan.tail,
            WalTailDisposition::TornFinalRecord { .. }
        ));

        let non_mono = encode_pending_wal_complete(&[p2, p1]).unwrap();
        assert!(matches!(
            scan_pending_wal(&non_mono),
            Err(PendingWalCorruption::NonMonotonicClientSeq { .. })
        ));
    }

    #[test]
    fn wal_v3_and_pending_v2_round_trip_and_upgrade_older_formats() {
        use super::{
            PendingWalCorruptionV2, encode_pending_wal_complete, encode_pending_wal_complete_v2,
            encode_wal_complete_v2, encode_wal_complete_v3, encode_wal_record_v3,
            replay_wal_records_v3, scan_pending_wal_any, scan_pending_wal_v2, scan_wal_any_v3,
            scan_wal_v3,
        };
        use crate::v1::WireOpV1;
        use crate::v2::{WireOpV2, WirePendingTxnV2, WireTxnV2, WireWalRecordV2};
        use crate::v3::{WireOpV3, WirePendingTxnV3, WireTxnV3, WireWalRecordV3};

        // Upgrade RTCLWAL1 and RTCLWAL2 through scan_wal_any_v3.
        let v1_bytes = encode_wal_complete(0, &[make_record(1, 0, 5.0)]).unwrap();
        let up_v1 = scan_wal_any_v3(&v1_bytes, 0, 1).unwrap();
        assert_eq!(up_v1.records.len(), 1);
        assert_eq!(up_v1.last_seq(), 1);

        let v2_op: Op<BoundFormula> = Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(1),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Number(15.0)),
                format: FormatPatch::Keep,
            }],
        };
        let v2_rec = WireWalRecordV2 {
            seq: 1,
            timestamp_ms: 100,
            label: String::from("v2"),
            ops: vec![WireOpV2::try_from(&v2_op).unwrap()],
        };
        let v2_bytes = encode_wal_complete_v2(0, &[v2_rec]).unwrap();
        let up_v2 = scan_wal_any_v3(&v2_bytes, 0, 1).unwrap();
        assert_eq!(up_v2.records.len(), 1);

        // Native RTCLWAL3 with PermuteContent & DefineNamedRange.
        let ops_v3 = [
            Op::PermuteContent {
                sheet: SheetId(0),
                rows: vec![RowId(0), RowId(1)],
                cols: vec![ColId(0)],
                row_perm: vec![1, 0],
            },
            Op::DefineNamedRange {
                scope: None,
                name: String::from("Tax"),
                formula: BoundFormula {
                    mode: reticula_core::ops::FormulaMode::Legacy,
                    root: reticula_formula::binder::BoundExpr::Literal(
                        reticula_formula::ast::Literal::Number(0.1),
                    ),
                },
                comment: None,
                hidden: false,
            },
        ];
        let rec3 = WireWalRecordV3 {
            seq: 2,
            timestamp_ms: 200,
            label: String::from("Sort & Name"),
            ops: ops_v3
                .iter()
                .map(|o| WireOpV3::try_from(o).unwrap())
                .collect(),
        };
        let wal3 = encode_wal_complete_v3(1, core::slice::from_ref(&rec3)).unwrap();
        let scan3 = scan_wal_v3(&wal3, 1, 2).unwrap();
        assert_eq!(scan3.records, vec![rec3.clone()]);
        assert_eq!(scan_wal_any_v3(&wal3, 1, 2).unwrap(), scan3);

        let mut eng = FormulaEngine::new();
        eng.edit_cell_raw(SheetId(0), RowId(0), ColId(0), "10")
            .unwrap();
        eng.edit_cell_raw(SheetId(0), RowId(1), ColId(0), "20")
            .unwrap();
        replay_wal_records_v3(&mut eng, &scan3.records, 0, true).unwrap();
        assert_eq!(
            eng.workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            reticula_core::value::Value::Number(20.0)
        );
        assert_eq!(eng.workbook().named_ranges().len(), 1);

        // Torn tail on RTCLWAL3.
        let extra = encode_wal_record_v3(&WireWalRecordV3 {
            seq: 3,
            timestamp_ms: 300,
            label: String::from("Torn"),
            ops: vec![WireOpV3::from(WireOpV1::DeleteSheet { sheet: 99 })],
        })
        .unwrap();
        let mut torn_wal3 = wal3;
        torn_wal3.extend_from_slice(&extra[..4]);
        let torn_scan3 = scan_wal_any_v3(&torn_wal3, 1, 2).unwrap();
        assert!(matches!(
            torn_scan3.tail,
            WalTailDisposition::TornFinalRecord { .. }
        ));

        // RTCLPWAL v2 round-trip + v1 upgrade.
        let p_v1 = encode_pending_wal_complete(&[WirePendingTxnV2 {
            replica: 1,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 50,
            txn: WireTxnV2 {
                label: String::from("p1"),
                ops: vec![],
            },
        }])
        .unwrap();
        assert_eq!(scan_pending_wal_any(&p_v1).unwrap().records.len(), 1);

        let pv2_1 = WirePendingTxnV3 {
            replica: 1,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 100,
            txn: WireTxnV3 {
                label: String::from("sort"),
                ops: rec3.ops,
            },
        };
        let pv2_2 = WirePendingTxnV3 {
            replica: 1,
            client_seq: 2,
            base_server_seq: 0,
            timestamp_ms: 101,
            txn: WireTxnV3 {
                label: String::from("del"),
                ops: vec![WireOpV3::DeleteNamedRange {
                    scope: None,
                    name: String::from("Tax"),
                }],
            },
        };
        let p_v2_bytes = encode_pending_wal_complete_v2(&[pv2_1.clone(), pv2_2.clone()]).unwrap();
        let p_v2_scan = scan_pending_wal_v2(&p_v2_bytes).unwrap();
        assert_eq!(p_v2_scan.records, vec![pv2_1.clone(), pv2_2.clone()]);
        assert_eq!(scan_pending_wal_any(&p_v2_bytes).unwrap(), p_v2_scan);

        let bad_v2 = encode_pending_wal_complete_v2(&[pv2_2, pv2_1]).unwrap();
        assert!(matches!(
            scan_pending_wal_any(&bad_v2),
            Err(PendingWalCorruptionV2::NonMonotonicClientSeq { .. })
        ));
    }
}

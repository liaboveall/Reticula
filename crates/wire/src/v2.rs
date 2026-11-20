//! Wire protocol v2 and collaborative WebSocket sync frames (`SYNC_PROTOCOL_VERSION = 1`).
//!
//! All types in this module are versioned wire contracts (D-020). Do not edit
//! field definitions or enum variant order after freezing `tests/compat/v2/`;
//! introduce a new version module instead.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::v1::{
    WireCellEditV1, WireCellInputV1, WireFormatPatchV1, WireFormatTargetV1, WireIdRunV1,
    WireNumberFormatV1, WireStylePatchV1, WireTemplateFormulaV1, WireWalRecordV1,
};

/// Binary WebSocket subprotocol / frame format version (`15-m2a.md` 5.2 & `06-sync.md`).
pub const SYNC_PROTOCOL_VERSION: u8 = 1;

/// Wire representation of [`reticula_core::ops::ConditionalCellEdit`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireConditionalCellEditV2 {
    /// Target row identity.
    pub row: u64,
    /// Target column identity.
    pub col: u64,
    /// Expected current logical input (`Formula { cached }` ignored on check).
    pub expected_input: WireCellInputV1,
    /// Expected explicit per-cell format (`None` means inherited).
    pub expected_format: Option<WireNumberFormatV1>,
    /// Replacement input when preconditions match.
    pub new_input: WireCellInputV1,
    /// Replacement format patch when preconditions match.
    pub new_format: WireFormatPatchV1,
}

/// Wire representation of [`reticula_core::ops::Op`] (M2a superset adding
/// [`Self::SetCellsIf`] while preserving every v1 variant).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireOpV2 {
    /// Cell writes on `sheet`.
    SetCells {
        /// Target worksheet ID.
        sheet: u32,
        /// Individual cell edits.
        edits: Vec<WireCellEditV1>,
    },
    /// Clears values and formulas over `rows × cols`.
    ClearRange {
        /// Target worksheet ID.
        sheet: u32,
        /// Target row identity runs.
        rows: Vec<WireIdRunV1>,
        /// Target column identity runs.
        cols: Vec<WireIdRunV1>,
    },
    /// Inserts row IDs after `after`.
    InsertRows {
        /// Target worksheet ID.
        sheet: u32,
        /// Preceding row anchor (`None` inserts at top).
        after: Option<u64>,
        /// Fresh row identity run.
        ids: WireIdRunV1,
    },
    /// Inserts column IDs after `after`.
    InsertCols {
        /// Target worksheet ID.
        sheet: u32,
        /// Preceding column anchor (`None` inserts at left).
        after: Option<u64>,
        /// Fresh column identity run.
        ids: WireIdRunV1,
    },
    /// Marks row runs deleted.
    DeleteRows {
        /// Target worksheet ID.
        sheet: u32,
        /// Row runs to delete.
        ids: Vec<WireIdRunV1>,
    },
    /// Marks column runs deleted.
    DeleteCols {
        /// Target worksheet ID.
        sheet: u32,
        /// Column runs to delete.
        ids: Vec<WireIdRunV1>,
    },
    /// Decrements tombstone counts on deleted row runs.
    UndeleteRows {
        /// Target worksheet ID.
        sheet: u32,
        /// Row runs to undelete.
        ids: Vec<WireIdRunV1>,
    },
    /// Decrements tombstone counts on deleted column runs.
    UndeleteCols {
        /// Target worksheet ID.
        sheet: u32,
        /// Column runs to undelete.
        ids: Vec<WireIdRunV1>,
    },
    /// Reorders row runs after `after` (applying E25 range anchor handoff).
    MoveRows {
        /// Target worksheet ID.
        sheet: u32,
        /// Row runs to move.
        ids: Vec<WireIdRunV1>,
        /// Destination predecessor (`None` moves to top).
        after: Option<u64>,
    },
    /// Reorders column runs after `after` (applying E25 range anchor handoff).
    MoveCols {
        /// Target worksheet ID.
        sheet: u32,
        /// Column runs to move.
        ids: Vec<WireIdRunV1>,
        /// Destination predecessor (`None` moves to left).
        after: Option<u64>,
    },
    /// Sets or resets custom row pixel heights.
    ResizeRows {
        /// Target worksheet ID.
        sheet: u32,
        /// `(row_run, Some(height_px) | None)` pairs.
        sizes: Vec<(WireIdRunV1, Option<u16>)>,
    },
    /// Sets or resets custom column pixel widths.
    ResizeCols {
        /// Target worksheet ID.
        sheet: u32,
        /// `(col_run, Some(width_px) | None)` pairs.
        sizes: Vec<(WireIdRunV1, Option<u16>)>,
    },
    /// Updates frozen header panes on `sheet`.
    SetFreeze {
        /// Target worksheet ID.
        sheet: u32,
        /// Frozen top rows.
        frozen_rows: u32,
        /// Frozen left columns.
        frozen_cols: u32,
    },
    /// Applies format and/or style patch overrides.
    SetFormat {
        /// Target worksheet ID.
        sheet: u32,
        /// Target scope.
        target: WireFormatTargetV1,
        /// Number format patch.
        number_format: WireFormatPatchV1,
        /// Composable style patch.
        style: WireStylePatchV1,
    },
    /// Column-major bulk cell write (`rows × cols`).
    SetBlock {
        /// Target worksheet ID.
        sheet: u32,
        /// Row identity runs.
        rows: Vec<WireIdRunV1>,
        /// Target column IDs.
        cols: Vec<u64>,
        /// Column-major cells (`cols.len()` × `total_rows`).
        columns_data: Vec<Vec<WireCellInputV1>>,
    },
    /// Constant-size relative formula fill over `rows × cols`.
    FillFormula {
        /// Target worksheet ID.
        sheet: u32,
        /// Row identity runs.
        rows: Vec<WireIdRunV1>,
        /// Column identity runs.
        cols: Vec<WireIdRunV1>,
        /// Relative formula template.
        template: WireTemplateFormulaV1,
    },
    /// Creates a new worksheet.
    AddSheet {
        /// Fresh worksheet ID.
        sheet: u32,
        /// Display name.
        name: String,
        /// Predecessor in tab order.
        after: Option<u32>,
    },
    /// Renames an existing worksheet.
    RenameSheet {
        /// Target worksheet ID.
        sheet: u32,
        /// New display name.
        name: String,
    },
    /// Tombstones a worksheet.
    DeleteSheet {
        /// Target worksheet ID.
        sheet: u32,
    },
    /// Decrements a worksheet's deletion count.
    UndeleteSheet {
        /// Target worksheet ID.
        sheet: u32,
    },
    /// Reorders `sheet` to follow `after`.
    MoveSheet {
        /// Target worksheet ID.
        sheet: u32,
        /// Preceding worksheet ID (`None` moves first).
        after: Option<u32>,
    },
    /// Conditional CAS cell write for collaborative cell undo (`06-sync.md`, E19).
    SetCellsIf {
        /// Target worksheet ID.
        sheet: u32,
        /// Conditional cell edits.
        edits: Vec<WireConditionalCellEditV2>,
    },
}

/// Wire v2 transaction envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireTxnV2 {
    /// Human-readable history action label.
    pub label: String,
    /// Ordered atomic operations in the transaction.
    pub ops: Vec<WireOpV2>,
}

/// Client-authored pending transaction in `Push` frames and `pending.wal`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WirePendingTxnV2 {
    /// Authoring replica ID allocated by the server.
    pub replica: u32,
    /// Strict `1`-based per-replica monotonic client sequence.
    pub client_seq: u64,
    /// Last confirmed server `seq` observed when authoring this transaction.
    pub base_server_seq: u64,
    /// Client wall-clock timestamp (UTC epoch milliseconds, advisory).
    pub timestamp_ms: u64,
    /// Transaction payload.
    pub txn: WireTxnV2,
}

/// Persisted `RTCLWAL2` frame payload (`seq`-ordered forward log record).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireWalRecordV2 {
    /// Monotonic WAL transaction sequence number (`1, 2, ...`).
    pub seq: u64,
    /// UTC epoch milliseconds when recorded.
    pub timestamp_ms: u64,
    /// Human-readable history action label.
    pub label: String,
    /// Ordered operations making up this transaction.
    pub ops: Vec<WireOpV2>,
}

impl From<WireWalRecordV1> for WireWalRecordV2 {
    fn from(r: WireWalRecordV1) -> Self {
        Self {
            seq: r.seq,
            timestamp_ms: r.timestamp_ms,
            label: r.label,
            ops: r.ops.into_iter().map(Into::into).collect(),
        }
    }
}

/// Wire encoding of [`reticula_core::state_digest::StateDigest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStateDigestV1 {
    /// Digest format version (`STATE_DIGEST_VERSION = 1`).
    pub version: u16,
    /// Lower 64 bits of `content`.
    pub content_lo: u64,
    /// Upper 64 bits of `content`.
    pub content_hi: u64,
    /// 64-bit tab and axis ordering digest.
    pub order: u64,
}

/// Explicit rejection reasons sent in [`WireSyncFrameV1::Reject`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireRejectReasonV1 {
    /// Caller lacks permission to mutate this document/branch.
    Forbidden,
    /// Operation failed static validation, ID ownership, or workbook apply.
    InvalidOp,
    /// Supplied `client_seq` skipped an uncommitted sequence number.
    NonContiguousClientSeq {
        /// Next expected `client_seq` on the server for this replica.
        expected: u64,
    },
    /// Document exceeded populated cell count or byte size quota.
    QuotaExceeded,
    /// Client's `known_epoch` does not match server's `log_epoch`.
    EpochMismatch {
        /// Current server `log_epoch`.
        server_epoch: u8,
    },
    /// Client binary frame version is incompatible with server protocol.
    UpgradeRequired,
    /// Push batch count or byte payload exceeded transport bounds.
    PayloadTooLarge,
}

/// Committed server log entry transmitted in [`WireSyncFrameV1::Commit`] and
/// [`WireCatchupV1::Txns`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireCommittedTxnV2 {
    /// Authoritative contiguous document-branch sequence number (`1, 2, ...`).
    pub seq: u64,
    /// Originating replica ID.
    pub replica: u32,
    /// Originating replica's `client_seq`.
    pub client_seq: u64,
    /// Author user UUID bytes.
    pub user_id: [u8; 16],
    /// Server commit timestamp (UTC epoch milliseconds).
    pub timestamp_ms: u64,
    /// Applied transaction payload.
    pub txn: WireTxnV2,
}

/// Catch-up plan returned inside [`WireSyncFrameV1::Welcome`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireCatchupV1 {
    /// Missing transactions fit inside the server's inline catch-up window.
    Txns {
        /// First `seq` carried in `txns` (`head_seq + 1` when `txns` is empty).
        from_seq: u64,
        /// Ordered missing transactions (`from_seq..=head_seq`).
        txns: Vec<WireCommittedTxnV2>,
    },
    /// Client must fetch a snapshot at `snapshot_seq` via HTTP before catching up.
    SnapshotRequired {
        /// Sequence number covered by the snapshot.
        snapshot_seq: u64,
        /// Authenticated relative snapshot download path.
        url: String,
    },
}

/// Optional contiguous rectangular selection range in [`WirePresenceV1`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePresenceRangeV1 {
    /// Inclusive top row ID.
    pub start_row: u64,
    /// Inclusive left column ID.
    pub start_col: u64,
    /// Inclusive bottom row ID.
    pub end_row: u64,
    /// Inclusive right column ID.
    pub end_col: u64,
}

/// Ephemeral remote cursor and selection presence broadcast (`06-sync.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePresenceV1 {
    /// Sender's replica ID.
    pub replica: u32,
    /// Sender user UUID bytes.
    pub user_id: [u8; 16],
    /// Display name rendered beside remote cursor.
    pub display_name: String,
    /// Deterministic palette slot (`0..8`).
    pub color_slot: u8,
    /// Focused worksheet ID.
    pub sheet_id: u32,
    /// Focused cursor row ID.
    pub cursor_row: u64,
    /// Focused cursor column ID.
    pub cursor_col: u64,
    /// Optional multi-cell selection rectangle.
    pub selection: Option<WirePresenceRangeV1>,
}

/// Binary WebSocket sync protocol frame (`SYNC_PROTOCOL_VERSION = 1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireSyncFrameV1 {
    /// Initial client handshake sent immediately on WebSocket open.
    Hello {
        /// Protocol version ([`SYNC_PROTOCOL_VERSION`]).
        protocol_version: u8,
        /// Document UUID bytes.
        doc_id: [u8; 16],
        /// Branch UUID bytes (all zero for `main`).
        branch_id: [u8; 16],
        /// Previously issued replica ID, or `None` on first connect / epoch reset.
        replica: Option<u32>,
        /// Last contiguous `seq` durably applied by the client.
        last_applied_seq: u64,
        /// Last server `log_epoch` known to the client.
        known_epoch: u8,
    },
    /// Server handshake response carrying high-watermarks and catch-up instructions.
    Welcome {
        /// Current authoritative `log_epoch`.
        epoch: u8,
        /// Current authoritative document-branch `head_seq`.
        head_seq: u64,
        /// Assigned or verified replica ID for this session.
        replica: u32,
        /// Highest `client_seq` already committed on the server for `replica`.
        last_client_seq: u64,
        /// Highest ID counter committed on the server for `(epoch, replica)`.
        max_id_counter: u32,
        /// Missing log delta or snapshot pointer.
        catchup: WireCatchupV1,
    },
    /// Client upload of up to `64` sequential pending transactions (`<= 2 MiB`).
    Push {
        /// Ordered transactions with contiguous `client_seq`.
        txns: Vec<WirePendingTxnV2>,
    },
    /// Durably persisted acknowledgment sent strictly to the authoring connection.
    Ack {
        /// Authoring replica ID.
        replica: u32,
        /// Acknowledged client sequence number.
        client_seq: u64,
        /// Assigned server sequence number.
        seq: u64,
        /// Authoritative commit timestamp (UTC epoch milliseconds).
        timestamp_ms: u64,
    },
    /// Durable or transient rejection of an uploaded transaction batch.
    Reject {
        /// Rejected `client_seq`.
        client_seq: u64,
        /// Structured reason.
        reason: WireRejectReasonV1,
    },
    /// Committed transaction broadcast to online sessions on the document.
    Commit {
        /// Assigned contiguous server sequence number.
        seq: u64,
        /// Originating replica ID.
        replica: u32,
        /// Originating replica's `client_seq`.
        client_seq: u64,
        /// Author user UUID bytes.
        user_id: [u8; 16],
        /// Authoritative commit timestamp (UTC epoch milliseconds).
        timestamp_ms: u64,
        /// Applied transaction payload.
        txn: WireTxnV2,
    },
    /// Periodic 192-bit workbook state verification digest from the server.
    Digest {
        /// Server `seq` whose state produced `state_digest`.
        seq: u64,
        /// Versioned `StateDigest` v1 payload.
        state_digest: WireStateDigestV1,
    },
    /// Ephemeral cursor / selection presence update.
    Presence(WirePresenceV1),
    /// Application-level heartbeat probe.
    Ping(u64),
    /// Application-level heartbeat response.
    Pong(u64),
}

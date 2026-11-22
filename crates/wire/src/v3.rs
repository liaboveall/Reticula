//! Wire v3 schema (`D-020` & `17-m3a.md` 5.5).
//!
//! Introduces support for M3a formula expressions (`ScopedName`, `SpillRef`,
//! `Opaque`, `PreservedCached`, `MissingArg`), collaborative `PermuteContent`
//! (E15/E16), `MoveCells` (E14/E24), named ranges, persisted E16 permute logs,
//! `RTCLWAL3` / `RTCLPWAL` v2, and `SYNC_PROTOCOL_VERSION_V2 = 2`.
//!
//! Frozen [`crate::v1`] and [`crate::v2`] remain completely untouched.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::v1::{
    WireBinaryOpV1, WireBoundExprV1, WireBoundFormulaV1, WireBoundRefV1, WireCellEditV1,
    WireCellInputV1, WireFormatPatchV1, WireFormatTargetV1, WireFormulaModeV1, WireIdRunV1,
    WireLiteralV1, WireNumberFormatV1, WireOpV1, WirePendingTxnV1, WireScalarValueV1,
    WireStylePatchV1, WireTemplateExprV1, WireTemplateFormulaV1, WireTemplateRefV1, WireTxnV1,
    WireUnaryOpV1, WireWalRecordV1,
};
use crate::v2::{
    WireCatchupV1, WireCommittedTxnV2, WireConditionalCellEditV2, WireOpV2, WirePendingTxnV2,
    WirePresenceV1, WireRejectReasonV1, WireStateDigestV1, WireSyncFrameV1, WireTxnV2,
    WireWalRecordV2,
};

/// Active collaborative sync wire protocol version introduced in M3a.
pub const SYNC_PROTOCOL_VERSION_V2: u8 = 2;

/// Identity-bound formula expression node for wire v3 (`M3a`).
///
/// Variant discriminants `0..=12` intentionally mirror `BoundExpr` for direct
/// `RTCLSNAP` section `9` deserialization parity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireBoundExprV3 {
    /// Scalar literal.
    Literal(WireLiteralV1),
    /// Constant 2D array literal.
    ArrayLiteral {
        /// Row count.
        rows: u32,
        /// Column count.
        cols: u32,
        /// Row-major elements.
        elements: Vec<WireLiteralV1>,
    },
    /// Bound cell/range reference.
    Reference(WireBoundRefV1),
    /// Unresolved identifier or defined name.
    Name(String),
    /// Explicit parentheses `(expr)`.
    Paren(Box<Self>),
    /// Unary operation.
    Unary {
        /// Operator.
        op: WireUnaryOpV1,
        /// Inner expression.
        expr: Box<Self>,
    },
    /// Binary operation.
    Binary {
        /// Operator.
        op: WireBinaryOpV1,
        /// Left expression.
        left: Box<Self>,
        /// Right expression.
        right: Box<Self>,
    },
    /// Function call.
    Call {
        /// Uppercase function name.
        name: String,
        /// Arguments.
        args: Vec<Self>,
    },
    /// Worksheet-qualified defined name (`Sheet2!TaxRate`).
    ScopedName {
        /// Resolved worksheet identity (`None` when missing/deleted).
        sheet: Option<u32>,
        /// Original worksheet name preserved when unresolved.
        missing_sheet: Option<String>,
        /// Identifier spelling.
        name: String,
    },
    /// Dynamic-array spill-range operator (`expr#`).
    SpillRef(Box<Self>),
    /// Unsupported external formula preserved verbatim without recalculating.
    Opaque {
        /// Original raw formula text.
        raw: String,
    },
    /// Unsupported external formula with preserved cached scalar evaluation.
    PreservedCached {
        /// Original raw formula text.
        raw: String,
        /// Cached scalar evaluation.
        cached: WireScalarValueV1,
    },
    /// Omitted optional argument in a function call (`XLOOKUP(A1,B1:B5,C1:C5,,0)`).
    MissingArg,
}

/// Persisted bound formula for wire v3.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireBoundFormulaV3 {
    /// Array evaluation mode.
    pub mode: WireFormulaModeV1,
    /// Root expression.
    pub root: WireBoundExprV3,
}

/// Template expression tree inside [`WireTemplateFormulaV3`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireTemplateExprV3 {
    /// Scalar literal.
    Literal(WireLiteralV1),
    /// Constant 2D array literal.
    ArrayLiteral {
        /// Row count.
        rows: u32,
        /// Column count.
        cols: u32,
        /// Row-major elements.
        elements: Vec<WireLiteralV1>,
    },
    /// Template reference.
    Reference(WireTemplateRefV1),
    /// Unresolved identifier or defined name.
    Name(String),
    /// Explicit parentheses.
    Paren(Box<Self>),
    /// Unary operation.
    Unary {
        /// Operator.
        op: WireUnaryOpV1,
        /// Inner expression.
        expr: Box<Self>,
    },
    /// Binary operation.
    Binary {
        /// Operator.
        op: WireBinaryOpV1,
        /// Left expression.
        left: Box<Self>,
        /// Right expression.
        right: Box<Self>,
    },
    /// Function call.
    Call {
        /// Uppercase function name.
        name: String,
        /// Arguments.
        args: Vec<Self>,
    },
    /// Worksheet-qualified defined name.
    ScopedName {
        /// Resolved worksheet identity.
        sheet: Option<u32>,
        /// Preserved unresolved worksheet name.
        missing_sheet: Option<String>,
        /// Identifier spelling.
        name: String,
    },
    /// Dynamic-array spill-range operator (`expr#`).
    SpillRef(Box<Self>),
    /// Unsupported external formula preserved verbatim.
    Opaque {
        /// Original raw formula text.
        raw: String,
    },
    /// Unsupported external formula with preserved cached scalar evaluation.
    PreservedCached {
        /// Original raw formula text.
        raw: String,
        /// Cached scalar value.
        cached: WireScalarValueV1,
    },
    /// Omitted optional argument in a function call.
    MissingArg,
}

/// Relative-ID formula template carried by [`WireOpV3::FillFormula`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireTemplateFormulaV3 {
    /// Array evaluation mode.
    pub mode: WireFormulaModeV1,
    /// Root template expression.
    pub root: WireTemplateExprV3,
}

/// Cell content payload in wire v3.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireCellInputV3 {
    /// Empty cell.
    Empty,
    /// Scalar constant.
    Scalar(WireScalarValueV1),
    /// Bound formula plus optional cached evaluation result.
    Formula {
        /// Bound formula.
        formula: WireBoundFormulaV3,
        /// Cached scalar value, if known.
        cached: Option<WireScalarValueV1>,
    },
}

/// Single cell edit inside [`WireOpV3::SetCells`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireCellEditV3 {
    /// Row identity.
    pub row: u64,
    /// Column identity.
    pub col: u64,
    /// Cell input.
    pub input: WireCellInputV3,
    /// Explicit number-format change.
    pub format: WireFormatPatchV1,
}

/// Expected-state cell edit inside [`WireOpV3::SetCellsIf`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireConditionalCellEditV3 {
    /// Target row identity.
    pub row: u64,
    /// Target column identity.
    pub col: u64,
    /// Expected prior cell value/formula.
    pub expected_input: WireCellInputV3,
    /// Expected explicit per-cell number format (`None` = no override expected).
    pub expected_format: Option<WireNumberFormatV1>,
    /// Replacement cell input when expectation matches.
    pub new_input: WireCellInputV3,
    /// Replacement format patch when expectation matches.
    pub new_format: WireFormatPatchV1,
}

/// Atomic workbook mutation on wire v3 (`M3a`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireOpV3 {
    /// Discrete cell edits.
    SetCells {
        /// Worksheet ID.
        sheet: u32,
        /// Cell edits.
        edits: Vec<WireCellEditV3>,
    },
    /// Conditional cell edits (E19 collaborative undo).
    SetCellsIf {
        /// Worksheet ID.
        sheet: u32,
        /// Conditional cell edits.
        edits: Vec<WireConditionalCellEditV3>,
    },
    /// Clears values and formulas across `rows × cols`.
    ClearRange {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs.
        rows: Vec<WireIdRunV1>,
        /// Column runs.
        cols: Vec<WireIdRunV1>,
    },
    /// Inserts fresh row run after `after`.
    InsertRows {
        /// Worksheet ID.
        sheet: u32,
        /// Predecessor row ID (`None` = top).
        after: Option<u64>,
        /// Inserted row run.
        ids: WireIdRunV1,
    },
    /// Inserts fresh column run after `after`.
    InsertCols {
        /// Worksheet ID.
        sheet: u32,
        /// Predecessor column ID (`None` = left).
        after: Option<u64>,
        /// Inserted column run.
        ids: WireIdRunV1,
    },
    /// Tombstones row runs.
    DeleteRows {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Tombstones column runs.
    DeleteCols {
        /// Worksheet ID.
        sheet: u32,
        /// Column runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Restores tombstoned row runs.
    UndeleteRows {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Restores tombstoned column runs.
    UndeleteCols {
        /// Worksheet ID.
        sheet: u32,
        /// Column runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Moves row runs after `after` (with E25 range-anchor handoff).
    MoveRows {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs to move.
        ids: Vec<WireIdRunV1>,
        /// Destination predecessor (`None` = top).
        after: Option<u64>,
    },
    /// Moves column runs after `after` (with E25 range-anchor handoff).
    MoveCols {
        /// Worksheet ID.
        sheet: u32,
        /// Column runs to move.
        ids: Vec<WireIdRunV1>,
        /// Destination predecessor (`None` = left).
        after: Option<u64>,
    },
    /// Sets or resets custom row pixel heights.
    ResizeRows {
        /// Worksheet ID.
        sheet: u32,
        /// `(row_run, height_px)` pairs.
        sizes: Vec<(WireIdRunV1, Option<u16>)>,
    },
    /// Sets or resets custom column pixel widths.
    ResizeCols {
        /// Worksheet ID.
        sheet: u32,
        /// `(col_run, width_px)` pairs.
        sizes: Vec<(WireIdRunV1, Option<u16>)>,
    },
    /// Sets frozen rows and columns.
    SetFreeze {
        /// Worksheet ID.
        sheet: u32,
        /// Frozen top rows.
        frozen_rows: u32,
        /// Frozen left columns.
        frozen_cols: u32,
    },
    /// Applies number format and/or composable style patch.
    SetFormat {
        /// Worksheet ID.
        sheet: u32,
        /// Scope target.
        target: WireFormatTargetV1,
        /// Number format patch.
        number_format: WireFormatPatchV1,
        /// Style patch.
        style: WireStylePatchV1,
    },
    /// Writes a column-major rectangular block of cells.
    SetBlock {
        /// Worksheet ID.
        sheet: u32,
        /// Target row runs.
        rows: Vec<WireIdRunV1>,
        /// Target column IDs.
        cols: Vec<u64>,
        /// Column-major cell vectors.
        columns_data: Vec<Vec<WireCellInputV3>>,
    },
    /// Fills `rows × cols` using a constant-size relative formula template.
    FillFormula {
        /// Worksheet ID.
        sheet: u32,
        /// Target row runs.
        rows: Vec<WireIdRunV1>,
        /// Target column runs.
        cols: Vec<WireIdRunV1>,
        /// Formula template.
        template: WireTemplateFormulaV3,
    },
    /// Adds a worksheet.
    AddSheet {
        /// Fresh worksheet ID.
        sheet: u32,
        /// Requested tab name.
        name: String,
        /// Predecessor sheet in tab order (`None` = first).
        after: Option<u32>,
    },
    /// Renames a worksheet.
    RenameSheet {
        /// Worksheet ID.
        sheet: u32,
        /// New tab name.
        name: String,
    },
    /// Tombstones a worksheet.
    DeleteSheet {
        /// Worksheet ID.
        sheet: u32,
    },
    /// Restores a tombstoned worksheet.
    UndeleteSheet {
        /// Worksheet ID.
        sheet: u32,
    },
    /// Reorders a worksheet in the tab bar.
    MoveSheet {
        /// Worksheet ID.
        sheet: u32,
        /// Predecessor sheet (`None` = first).
        after: Option<u32>,
    },
    /// Permutes cell contents, explicit formats, and explicit styles across rows
    /// inside `rows × cols` without altering axis identities (E15 sort).
    PermuteContent {
        /// Target worksheet ID.
        sheet: u32,
        /// Unique live row identities.
        rows: Vec<u64>,
        /// Unique live column identities.
        cols: Vec<u64>,
        /// Permutation over `0..rows.len()` mapping `dst_idx -> src_idx`.
        row_perm: Vec<u32>,
    },
    /// Cuts and pastes a contiguous cell block under E14/E24 reference rewrite rules.
    MoveCells {
        /// Source worksheet ID.
        src_sheet: u32,
        /// Contiguous live source row IDs.
        src_rows: Vec<u64>,
        /// Contiguous live source column IDs.
        src_cols: Vec<u64>,
        /// Destination worksheet ID.
        dst_sheet: u32,
        /// Contiguous live destination row IDs.
        dst_rows: Vec<u64>,
        /// Contiguous live destination column IDs.
        dst_cols: Vec<u64>,
    },
    /// Defines or updates a workbook- or sheet-scoped named range.
    DefineNamedRange {
        /// Workbook (`None`) or sheet (`Some(sheet_id)`) scope.
        scope: Option<u32>,
        /// Display identifier.
        name: String,
        /// Formula payload.
        formula: WireBoundFormulaV3,
        /// Optional comment.
        comment: Option<String>,
        /// Whether hidden from Name Manager / autocomplete.
        hidden: bool,
    },
    /// Deletes an existing workbook- or sheet-scoped named range.
    DeleteNamedRange {
        /// Workbook (`None`) or sheet (`Some(sheet_id)`) scope.
        scope: Option<u32>,
        /// Identifier to delete.
        name: String,
    },
}

/// Named batch of atomic v3 operations forming one user transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireTxnV3 {
    /// Human-readable undo/history label.
    pub label: String,
    /// Atomic operations executed in order.
    pub ops: Vec<WireOpV3>,
}

/// Unacknowledged client transaction queued for server sequencing (M3a).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WirePendingTxnV3 {
    /// Originating replica ID.
    pub replica: u32,
    /// Monotonic client-local sequence number.
    pub client_seq: u64,
    /// Latest server sequence observed when authored.
    pub base_server_seq: u64,
    /// Wall-clock authoring timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Authored transaction payload.
    pub txn: WireTxnV3,
}

/// Committed transaction frame inside an `RTCLWAL3` log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireWalRecordV3 {
    /// Strictly contiguous sequence number (`base_snapshot_seq + 1`, ...).
    pub seq: u64,
    /// Wall-clock timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Human-readable transaction label.
    pub label: String,
    /// Atomic operations in this transaction.
    pub ops: Vec<WireOpV3>,
}

/// Sequenced authoritative transaction returned in `Commit` and catch-up batches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireCommittedTxnV3 {
    /// Global monotonically increasing server sequence number (`1..`).
    pub seq: u64,
    /// Originating replica ID.
    pub replica: u32,
    /// Author's monotonic client sequence number.
    pub client_seq: u64,
    /// Server-verified authoring user UUID bytes.
    pub user_id: [u8; 16],
    /// Authoritative server commit timestamp (Unix milliseconds).
    pub timestamp_ms: u64,
    /// Committed transaction payload.
    pub txn: WireTxnV3,
}

/// Catch-up payload returned inside [`WireSyncFrameV2::Welcome`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireCatchupV2 {
    /// Inline transaction catch-up (`<= catchup_window` commits).
    Txns {
        /// First server sequence included (`last_applied_seq + 1`).
        from_seq: u64,
        /// Ordered contiguous committed transactions.
        txns: Vec<WireCommittedTxnV3>,
    },
    /// Inline catch-up unavailable or oversized; fetch snapshot first.
    SnapshotRequired {
        /// Sequence number covered by the snapshot at `url`.
        snapshot_seq: u64,
        /// HTTP fetch path (e.g. `/api/workbooks/{id}/snapshots/{seq}`).
        url: String,
    },
}

/// WebSocket binary protocol v2 frame envelope (`SYNC_PROTOCOL_VERSION_V2 = 2`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireSyncFrameV2 {
    /// Client -> Server handshake initiation.
    Hello {
        /// Protocol version (`SYNC_PROTOCOL_VERSION_V2 = 2`).
        protocol_version: u8,
        /// Document UUID bytes.
        doc_id: [u8; 16],
        /// Branch UUID bytes (`0` = main branch).
        branch_id: [u8; 16],
        /// Existing persistent replica ID, if reconnecting in same epoch.
        replica: Option<u32>,
        /// Client's latest durably applied `confirmed_seq`.
        last_applied_seq: u64,
        /// Last server `log_epoch` known to the client.
        known_epoch: u8,
    },
    /// Server -> Client handshake acknowledgement + catch-up plan.
    Welcome {
        /// Authoritative server `log_epoch`.
        epoch: u8,
        /// Server head sequence at handshake moment.
        head_seq: u64,
        /// Assigned writable replica ID, or `0` (`VIEWER_REPLICA`).
        replica: u32,
        /// Highest `client_seq` durably committed for `replica`.
        last_client_seq: u64,
        /// Highest ID counter observed for `(epoch, replica)`.
        max_id_counter: u32,
        /// Catch-up transactions or snapshot instruction.
        catchup: WireCatchupV2,
    },
    /// Client -> Server batch of pending transactions (contiguous `client_seq`).
    Push {
        /// Ordered pending transactions (`1..=64`, encoded frame `<= 2 MiB`).
        txns: Vec<WirePendingTxnV3>,
    },
    /// Server -> Submitting client acknowledgement.
    Ack {
        /// Submitting replica ID.
        replica: u32,
        /// Acknowledged `client_seq`.
        client_seq: u64,
        /// Assigned authoritative server `seq`.
        seq: u64,
        /// Server commit timestamp (Unix milliseconds).
        timestamp_ms: u64,
    },
    /// Server -> Client rejection of a pending transaction or handshake.
    Reject {
        /// Rejected `client_seq` (`0` for handshake rejections).
        client_seq: u64,
        /// Machine-readable reason.
        reason: WireRejectReasonV1,
    },
    /// Server -> All members broadcast of one newly committed transaction.
    Commit {
        /// Global server sequence (`1..`).
        seq: u64,
        /// Originating replica ID.
        replica: u32,
        /// Author's `client_seq`.
        client_seq: u64,
        /// Verified author user UUID bytes.
        user_id: [u8; 16],
        /// Server commit timestamp (Unix milliseconds).
        timestamp_ms: u64,
        /// Committed payload.
        txn: WireTxnV3,
    },
    /// Server -> All members periodic state-digest verification frame (`D-026`).
    Digest {
        /// Sequence number at which `state_digest` was computed.
        seq: u64,
        /// 192-bit snapshot of server `StateDigest`.
        state_digest: WireStateDigestV1,
    },
    /// Bidirectional ephemeral cursor / selection presence broadcast.
    Presence(WirePresenceV1),
    /// Keepalive ping.
    Ping(u64),
    /// Keepalive pong.
    Pong(u64),
}

/// One persisted v3 cell formula entry inside `RTCLSNAP` section `9` (`FormulasV3`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireFormulaEntryV3 {
    /// Worksheet ID.
    pub sheet: u32,
    /// Row ID.
    pub row: u64,
    /// Column ID.
    pub col: u64,
    /// Bound v3 formula AST.
    pub formula: WireBoundFormulaV3,
}

/// One defined named range stored in `RTCLSNAP` section `10` (`NamedRanges`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireNamedRangeEntryV1 {
    /// Workbook (`None`) or sheet (`Some(sheet_id)`) scope.
    pub scope: Option<u32>,
    /// Original display spelling.
    pub name: String,
    /// Formula definition.
    pub formula: WireBoundFormulaV3,
    /// Optional descriptive note.
    pub comment: Option<String>,
    /// Whether hidden from Name Manager / autocomplete.
    pub hidden: bool,
}

/// `NamedRanges` section payload (`RTCLSNAP` optional section `10`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WireNamedRangesSectionV1 {
    /// All workbook- and sheet-scoped named range definitions.
    pub entries: Vec<WireNamedRangeEntryV1>,
}

/// Compact committed `PermuteContent` mapping persisted for E16 across snapshot
/// compaction and server cold restarts (`17-m3a.md` 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCommittedPermuteV3 {
    /// Committed server sequence number where this sort/permutation took effect.
    pub seq: u64,
    /// Originating replica ID (`0` when synthetic/local).
    pub replica: u32,
    /// Originating author `client_seq` (`0` when synthetic/local).
    pub client_seq: u64,
    /// Worksheet ID permuted.
    pub sheet: u32,
    /// Sorted list of column IDs participating in the permutation.
    pub cols: Vec<u64>,
    /// Non-identity `(old_row_id, new_row_id)` pairs mapping original source
    /// record `RowId` -> destination `RowId`.
    pub row_pairs: Vec<(u64, u64)>,
}

/// `PermuteLog` section payload (`RTCLSNAP` optional section `11`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePermuteLogSectionV1 {
    /// Ordered history of non-identity committed permutations up to snapshot `seq`.
    pub entries: Vec<WireCommittedPermuteV3>,
}

// ------------------------------------------------------------ Lossless V1/V2 -> V3 upgrades

impl From<WireBoundExprV1> for WireBoundExprV3 {
    fn from(e: WireBoundExprV1) -> Self {
        match e {
            WireBoundExprV1::Literal(l) => Self::Literal(l),
            WireBoundExprV1::ArrayLiteral {
                rows,
                cols,
                elements,
            } => Self::ArrayLiteral {
                rows,
                cols,
                elements,
            },
            WireBoundExprV1::Reference(r) => Self::Reference(r),
            WireBoundExprV1::Name(n) => Self::Name(n),
            WireBoundExprV1::Paren(p) => Self::Paren(Box::new((*p).into())),
            WireBoundExprV1::Unary { op, expr } => Self::Unary {
                op,
                expr: Box::new((*expr).into()),
            },
            WireBoundExprV1::Binary { op, left, right } => Self::Binary {
                op,
                left: Box::new((*left).into()),
                right: Box::new((*right).into()),
            },
            WireBoundExprV1::Call { name, args } => Self::Call {
                name,
                args: args.into_iter().map(Self::from).collect(),
            },
        }
    }
}

impl From<WireBoundFormulaV1> for WireBoundFormulaV3 {
    fn from(f: WireBoundFormulaV1) -> Self {
        Self {
            mode: f.mode,
            root: f.root.into(),
        }
    }
}

impl From<WireTemplateExprV1> for WireTemplateExprV3 {
    fn from(e: WireTemplateExprV1) -> Self {
        match e {
            WireTemplateExprV1::Literal(l) => Self::Literal(l),
            WireTemplateExprV1::ArrayLiteral {
                rows,
                cols,
                elements,
            } => Self::ArrayLiteral {
                rows,
                cols,
                elements,
            },
            WireTemplateExprV1::Reference(r) => Self::Reference(r),
            WireTemplateExprV1::Name(n) => Self::Name(n),
            WireTemplateExprV1::Paren(p) => Self::Paren(Box::new((*p).into())),
            WireTemplateExprV1::Unary { op, expr } => Self::Unary {
                op,
                expr: Box::new((*expr).into()),
            },
            WireTemplateExprV1::Binary { op, left, right } => Self::Binary {
                op,
                left: Box::new((*left).into()),
                right: Box::new((*right).into()),
            },
            WireTemplateExprV1::Call { name, args } => Self::Call {
                name,
                args: args.into_iter().map(Self::from).collect(),
            },
        }
    }
}

impl From<WireTemplateFormulaV1> for WireTemplateFormulaV3 {
    fn from(t: WireTemplateFormulaV1) -> Self {
        Self {
            mode: t.mode,
            root: t.root.into(),
        }
    }
}

impl From<WireCellInputV1> for WireCellInputV3 {
    fn from(c: WireCellInputV1) -> Self {
        match c {
            WireCellInputV1::Empty => Self::Empty,
            WireCellInputV1::Scalar(s) => Self::Scalar(s),
            WireCellInputV1::Formula { formula, cached } => Self::Formula {
                formula: formula.into(),
                cached,
            },
        }
    }
}

impl From<WireCellEditV1> for WireCellEditV3 {
    fn from(e: WireCellEditV1) -> Self {
        Self {
            row: e.row,
            col: e.col,
            input: e.input.into(),
            format: e.format,
        }
    }
}

impl From<WireConditionalCellEditV2> for WireConditionalCellEditV3 {
    fn from(e: WireConditionalCellEditV2) -> Self {
        Self {
            row: e.row,
            col: e.col,
            expected_input: e.expected_input.into(),
            expected_format: e.expected_format,
            new_input: e.new_input.into(),
            new_format: e.new_format,
        }
    }
}

impl From<WireOpV1> for WireOpV3 {
    fn from(op: WireOpV1) -> Self {
        WireOpV2::from(op).into()
    }
}

impl From<WireOpV2> for WireOpV3 {
    fn from(op: WireOpV2) -> Self {
        match op {
            WireOpV2::SetCells { sheet, edits } => Self::SetCells {
                sheet,
                edits: edits.into_iter().map(WireCellEditV3::from).collect(),
            },
            WireOpV2::SetCellsIf { sheet, edits } => Self::SetCellsIf {
                sheet,
                edits: edits
                    .into_iter()
                    .map(WireConditionalCellEditV3::from)
                    .collect(),
            },
            WireOpV2::ClearRange { sheet, rows, cols } => Self::ClearRange { sheet, rows, cols },
            WireOpV2::InsertRows { sheet, after, ids } => Self::InsertRows { sheet, after, ids },
            WireOpV2::InsertCols { sheet, after, ids } => Self::InsertCols { sheet, after, ids },
            WireOpV2::DeleteRows { sheet, ids } => Self::DeleteRows { sheet, ids },
            WireOpV2::DeleteCols { sheet, ids } => Self::DeleteCols { sheet, ids },
            WireOpV2::UndeleteRows { sheet, ids } => Self::UndeleteRows { sheet, ids },
            WireOpV2::UndeleteCols { sheet, ids } => Self::UndeleteCols { sheet, ids },
            WireOpV2::MoveRows { sheet, ids, after } => Self::MoveRows { sheet, ids, after },
            WireOpV2::MoveCols { sheet, ids, after } => Self::MoveCols { sheet, ids, after },
            WireOpV2::ResizeRows { sheet, sizes } => Self::ResizeRows { sheet, sizes },
            WireOpV2::ResizeCols { sheet, sizes } => Self::ResizeCols { sheet, sizes },
            WireOpV2::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Self::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            },
            WireOpV2::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => Self::SetFormat {
                sheet,
                target,
                number_format,
                style,
            },
            WireOpV2::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            } => Self::SetBlock {
                sheet,
                rows,
                cols,
                columns_data: columns_data
                    .into_iter()
                    .map(|col| col.into_iter().map(WireCellInputV3::from).collect())
                    .collect(),
            },
            WireOpV2::FillFormula {
                sheet,
                rows,
                cols,
                template,
            } => Self::FillFormula {
                sheet,
                rows,
                cols,
                template: template.into(),
            },
            WireOpV2::AddSheet { sheet, name, after } => Self::AddSheet { sheet, name, after },
            WireOpV2::RenameSheet { sheet, name } => Self::RenameSheet { sheet, name },
            WireOpV2::DeleteSheet { sheet } => Self::DeleteSheet { sheet },
            WireOpV2::UndeleteSheet { sheet } => Self::UndeleteSheet { sheet },
            WireOpV2::MoveSheet { sheet, after } => Self::MoveSheet { sheet, after },
        }
    }
}

impl From<WireTxnV1> for WireTxnV3 {
    fn from(t: WireTxnV1) -> Self {
        Self {
            label: t.label,
            ops: t.ops.into_iter().map(WireOpV3::from).collect(),
        }
    }
}

impl From<WireTxnV2> for WireTxnV3 {
    fn from(t: WireTxnV2) -> Self {
        Self {
            label: t.label,
            ops: t.ops.into_iter().map(WireOpV3::from).collect(),
        }
    }
}

impl From<WirePendingTxnV1> for WirePendingTxnV3 {
    fn from(p: WirePendingTxnV1) -> Self {
        Self {
            replica: p.replica,
            client_seq: p.client_seq,
            base_server_seq: p.base_server_seq,
            timestamp_ms: p.timestamp_ms,
            txn: p.txn.into(),
        }
    }
}

impl From<WirePendingTxnV2> for WirePendingTxnV3 {
    fn from(p: WirePendingTxnV2) -> Self {
        Self {
            replica: p.replica,
            client_seq: p.client_seq,
            base_server_seq: p.base_server_seq,
            timestamp_ms: p.timestamp_ms,
            txn: p.txn.into(),
        }
    }
}

impl From<WireWalRecordV1> for WireWalRecordV3 {
    fn from(r: WireWalRecordV1) -> Self {
        Self {
            seq: r.seq,
            timestamp_ms: r.timestamp_ms,
            label: r.label,
            ops: r.ops.into_iter().map(WireOpV3::from).collect(),
        }
    }
}

impl From<WireWalRecordV2> for WireWalRecordV3 {
    fn from(r: WireWalRecordV2) -> Self {
        Self {
            seq: r.seq,
            timestamp_ms: r.timestamp_ms,
            label: r.label,
            ops: r.ops.into_iter().map(WireOpV3::from).collect(),
        }
    }
}

impl From<WireCommittedTxnV2> for WireCommittedTxnV3 {
    fn from(c: WireCommittedTxnV2) -> Self {
        Self {
            seq: c.seq,
            replica: c.replica,
            client_seq: c.client_seq,
            user_id: c.user_id,
            timestamp_ms: c.timestamp_ms,
            txn: c.txn.into(),
        }
    }
}

impl From<WireCatchupV1> for WireCatchupV2 {
    fn from(c: WireCatchupV1) -> Self {
        match c {
            WireCatchupV1::Txns { from_seq, txns } => Self::Txns {
                from_seq,
                txns: txns.into_iter().map(WireCommittedTxnV3::from).collect(),
            },
            WireCatchupV1::SnapshotRequired { snapshot_seq, url } => {
                Self::SnapshotRequired { snapshot_seq, url }
            }
        }
    }
}

impl From<WireSyncFrameV1> for WireSyncFrameV2 {
    fn from(f: WireSyncFrameV1) -> Self {
        match f {
            WireSyncFrameV1::Hello {
                protocol_version,
                doc_id,
                branch_id,
                replica,
                last_applied_seq,
                known_epoch,
            } => Self::Hello {
                protocol_version,
                doc_id,
                branch_id,
                replica,
                last_applied_seq,
                known_epoch,
            },
            WireSyncFrameV1::Welcome {
                epoch,
                head_seq,
                replica,
                last_client_seq,
                max_id_counter,
                catchup,
            } => Self::Welcome {
                epoch,
                head_seq,
                replica,
                last_client_seq,
                max_id_counter,
                catchup: catchup.into(),
            },
            WireSyncFrameV1::Push { txns } => Self::Push {
                txns: txns.into_iter().map(WirePendingTxnV3::from).collect(),
            },
            WireSyncFrameV1::Ack {
                replica,
                client_seq,
                seq,
                timestamp_ms,
            } => Self::Ack {
                replica,
                client_seq,
                seq,
                timestamp_ms,
            },
            WireSyncFrameV1::Reject { client_seq, reason } => Self::Reject { client_seq, reason },
            WireSyncFrameV1::Commit {
                seq,
                replica,
                client_seq,
                user_id,
                timestamp_ms,
                txn,
            } => Self::Commit {
                seq,
                replica,
                client_seq,
                user_id,
                timestamp_ms,
                txn: txn.into(),
            },
            WireSyncFrameV1::Digest { seq, state_digest } => Self::Digest { seq, state_digest },
            WireSyncFrameV1::Presence(p) => Self::Presence(p),
            WireSyncFrameV1::Ping(n) => Self::Ping(n),
            WireSyncFrameV1::Pong(n) => Self::Pong(n),
        }
    }
}

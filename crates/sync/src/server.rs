//! Authoritative single-document synchronization state machine (`ServerDocMachine`).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::id::pack_replica_id;
use reticula_core::ops::Workbook;
use reticula_formula::binder::BoundFormula;
use reticula_wire::convert_v2::{ops_from_wire_txn_v2, ops_to_wire_txn_v2};
use reticula_wire::v2::{
    SYNC_PROTOCOL_VERSION, WireCatchupV1, WireCommittedTxnV2, WirePendingTxnV2, WireRejectReasonV1,
    WireStateDigestV1, WireSyncFrameV1,
};

use crate::types::{
    MemberRole, ReplicaWatermark, ServerLimits, StagedTxn, TryApplyOutcome, apply_ops_atomic,
    rewind_inverses,
};
use crate::validate::{max_id_counter_in_ops, validate_txn_ownership};

/// Authoritative in-memory server state machine for one document branch
/// (`15-m2a.md` 5.3 & `06-sync.md`).
#[derive(Clone, Debug)]
pub struct ServerDocMachine {
    doc_id: [u8; 16],
    branch_id: [u8; 16],
    snapshot_url_prefix: String,
    epoch: u8,
    head_seq: u64,
    next_per_epoch_replica: u16,
    workbook: Workbook<BoundFormula>,
    replicas: BTreeMap<u32, ReplicaWatermark>,
    recent_txns: VecDeque<WireCommittedTxnV2>,
    /// Lookup `(replica, client_seq) -> (seq, timestamp_ms)` for exact idempotent resend Acks.
    receipts: BTreeMap<(u32, u64), (u64, u64)>,
    last_snapshot_seq: u64,
    txns_since_snapshot: u32,
    bytes_since_snapshot: usize,
    limits: ServerLimits,
}

impl ServerDocMachine {
    /// Initializes an empty collaborative document (`Sheet1`, `seq = 0`, `epoch = 0`).
    #[must_use]
    pub fn new(
        doc_id: [u8; 16],
        branch_id: [u8; 16],
        snapshot_url_prefix: String,
        limits: ServerLimits,
    ) -> Self {
        Self {
            doc_id,
            branch_id,
            snapshot_url_prefix,
            epoch: 0,
            head_seq: 0,
            next_per_epoch_replica: 1,
            workbook: Workbook::new(),
            replicas: BTreeMap::new(),
            recent_txns: VecDeque::new(),
            receipts: BTreeMap::new(),
            last_snapshot_seq: 0,
            txns_since_snapshot: 0,
            bytes_since_snapshot: 0,
            limits,
        }
    }

    /// Reconstructs server state after restart or backup restore from a loaded
    /// snapshot (`workbook` at `snapshot_seq`), persisted `replicas`, `tail_commits`
    /// (`seq > snapshot_seq`), and `epoch`.
    #[expect(
        clippy::too_many_arguments,
        reason = "cold reload binds persisted DB snapshot, watermarks, epoch and tail log cleanly"
    )]
    pub fn from_snapshot_and_log(
        doc_id: [u8; 16],
        branch_id: [u8; 16],
        snapshot_url_prefix: String,
        epoch: u8,
        snapshot_seq: u64,
        mut workbook: Workbook<BoundFormula>,
        next_per_epoch_replica: u16,
        watermarks: Vec<ReplicaWatermark>,
        tail_commits: Vec<WireCommittedTxnV2>,
        limits: ServerLimits,
    ) -> Result<Self, WireRejectReasonV1> {
        let mut replicas = BTreeMap::new();
        let mut receipts = BTreeMap::new();
        let mut max_seq_for_epoch: u16 = 0;

        for wm in watermarks {
            let (wm_epoch, wm_seq) = reticula_core::id::unpack_replica_id(wm.replica);
            if wm_epoch == epoch {
                max_seq_for_epoch = max_seq_for_epoch.max(wm_seq);
            }
            replicas.insert(wm.replica, wm);
        }

        let mut head_seq = snapshot_seq;
        let mut recent_txns = VecDeque::new();
        let mut txns_since_snapshot = 0_u32;
        let mut bytes_since_snapshot = 0_usize;

        for commit in tail_commits {
            if commit.seq <= snapshot_seq {
                continue;
            }
            if commit.seq != head_seq.saturating_add(1) {
                return Err(WireRejectReasonV1::InvalidOp);
            }
            let (_, ops) = ops_from_wire_txn_v2(commit.txn.clone())
                .map_err(|_| WireRejectReasonV1::InvalidOp)?;
            let _ =
                apply_ops_atomic(&mut workbook, &ops).map_err(|_| WireRejectReasonV1::InvalidOp)?;

            let used_max = max_id_counter_in_ops(epoch, commit.replica, &ops);
            let wm = replicas.entry(commit.replica).or_insert(ReplicaWatermark {
                replica: commit.replica,
                user_id: commit.user_id,
                last_client_seq: 0,
                max_id_counter: 0,
            });
            wm.last_client_seq = wm.last_client_seq.max(commit.client_seq);
            wm.max_id_counter = wm.max_id_counter.max(used_max);
            receipts.insert(
                (commit.replica, commit.client_seq),
                (commit.seq, commit.timestamp_ms),
            );

            let (c_epoch, c_seq) = reticula_core::id::unpack_replica_id(commit.replica);
            if c_epoch == epoch {
                max_seq_for_epoch = max_seq_for_epoch.max(c_seq);
            }

            let approx_bytes = postcard::to_allocvec(&commit.txn).map_or(64, |v| v.len());
            bytes_since_snapshot = bytes_since_snapshot.saturating_add(approx_bytes);
            txns_since_snapshot = txns_since_snapshot.saturating_add(1);
            head_seq = commit.seq;
            recent_txns.push_back(commit);
        }

        let safe_next = next_per_epoch_replica
            .max(1)
            .max(max_seq_for_epoch.saturating_add(1));

        let mut machine = Self {
            doc_id,
            branch_id,
            snapshot_url_prefix,
            epoch,
            head_seq,
            next_per_epoch_replica: safe_next,
            workbook,
            replicas,
            recent_txns,
            receipts,
            last_snapshot_seq: snapshot_seq,
            txns_since_snapshot,
            bytes_since_snapshot,
            limits,
        };
        machine.trim_recent_txns();
        Ok(machine)
    }

    /// Document UUID bytes.
    #[must_use]
    pub const fn doc_id(&self) -> [u8; 16] {
        self.doc_id
    }

    /// Branch UUID bytes.
    #[must_use]
    pub const fn branch_id(&self) -> [u8; 16] {
        self.branch_id
    }

    /// Current authoritative `log_epoch`.
    #[must_use]
    pub const fn epoch(&self) -> u8 {
        self.epoch
    }

    /// Current authoritative committed `head_seq`.
    #[must_use]
    pub const fn head_seq(&self) -> u64 {
        self.head_seq
    }

    /// Sequence covered by the latest persisted snapshot (`0` initially).
    #[must_use]
    pub const fn last_snapshot_seq(&self) -> u64 {
        self.last_snapshot_seq
    }

    /// Next per-epoch replica counter to allocate (`1..=65_535`).
    #[must_use]
    pub const fn next_per_epoch_replica(&self) -> u16 {
        self.next_per_epoch_replica
    }

    /// Read-only reference to the authoritative in-memory workbook.
    #[must_use]
    pub const fn workbook(&self) -> &Workbook<BoundFormula> {
        &self.workbook
    }

    /// Current collaborative 192-bit `StateDigest`.
    #[must_use]
    pub fn state_digest(&self) -> WireStateDigestV1 {
        WireStateDigestV1::from(self.workbook.state_digest())
    }

    /// Ready-to-send periodic [`WireSyncFrameV1::Digest`] frame at current `head_seq`.
    #[must_use]
    pub fn digest_frame(&self) -> WireSyncFrameV1 {
        WireSyncFrameV1::Digest {
            seq: self.head_seq,
            state_digest: self.state_digest(),
        }
    }

    /// Returns all tracked replica watermarks ordered deterministically.
    #[must_use]
    pub fn watermarks(&self) -> Vec<ReplicaWatermark> {
        self.replicas.values().copied().collect()
    }

    /// Returns whether committed transactions or payload bytes since the last
    /// snapshot reached configured thresholds (`15-m2a.md` 7.2).
    #[must_use]
    pub const fn needs_snapshot(&self) -> bool {
        self.txns_since_snapshot >= self.limits.snapshot_txn_threshold
            || self.bytes_since_snapshot >= self.limits.snapshot_bytes_threshold
    }

    /// Advances snapshot tracking after a durable snapshot at `seq` has been written.
    pub fn mark_snapshot_written(&mut self, seq: u64) {
        if seq > self.last_snapshot_seq && seq <= self.head_seq {
            self.last_snapshot_seq = seq;
            self.txns_since_snapshot = 0;
            self.bytes_since_snapshot = 0;
            self.trim_recent_txns();
        }
    }

    /// Allocates and registers a fresh replica ID for `user_id` in the current epoch.
    pub fn allocate_replica(&mut self, user_id: [u8; 16]) -> Option<u32> {
        let seq = self.next_per_epoch_replica;
        let replica = pack_replica_id(self.epoch, seq)?;
        if seq == u16::MAX {
            return None;
        }
        self.next_per_epoch_replica = seq.saturating_add(1);
        self.replicas.insert(
            replica,
            ReplicaWatermark {
                replica,
                user_id,
                last_client_seq: 0,
                max_id_counter: 0,
            },
        );
        Some(replica)
    }

    /// Processes an incoming `Hello` handshake frame cleanly, checking permissions,
    /// protocol version, epoch/replica ownership, and catch-up eligibility.
    pub fn on_hello(
        &mut self,
        user_id: [u8; 16],
        role: Option<MemberRole>,
        frame: &WireSyncFrameV1,
    ) -> Result<WireSyncFrameV1, WireRejectReasonV1> {
        let _member_role = role.ok_or(WireRejectReasonV1::Forbidden)?;
        let WireSyncFrameV1::Hello {
            protocol_version,
            doc_id,
            branch_id,
            replica,
            last_applied_seq,
            known_epoch,
        } = frame
        else {
            return Err(WireRejectReasonV1::InvalidOp);
        };
        if *protocol_version != SYNC_PROTOCOL_VERSION {
            return Err(WireRejectReasonV1::UpgradeRequired);
        }
        if *doc_id != self.doc_id || *branch_id != self.branch_id {
            return Err(WireRejectReasonV1::InvalidOp);
        }

        let (assigned_replica, last_client_seq, max_id_counter, effective_last_seq) =
            if *known_epoch != self.epoch {
                let r = self
                    .allocate_replica(user_id)
                    .ok_or(WireRejectReasonV1::QuotaExceeded)?;
                (r, 0, 0, 0)
            } else if let Some(r) = *replica {
                let Some(wm) = self.replicas.get(&r).copied() else {
                    return Err(WireRejectReasonV1::Forbidden);
                };
                if wm.user_id != user_id {
                    return Err(WireRejectReasonV1::Forbidden);
                }
                let eff_seq = if *last_applied_seq > self.head_seq {
                    0
                } else {
                    *last_applied_seq
                };
                (r, wm.last_client_seq, wm.max_id_counter, eff_seq)
            } else {
                let r = self
                    .allocate_replica(user_id)
                    .ok_or(WireRejectReasonV1::QuotaExceeded)?;
                let eff_seq = if *last_applied_seq > self.head_seq {
                    0
                } else {
                    *last_applied_seq
                };
                (r, 0, 0, eff_seq)
            };

        let catchup = self.build_catchup(effective_last_seq);
        Ok(WireSyncFrameV1::Welcome {
            epoch: self.epoch,
            head_seq: self.head_seq,
            replica: assigned_replica,
            last_client_seq,
            max_id_counter,
            catchup,
        })
    }

    fn build_catchup(&self, effective_last_seq: u64) -> WireCatchupV1 {
        let from_seq = effective_last_seq.saturating_add(1);
        if effective_last_seq == self.head_seq {
            return WireCatchupV1::Txns {
                from_seq,
                txns: Vec::new(),
            };
        }
        let gap = self.head_seq.saturating_sub(effective_last_seq);
        let earliest_in_mem = self.recent_txns.front().map(|t| t.seq);
        let can_inline = gap <= (self.limits.catchup_window as u64)
            && earliest_in_mem.is_some_and(|e| e <= from_seq);
        if can_inline {
            let txns: Vec<WireCommittedTxnV2> = self
                .recent_txns
                .iter()
                .filter(|t| t.seq >= from_seq)
                .cloned()
                .collect();
            return WireCatchupV1::Txns { from_seq, txns };
        }
        WireCatchupV1::SnapshotRequired {
            snapshot_seq: self.last_snapshot_seq,
            url: format!(
                "{}/snapshots/{}",
                self.snapshot_url_prefix, self.last_snapshot_seq
            ),
        }
    }

    /// Validates `Push` frame batch count and serialized byte cap.
    pub fn validate_push_batch(
        &self,
        txns: &[WirePendingTxnV2],
        raw_frame_bytes: usize,
    ) -> Result<(), WireRejectReasonV1> {
        if txns.len() > self.limits.max_push_batch || raw_frame_bytes > self.limits.max_push_bytes {
            return Err(WireRejectReasonV1::PayloadTooLarge);
        }
        Ok(())
    }

    /// Validates permissions, contiguous `client_seq`, ID ownership, and workbook
    /// limits, atomically staging the transaction in memory (`commit_staged` must
    /// follow once database persistence commits).
    pub fn validate_and_try_apply(
        &mut self,
        user_id: [u8; 16],
        role: Option<MemberRole>,
        session_replica: u32,
        pending: WirePendingTxnV2,
        server_timestamp_ms: u64,
    ) -> Result<TryApplyOutcome, WireRejectReasonV1> {
        let _member_role = role.ok_or(WireRejectReasonV1::Forbidden)?;

        if pending.replica != session_replica {
            return Err(WireRejectReasonV1::Forbidden);
        }
        let Some(wm) = self.replicas.get(&session_replica).copied() else {
            return Err(WireRejectReasonV1::Forbidden);
        };
        if wm.user_id != user_id {
            return Err(WireRejectReasonV1::Forbidden);
        }

        if pending.client_seq == 0 {
            return Err(WireRejectReasonV1::InvalidOp);
        }

        if pending.client_seq <= wm.last_client_seq {
            if let Some(&(orig_seq, orig_ts)) =
                self.receipts.get(&(session_replica, pending.client_seq))
            {
                return Ok(TryApplyOutcome::DuplicateAck {
                    replica: session_replica,
                    client_seq: pending.client_seq,
                    seq: orig_seq,
                    timestamp_ms: orig_ts,
                });
            }
            if let Some(orig) = self
                .recent_txns
                .iter()
                .rev()
                .find(|t| t.replica == session_replica && t.client_seq == pending.client_seq)
            {
                return Ok(TryApplyOutcome::DuplicateAck {
                    replica: session_replica,
                    client_seq: pending.client_seq,
                    seq: orig.seq,
                    timestamp_ms: orig.timestamp_ms,
                });
            }
            return Err(WireRejectReasonV1::InvalidOp);
        }

        let expected = wm.last_client_seq.saturating_add(1);
        if pending.client_seq != expected {
            return Err(WireRejectReasonV1::NonContiguousClientSeq { expected });
        }

        let payload_vec =
            postcard::to_allocvec(&pending.txn).map_err(|_| WireRejectReasonV1::InvalidOp)?;
        if payload_vec.len() > self.limits.max_push_bytes {
            return Err(WireRejectReasonV1::PayloadTooLarge);
        }

        let (label, ops) =
            ops_from_wire_txn_v2(pending.txn).map_err(|_| WireRejectReasonV1::InvalidOp)?;
        let new_max_id =
            validate_txn_ownership(self.epoch, session_replica, wm.max_id_counter, &ops)?;

        let inverses = apply_ops_atomic(&mut self.workbook, &ops)
            .map_err(|_| WireRejectReasonV1::InvalidOp)?;

        if self.workbook.populated_cell_count() > self.limits.max_populated_cells
            || self.workbook.estimated_bytes() > self.limits.max_doc_bytes
        {
            rewind_inverses(&mut self.workbook, inverses);
            return Err(WireRejectReasonV1::QuotaExceeded);
        }

        let next_seq = self.head_seq.saturating_add(1);
        self.head_seq = next_seq;
        if let Some(entry) = self.replicas.get_mut(&session_replica) {
            entry.last_client_seq = pending.client_seq;
            entry.max_id_counter = new_max_id;
        }

        let wire_txn = ops_to_wire_txn_v2(label, &ops);
        Ok(TryApplyOutcome::Staged(StagedTxn {
            committed: WireCommittedTxnV2 {
                seq: next_seq,
                replica: session_replica,
                client_seq: pending.client_seq,
                user_id,
                timestamp_ms: server_timestamp_ms,
                txn: wire_txn,
            },
            inverses,
            payload_bytes: payload_vec.len(),
            prior_watermark: wm,
            max_id_counter: new_max_id,
        }))
    }

    /// Finalizes a previously staged transaction after durable Postgres `COMMIT`,
    /// recording it in `recent_txns`, `receipts`, and snapshot byte/count counters.
    pub fn commit_staged(&mut self, staged: StagedTxn) -> WireCommittedTxnV2 {
        let commit = staged.committed;
        self.receipts.insert(
            (commit.replica, commit.client_seq),
            (commit.seq, commit.timestamp_ms),
        );
        self.txns_since_snapshot = self.txns_since_snapshot.saturating_add(1);
        self.bytes_since_snapshot = self
            .bytes_since_snapshot
            .saturating_add(staged.payload_bytes);
        self.recent_txns.push_back(commit.clone());
        self.trim_recent_txns();
        commit
    }

    /// Rolls back a staged transaction in memory (LIFO order if multiple were
    /// staged in one aborted batch).
    pub fn rollback_staged(&mut self, staged: StagedTxn) {
        rewind_inverses(&mut self.workbook, staged.inverses);
        self.head_seq = self.head_seq.saturating_sub(1);
        self.replicas
            .insert(staged.committed.replica, staged.prior_watermark);
    }

    /// Convenience helper combining [`Self::validate_and_try_apply`] and immediate
    /// [`Self::commit_staged`] for tests and single-step synchronous callers.
    pub fn apply_and_commit(
        &mut self,
        user_id: [u8; 16],
        role: Option<MemberRole>,
        session_replica: u32,
        pending: WirePendingTxnV2,
        server_timestamp_ms: u64,
    ) -> Result<ApplyResult, WireRejectReasonV1> {
        match self.validate_and_try_apply(
            user_id,
            role,
            session_replica,
            pending,
            server_timestamp_ms,
        )? {
            TryApplyOutcome::DuplicateAck {
                replica,
                client_seq,
                seq,
                timestamp_ms,
            } => Ok(ApplyResult::Duplicate {
                replica,
                client_seq,
                seq,
                timestamp_ms,
            }),
            TryApplyOutcome::Staged(staged) => {
                Ok(ApplyResult::Committed(self.commit_staged(staged)))
            }
        }
    }

    /// Returns recent committed transactions strictly after `after_seq`, if
    /// all of them remain buffered in memory.
    #[must_use]
    pub fn txns_since(&self, after_seq: u64) -> Option<Vec<WireCommittedTxnV2>> {
        if after_seq >= self.head_seq {
            return Some(Vec::new());
        }
        let need_from = after_seq.saturating_add(1);
        let earliest = self.recent_txns.front().map(|t| t.seq)?;
        if earliest > need_from {
            return None;
        }
        Some(
            self.recent_txns
                .iter()
                .filter(|t| t.seq >= need_from)
                .cloned()
                .collect(),
        )
    }

    fn trim_recent_txns(&mut self) {
        while self.recent_txns.len() > self.limits.catchup_window {
            let can_drop = self
                .recent_txns
                .front()
                .is_some_and(|front| front.seq <= self.last_snapshot_seq);
            if !can_drop {
                break;
            }
            let _ = self.recent_txns.pop_front();
        }
    }
}

/// Result of [`ServerDocMachine::apply_and_commit`].
#[derive(Clone, Debug, PartialEq)]
pub enum ApplyResult {
    /// Transaction was newly committed at `commit.seq`.
    Committed(WireCommittedTxnV2),
    /// Duplicate `client_seq` was ignored and matched an earlier commit.
    Duplicate {
        /// Originating replica ID.
        replica: u32,
        /// Already-committed `client_seq`.
        client_seq: u64,
        /// Previously assigned server `seq`.
        seq: u64,
        /// Original authoritative commit timestamp.
        timestamp_ms: u64,
    },
}

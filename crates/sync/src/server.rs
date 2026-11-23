//! Authoritative single-document synchronization state machine (`ServerDocMachine`).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::id::pack_replica_id;
use reticula_core::ops::Workbook;
use reticula_formula::binder::BoundFormula;
use reticula_wire::convert_v3::{ops_from_wire_txn_v3, ops_to_wire_txn_v3};
use reticula_wire::v2::{
    SYNC_PROTOCOL_VERSION, WireRejectReasonV1, WireStateDigestV1, WireSyncFrameV1,
};
use reticula_wire::v3::{
    SYNC_PROTOCOL_VERSION_V2, WireCatchupV2, WireCommittedTxnV3, WirePendingTxnV3, WireSyncFrameV2,
};

use crate::types::{
    CommittedPermute, MemberRole, ReplicaWatermark, ServerLimits, StagedTxn, TryApplyOutcome,
    VIEWER_REPLICA, apply_ops_atomic, rewind_inverses,
};
use crate::validate::{
    max_id_counter_in_ops, transform_ops_through_permute, validate_txn_ownership,
};

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
    recent_txns: VecDeque<WireCommittedTxnV3>,
    committed_permutes: Vec<CommittedPermute>,
    /// Lookup `(replica, client_seq) -> (seq, timestamp_ms)` for exact idempotent resend Acks.
    receipts: BTreeMap<(u32, u64), (u64, u64)>,
    last_snapshot_seq: u64,
    non_empty_base: bool,
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
            committed_permutes: Vec::new(),
            receipts: BTreeMap::new(),
            last_snapshot_seq: 0,
            non_empty_base: false,
            txns_since_snapshot: 0,
            bytes_since_snapshot: 0,
            limits,
        }
    }

    /// Reconstructs server state after restart or backup restore from a loaded
    /// snapshot (`workbook` at `snapshot_seq`), persisted `replicas`, E16
    /// `snapshot_permutes`, `tail_commits` (`seq > snapshot_seq`), and `epoch`.
    #[expect(
        clippy::too_many_arguments,
        reason = "cold reload binds persisted DB snapshot, permute log, watermarks, epoch and tail log cleanly"
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
        snapshot_permutes: Vec<CommittedPermute>,
        tail_commits: Vec<WireCommittedTxnV3>,
        limits: ServerLimits,
    ) -> Result<Self, WireRejectReasonV1> {
        let non_empty_base = snapshot_seq == 0
            && workbook.state_digest() != Workbook::<BoundFormula>::new().state_digest();
        let mut replicas = BTreeMap::new();
        let mut receipts = BTreeMap::new();
        let mut max_seq_for_epoch: u16 = 0;
        let mut committed_permutes = snapshot_permutes;

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
            let (_, ops) = ops_from_wire_txn_v3(commit.txn.clone())
                .map_err(|_| WireRejectReasonV1::InvalidOp)?;
            let _ =
                apply_ops_atomic(&mut workbook, &ops).map_err(|_| WireRejectReasonV1::InvalidOp)?;
            for op in &ops {
                if let Some(cp) =
                    CommittedPermute::from_op(commit.seq, commit.replica, commit.client_seq, op)
                {
                    committed_permutes.push(cp);
                }
            }

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
            committed_permutes,
            receipts,
            last_snapshot_seq: snapshot_seq,
            non_empty_base,
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

    /// E16 committed permutation history accumulated across the document lifetime.
    #[must_use]
    pub fn committed_permutes(&self) -> &[CommittedPermute] {
        &self.committed_permutes
    }

    /// Current collaborative 192-bit `StateDigest`.
    #[must_use]
    pub fn state_digest(&self) -> WireStateDigestV1 {
        WireStateDigestV1::from(self.workbook.state_digest())
    }

    /// Ready-to-send periodic [`WireSyncFrameV2::Digest`] frame at current `head_seq`.
    #[must_use]
    pub fn digest_frame(&self) -> WireSyncFrameV2 {
        WireSyncFrameV2::Digest {
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
            let remaining = self.head_seq.saturating_sub(seq);
            self.txns_since_snapshot = u32::try_from(remaining).unwrap_or(u32::MAX);
            let tail = self.recent_txns.iter().filter(|txn| txn.seq > seq);
            let (count, bytes) = tail.fold((0_u64, 0_usize), |(count, bytes), txn| {
                let len = postcard::to_allocvec(&txn.txn).map_or(64, |encoded| encoded.len());
                (count.saturating_add(1), bytes.saturating_add(len))
            });
            self.bytes_since_snapshot = if count == remaining {
                bytes
            } else {
                bytes.max(self.limits.snapshot_bytes_threshold)
            };
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

    /// Registers a replica ID allocated externally (for example by PostgreSQL's
    /// atomic per-workbook allocator across branches), keeping `replicas` and
    /// `next_per_epoch_replica` synchronized without triggering `StaleActor`.
    pub fn register_external_replica(&mut self, replica: u32, user_id: [u8; 16]) {
        let (rep_epoch, rep_seq) = reticula_core::id::unpack_replica_id(replica);
        if rep_epoch == self.epoch {
            self.next_per_epoch_replica =
                self.next_per_epoch_replica.max(rep_seq.saturating_add(1));
        }
        self.replicas.entry(replica).or_insert(ReplicaWatermark {
            replica,
            user_id,
            last_client_seq: 0,
            max_id_counter: 0,
        });
    }

    /// Processes an incoming `Hello` handshake frame cleanly, checking permissions,
    /// protocol version (`1` or `2`), epoch/replica ownership, and catch-up eligibility.
    pub fn on_hello(
        &mut self,
        user_id: [u8; 16],
        role: Option<MemberRole>,
        frame: &WireSyncFrameV2,
    ) -> Result<WireSyncFrameV2, WireRejectReasonV1> {
        self.on_hello_with_external_replica(user_id, role, frame, None)
    }

    /// Variant of [`Self::on_hello`] that uses a caller-provided externally allocated
    /// replica ID whenever the handshake requires a fresh writable replica.
    pub fn on_hello_with_external_replica(
        &mut self,
        user_id: [u8; 16],
        role: Option<MemberRole>,
        frame: &WireSyncFrameV2,
        external_replica: Option<u32>,
    ) -> Result<WireSyncFrameV2, WireRejectReasonV1> {
        let member_role = role.ok_or(WireRejectReasonV1::Forbidden)?;
        let WireSyncFrameV2::Hello {
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
        if *protocol_version != SYNC_PROTOCOL_VERSION_V2
            && *protocol_version != SYNC_PROTOCOL_VERSION
        {
            return Err(WireRejectReasonV1::UpgradeRequired);
        }
        if *doc_id != self.doc_id || *branch_id != self.branch_id {
            return Err(WireRejectReasonV1::InvalidOp);
        }

        let (assigned_replica, last_client_seq, max_id_counter, effective_last_seq) =
            if !member_role.can_write() {
                let eff_seq = if *known_epoch == self.epoch && *last_applied_seq <= self.head_seq {
                    *last_applied_seq
                } else {
                    0
                };
                (VIEWER_REPLICA, 0, 0, eff_seq)
            } else if *known_epoch != self.epoch {
                let r = if let Some(ext) = external_replica {
                    self.register_external_replica(ext, user_id);
                    ext
                } else {
                    self.allocate_replica(user_id)
                        .ok_or(WireRejectReasonV1::QuotaExceeded)?
                };
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
                let r = if let Some(ext) = external_replica {
                    self.register_external_replica(ext, user_id);
                    ext
                } else {
                    self.allocate_replica(user_id)
                        .ok_or(WireRejectReasonV1::QuotaExceeded)?
                };
                let eff_seq = if *last_applied_seq > self.head_seq {
                    0
                } else {
                    *last_applied_seq
                };
                (r, 0, 0, eff_seq)
            };

        let catchup = self.build_catchup(effective_last_seq);
        let welcome = WireSyncFrameV2::Welcome {
            epoch: self.epoch,
            head_seq: self.head_seq,
            replica: assigned_replica,
            last_client_seq,
            max_id_counter,
            catchup,
        };
        if *protocol_version == SYNC_PROTOCOL_VERSION
            && WireSyncFrameV1::try_from(&welcome).is_err()
        {
            return Err(WireRejectReasonV1::UpgradeRequired);
        }
        Ok(welcome)
    }

    fn build_catchup(&self, effective_last_seq: u64) -> WireCatchupV2 {
        if effective_last_seq == 0 && self.non_empty_base {
            return WireCatchupV2::SnapshotRequired {
                snapshot_seq: self.last_snapshot_seq,
                url: format!(
                    "{}/snapshots/{}",
                    self.snapshot_url_prefix, self.last_snapshot_seq
                ),
            };
        }
        let from_seq = effective_last_seq.saturating_add(1);
        if effective_last_seq == self.head_seq {
            return WireCatchupV2::Txns {
                from_seq,
                txns: Vec::new(),
            };
        }
        let gap = self.head_seq.saturating_sub(effective_last_seq);
        let earliest_in_mem = self.recent_txns.front().map(|t| t.seq);
        let can_inline = gap <= (self.limits.catchup_window as u64)
            && earliest_in_mem.is_some_and(|e| e <= from_seq);
        if can_inline {
            let txns: Vec<WireCommittedTxnV3> = self
                .recent_txns
                .iter()
                .filter(|t| t.seq >= from_seq)
                .cloned()
                .collect();
            return WireCatchupV2::Txns { from_seq, txns };
        }
        WireCatchupV2::SnapshotRequired {
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
        txns: &[WirePendingTxnV3],
        raw_frame_bytes: usize,
    ) -> Result<(), WireRejectReasonV1> {
        if txns.len() > self.limits.max_push_batch || raw_frame_bytes > self.limits.max_push_bytes {
            return Err(WireRejectReasonV1::PayloadTooLarge);
        }
        Ok(())
    }

    /// Validates permissions, contiguous `client_seq`, ID ownership, E16 permute
    /// transforms, and workbook limits, atomically staging the transaction in memory
    /// (`commit_staged` must follow once database persistence commits).
    pub fn validate_and_try_apply(
        &mut self,
        user_id: [u8; 16],
        role: Option<MemberRole>,
        session_replica: u32,
        pending: WirePendingTxnV3,
        server_timestamp_ms: u64,
    ) -> Result<TryApplyOutcome, WireRejectReasonV1> {
        let member_role = role.ok_or(WireRejectReasonV1::Forbidden)?;
        if !member_role.can_write() || session_replica == VIEWER_REPLICA {
            return Err(WireRejectReasonV1::Forbidden);
        }

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
            return Ok(TryApplyOutcome::DuplicateAck {
                replica: session_replica,
                client_seq: pending.client_seq,
                seq: self.last_snapshot_seq,
                timestamp_ms: 0,
            });
        }

        let expected = wm.last_client_seq.saturating_add(1);
        if pending.client_seq != expected {
            return Err(WireRejectReasonV1::NonContiguousClientSeq { expected });
        }
        if pending.base_server_seq > self.head_seq {
            return Err(WireRejectReasonV1::InvalidOp);
        }

        let payload_vec =
            postcard::to_allocvec(&pending.txn).map_err(|_| WireRejectReasonV1::InvalidOp)?;
        if payload_vec.len() > self.limits.max_push_bytes {
            return Err(WireRejectReasonV1::PayloadTooLarge);
        }

        let (label, raw_ops) =
            ops_from_wire_txn_v3(pending.txn).map_err(|_| WireRejectReasonV1::InvalidOp)?;
        let new_max_id =
            validate_txn_ownership(self.epoch, session_replica, wm.max_id_counter, &raw_ops)?;
        let ops = transform_ops_through_permute(
            &raw_ops,
            &self.committed_permutes,
            pending.base_server_seq,
            session_replica,
            pending.client_seq,
        );

        let inverses = apply_ops_atomic(&mut self.workbook, &ops)
            .map_err(|_| WireRejectReasonV1::InvalidOp)?;

        if self.workbook.populated_cell_count() > self.limits.max_populated_cells
            || self.workbook.estimated_bytes() > self.limits.max_doc_bytes
        {
            rewind_inverses(&mut self.workbook, inverses);
            return Err(WireRejectReasonV1::QuotaExceeded);
        }

        let Ok(wire_txn) = ops_to_wire_txn_v3(label, &ops) else {
            rewind_inverses(&mut self.workbook, inverses);
            return Err(WireRejectReasonV1::InvalidOp);
        };

        let next_seq = self.head_seq.saturating_add(1);
        self.head_seq = next_seq;
        if let Some(entry) = self.replicas.get_mut(&session_replica) {
            entry.last_client_seq = pending.client_seq;
            entry.max_id_counter = new_max_id;
        }
        let mut staged_permute_count = 0_usize;
        for op in &ops {
            if let Some(cp) =
                CommittedPermute::from_op(next_seq, session_replica, pending.client_seq, op)
            {
                self.committed_permutes.push(cp);
                staged_permute_count = staged_permute_count.saturating_add(1);
            }
        }

        Ok(TryApplyOutcome::Staged(StagedTxn {
            committed: WireCommittedTxnV3 {
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
            staged_permute_count,
        }))
    }

    /// Finalizes a previously staged transaction after durable Postgres `COMMIT`,
    /// recording it in `recent_txns`, `receipts`, and snapshot byte/count counters.
    pub fn commit_staged(&mut self, staged: StagedTxn) -> WireCommittedTxnV3 {
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
        let keep_permutes = self
            .committed_permutes
            .len()
            .saturating_sub(staged.staged_permute_count);
        self.committed_permutes.truncate(keep_permutes);
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
        pending: WirePendingTxnV3,
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
    pub fn txns_since(&self, after_seq: u64) -> Option<Vec<WireCommittedTxnV3>> {
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
    Committed(WireCommittedTxnV3),
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

//! The smallest model of server-ordered synchronisation that still shows what
//! each user ends up seeing. Not the protocol: no network, no failures.
//!
//! Every replica edits its own copy at once, then the server puts all
//! transactions in one order and everyone applies them in that order. A replica
//! shows its confirmed state with its own unconfirmed edits replayed on top.

use crate::model::{Op, RowId, Sheet, row_id};

/// How an edit made without knowledge of a sort is applied after that sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterSort {
    /// To the row identity it named, which by now holds another record.
    SameRow,
    /// To the row its record was sorted into: the target is mapped through
    /// every sort the author had not seen (which is what `base_seq` records).
    FollowRecord,
}

#[derive(Clone, Debug)]
pub struct Txn {
    pub replica: u32,
    /// The last server sequence number the author had applied when editing.
    pub base_seq: u64,
    pub ops: Vec<Op>,
}

pub struct Server {
    pub sheet: Sheet,
    /// Transactions as applied, in order; `log[n]` has sequence number n + 1.
    pub log: Vec<Txn>,
    pub after_sort: AfterSort,
}

impl Server {
    #[must_use]
    pub fn new(sheet: Sheet, after_sort: AfterSort) -> Self {
        Self {
            sheet,
            log: Vec::new(),
            after_sort,
        }
    }

    /// Orders a transaction, applies it, and returns it as every replica must apply it.
    pub fn accept(&mut self, txn: &Txn) -> Txn {
        let ops = rebase(&txn.ops, txn.base_seq, &self.log, self.after_sort);
        for op in &ops {
            // An operation the sheet rejects changes nothing; the rest still apply.
            let _ = self.sheet.apply(op);
        }
        let applied = Txn {
            replica: txn.replica,
            base_seq: txn.base_seq,
            ops,
        };
        self.log.push(applied.clone());
        applied
    }
}

/// Maps the cells an edit writes through the sorts its author had not seen.
fn rebase(ops: &[Op], base_seq: u64, log: &[Txn], after_sort: AfterSort) -> Vec<Op> {
    let mut ops = ops.to_vec();
    if after_sort == AfterSort::SameRow {
        return ops;
    }
    let unseen = usize::try_from(base_seq)
        .unwrap_or(usize::MAX)
        .min(log.len());
    for earlier in &log[unseen..] {
        for sort in &earlier.ops {
            let Op::Permute { rows, sources } = sort else {
                continue;
            };
            let follow = |row: &mut RowId| {
                if let Some(index) = sources.iter().position(|source| source == row) {
                    *row = rows[index];
                }
            };
            for op in &mut ops {
                match op {
                    Op::Set { row, .. } | Op::SetIf { row, .. } => follow(row),
                    _ => {}
                }
            }
        }
    }
    ops
}

pub struct Replica {
    pub id: u32,
    confirmed: Sheet,
    seq: u64,
    pending: Vec<Txn>,
    log: Vec<Txn>,
    after_sort: AfterSort,
    next_counter: u64,
}

impl Replica {
    #[must_use]
    pub fn new(id: u32, sheet: Sheet, after_sort: AfterSort) -> Self {
        Self {
            id,
            confirmed: sheet,
            seq: 0,
            pending: Vec::new(),
            log: Vec::new(),
            after_sort,
            next_counter: 1,
        }
    }

    /// A fresh row identity that no other replica can produce.
    pub fn new_row(&mut self) -> RowId {
        let id = row_id(self.id, self.next_counter);
        self.next_counter += 1;
        id
    }

    /// Edits locally, at once, and returns the transaction to send.
    pub fn edit(&mut self, ops: Vec<Op>) -> Txn {
        let txn = Txn {
            replica: self.id,
            base_seq: self.seq,
            ops,
        };
        self.pending.push(txn.clone());
        txn
    }

    /// Applies the next transaction from the server.
    pub fn receive(&mut self, applied: &Txn) {
        for op in &applied.ops {
            let _ = self.confirmed.apply(op);
        }
        self.log.push(applied.clone());
        self.seq += 1;
        if applied.replica == self.id && !self.pending.is_empty() {
            self.pending.remove(0);
        }
    }

    /// What this user sees: the confirmed state with their own unconfirmed
    /// edits replayed on top.
    #[must_use]
    pub fn visible(&self) -> Sheet {
        let mut sheet = self.confirmed.clone();
        for txn in &self.pending {
            for op in rebase(&txn.ops, txn.base_seq, &self.log, self.after_sort) {
                let _ = sheet.apply(&op);
            }
        }
        sheet
    }
}

/// Runs transactions made at the same time through the server in the given
/// order and returns what everyone agrees on afterwards. Panics if the
/// replicas disagree with the server or with each other: that would be a bug
/// in the model, which is what the tests are for.
#[must_use]
pub fn settle(
    start: &Sheet,
    after_sort: AfterSort,
    edits: &[(u32, Vec<Op>)],
    order: &[usize],
) -> Sheet {
    let mut server = Server::new(start.clone(), after_sort);
    let mut replica_ids: Vec<u32> = edits.iter().map(|(replica, _)| *replica).collect();
    replica_ids.sort_unstable();
    replica_ids.dedup();
    let mut replicas: Vec<Replica> = replica_ids
        .iter()
        .map(|id| Replica::new(*id, start.clone(), after_sort))
        .collect();

    let mut sent = Vec::new();
    for (replica, ops) in edits {
        let author = replicas
            .iter_mut()
            .find(|candidate| candidate.id == *replica);
        if let Some(author) = author {
            sent.push(author.edit(ops.clone()));
        }
    }
    for index in order {
        let applied = server.accept(&sent[*index]);
        for replica in &mut replicas {
            replica.receive(&applied);
        }
    }
    for replica in &replicas {
        assert_eq!(
            replica.visible(),
            server.sheet,
            "replica {} disagrees with the server",
            replica.id
        );
    }
    server.sheet
}

/// Two users creating or renaming sheets to the same name at the same time:
/// the one ordered later gets a suffix. Decided when the operation is applied,
/// so every replica picks the same name.
#[must_use]
pub fn unique_sheet_name(existing: &[String], wanted: &str) -> String {
    if !existing.iter().any(|name| name == wanted) {
        return wanted.to_owned();
    }
    (2..)
        .map(|number| format!("{wanted} ({number})"))
        .find(|candidate| !existing.contains(candidate))
        .unwrap_or_else(|| wanted.to_owned())
}

//! Settlement of spills whose outcome depends on one another (E22).
//!
//! Committing every array as soon as its formula is evaluated is right while
//! spills depend on each other in one direction only. Two situations break
//! that: arrays that want the same cells, and arrays that end up reading cells
//! they spilled themselves. What committing in passing leaves behind then
//! depends on which formula happened to be evaluated first, that is, on the
//! edits that led here. The result has to be a function of the workbook alone,
//! so in either situation the engine drops every spill and starts over:
//!
//! - First the way a recalculation from scratch would go, which clears up
//!   disputes with spills left over from before the edit.
//! - Then once more, taking back the spills of formulas that turn out to depend
//!   on a later one. This finds the order in which conditional spills fit
//!   together, if evaluating them as they come does not.
//! - If there is still a dispute, in rounds. Within a round the ownership of
//!   cells is frozen, so every formula has exactly one value; formulas on a
//!   loop, including loops through owned cells, are `#CYCLE!`. Between rounds
//!   ownership is decided for all arrays together: an array that depends on a
//!   rival yields to it, and rivals without such an order (independent or
//!   mutually dependent) all get `#SPILL!`. The rounds end once ownership stays
//!   the same. When it repeats an earlier state instead, the formulas on the
//!   loops seen since then can never agree with their own spills: they stay
//!   `#CYCLE!` and the rounds go on without them.
//!
//! None of this looks at slot numbers, earlier results or the order of edits.
//! While something is contested or looping, every change that could tip the
//! balance settles it anew.
//!
//! What goes unnoticed is formulas that decide whether to spill by looking for
//! each other's spill: each of the two possible states is free of disputes, and
//! the one reached first stays.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use reticula_core::id::{ColId, IdRun, RowId};
use reticula_core::ops::WorkbookCellKey;
use reticula_core::value::CalcError;
use smallvec::SmallVec;

use super::{
    DirtyRoots, FormulaEngine, Frame, FxSet, STATE_CLEAN, STATE_DIRTY, STATE_EVALUATING,
    STATE_EVALUATING_MAYBE, STATE_MAYBE_DIRTY, STATE_SCC_WAIT,
};
use crate::dep_graph::{is_strip_fid, strip_fid, strip_index};
use crate::spill::{SpillFit, SpillRegistry, footprint_cells};
use crate::value_eval::{Array2D, EvalScalar};

/// What the last settlement left contested or looping. Anything that could
/// change its outcome has to pass through one of these cells.
#[derive(Clone, Debug, Default)]
pub(super) struct SpillTangle {
    /// Contested anchors and formulas held at `#CYCLE!`.
    pub(super) members: BTreeSet<WorkbookCellKey>,
    /// Graph nodes of the members and of everything that depends on them.
    downstream: FxSet<u32>,
    /// The part of `downstream` that leads on to a member or to an array.
    region: FxSet<u32>,
    /// Cells an array wanted at some point and did not keep.
    watch: BTreeSet<WorkbookCellKey>,
    /// Whether formulas were added since `downstream` was collected.
    pub(super) has_new_formulas: bool,
    /// Whether spills must be settled before evaluation goes on.
    pub(super) pending: bool,
}

impl SpillTangle {
    pub(super) fn is_empty(&self) -> bool {
        self.members.is_empty() && self.watch.is_empty()
    }

    pub(super) fn involves(&self, key: WorkbookCellKey) -> bool {
        self.members.contains(&key) || self.watch.contains(&key)
    }

    /// Whether a new value of `node` may change the outcome.
    pub(super) fn reaches(&self, node: u32) -> bool {
        self.region.contains(&node)
    }

    pub(super) fn is_downstream(&self, node: u32) -> bool {
        self.downstream.contains(&node)
    }

    fn watches_any(&self, anchor: WorkbookCellKey, rows: &[RowId], cols: &[ColId]) -> bool {
        !self.watch.is_empty()
            && rows.iter().any(|&row| {
                cols.iter()
                    .any(|&col| self.watch.contains(&(anchor.0, row, col)))
            })
    }
}

/// How an array stands in the current round.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SpillStanding {
    fit: SpillFit,
    /// Whether it lost its cells to a rival, or lost them together with it.
    contested: bool,
}

impl SpillStanding {
    /// Whether the array owns its cells.
    const fn is_open(&self) -> bool {
        !self.contested && matches!(self.fit, SpillFit::Free { .. })
    }
}

/// What a settlement knows about a formula that evaluated to an array.
#[derive(Clone, Debug, Default)]
struct SettlingAnchor {
    /// Latest array and where it would land.
    array: Option<(Array2D, SpillFit)>,
    /// Ownership in force during the current round.
    standing: Option<SpillStanding>,
    /// Footprints asked for in any attempt.
    wanted: Vec<SpillFit>,
}

impl SettlingAnchor {
    fn note_wanted(&mut self, fit: &SpillFit) {
        if !self.wanted.contains(fit) {
            self.wanted.push(fit.clone());
        }
    }

    /// Whether the standing is what `array` and `contested` ask for.
    fn is_settled(&self, contested: bool) -> bool {
        match (&self.array, &self.standing) {
            (Some((_, fit)), Some(standing)) => {
                standing.contested == contested && standing.fit == *fit
            }
            (None, None) => true,
            _ => false,
        }
    }
}

/// Ways to settle a dispute, from the cheapest to the one that always ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettleAttempt {
    /// No dispute so far.
    #[default]
    None,
    /// Evaluate again without the spills of earlier waves.
    Restart,
    /// The same, taking back spills evaluated before one they depend on.
    RestartInOrder,
    /// Rounds.
    Rounds,
}

/// State of a settlement in progress (it can be paused between slices).
#[derive(Clone, Debug, Default)]
pub(super) struct SpillSettlement {
    /// How the current wave deals with spills, once it ran into a dispute.
    attempt: SettleAttempt,
    /// Whether rounds are running.
    pub(super) active: bool,
    /// Formulas that evaluated to an array in any attempt.
    anchors: BTreeMap<WorkbookCellKey, SettlingAnchor>,
    /// Formulas held at `#CYCLE!` for the rest of the settlement.
    pinned: BTreeSet<WorkbookCellKey>,
    /// Formulas re-evaluated without end within one round.
    stuck: BTreeSet<WorkbookCellKey>,
    /// Ownership to recognise a repetition by, replaced after 1, 2, 4, ... rounds.
    mark: Vec<(WorkbookCellKey, SpillStanding)>,
    lap: usize,
    lap_limit: usize,
    rounds: usize,
    /// Loops through owned cells since `mark`, each in key order.
    loops: BTreeSet<Vec<WorkbookCellKey>>,
    /// Anchors whose standing changed since `mark`.
    moved: BTreeSet<WorkbookCellKey>,
}

impl SpillSettlement {
    /// Whether a restarted wave is still looking for an order without disputes.
    pub(super) fn withdraws_stale_spills(&self) -> bool {
        self.attempt == SettleAttempt::RestartInOrder
    }

    /// Ends a restarted wave that got through without a dispute.
    pub(super) fn end_wave(&mut self) {
        if self.attempt != SettleAttempt::None {
            *self = Self::default();
        }
    }

    pub(super) fn holds_at_cycle(&self, key: WorkbookCellKey) -> bool {
        self.pinned.contains(&key)
    }

    pub(super) fn note_stuck(&mut self, key: WorkbookCellKey) {
        self.stuck.insert(key);
    }

    /// Records where an array would land, once a dispute is being settled.
    pub(super) fn note_wanted(&mut self, anchor: WorkbookCellKey, fit: &SpillFit) {
        if self.attempt != SettleAttempt::None {
            self.anchors.entry(anchor).or_default().note_wanted(fit);
        }
    }

    fn standings(&self) -> impl Iterator<Item = (&WorkbookCellKey, &SpillStanding)> {
        self.anchors
            .iter()
            .filter_map(|(key, anchor)| Some((key, anchor.standing.as_ref()?)))
    }

    fn set_mark(&mut self, lap_limit: usize) {
        self.mark = self
            .standings()
            .map(|(key, standing)| (*key, standing.clone()))
            .collect();
        self.lap = 0;
        self.lap_limit = lap_limit;
        self.loops.clear();
        self.moved.clear();
    }
}

/// Footprint of a free array in live axis positions.
struct SpillClaim {
    anchor: WorkbookCellKey,
    rows: (u32, u32),
    cols: (u32, u32),
}

impl FormulaEngine {
    /// Called before the graph node of a formula goes away.
    pub(super) fn note_node_removed(&mut self, node: u32) {
        if self.tangle.is_downstream(node) {
            self.flag_spill_tangle();
        }
    }

    /// Called when a formula gets a graph node.
    pub(super) fn note_node_added(&mut self) {
        if !self.tangle.is_empty() {
            self.tangle.has_new_formulas = true;
        }
    }

    /// Finds out whether formulas added since the last settlement depend on what it
    /// left in dispute.
    pub(super) fn check_new_formulas(&mut self) {
        self.tangle.has_new_formulas = false;
        let walk = self.walk_downstream_of_members(&self.tangle.members, false);
        if walk.nodes().any(|node| !self.tangle.is_downstream(node)) {
            self.flag_spill_tangle();
        }
    }

    /// Requests a settlement, unless one is already running.
    pub(super) fn flag_spill_tangle(&mut self) {
        if !self.settle.active {
            self.tangle.pending = true;
        }
    }

    /// Whether giving `anchor` the footprint `rows × cols` (empty for none)
    /// would take or return a cell the last settlement left in dispute.
    pub(super) fn footprint_change_is_disputed(
        &self,
        anchor: WorkbookCellKey,
        rows: &[RowId],
        cols: &[ColId],
    ) -> bool {
        if self.tangle.is_empty() {
            return false;
        }
        match self.spills.active_spill(anchor) {
            Some(entry) if entry.rows == rows && entry.cols == cols => false,
            Some(entry) => {
                self.tangle.watches_any(anchor, &entry.rows, &entry.cols)
                    || self.tangle.watches_any(anchor, rows, cols)
            }
            None => self.tangle.watches_any(anchor, rows, cols),
        }
    }

    /// Drops a settlement interrupted by an edit; the next recalculation starts
    /// a new one.
    pub(super) fn abandon_settlement(&mut self) {
        if self.settle.attempt == SettleAttempt::None {
            return;
        }
        let run = core::mem::take(&mut self.settle);
        self.tangle.members.extend(run.anchors.into_keys());
        self.tangle.members.extend(run.pinned);
        self.tangle.pending = true;
    }

    /// Drops every spill and makes the next attempt.
    pub(super) fn begin_settlement(&mut self) {
        self.tangle.pending = false;
        self.unwind_call_stack();
        let mut anchors = self.spills.all_anchors();
        anchors.append(&mut self.tangle.members);
        self.tangle.downstream.clear();
        self.tangle.region.clear();
        self.tangle.watch.clear();
        self.tangle.has_new_formulas = false;
        let attempt = match self.settle.attempt {
            SettleAttempt::None => SettleAttempt::Restart,
            SettleAttempt::Restart => SettleAttempt::RestartInOrder,
            SettleAttempt::RestartInOrder | SettleAttempt::Rounds => SettleAttempt::Rounds,
        };
        let mut settling = core::mem::take(&mut self.settle.anchors);
        for anchor in settling.values_mut() {
            anchor.array = None;
            anchor.standing = None;
        }
        self.settle = SpillSettlement {
            attempt,
            active: attempt == SettleAttempt::Rounds,
            anchors: settling,
            lap_limit: 1,
            ..SpillSettlement::default()
        };
        let mut roots = DirtyRoots::default();
        for &anchor in &anchors {
            let released = self.spills.release(anchor);
            self.record_changed_cells(&released.cleared_shadows);
            for &cell in &released.cleared_shadows {
                self.collect_direct_dependents(cell, &mut roots);
            }
            if released.had_state {
                self.collect_spill_anchor_dependents(anchor, &mut roots.singles);
            }
            self.push_cell_root(anchor, &mut roots);
        }
        self.start_settle_round(roots);
    }

    /// After a restart, takes back what spills downstream of the new footprint of
    /// `key` own: they were evaluated without it, and cells they no longer fill
    /// must not count as read by what they depend on.
    pub(super) fn withdraw_spills_downstream_of(
        &mut self,
        key: WorkbookCellKey,
        changed_shadows: &[WorkbookCellKey],
        roots: &mut DirtyRoots,
    ) {
        let mut walk = DependentWalk::default();
        // The spill itself is not stale, whatever else depends on it.
        if let Some(&fid) = self.cell_to_fid.get(&key) {
            walk.seen.insert(fid);
        }
        for &cell in changed_shadows {
            self.push_cell_dependents(cell, &mut walk);
        }
        self.push_spill_ref_dependents(key, &mut walk);
        self.walk_dependents(&mut walk, |cell| self.owned_cells(cell));
        let own_fid = self.cell_to_fid.get(&key);
        let stale: BTreeSet<WorkbookCellKey> = walk
            .seen
            .iter()
            .filter(|&fid| Some(fid) != own_fid)
            .filter_map(|&fid| self.slot_of(fid).map(|s| s.key))
            .filter(|cell| self.spills.active_spill(*cell).is_some())
            .collect();
        for cell in stale {
            let released = self.spills.release(cell);
            self.record_changed_cells(&released.cleared_shadows);
            for &shadow in &released.cleared_shadows {
                self.upgrade_cell_dependents_to_dirty(shadow);
                self.collect_direct_dependents(shadow, roots);
            }
            self.collect_spill_anchor_dependents(cell, &mut roots.singles);
            self.push_cell_root(cell, roots);
        }
    }

    fn start_settle_round(&mut self, roots: DirtyRoots) {
        self.wave_spill_requeues.clear();
        self.propagate_dirty_impl(roots.singles, roots.strips, &[], false);
    }

    /// Returns frames of the paused evaluation to the dirty queue.
    pub(super) fn unwind_call_stack(&mut self) {
        self.wave_deferred_dirty.clear();
        self.cur_strip_eval = None;
        if self.call_stack.is_empty() && self.scc_stack.is_empty() {
            return;
        }
        let paused = core::mem::take(&mut self.call_stack);
        let parked = core::mem::take(&mut self.scc_stack);
        self.scc_low.clear();
        for Frame { node: fid, .. } in paused.into_iter().chain(parked) {
            let state = match self.node_state(fid) {
                STATE_EVALUATING | STATE_SCC_WAIT => STATE_DIRTY,
                STATE_EVALUATING_MAYBE => STATE_MAYBE_DIRTY,
                state @ (STATE_DIRTY | STATE_MAYBE_DIRTY) => state,
                _ => continue,
            };
            self.set_node_state(fid, state);
            self.dirty_queue.push(fid);
            self.dirty_needs_sort = true;
        }
    }

    pub(super) fn push_cell_root(&self, key: WorkbookCellKey, roots: &mut DirtyRoots) {
        if let Some(&fid) = self.cell_to_fid.get(&key) {
            roots.singles.push(fid);
        } else if let Some((sid, off)) = self.strip_covering_cell(key) {
            roots.strips.push((sid, off, off.saturating_add(1)));
        }
    }

    /// Stores what a formula evaluated to in this round. Its footprint stays as
    /// the round found it; a changed shape only counts in the next one.
    pub(super) fn commit_settling(
        &mut self,
        cur_fid: u32,
        key: WorkbookCellKey,
        scalar: EvalScalar,
        array: Option<Array2D>,
    ) {
        let changed_shadows = self.spills.refill(key, array.as_ref());
        let mut shown = scalar;
        match array {
            Some(array) => {
                let anchor = self.settle.anchors.entry(key).or_default();
                let fit = match anchor.array.take() {
                    Some((prev, fit)) if prev.rows == array.rows && prev.cols == array.cols => fit,
                    _ => SpillRegistry::fit(&self.workbook, key, array.rows, array.cols),
                };
                anchor.note_wanted(&fit);
                anchor.array = Some((array, fit));
                if anchor.standing.as_ref().is_some_and(|s| !s.is_open()) {
                    shown = EvalScalar::Error(CalcError::Spill);
                }
            }
            None => {
                if let Some(anchor) = self.settle.anchors.get_mut(&key) {
                    anchor.array = None;
                }
            }
        }
        if self.settle.holds_at_cycle(key) {
            self.loop_members.insert(key);
        } else {
            self.leave_loop(key);
        }
        let anchor_changed = self.store_eval_scalar(key, shown);
        self.set_state(cur_fid, STATE_CLEAN);
        let _ = self.call_stack.pop();
        if anchor_changed {
            self.upgrade_cell_dependents_to_dirty(key);
        }
        if changed_shadows.is_empty() {
            return;
        }
        self.record_changed_cells(&changed_shadows);
        let mut post_dirty = DirtyRoots::default();
        for &cell in &changed_shadows {
            self.upgrade_cell_dependents_to_dirty(cell);
            self.collect_direct_dependents(cell, &mut post_dirty);
        }
        self.collect_spill_anchor_dependents(key, &mut post_dirty.singles);
        self.propagate_dirty_in_wave_roots(post_dirty, &[cur_fid]);
    }

    /// Marks the formulas of one loop `#CYCLE!` for this round.
    pub(super) fn commit_settling_loop(&mut self, loop_fids: &[u32]) {
        let mut keys: Vec<WorkbookCellKey> = Vec::with_capacity(loop_fids.len());
        let mut through_spill = false;
        let mut post_dirty = DirtyRoots::default();
        for &fid in loop_fids {
            if let Some(key) = self.slot_of(fid).map(|s| s.key) {
                keys.push(key);
                through_spill |= self.spills.active_spill(key).is_some();
                if let Some(anchor) = self.settle.anchors.get_mut(&key) {
                    anchor.array = None;
                }
                self.loop_members.insert(key);
                let changed_shadows = self.spills.refill(key, None);
                self.record_changed_cells(&changed_shadows);
                for &cell in &changed_shadows {
                    self.upgrade_cell_dependents_to_dirty(cell);
                    self.collect_direct_dependents(cell, &mut post_dirty);
                }
                if self.store_eval_scalar(key, EvalScalar::Error(CalcError::Cycle)) {
                    self.upgrade_cell_dependents_to_dirty(key);
                    self.spilled_cells_changed = true;
                }
            }
            self.set_state(fid, STATE_CLEAN);
            self.wave_done = self.wave_done.saturating_add(1);
        }
        if through_spill {
            keys.sort_unstable();
            self.settle.loops.insert(keys);
        }
        if !post_dirty.singles.is_empty() || !post_dirty.strips.is_empty() {
            self.propagate_dirty_in_wave_roots(post_dirty, loop_fids);
        }
        self.flush_deferred_dirty();
    }

    /// Decides ownership for the next round, or ends the settlement when nothing
    /// changes any more.
    pub(super) fn close_settle_round(&mut self) {
        let mut roots = DirtyRoots::default();
        let stuck = core::mem::take(&mut self.settle.stuck);
        self.pin_at_cycle(&stuck, &mut roots);

        let contested = self.contested_spills();
        let anchors = &self.settle.anchors;
        let moved: Vec<WorkbookCellKey> = anchors
            .iter()
            .filter(|&(key, anchor)| !anchor.is_settled(contested.contains(key)))
            .map(|(key, _)| *key)
            .collect();
        if moved.is_empty() && stuck.is_empty() {
            self.finish_settlement();
            return;
        }
        self.move_spills(&moved, &contested, &mut roots);

        let run = &mut self.settle;
        run.moved.extend(moved);
        run.rounds = run.rounds.saturating_add(1);
        run.lap = run.lap.saturating_add(1);
        // A chain of n spills needs about n rounds, so running far past that means
        // ownership goes through a long repetition.
        let max_rounds = run.anchors.len().saturating_add(8).saturating_mul(4);
        let repeats = run
            .standings()
            .eq(run.mark.iter().map(|(key, standing)| (key, standing)));
        if repeats || run.rounds > max_rounds {
            let mut pins = smallest_loops(&run.loops);
            if pins.is_empty() {
                pins = core::mem::take(&mut run.moved);
            }
            self.pin_at_cycle(&pins, &mut roots);
        } else if run.lap >= run.lap_limit {
            run.set_mark(run.lap_limit.saturating_mul(2));
        }
        self.start_settle_round(roots);
    }

    fn pin_at_cycle(&mut self, keys: &BTreeSet<WorkbookCellKey>, roots: &mut DirtyRoots) {
        if keys.is_empty() {
            return;
        }
        for &key in keys {
            if let Some(anchor) = self.settle.anchors.get_mut(&key) {
                anchor.array = None;
            }
            self.settle.pinned.insert(key);
            self.push_cell_root(key, roots);
            self.withdraw_standing(key, roots);
        }
        self.settle.rounds = 0;
        self.settle.set_mark(1);
    }

    fn withdraw_standing(&mut self, key: WorkbookCellKey, roots: &mut DirtyRoots) {
        let anchor = self.settle.anchors.get_mut(&key);
        if anchor.and_then(|a| a.standing.take()).is_none() {
            return;
        }
        let released = self.spills.release(key);
        self.record_changed_cells(&released.cleared_shadows);
        for &cell in &released.cleared_shadows {
            self.collect_direct_dependents(cell, roots);
        }
        self.collect_spill_anchor_dependents(key, &mut roots.singles);
    }

    fn move_spills(
        &mut self,
        moved: &[WorkbookCellKey],
        contested: &BTreeSet<WorkbookCellKey>,
        roots: &mut DirtyRoots,
    ) {
        // Cells may pass from one spill to another, so all of them are given back
        // before any is taken.
        for &key in moved {
            self.withdraw_standing(key, roots);
        }
        for &key in moved {
            let Some(anchor) = self.settle.anchors.get_mut(&key) else {
                continue;
            };
            let Some((array, fit)) = &anchor.array else {
                continue;
            };
            let contested = contested.contains(&key);
            anchor.standing = Some(SpillStanding {
                fit: fit.clone(),
                contested,
            });
            let shown = match fit {
                SpillFit::Free { rows, cols } if !contested => {
                    let shown = array.values.first().cloned();
                    let changed =
                        self.spills
                            .materialize(key, rows.clone(), cols.clone(), array.clone());
                    self.record_changed_cells(&changed);
                    for &cell in &changed {
                        self.collect_direct_dependents(cell, roots);
                    }
                    shown.unwrap_or(EvalScalar::Error(CalcError::Calc))
                }
                _ => {
                    let _ = self.spills.block(key, fit.footprint(key));
                    EvalScalar::Error(CalcError::Spill)
                }
            };
            if self.store_eval_scalar(key, shown) {
                self.collect_direct_dependents(key, roots);
                self.spilled_cells_changed = true;
            }
            self.collect_spill_anchor_dependents(key, &mut roots.singles);
        }
    }

    fn finish_settlement(&mut self) {
        let run = core::mem::take(&mut self.settle);
        let mut members = run.pinned;
        for (&key, anchor) in &run.anchors {
            if anchor.standing.as_ref().is_some_and(|s| s.contested) {
                members.insert(key);
            }
        }
        if members.is_empty() {
            return;
        }
        let mut watch = BTreeSet::new();
        for (&key, anchor) in &run.anchors {
            for fit in &anchor.wanted {
                let is_kept = anchor
                    .standing
                    .as_ref()
                    .is_some_and(|s| s.is_open() && s.fit == *fit);
                if !is_kept {
                    watch.extend(fit.footprint(key));
                }
            }
        }
        let walk = self.walk_downstream_of_members(&members, true);
        let targets = members.iter().chain(run.anchors.keys());
        let targets = targets.filter_map(|key| self.cell_to_fid.get(key).copied());
        self.tangle.region = walk.nodes_leading_to(targets);
        self.tangle.downstream = walk.nodes().collect();
        self.tangle.members = members;
        self.tangle.watch = watch;
    }

    fn walk_downstream_of_members(
        &self,
        members: &BTreeSet<WorkbookCellKey>,
        keep_edges: bool,
    ) -> DependentWalk {
        let mut walk = DependentWalk {
            keep_edges,
            ..DependentWalk::default()
        };
        for key in members {
            if let Some(&fid) = self.cell_to_fid.get(key) {
                walk.push_single(fid);
            }
        }
        self.walk_dependents(&mut walk, |cell| self.owned_cells(cell));
        walk
    }

    fn owned_cells(&self, anchor: WorkbookCellKey) -> Option<(&[RowId], &[ColId])> {
        self.spills
            .active_spill(anchor)
            .map(|entry| (entry.rows.as_slice(), entry.cols.as_slice()))
    }

    /// Decides for all arrays together which of them cannot have their cells
    /// because another array wants some of them too.
    fn contested_spills(&self) -> BTreeSet<WorkbookCellKey> {
        let anchors = &self.settle.anchors;
        let mut claims: Vec<SpillClaim> = anchors
            .iter()
            .filter_map(|(&key, anchor)| match &anchor.array {
                Some((_, SpillFit::Free { rows, cols })) => {
                    self.spill_claim(key, rows.len(), cols.len())
                }
                _ => None,
            })
            .collect();
        let rivals = overlapping_claims(&mut claims);
        let downstream: BTreeMap<WorkbookCellKey, BTreeSet<WorkbookCellKey>> = rivals
            .keys()
            .map(|&key| (key, self.rivals_downstream_of(key, &rivals)))
            .collect();
        let feeds = |from: &WorkbookCellKey, to: &WorkbookCellKey| {
            downstream.get(from).is_some_and(|set| set.contains(to))
        };
        // Whoever feeds a rival one-way has strictly fewer rivals feeding it, so
        // this order settles winners before the arrays that yield to them.
        let mut order: Vec<(usize, WorkbookCellKey)> = rivals.keys().map(|&key| (0, key)).collect();
        for (from, reached) in &downstream {
            for to in reached.iter().filter(|&to| to != from) {
                if let Ok(at) = order.binary_search_by(|(_, key)| key.cmp(to))
                    && let Some((feeders, _)) = order.get_mut(at)
                {
                    *feeders = feeders.saturating_add(1);
                }
            }
        }
        order.sort_unstable();

        let mut yielded: BTreeSet<WorkbookCellKey> = BTreeSet::new();
        for (_, key) in &order {
            let mut against = rivals.get(key).into_iter().flatten();
            if against
                .any(|rival| feeds(rival, key) && !feeds(key, rival) && !yielded.contains(rival))
            {
                yielded.insert(*key);
            }
        }
        let mut contested = yielded.clone();
        for (&key, against) in &rivals {
            if against.iter().any(|rival| !yielded.contains(rival)) {
                contested.insert(key);
            }
        }
        contested
    }

    fn spill_claim(
        &self,
        anchor: WorkbookCellKey,
        height: usize,
        width: usize,
    ) -> Option<SpillClaim> {
        if height <= 1 && width <= 1 {
            return None;
        }
        let sheet = self.workbook.live_sheet(anchor.0)?;
        let row = sheet.row_axis().id_to_pos(anchor.1.0)?;
        let col = sheet.col_axis().id_to_pos(anchor.2.0)?;
        let last = |first: u32, len: usize| {
            first.saturating_add(u32::try_from(len.saturating_sub(1)).unwrap_or(u32::MAX))
        };
        Some(SpillClaim {
            anchor,
            rows: (row, last(row, height)),
            cols: (col, last(col, width)),
        })
    }

    /// Rivals reached from `start` along dependencies, where every array that is
    /// not obstructed counts as filling its cells.
    fn rivals_downstream_of(
        &self,
        start: WorkbookCellKey,
        rivals: &BTreeMap<WorkbookCellKey, BTreeSet<WorkbookCellKey>>,
    ) -> BTreeSet<WorkbookCellKey> {
        let mut walk = DependentWalk::default();
        if let Some(&fid) = self.cell_to_fid.get(&start) {
            walk.push_single(fid);
        }
        let anchors = &self.settle.anchors;
        self.walk_dependents(&mut walk, |cell| match &anchors.get(&cell)?.array {
            Some((_, SpillFit::Free { rows, cols })) => Some((rows.as_slice(), cols.as_slice())),
            _ => None,
        });
        let reached = walk.seen.iter().filter_map(|&fid| self.slot_of(fid));
        reached
            .map(|slot| slot.key)
            .filter(|key| rivals.contains_key(key))
            .collect()
    }

    /// Follows dependencies from what `walk` holds until nothing new is found.
    /// `owned` gives the cells of the spill anchored at a cell. Strips are followed
    /// row by row: how formulas are grouped into strips must not show. Formulas in
    /// deleted rows and columns lead nowhere, since references to them are `#REF!`.
    fn walk_dependents<'a>(
        &self,
        walk: &mut DependentWalk,
        owned: impl Fn(WorkbookCellKey) -> Option<(&'a [RowId], &'a [ColId])>,
    ) {
        while let Some(node) = walk.queue.pop() {
            match node {
                DependentNode::Single(fid) => {
                    walk.from = fid;
                    let Some(cell) = self.slot_of(fid).map(|s| s.key) else {
                        continue;
                    };
                    let is_live = self.workbook.live_sheet(cell.0).is_some_and(|sheet| {
                        sheet.row_axis().id_to_pos(cell.1.0).is_some()
                            && sheet.col_axis().id_to_pos(cell.2.0).is_some()
                    });
                    if !is_live {
                        continue;
                    }
                    self.push_cell_dependents(cell, walk);
                    self.push_spill_ref_dependents(cell, walk);
                    if let Some((rows, cols)) = owned(cell) {
                        for shadow in footprint_cells(cell, rows, cols) {
                            self.push_cell_dependents(shadow, walk);
                        }
                    }
                }
                DependentNode::Rows { sid, lo, hi } => {
                    walk.from = strip_fid(sid);
                    self.push_strip_rows_dependents(sid, lo, hi, walk);
                }
            }
        }
    }

    fn push_cell_dependents(&self, key: WorkbookCellKey, walk: &mut DependentWalk) {
        let mut rows: SmallVec<[(u32, u32, u32); 4]> = SmallVec::new();
        self.dep_graph.for_each_cell_dependent_rows(
            key,
            &self.workbook,
            true,
            |fid| walk.push_single(fid),
            |sid, lo, hi| rows.push((sid, lo, hi)),
        );
        for (sid, lo, hi) in rows {
            self.push_strip_rows(sid, lo, hi, walk);
        }
        if !self.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            self.name_dep_graph
                .for_each_cell_dependent(key, &self.workbook, |nid| seed_nids.push(nid));
            self.push_name_dependents(&seed_nids, walk);
        }
    }

    fn push_spill_ref_dependents(&self, anchor: WorkbookCellKey, walk: &mut DependentWalk) {
        let mut nodes: SmallVec<[u32; 4]> = SmallVec::new();
        self.dep_graph
            .for_each_spill_dependent(anchor, |node| nodes.push(node));
        for node in nodes {
            self.push_node(node, walk);
        }
        if !self.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            self.name_dep_graph
                .for_each_spill_dependent(anchor, |nid| seed_nids.push(nid));
            self.push_name_dependents(&seed_nids, walk);
        }
    }

    fn push_strip_rows_dependents(&self, sid: u32, lo: u32, hi: u32, walk: &mut DependentWalk) {
        let Some(strip) = self.strip_slot_of(sid) else {
            return;
        };
        let Some(sheet) = self.workbook.live_sheet(strip.sheet) else {
            return;
        };
        if sheet.col_axis().id_to_pos(strip.col.0).is_none() || lo >= hi {
            return;
        }
        let first = strip.start_row.0.saturating_add(u64::from(lo));
        let len = hi.saturating_sub(lo);
        let row_axis = sheet.row_axis();
        let mut live_runs: Vec<IdRun> = Vec::new();
        if row_axis.live_run_covers(first, len).is_some() {
            live_runs.extend(IdRun::new(first, len));
        } else {
            let mut run_first = None;
            for offset in 0..=len {
                let row = first.saturating_add(u64::from(offset));
                let is_live = offset < len && row_axis.id_to_pos(row).is_some();
                match (is_live, run_first) {
                    (true, None) => run_first = Some(offset),
                    (false, Some(start)) => {
                        live_runs.extend(IdRun::new(
                            first.saturating_add(u64::from(start)),
                            offset.saturating_sub(start),
                        ));
                        run_first = None;
                    }
                    _ => {}
                }
            }
        }
        for run in live_runs {
            let mut rows: SmallVec<[(u32, u32, u32); 4]> = SmallVec::new();
            self.dep_graph.for_each_col_run_dependent_rows(
                strip.sheet,
                strip.col,
                run,
                None,
                &self.workbook,
                true,
                |fid| walk.push_single(fid),
                |dep_sid, dep_lo, dep_hi| rows.push((dep_sid, dep_lo, dep_hi)),
            );
            for (dep_sid, dep_lo, dep_hi) in rows {
                self.push_strip_rows(dep_sid, dep_lo, dep_hi, walk);
            }
            if !self.name_slots.is_empty() {
                let mut seed_nids = Vec::new();
                self.name_dep_graph.on_rect_written(
                    strip.sheet,
                    core::slice::from_ref(&run),
                    &[IdRun {
                        first_id: strip.col.0,
                        len: 1,
                    }],
                    &self.workbook,
                    |nid| seed_nids.push(nid),
                );
                self.push_name_dependents(&seed_nids, walk);
            }
        }
    }

    fn push_name_dependents(&self, seed_nids: &[u32], walk: &mut DependentWalk) {
        let mut nodes: Vec<u32> = Vec::new();
        Self::collect_name_dependents_from_nids_with(
            &self.dep_graph,
            &self.name_dep_graph,
            &self.name_slots,
            seed_nids,
            |node| nodes.push(node),
        );
        for node in nodes {
            self.push_node(node, walk);
        }
    }

    fn push_node(&self, node: u32, walk: &mut DependentWalk) {
        if is_strip_fid(node) {
            self.push_strip_rows(strip_index(node), 0, u32::MAX, walk);
        } else {
            walk.push_single(node);
        }
    }

    fn push_strip_rows(&self, sid: u32, lo: u32, hi: u32, walk: &mut DependentWalk) {
        let Some(strip) = self.strip_slot_of(sid) else {
            return;
        };
        let hi = if strip.sequential_self_dep {
            strip.len
        } else {
            hi.min(strip.len)
        };
        walk.push_rows(sid, lo, hi);
    }
}

#[derive(Clone, Copy)]
enum DependentNode {
    Single(u32),
    /// Rows `lo..hi` of a strip.
    Rows {
        sid: u32,
        lo: u32,
        hi: u32,
    },
}

/// Formulas reached so far and those whose dependents are still to be followed.
#[derive(Default)]
struct DependentWalk {
    queue: Vec<DependentNode>,
    seen: FxSet<u32>,
    /// Disjoint row intervals reached per strip.
    seen_rows: BTreeMap<u32, Vec<(u32, u32)>>,
    /// Node whose dependents are being pushed.
    from: u32,
    keep_edges: bool,
    /// Pairs of a node and one that depends on it.
    edges: Vec<(u32, u32)>,
}

impl DependentWalk {
    fn nodes(&self) -> impl Iterator<Item = u32> {
        let strips = self.seen_rows.keys().map(|&sid| strip_fid(sid));
        self.seen.iter().copied().chain(strips)
    }

    /// The nodes from which one of `targets` is reached, themselves included.
    fn nodes_leading_to(&self, targets: impl Iterator<Item = u32>) -> FxSet<u32> {
        let mut leading: FxSet<u32> = targets.filter(|fid| self.seen.contains(fid)).collect();
        // The walk mostly found a node before its dependents, so going backwards
        // takes few passes.
        let mut grew = !leading.is_empty();
        while grew {
            grew = false;
            for &(from, to) in self.edges.iter().rev() {
                if leading.contains(&to) {
                    grew |= leading.insert(from);
                }
            }
        }
        leading
    }

    fn note_edge(&mut self, to: u32) {
        if self.keep_edges {
            self.edges.push((self.from, to));
        }
    }

    fn push_single(&mut self, fid: u32) {
        self.note_edge(fid);
        if self.seen.insert(fid) {
            self.queue.push(DependentNode::Single(fid));
        }
    }

    fn push_rows(&mut self, sid: u32, lo: u32, hi: u32) {
        self.note_edge(strip_fid(sid));
        if lo >= hi {
            return;
        }
        let seen = self.seen_rows.entry(sid).or_default();
        let mut joined = (lo, hi);
        let mut merged = Vec::with_capacity(seen.len().saturating_add(1));
        let mut from = lo;
        for &(a, b) in seen.iter() {
            if b < lo || a > hi {
                merged.push((a, b));
                continue;
            }
            if from < a {
                self.queue.push(DependentNode::Rows {
                    sid,
                    lo: from,
                    hi: a,
                });
            }
            from = from.max(b);
            joined = (joined.0.min(a), joined.1.max(b));
        }
        if from < hi {
            self.queue.push(DependentNode::Rows { sid, lo: from, hi });
        }
        let at = merged.partition_point(|interval| *interval < joined);
        merged.insert(at, joined);
        *seen = merged;
    }
}

/// The formulas on loops that contain no smaller recorded loop. A larger one
/// may only exist because the smaller one keeps changing its spill.
fn smallest_loops(loops: &BTreeSet<Vec<WorkbookCellKey>>) -> BTreeSet<WorkbookCellKey> {
    let contains = |keys: &Vec<WorkbookCellKey>, smaller: &Vec<WorkbookCellKey>| {
        smaller.len() < keys.len() && smaller.iter().all(|key| keys.binary_search(key).is_ok())
    };
    let smallest = loops
        .iter()
        .filter(|&keys| !loops.iter().any(|other| contains(keys, other)));
    smallest.flatten().copied().collect()
}

/// Maps every anchor whose claim overlaps another one to the rivals it overlaps.
fn overlapping_claims(
    claims: &mut [SpillClaim],
) -> BTreeMap<WorkbookCellKey, BTreeSet<WorkbookCellKey>> {
    claims.sort_unstable_by_key(|claim| (claim.anchor.0, claim.rows.0, claim.anchor));
    let mut rivals: BTreeMap<WorkbookCellKey, BTreeSet<WorkbookCellKey>> = BTreeMap::new();
    let mut open: Vec<&SpillClaim> = Vec::new();
    for claim in claims.iter() {
        open.retain(|other| other.anchor.0 == claim.anchor.0 && other.rows.1 >= claim.rows.0);
        for other in &open {
            if other.cols.0 <= claim.cols.1 && claim.cols.0 <= other.cols.1 {
                rivals.entry(claim.anchor).or_default().insert(other.anchor);
                rivals.entry(other.anchor).or_default().insert(claim.anchor);
            }
        }
        open.push(claim);
    }
    rivals
}

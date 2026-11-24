//! Isolated read-only history player for collaborative revision scrubbing.
//!
//! Keeps its own 3-entry LRU base snapshot cache, ordered transaction tail, and
//! LIFO inverse-operation stack so scrubbing never mutates the live
//! `CollabSession` or triggers OPFS writes.

use std::collections::VecDeque;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{InverseOp, Op, Workbook};
use reticula_core::viewport::CellSpan;
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_wire::convert_v3::ops_from_wire_txn_v3;
use reticula_wire::snapshot::decode_snapshot_any;
use reticula_wire::v2::WireCommittedTxnV2;
use reticula_wire::v3::WireCommittedTxnV3;
use tsify::Ts;
use wasm_bindgen::prelude::*;

use crate::clipboard::RectPos;
use crate::pages::{self, PageQueue, PageRequest};
use crate::{EngineCommand, EngineEvent, SheetInfo, build_sheet_info_from_engine, query};

const MAX_CACHED_SNAPSHOTS: usize = 3;

#[wasm_bindgen]
pub struct HistoryPlayer {
    engine: FormulaEngine,
    snapshot_cache: VecDeque<(u64, Workbook<BoundFormula>)>,
    base_seq: u64,
    current_seq: u64,
    active_sheet: SheetId,
    txns: Vec<WireCommittedTxnV3>,
    applied_inverses: Vec<(u64, Vec<InverseOp<BoundFormula>>)>,
    text_scratch: String,
    subscribed_sheet: SheetId,
    epoch: u32,
    pages: PageQueue,
    /// Number of style entries sent in the latest `SheetInfo` / `StylesUpdated`.
    last_sent_styles_len: usize,
    pending_aux_event: Option<EngineEvent>,
}

impl Default for HistoryPlayer {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl HistoryPlayer {
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: FormulaEngine::new(),
            snapshot_cache: VecDeque::with_capacity(MAX_CACHED_SNAPSHOTS),
            base_seq: 0,
            current_seq: 0,
            active_sheet: SheetId(0),
            txns: Vec::new(),
            applied_inverses: Vec::new(),
            text_scratch: String::with_capacity(1024),
            subscribed_sheet: SheetId(0),
            epoch: 0,
            pages: PageQueue::default(),
            last_sent_styles_len: 1,
            pending_aux_event: None,
        }
    }

    #[wasm_bindgen(js_name = hasSnapshot)]
    #[must_use]
    pub fn has_snapshot(&self, seq: u64) -> bool {
        self.snapshot_cache
            .iter()
            .any(|(cached_seq, _)| *cached_seq == seq)
    }

    #[wasm_bindgen(js_name = loadSnapshot)]
    pub fn load_snapshot(&mut self, seq: u64, bytes: &[u8]) -> Result<(), JsError> {
        self.load_snapshot_bytes(seq, bytes)
            .map_err(|msg| JsError::new(&msg))
    }

    #[wasm_bindgen(js_name = loadEmptyBase)]
    pub fn load_empty_base(&mut self) {
        self.load_empty_base_state();
    }

    #[wasm_bindgen(js_name = activateCachedSnapshot)]
    pub fn activate_cached_snapshot_wasm(&mut self, seq: u64) -> Result<(), JsError> {
        self.activate_cached_snapshot(seq)
            .map_err(|msg| JsError::new(&msg))
    }

    #[wasm_bindgen(js_name = appendTxns)]
    pub fn append_txns(&mut self, postcard_bytes: &[u8]) -> Result<u64, JsError> {
        self.append_txns_bytes(postcard_bytes)
            .map_err(|msg| JsError::new(&msg))
    }

    #[wasm_bindgen(js_name = baseSeq)]
    #[must_use]
    pub fn base_seq(&self) -> u64 {
        self.base_seq
    }

    #[wasm_bindgen(js_name = currentSeq)]
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.current_seq
    }

    #[wasm_bindgen(js_name = maxLoadedSeq)]
    #[must_use]
    pub fn max_loaded_seq(&self) -> u64 {
        self.txns.last().map_or(self.base_seq, |txn| txn.seq)
    }

    #[wasm_bindgen(js_name = seekTo)]
    pub fn seek_to(&mut self, target_seq: u64) -> Result<Ts<SheetInfo>, JsError> {
        let info = self
            .seek_to_seq(target_seq)
            .map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&info).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = setSheet)]
    pub fn set_sheet(&mut self, sheet: u32) -> Result<Ts<SheetInfo>, JsError> {
        let info = self
            .set_sheet_id(SheetId(sheet))
            .map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&info).map_err(Into::into)
    }

    /// Carries out a command that changes nothing, as `Engine.handle` does, and
    /// rejects the others. A subscription is answered with pages.
    pub fn handle(
        &mut self,
        command: &Ts<EngineCommand>,
    ) -> Result<Option<Ts<EngineEvent>>, JsError> {
        let event = self.handle_command(command.to_rust()?);
        Ok(event.as_ref().map(Ts::from_rust).transpose()?)
    }

    #[wasm_bindgen(js_name = takePage)]
    pub fn take_page(&mut self) -> Option<Vec<u8>> {
        let page = self.pages.pop()?;
        pages::encode_due_page(
            &mut self.engine,
            self.subscribed_sheet,
            page,
            self.epoch,
            &mut self.text_scratch,
            &mut self.last_sent_styles_len,
            &mut self.pending_aux_event,
        )
    }

    /// Takes an event that has to reach the grid before the page that was taken
    /// last.
    #[wasm_bindgen(js_name = takeAuxEvent)]
    pub fn take_aux_event(&mut self) -> Result<Option<Ts<EngineEvent>>, JsError> {
        let event = self.pending_aux_event.take();
        Ok(event.as_ref().map(Ts::from_rust).transpose()?)
    }
}

impl HistoryPlayer {
    pub fn load_empty_base_state(&mut self) {
        let wb = Workbook::<BoundFormula>::new();
        self.insert_cached_snapshot(0, wb.clone());
        self.engine = FormulaEngine::from_workbook_clean(wb);
        self.base_seq = 0;
        self.current_seq = 0;
        self.txns.clear();
        self.applied_inverses.clear();
        self.forget_subscription();
        self.clamp_active_sheet();
    }

    pub fn load_snapshot_bytes(&mut self, seq: u64, bytes: &[u8]) -> Result<(), String> {
        let mut decoded =
            decode_snapshot_any(bytes).map_err(|err| format!("snapshot decode failed: {err:?}"))?;
        decoded.engine.recalc_all();
        let wb = decoded.engine.workbook().clone();
        self.insert_cached_snapshot(seq, wb);
        self.engine = decoded.engine;
        self.base_seq = seq;
        self.current_seq = seq;
        self.txns.clear();
        self.applied_inverses.clear();
        self.forget_subscription();
        self.clamp_active_sheet();
        Ok(())
    }

    pub fn activate_cached_snapshot(&mut self, seq: u64) -> Result<(), String> {
        let pos = self
            .snapshot_cache
            .iter()
            .position(|(cached_seq, _)| *cached_seq == seq)
            .ok_or_else(|| format!("snapshot #{seq} is not in LRU cache"))?;
        let (_, wb) = self
            .snapshot_cache
            .remove(pos)
            .ok_or_else(|| String::from("missing cached snapshot"))?;
        let engine = FormulaEngine::from_workbook_clean(wb.clone());
        self.snapshot_cache.push_back((seq, wb));
        self.engine = engine;
        self.base_seq = seq;
        self.current_seq = seq;
        self.txns.clear();
        self.applied_inverses.clear();
        self.forget_subscription();
        self.clamp_active_sheet();
        Ok(())
    }

    pub fn append_txns_bytes(&mut self, postcard_bytes: &[u8]) -> Result<u64, String> {
        let batch: Vec<WireCommittedTxnV3> = match postcard::from_bytes(postcard_bytes) {
            Ok(txns) => txns,
            Err(_) => postcard::from_bytes::<Vec<WireCommittedTxnV2>>(postcard_bytes)
                .map(|legacy| legacy.into_iter().map(WireCommittedTxnV3::from).collect())
                .map_err(|_| String::from("could not decode history transaction batch"))?,
        };
        for txn in batch {
            let max_seq = self.max_loaded_seq();
            if txn.seq <= max_seq {
                continue;
            }
            if txn.seq != max_seq.saturating_add(1) {
                return Err(format!(
                    "non-contiguous history sequence: expected {}, got {}",
                    max_seq.saturating_add(1),
                    txn.seq
                ));
            }
            self.txns.push(txn);
        }
        Ok(self.max_loaded_seq())
    }

    pub fn seek_to_seq(&mut self, target_seq: u64) -> Result<SheetInfo, String> {
        if self.snapshot_cache.is_empty() {
            return Err(String::from("no base snapshot loaded"));
        }
        let max_seq = self.max_loaded_seq();
        if target_seq < self.base_seq || target_seq > max_seq {
            return Err(format!(
                "history target #{target_seq} outside loaded span #{}..=#{max_seq}",
                self.base_seq
            ));
        }

        // Every page may show something else from here on, even if this fails half
        // way, so whatever was sent is void.
        self.forget_subscription();
        if target_seq > self.current_seq {
            let start = usize::try_from(self.current_seq - self.base_seq)
                .map_err(|_| String::from("sequence offset overflow"))?;
            let end = usize::try_from(target_seq - self.base_seq)
                .map_err(|_| String::from("sequence offset overflow"))?;
            self.apply_forward_slice(start, end)?;
            self.engine.recalc_all();
        } else if target_seq < self.current_seq {
            let mut inverse_ok = true;
            while self.current_seq > target_seq {
                let Some((seq, invs)) = self.applied_inverses.pop() else {
                    inverse_ok = false;
                    break;
                };
                if seq != self.current_seq {
                    inverse_ok = false;
                    break;
                }
                for inv in invs.into_iter().rev() {
                    if self.engine.apply_inverse(inv).is_err() {
                        inverse_ok = false;
                        break;
                    }
                }
                if !inverse_ok {
                    break;
                }
                self.current_seq -= 1;
            }
            if !inverse_ok {
                self.replay_from_cached_base(target_seq)?;
            } else {
                self.engine.recalc_all();
            }
        }

        self.clamp_active_sheet();
        Ok(self.build_sheet_info(self.active_sheet))
    }

    pub fn set_sheet_id(&mut self, sheet_id: SheetId) -> Result<SheetInfo, String> {
        if self.engine.workbook().sheet(sheet_id).is_none() {
            return Err(format!(
                "worksheet {} does not exist at #{}",
                sheet_id.0, self.current_seq
            ));
        }
        self.active_sheet = sheet_id;
        self.forget_subscription();
        Ok(self.build_sheet_info(sheet_id))
    }

    pub fn handle_command(&mut self, command: EngineCommand) -> Option<EngineEvent> {
        match command {
            EngineCommand::Subscribe {
                sheet,
                epoch,
                first_row,
                row_count,
                first_col,
                col_count,
                visible,
                resend,
            } => {
                let sheet = SheetId(sheet);
                if sheet != self.subscribed_sheet {
                    self.pages.clear();
                }
                let request = PageRequest {
                    rows: (first_row, row_count),
                    cols: (first_col, col_count),
                    visible: visible.map_or_else(CellSpan::default, Into::into),
                };
                self.pages.subscribe(&self.engine, sheet, request, resend);
                self.subscribed_sheet = sheet;
                self.epoch = epoch;
                None
            }
            EngineCommand::InspectCell {
                sheet,
                row_id,
                col_id,
            } => query::inspect_cell(&self.engine, SheetId(sheet), RowId(row_id), ColId(col_id)),
            EngineCommand::JumpEdge {
                sheet,
                row,
                col,
                dir,
                select,
            } => Some(query::jump_edge(
                &self.engine,
                SheetId(sheet),
                row,
                col,
                dir,
                select,
            )),
            EngineCommand::JumpUsedEnd { sheet, select } => {
                Some(query::jump_used_end(&self.engine, SheetId(sheet), select))
            }
            EngineCommand::CopyRange {
                sheet,
                first_row,
                row_count,
                first_col,
                col_count,
                cut: _,
            } => {
                let rect = RectPos {
                    first_row,
                    row_count,
                    first_col,
                    col_count,
                };
                // Nothing can be cut from the past, and what a paste inside the
                // live workbook keeps belongs to the live engine.
                Some(
                    match query::copy_selection(&self.engine, SheetId(sheet), rect, false) {
                        Ok((event, _)) => event,
                        Err(reason) => EngineEvent::Rejected { reason },
                    },
                )
            }
            EngineCommand::FindAll {
                sheet,
                options,
                max_results,
            } => Some(query::find_all(
                &self.engine,
                SheetId(sheet),
                &options.into(),
                max_results.unwrap_or(500) as usize,
            )),
            EngineCommand::FindNext {
                sheet,
                from_row,
                from_col,
                backwards,
                options,
            } => Some(query::find_next(
                &self.engine,
                SheetId(sheet),
                from_row,
                from_col,
                backwards,
                &options.into(),
            )),
            EngineCommand::SetEvalEnv { .. } => None,
            _ => Some(EngineEvent::Rejected {
                reason: String::from("Return to the live workbook before editing."),
            }),
        }
    }

    fn forget_subscription(&mut self) {
        self.pages.clear();
        self.pending_aux_event = None;
    }

    #[must_use]
    pub fn engine(&self) -> &FormulaEngine {
        &self.engine
    }

    fn insert_cached_snapshot(&mut self, seq: u64, wb: Workbook<BoundFormula>) {
        if let Some(existing) = self.snapshot_cache.iter().position(|(s, _)| *s == seq) {
            self.snapshot_cache.remove(existing);
        } else if self.snapshot_cache.len() >= MAX_CACHED_SNAPSHOTS {
            self.snapshot_cache.pop_front();
        }
        self.snapshot_cache.push_back((seq, wb));
    }

    fn apply_forward_slice(&mut self, start: usize, end: usize) -> Result<(), String> {
        for idx in start..end {
            let txn = self
                .txns
                .get(idx)
                .ok_or_else(|| String::from("missing transaction in history slice"))?;
            let (_, ops): (_, Vec<Op<BoundFormula>>) = ops_from_wire_txn_v3(txn.txn.clone())
                .map_err(|err| format!("failed to decode history txn #{}: {err:?}", txn.seq))?;
            let invs = self
                .engine
                .apply_txn(ops)
                .map_err(|err| format!("failed to apply history txn #{}: {err:?}", txn.seq))?;
            self.applied_inverses.push((txn.seq, invs));
            self.current_seq = txn.seq;
        }
        Ok(())
    }

    fn replay_from_cached_base(&mut self, target_seq: u64) -> Result<(), String> {
        let base_wb = self
            .snapshot_cache
            .iter()
            .rev()
            .find(|(seq, _)| *seq == self.base_seq)
            .map(|(_, wb)| wb.clone())
            .ok_or_else(|| {
                format!(
                    "base snapshot #{} evicted before fallback replay",
                    self.base_seq
                )
            })?;
        self.engine = FormulaEngine::from_workbook_clean(base_wb);
        self.applied_inverses.clear();
        self.current_seq = self.base_seq;
        let end = usize::try_from(target_seq - self.base_seq)
            .map_err(|_| String::from("sequence offset overflow"))?;
        self.apply_forward_slice(0, end)?;
        self.engine.recalc_all();
        Ok(())
    }

    fn clamp_active_sheet(&mut self) {
        if self.engine.workbook().sheet(self.active_sheet).is_some() {
            return;
        }
        if let Some(first) = self.engine.workbook().live_sheets().next() {
            self.active_sheet = first.id();
        }
    }

    fn build_sheet_info(&mut self, sheet_id: SheetId) -> SheetInfo {
        let (info, styles_len) = build_sheet_info_from_engine(
            &mut self.engine,
            sheet_id,
            0,
            0.0,
            false,
            false,
            None,
            None,
            self.current_seq,
            false,
        );
        self.last_sent_styles_len = styles_len;
        info
    }
}

#[cfg(test)]
mod tests {
    use reticula_core::id::{ColId, IdRun, RowId, SheetId, pack_id};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue, Workbook};
    use reticula_core::value::Value;
    use reticula_formula::binder::BoundFormula;
    use reticula_formula::engine::FormulaEngine;
    use reticula_wire::convert_v2::{ops_from_wire_txn_v2, ops_to_wire_txn_v2};
    use reticula_wire::snapshot::encode_snapshot_v1;
    use reticula_wire::v2::WireCommittedTxnV2;

    use super::HistoryPlayer;
    use crate::{
        EngineCommand, EngineEvent, JumpDirection, WireCellSpan, WireFindOptions, WireSearchIn,
        WireSearchScope,
    };

    fn make_edit_txn(
        seq: u64,
        row: u64,
        col: u64,
        input: CellInput<BoundFormula>,
        label: &str,
    ) -> WireCommittedTxnV2 {
        let ops = vec![Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(row),
                col: ColId(col),
                input,
                format: FormatPatch::Keep,
            }],
        }];
        WireCommittedTxnV2 {
            seq,
            replica: 1,
            client_seq: seq,
            user_id: [1; 16],
            timestamp_ms: 1_795_000_000_000 + seq,
            txn: ops_to_wire_txn_v2(label.to_owned(), &ops).unwrap(),
        }
    }

    fn fresh_replay_digest(
        base: &Workbook<BoundFormula>,
        txns: &[WireCommittedTxnV2],
        upto: usize,
    ) -> reticula_core::state_digest::StateDigest {
        let mut engine = FormulaEngine::from_workbook_clean(base.clone());
        for txn in &txns[..upto] {
            let (_, ops) = ops_from_wire_txn_v2(txn.txn.clone()).unwrap();
            engine.apply_txn(ops).unwrap();
        }
        engine.recalc_all();
        engine.workbook().state_digest()
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn forward_and_backward_scrub_matches_fresh_replay_and_formulas() {
        let base = Workbook::<BoundFormula>::new();
        let snap0 = encode_snapshot_v1(&base, &[]).unwrap();

        let sheet0 = base.sheet(SheetId(0)).unwrap();
        let row_ids = [
            sheet0.row_axis().pos_to_id(0).unwrap(),
            sheet0.row_axis().pos_to_id(1).unwrap(),
            sheet0.row_axis().pos_to_id(2).unwrap(),
        ];
        let c1 = sheet0.col_axis().pos_to_id(0).unwrap();
        let helper_engine = FormulaEngine::from_workbook_clean(base.clone());
        let sum_op = helper_engine
            .build_cell_edit(SheetId(0), RowId(row_ids[2]), ColId(c1), "=SUM(A1:A2)")
            .unwrap();

        let mut txns = Vec::new();
        for seq in 1..=10_u64 {
            let txn = match seq {
                1 => make_edit_txn(
                    seq,
                    row_ids[0],
                    c1,
                    CellInput::Scalar(ScalarValue::Number(10.0)),
                    "A1=10",
                ),
                2 => make_edit_txn(
                    seq,
                    row_ids[1],
                    c1,
                    CellInput::Scalar(ScalarValue::Number(20.0)),
                    "A2=20",
                ),
                3 => WireCommittedTxnV2 {
                    seq,
                    replica: 1,
                    client_seq: seq,
                    user_id: [1; 16],
                    timestamp_ms: 1_795_000_000_000 + seq,
                    txn: ops_to_wire_txn_v2("A3=SUM".into(), core::slice::from_ref(&sum_op))
                        .unwrap(),
                },
                4 => {
                    let ins = pack_id(0, 1, 99).unwrap();
                    let ops = vec![Op::InsertRows {
                        sheet: SheetId(0),
                        after: Some(RowId(row_ids[0])),
                        ids: IdRun {
                            first_id: ins,
                            len: 1,
                        },
                    }];
                    WireCommittedTxnV2 {
                        seq,
                        replica: 1,
                        client_seq: seq,
                        user_id: [1; 16],
                        timestamp_ms: 1_795_000_000_000 + seq,
                        txn: ops_to_wire_txn_v2("Insert row".into(), &ops).unwrap(),
                    }
                }
                _ => make_edit_txn(
                    seq,
                    row_ids[1],
                    c1,
                    CellInput::Scalar(ScalarValue::Number((seq * 5) as f64)),
                    "Update A2",
                ),
            };
            txns.push(txn);
        }

        let mut player = HistoryPlayer::new();
        player.load_snapshot_bytes(0, &snap0).unwrap();
        let encoded = postcard::to_allocvec(&txns).unwrap();
        assert_eq!(player.append_txns_bytes(&encoded).unwrap(), 10);

        for target in [5_usize, 10, 3, 4, 0, 7, 1, 10] {
            let info = player.seek_to_seq(target as u64).unwrap();
            assert_eq!(info.wal_seq, target as u64);
            assert_eq!(
                player.engine().workbook().state_digest(),
                fresh_replay_digest(&base, &txns, target),
                "digest mismatch at target={target}"
            );
        }

        // Verify formula evaluation at seq=3 (10 + 20 = 30) and seq=10 (10 + 50 = 60).
        player.seek_to_seq(3).unwrap();
        assert_eq!(
            player
                .engine()
                .workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(row_ids[2]), ColId(c1)),
            Value::Number(30.0)
        );
        player.seek_to_seq(10).unwrap();
        assert_eq!(
            player
                .engine()
                .workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(row_ids[2]), ColId(c1)),
            Value::Number(60.0)
        );
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn snapshot_lru_cache_evicts_oldest_and_reactivates() {
        let mut player = HistoryPlayer::new();
        let wb0 = Workbook::<BoundFormula>::new();
        let snap = encode_snapshot_v1(&wb0, &[]).unwrap();
        for seq in [0_u64, 100, 200, 300] {
            player.load_snapshot_bytes(seq, &snap).unwrap();
        }
        assert!(!player.has_snapshot(0));
        assert!(player.has_snapshot(100));
        assert!(player.has_snapshot(200));
        assert!(player.has_snapshot(300));

        player.activate_cached_snapshot(100).unwrap();
        assert_eq!(player.base_seq(), 100);
        assert_eq!(player.current_seq(), 100);

        // Next insert should evict 200 (LRU after 100 was promoted).
        player.load_snapshot_bytes(400, &snap).unwrap();
        assert!(player.has_snapshot(100));
        assert!(!player.has_snapshot(200));
        assert!(player.has_snapshot(300));
        assert!(player.has_snapshot(400));
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn scrub_handles_500_txn_batch_deterministically() {
        let wb0 = Workbook::<BoundFormula>::new();
        let snap0 = encode_snapshot_v1(&wb0, &[]).unwrap();
        let sheet = wb0.sheet(SheetId(0)).unwrap();
        let r1 = sheet.row_axis().pos_to_id(0).unwrap();
        let c1 = sheet.col_axis().pos_to_id(0).unwrap();

        let txns: Vec<WireCommittedTxnV2> = (1..=500_u64)
            .map(|seq| {
                make_edit_txn(
                    seq,
                    r1,
                    c1,
                    CellInput::Scalar(ScalarValue::Number(seq as f64)),
                    "Num",
                )
            })
            .collect();
        let encoded = postcard::to_allocvec(&txns).unwrap();

        let mut player = HistoryPlayer::new();
        player.load_snapshot_bytes(0, &snap0).unwrap();
        player.append_txns_bytes(&encoded).unwrap();

        player.seek_to_seq(500).unwrap();
        assert_eq!(
            player
                .engine()
                .workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(r1), ColId(c1)),
            Value::Number(500.0)
        );
        player.seek_to_seq(400).unwrap();
        assert_eq!(
            player
                .engine()
                .workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(r1), ColId(c1)),
            Value::Number(400.0)
        );
        assert_eq!(
            player.engine().workbook().state_digest(),
            fresh_replay_digest(&wb0, &txns, 400)
        );
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn load_empty_base_allows_scrub_from_zero() {
        let wb0 = Workbook::<BoundFormula>::new();
        let sheet = wb0.sheet(SheetId(0)).unwrap();
        let r1 = sheet.row_axis().pos_to_id(0).unwrap();
        let c1 = sheet.col_axis().pos_to_id(0).unwrap();
        let txns: Vec<WireCommittedTxnV2> = (1..=5_u64)
            .map(|seq| {
                make_edit_txn(
                    seq,
                    r1,
                    c1,
                    CellInput::Scalar(ScalarValue::Number((seq * 10) as f64)),
                    "Edit",
                )
            })
            .collect();
        let encoded = postcard::to_allocvec(&txns).unwrap();

        let mut player = HistoryPlayer::new();
        player.load_empty_base_state();
        assert!(player.has_snapshot(0));
        assert_eq!(player.base_seq(), 0);
        assert_eq!(player.max_loaded_seq(), 0);

        player.append_txns_bytes(&encoded).unwrap();
        player.seek_to_seq(5).unwrap();
        assert_eq!(
            player
                .engine()
                .workbook()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(r1), ColId(c1)),
            Value::Number(50.0)
        );
        player.seek_to_seq(0).unwrap();
        assert_eq!(
            player.engine().workbook().state_digest(),
            fresh_replay_digest(&wb0, &txns, 0)
        );
    }

    /// A player at the last of `values.len()` edits, which put those values into
    /// column A from the top.
    fn player_with(values: &[&str]) -> HistoryPlayer {
        let base = Workbook::<BoundFormula>::new();
        let sheet = base.sheet(SheetId(0)).unwrap();
        let col = sheet.col_axis().pos_to_id(0).unwrap();
        let txns: Vec<WireCommittedTxnV2> = (1..)
            .zip(values)
            .map(|(seq, value)| {
                let row = sheet
                    .row_axis()
                    .pos_to_id(u32::try_from(seq - 1).unwrap())
                    .unwrap();
                let value = ScalarValue::Text(String::from(*value));
                make_edit_txn(seq, row, col, CellInput::Scalar(value), "Edit")
            })
            .collect();
        let mut player = HistoryPlayer::new();
        player.load_empty_base_state();
        player
            .append_txns_bytes(&postcard::to_allocvec(&txns).unwrap())
            .unwrap();
        player.seek_to_seq(values.len() as u64).unwrap();
        player
    }

    fn subscribe(
        player: &mut HistoryPlayer,
        (first_row, row_count): (u32, u32),
        resend: bool,
    ) -> Vec<(u32, u32)> {
        let event = player.handle_command(EngineCommand::Subscribe {
            sheet: 0,
            epoch: 1,
            first_row,
            row_count,
            first_col: 16,
            col_count: 16,
            visible: Some(WireCellSpan {
                first_row,
                end_row: first_row + row_count,
                first_col: 16,
                end_col: 32,
            }),
            resend,
        });
        assert!(event.is_none());
        std::iter::from_fn(|| player.take_page())
            .map(|page| {
                let field = |at: usize| u32::from_le_bytes(page[at..at + 4].try_into().unwrap());
                (field(8) / 64, field(12) / 16)
            })
            .collect()
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn a_subscription_gets_the_pages_it_asks_for_and_then_only_new_ones() {
        let mut player = player_with(&["one", "two"]);
        // 128 rows from row 640 are page rows 10 and 11, and not 10 to 21.
        assert_eq!(
            subscribe(&mut player, (640, 128), false),
            [(10, 1), (11, 1)]
        );
        assert_eq!(subscribe(&mut player, (640, 128), false), []);
        assert_eq!(subscribe(&mut player, (704, 128), false), [(12, 1)]);
        assert_eq!(subscribe(&mut player, (704, 128), true), [(11, 1), (12, 1)]);
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn pages_sent_for_another_revision_or_sheet_do_not_count() {
        let mut player = player_with(&["one", "two"]);
        let all = [(10, 1), (11, 1)];
        assert_eq!(subscribe(&mut player, (640, 128), false), all);
        player.seek_to_seq(1).unwrap();
        assert!(player.take_page().is_none());
        assert_eq!(subscribe(&mut player, (640, 128), false), all);
        player.set_sheet_id(SheetId(0)).unwrap();
        assert_eq!(subscribe(&mut player, (640, 128), false), all);
        player.activate_cached_snapshot(0).unwrap();
        assert_eq!(subscribe(&mut player, (640, 128), false), all);
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn pages_sent_before_a_seek_that_fails_half_way_do_not_count() {
        let mut player = player_with(&["one", "two"]);
        let value = CellInput::Scalar(ScalarValue::Text(String::from("three")));
        let txns = vec![
            make_edit_txn(3, 2, 0, value.clone(), "Edit"),
            make_edit_txn(4, u64::MAX >> 1, 0, value, "Edit"),
        ];
        player
            .append_txns_bytes(&postcard::to_allocvec(&txns).unwrap())
            .unwrap();
        let all = [(10, 1), (11, 1)];
        assert_eq!(subscribe(&mut player, (640, 128), false), all);
        assert!(player.seek_to_seq(4).is_err());
        assert_eq!(player.current_seq, 3);
        assert_eq!(subscribe(&mut player, (640, 128), false), all);
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn queries_read_the_revision_shown_and_edits_are_rejected() {
        let mut player = player_with(&["one", "two", "three"]);
        player.seek_to_seq(2).unwrap();
        let before = player.engine().workbook().state_digest();

        let jumped = player.handle_command(EngineCommand::JumpEdge {
            sheet: 0,
            row: 0,
            col: 0,
            dir: JumpDirection::Down,
            select: false,
        });
        assert!(matches!(
            jumped,
            Some(EngineEvent::EdgeJumped { row: 1, col: 0, .. })
        ));
        let end = player.handle_command(EngineCommand::JumpUsedEnd {
            sheet: 0,
            select: true,
        });
        assert!(matches!(
            end,
            Some(EngineEvent::EdgeJumped {
                row: 1,
                col: 0,
                select: true,
                ..
            })
        ));
        let copied = player.handle_command(EngineCommand::CopyRange {
            sheet: 0,
            first_row: 0,
            row_count: 3,
            first_col: 0,
            col_count: 1,
            cut: true,
        });
        let Some(EngineEvent::ClipboardCopied { tsv, cut, .. }) = copied else {
            panic!("expected ClipboardCopied, got {copied:?}");
        };
        assert_eq!(tsv, "one\ntwo\n");
        assert!(!cut);

        let options = |query: &str| WireFindOptions {
            query: String::from(query),
            scope: WireSearchScope::WholeWorkbook,
            search_in: WireSearchIn::DisplayValues,
            case_sensitive: false,
            whole_cell: false,
            use_regex: false,
            skip_hidden: false,
        };
        let mut found = |query: &str| {
            let all = player.handle_command(EngineCommand::FindAll {
                sheet: 0,
                options: options(query),
                max_results: None,
            });
            let Some(EngineEvent::SearchResults { total, .. }) = all else {
                panic!("expected SearchResults, got {all:?}");
            };
            let next = player.handle_command(EngineCommand::FindNext {
                sheet: 0,
                from_row: 0,
                from_col: 0,
                backwards: false,
                options: options(query),
            });
            let Some(EngineEvent::SearchNavigated { found }) = next else {
                panic!("expected SearchNavigated, got {next:?}");
            };
            (total, found.map(|m| m.row))
        };
        assert_eq!(found("o"), (2, Some(1)));
        // The third edit is not part of this revision.
        assert_eq!(found("three"), (0, None));

        let inspected = player.handle_command(EngineCommand::InspectCell {
            sheet: 0,
            row_id: 1,
            col_id: 0,
        });
        assert!(matches!(inspected, Some(EngineEvent::CellInspected { raw, .. }) if raw == "two"));
        let rejected = player.handle_command(EngineCommand::EditCell {
            sheet: 0,
            row_id: 0,
            col_id: 0,
            raw: String::from("changed"),
        });
        assert!(matches!(rejected, Some(EngineEvent::Rejected { .. })));
        assert!(
            player
                .handle_command(EngineCommand::SetEvalEnv {
                    now_serial: 1.0,
                    seed: 1,
                    timestamp_ms: None,
                })
                .is_none()
        );
        assert_eq!(player.engine().workbook().state_digest(), before);
    }
}

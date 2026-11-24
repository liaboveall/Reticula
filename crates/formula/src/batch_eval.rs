//! Columnar `1024`-row strip bytecode evaluator (`19-m3b.md` §4).
//!
//! Executes [`CompiledStripProgram`] over chunks of up to [`CHUNK_LEN`] rows
//! using reusable [`BatchScratch`] vector buffers so `B3`, `B4`, `B5`, `B6`, and
//! `B7a` Group 3 execute without per-row AST traversal or heap allocations.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::decimal15::{cmp_excel_f64, snap_cancelled_zero};
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::Workbook;
use reticula_core::sheet::Sheet;
use reticula_core::value::{CalcError, Value};

use crate::ast::{BinaryOp, UnaryOp};
use crate::binder::BoundFormula;
use crate::bytecode::{CHUNK_LEN, CompiledStripProgram, FixedAreaSpec, Instr, UnaryByteOp};
use crate::functions::{
    eval_average, eval_count, eval_counta, eval_min_max, eval_round_scalar, eval_sum, lookup,
};
use crate::spill::SpillRegistry;
use crate::value_eval::{
    EvalScalar, EvalValue, ResolvedRef, apply_binary_scalar, apply_unary_scalar,
};

const TAG_NUM: u8 = 0;
const TAG_EMPTY: u8 = 1;
const TAG_BOOL: u8 = 2;
const TAG_ERROR: u8 = 3;
const TAG_TEXT: u8 = 4;

/// `2^-48`: when `|raw| > |lhs| * 2^-48` and `|raw| >= MIN_POSITIVE`,
/// `snap_cancelled_zero(lhs, raw)` cannot cancel to `0.0`.
const CANCEL_GUARD_MULT: f64 = f64::from_bits(0x3CF0_0000_0000_0000);

/// Mask matching any NaN, Infinity, or boxed non-number word in `Value`.
const EXP_ALL_ONES: u64 = 0x7FF0_0000_0000_0000;
const NEG_ZERO_BITS: u64 = 0x8000_0000_0000_0000;

/// Columnar 1024-row virtual vector register.
#[derive(Clone, Debug)]
struct RegVec {
    nums: Vec<f64>,
    tags: Vec<u8>,
    payloads: Vec<u32>,
    all_num: bool,
}

impl RegVec {
    fn new() -> Self {
        Self {
            nums: vec![0.0; CHUNK_LEN],
            tags: vec![TAG_EMPTY; CHUNK_LEN],
            payloads: vec![0; CHUNK_LEN],
            all_num: false,
        }
    }

    #[inline]
    fn fill_scalar(&mut self, scalar: &EvalScalar, len: usize, text_pool: &mut Vec<String>) {
        match scalar {
            EvalScalar::Empty => {
                self.all_num = false;
                if let Some(t) = self.tags.get_mut(..len) {
                    t.fill(TAG_EMPTY);
                }
            }
            EvalScalar::Number(n) => {
                if !n.is_finite() {
                    self.all_num = false;
                    if let Some(t) = self.tags.get_mut(..len) {
                        t.fill(TAG_ERROR);
                    }
                    if let Some(p) = self.payloads.get_mut(..len) {
                        p.fill(err_to_u32(CalcError::Num));
                    }
                } else {
                    let v = if *n == 0.0 { 0.0 } else { *n };
                    self.all_num = true;
                    if let Some(dst) = self.nums.get_mut(..len) {
                        dst.fill(v);
                    }
                }
            }
            EvalScalar::Bool(b) => {
                self.all_num = false;
                if let Some(t) = self.tags.get_mut(..len) {
                    t.fill(TAG_BOOL);
                }
                if let Some(p) = self.payloads.get_mut(..len) {
                    p.fill(u32::from(*b));
                }
            }
            EvalScalar::Error(e) => {
                self.all_num = false;
                if let Some(t) = self.tags.get_mut(..len) {
                    t.fill(TAG_ERROR);
                }
                if let Some(p) = self.payloads.get_mut(..len) {
                    p.fill(err_to_u32(*e));
                }
            }
            EvalScalar::Text(s) => {
                let idx = intern_pool_string(text_pool, s);
                self.all_num = false;
                if let Some(t) = self.tags.get_mut(..len) {
                    t.fill(TAG_TEXT);
                }
                if let Some(p) = self.payloads.get_mut(..len) {
                    p.fill(idx);
                }
            }
        }
    }

    #[inline]
    fn load_packed_slice(&mut self, sheet: &Sheet, words: &[u64], text_pool: &mut Vec<String>) {
        let len = words.len();
        if words.iter().all(|&w| (w & EXP_ALL_ONES) != EXP_ALL_ONES) {
            self.all_num = true;
            if let Some(dst) = self.nums.get_mut(..len) {
                for (d, &w) in dst.iter_mut().zip(words.iter()) {
                    let canon = if w == NEG_ZERO_BITS { 0 } else { w };
                    *d = f64::from_bits(canon);
                }
            }
            return;
        }
        self.all_num = false;
        let (Some(d_nums), Some(d_tags), Some(d_pay)) = (
            self.nums.get_mut(..len),
            self.tags.get_mut(..len),
            self.payloads.get_mut(..len),
        ) else {
            return;
        };
        for (((dst_n, dst_t), dst_p), &w) in d_nums
            .iter_mut()
            .zip(d_tags.iter_mut())
            .zip(d_pay.iter_mut())
            .zip(words.iter())
        {
            match Value::unpack(w) {
                Value::Empty => *dst_t = TAG_EMPTY,
                Value::Number(n) => {
                    *dst_t = TAG_NUM;
                    *dst_n = if n == 0.0 { 0.0 } else { n };
                }
                Value::Bool(b) => {
                    *dst_t = TAG_BOOL;
                    *dst_p = u32::from(b);
                }
                Value::Error(e) => {
                    *dst_t = TAG_ERROR;
                    *dst_p = err_to_u32(e);
                }
                Value::Text(id) => {
                    *dst_t = TAG_TEXT;
                    *dst_p = intern_pool_string(text_pool, sheet.strings().get(id));
                }
                Value::Array(_) => {
                    *dst_t = TAG_ERROR;
                    *dst_p = err_to_u32(CalcError::Spill);
                }
            }
        }
    }

    #[inline]
    fn read_scalar(&self, lane: usize, text_pool: &[String]) -> EvalScalar {
        if self.all_num {
            let n = self.nums.get(lane).copied().unwrap_or(0.0);
            return EvalScalar::number(n);
        }
        let tag = self.tags.get(lane).copied().unwrap_or(TAG_EMPTY);
        match tag {
            TAG_NUM => EvalScalar::number(self.nums.get(lane).copied().unwrap_or(0.0)),
            TAG_EMPTY => EvalScalar::Empty,
            TAG_BOOL => EvalScalar::Bool(self.payloads.get(lane).copied().unwrap_or(0) != 0),
            TAG_ERROR => {
                let code = self.payloads.get(lane).copied().unwrap_or(0);
                EvalScalar::Error(err_from_u32(code))
            }
            TAG_TEXT => {
                let idx = self.payloads.get(lane).copied().unwrap_or(0) as usize;
                let s = text_pool.get(idx).cloned().unwrap_or_default();
                EvalScalar::Text(s)
            }
            _ => EvalScalar::Empty,
        }
    }

    #[inline]
    fn write_scalar(&mut self, lane: usize, scalar: EvalScalar, text_pool: &mut Vec<String>) {
        self.all_num = false;
        let Some(tag_slot) = self.tags.get_mut(lane) else {
            return;
        };
        match scalar {
            EvalScalar::Empty => *tag_slot = TAG_EMPTY,
            EvalScalar::Number(n) => {
                if !n.is_finite() {
                    *tag_slot = TAG_ERROR;
                    if let Some(p) = self.payloads.get_mut(lane) {
                        *p = err_to_u32(CalcError::Num);
                    }
                } else {
                    *tag_slot = TAG_NUM;
                    if let Some(dst) = self.nums.get_mut(lane) {
                        *dst = if n == 0.0 { 0.0 } else { n };
                    }
                }
            }
            EvalScalar::Bool(b) => {
                *tag_slot = TAG_BOOL;
                if let Some(p) = self.payloads.get_mut(lane) {
                    *p = u32::from(b);
                }
            }
            EvalScalar::Error(e) => {
                *tag_slot = TAG_ERROR;
                if let Some(p) = self.payloads.get_mut(lane) {
                    *p = err_to_u32(e);
                }
            }
            EvalScalar::Text(s) => {
                let idx = intern_pool_string(text_pool, &s);
                *tag_slot = TAG_TEXT;
                if let Some(p) = self.payloads.get_mut(lane) {
                    *p = idx;
                }
            }
        }
    }

    #[inline]
    fn copy_lane_from(&mut self, lane: usize, src: &Self, zero_empty: bool) {
        if src.all_num {
            if let (Some(dt), Some(dn), Some(&sn)) = (
                self.tags.get_mut(lane),
                self.nums.get_mut(lane),
                src.nums.get(lane),
            ) {
                *dt = TAG_NUM;
                *dn = sn;
            }
            return;
        }
        let st = src.tags.get(lane).copied().unwrap_or(TAG_EMPTY);
        if zero_empty && st == TAG_EMPTY {
            if let (Some(dt), Some(dn)) = (self.tags.get_mut(lane), self.nums.get_mut(lane)) {
                *dt = TAG_NUM;
                *dn = 0.0;
            }
            return;
        }
        if let (Some(dt), Some(dn), Some(dp)) = (
            self.tags.get_mut(lane),
            self.nums.get_mut(lane),
            self.payloads.get_mut(lane),
        ) {
            *dt = st;
            *dn = src.nums.get(lane).copied().unwrap_or(0.0);
            *dp = src.payloads.get(lane).copied().unwrap_or(0);
        }
    }

    #[inline]
    fn refresh_all_num(&mut self, len: usize) {
        self.all_num = self
            .tags
            .get(..len)
            .is_some_and(|slice| slice.iter().all(|&t| t == TAG_NUM));
    }
}

/// Reusable heap buffers kept on `FormulaEngine` so `1024`-row strip evaluation
/// performs zero per-chunk allocations during `1M`-row recalculations.
#[derive(Clone, Debug, Default)]
pub(crate) struct BatchScratch {
    regs: Vec<RegVec>,
    stride_words: Vec<Vec<u64>>,
    text_pool: Vec<String>,
    self_words: Vec<u64>,
    self_texts: Vec<Option<u32>>,
    lane_nums: Vec<f64>,
    lane_tags: Vec<u8>,
    lane_payloads: Vec<u32>,
    fixed_area_cache: Vec<((usize, u8), EvalScalar)>,
    /// Packed output words for `[0..chunk_len)` ready for `write_row_id_span_packed`.
    pub out_words: Vec<u64>,
    /// Text outputs `(lane, String)` requiring interning on the host sheet before writing.
    pub text_fixups: Vec<(usize, String)>,
}

impl BatchScratch {
    fn prepare(
        &mut self,
        num_regs: usize,
        num_strides: usize,
        chunk_len: usize,
        const_strings: &[String],
    ) {
        if self.regs.len() < num_regs {
            self.regs.resize_with(num_regs, RegVec::new);
        }
        if self.stride_words.len() < num_strides {
            self.stride_words
                .resize_with(num_strides, || vec![Value::EMPTY_BITS; CHUNK_LEN]);
        }
        self.text_pool.clear();
        for s in const_strings {
            self.text_pool.push(s.clone());
        }
        self.out_words.resize(chunk_len, Value::EMPTY_BITS);
        self.text_fixups.clear();
    }

    fn prepare_recurrence(
        &mut self,
        num_regs: usize,
        num_strides: usize,
        total_self_window: usize,
        chunk_len: usize,
        const_strings: &[String],
    ) {
        if self.stride_words.len() < num_strides {
            self.stride_words
                .resize_with(num_strides, || vec![Value::EMPTY_BITS; CHUNK_LEN]);
        }
        self.lane_nums.resize(num_regs, 0.0);
        self.lane_tags.resize(num_regs, TAG_EMPTY);
        self.lane_payloads.resize(num_regs, 0);
        self.fixed_area_cache.clear();
        self.self_words.resize(total_self_window, Value::EMPTY_BITS);
        self.self_texts.clear();
        self.self_texts.resize(total_self_window, None);
        self.text_pool.clear();
        for s in const_strings {
            self.text_pool.push(s.clone());
        }
        self.out_words.resize(chunk_len, Value::EMPTY_BITS);
        self.text_fixups.clear();
    }

    /// Evaluates chunk `[lo .. lo + chunk_len)` of `program`, populating
    /// `self.out_words[..chunk_len]` and `self.text_fixups`.
    #[expect(
        clippy::too_many_arguments,
        reason = "pre-validated strip chunk geometry passed directly from evaluate_strip_slot"
    )]
    pub(crate) fn eval_strip_chunk(
        &mut self,
        program: &CompiledStripProgram,
        workbook: &Workbook<BoundFormula>,
        host_sheet: SheetId,
        host_base_pos: u32,
        strip_col: ColId,
        start_row: RowId,
        lo: u32,
        chunk_len: usize,
    ) {
        let len = chunk_len.min(CHUNK_LEN);
        if program.sequential_self_dep {
            self.eval_recurrence_chunk(
                program,
                workbook,
                host_sheet,
                host_base_pos,
                strip_col,
                start_row,
                lo,
                len,
            );
            return;
        }

        self.prepare(
            program.num_regs,
            program.stride_cols.len(),
            len,
            &program.const_strings,
        );
        for (spec, buf) in program.stride_cols.iter().zip(self.stride_words.iter_mut()) {
            if let Some(sheet) = workbook.sheet(spec.sheet)
                && let Some(dst) = buf.get_mut(..len)
            {
                let first_r = RowId(spec.base_row.saturating_add(u64::from(lo)));
                sheet.read_row_id_span_packed(first_r, spec.col, dst);
            } else if let Some(dst) = buf.get_mut(..len) {
                dst.fill(Value::EMPTY_BITS);
            }
        }

        let spills = SpillRegistry::new();
        let ctx = VecEvalCtx {
            program,
            workbook,
            spills: &spills,
            stride_words: &self.stride_words,
            host_base_pos,
            chunk_len: len,
        };
        exec_vec_instrs(&program.instrs, &ctx, &mut self.regs, &mut self.text_pool);

        let Some(root_reg) = self.regs.get(program.result_reg as usize) else {
            self.out_words.fill(Value::Error(CalcError::Calc).pack());
            return;
        };
        pack_root_vec(
            root_reg,
            len,
            &self.text_pool,
            &mut self.out_words,
            &mut self.text_fixups,
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "pre-validated chunk geometry passed directly from evaluate_strip_slot"
    )]
    fn eval_recurrence_chunk(
        &mut self,
        program: &CompiledStripProgram,
        workbook: &Workbook<BoundFormula>,
        host_sheet_id: SheetId,
        host_base_pos: u32,
        strip_col: ColId,
        start_row: RowId,
        lo: u32,
        len: usize,
    ) {
        let max_back = program.max_self_back as usize;
        let total_window = max_back.saturating_add(len);
        self.prepare_recurrence(
            program.num_regs,
            program.stride_cols.len(),
            total_window,
            len,
            &program.const_strings,
        );

        for (spec, buf) in program.stride_cols.iter().zip(self.stride_words.iter_mut()) {
            if let Some(sheet) = workbook.sheet(spec.sheet)
                && let Some(dst) = buf.get_mut(..len)
            {
                let first_r = RowId(spec.base_row.saturating_add(u64::from(lo)));
                sheet.read_row_id_span_packed(first_r, spec.col, dst);
            } else if let Some(dst) = buf.get_mut(..len) {
                dst.fill(Value::EMPTY_BITS);
            }
        }

        if max_back > 0
            && let Some(host_sheet) = workbook.sheet(host_sheet_id)
            && let Some(seed_dst) = self.self_words.get_mut(..max_back)
        {
            let seed_start_u64 = start_row
                .0
                .saturating_add(u64::from(lo))
                .saturating_sub(program.max_self_back as u64);
            host_sheet.read_row_id_span_packed(RowId(seed_start_u64), strip_col, seed_dst);
        }

        let spills = SpillRegistry::new();

        for lane in 0..len {
            let mut ctx = LaneEvalCtx {
                program,
                workbook,
                host_sheet_id,
                spills: &spills,
                stride_words: &self.stride_words,
                host_base_pos,
                lane,
                max_back,
                self_words: &self.self_words,
                self_texts: &self.self_texts,
                fixed_area_cache: &mut self.fixed_area_cache,
                lane_nums: &mut self.lane_nums,
                lane_tags: &mut self.lane_tags,
                lane_payloads: &mut self.lane_payloads,
                text_pool: &mut self.text_pool,
            };
            ctx.exec_instrs(&program.instrs);

            let res_reg = program.result_reg as usize;
            let res_tag = self.lane_tags.get(res_reg).copied().unwrap_or(TAG_EMPTY);
            let win_idx = max_back.saturating_add(lane);
            match res_tag {
                TAG_NUM => {
                    let n = self.lane_nums.get(res_reg).copied().unwrap_or(0.0);
                    let w = Value::number(n).pack();
                    if let Some(sw) = self.self_words.get_mut(win_idx) {
                        *sw = w;
                    }
                    if let Some(ow) = self.out_words.get_mut(lane) {
                        *ow = w;
                    }
                }
                TAG_EMPTY => {
                    // Bare cell/IF reference empty becomes `0.0` at top-level commit.
                    let w = Value::Number(0.0).pack();
                    if let Some(sw) = self.self_words.get_mut(win_idx) {
                        *sw = w;
                    }
                    if let Some(ow) = self.out_words.get_mut(lane) {
                        *ow = w;
                    }
                }
                TAG_BOOL => {
                    let b = self.lane_payloads.get(res_reg).copied().unwrap_or(0) != 0;
                    let w = Value::Bool(b).pack();
                    if let Some(sw) = self.self_words.get_mut(win_idx) {
                        *sw = w;
                    }
                    if let Some(ow) = self.out_words.get_mut(lane) {
                        *ow = w;
                    }
                }
                TAG_ERROR => {
                    let e = err_from_u32(self.lane_payloads.get(res_reg).copied().unwrap_or(0));
                    let w = Value::Error(e).pack();
                    if let Some(sw) = self.self_words.get_mut(win_idx) {
                        *sw = w;
                    }
                    if let Some(ow) = self.out_words.get_mut(lane) {
                        *ow = w;
                    }
                }
                TAG_TEXT => {
                    let p = self.lane_payloads.get(res_reg).copied().unwrap_or(0);
                    let s = self.text_pool.get(p as usize).cloned().unwrap_or_default();
                    if let Some(st) = self.self_texts.get_mut(win_idx) {
                        *st = Some(p);
                    }
                    self.text_fixups.push((lane, s));
                    if let Some(ow) = self.out_words.get_mut(lane) {
                        *ow = Value::EMPTY_BITS;
                    }
                }
                _ => {}
            }
        }
    }
}

struct VecEvalCtx<'a> {
    program: &'a CompiledStripProgram,
    workbook: &'a Workbook<BoundFormula>,
    spills: &'a SpillRegistry,
    stride_words: &'a [Vec<u64>],
    host_base_pos: u32,
    chunk_len: usize,
}

fn exec_vec_instrs(
    instrs: &[Instr],
    ctx: &VecEvalCtx<'_>,
    regs: &mut [RegVec],
    text_pool: &mut Vec<String>,
) {
    let len = ctx.chunk_len;
    for instr in instrs {
        match instr {
            Instr::ConstNum { dst, val } => {
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.all_num = true;
                    if let Some(slice) = r.nums.get_mut(..len) {
                        slice.fill(*val);
                    }
                }
            }
            Instr::ConstBool { dst, val } => {
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.fill_scalar(&EvalScalar::Bool(*val), len, text_pool);
                }
            }
            Instr::ConstText { dst, const_str_idx } => {
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.all_num = false;
                    if let Some(t) = r.tags.get_mut(..len) {
                        t.fill(TAG_TEXT);
                    }
                    if let Some(p) = r.payloads.get_mut(..len) {
                        p.fill(u32::try_from(*const_str_idx).unwrap_or(0));
                    }
                }
            }
            Instr::ConstError { dst, err } => {
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.fill_scalar(&EvalScalar::Error(*err), len, text_pool);
                }
            }
            Instr::ConstEmpty { dst } => {
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.all_num = false;
                    if let Some(t) = r.tags.get_mut(..len) {
                        t.fill(TAG_EMPTY);
                    }
                }
            }
            Instr::LoadFixedCell {
                dst,
                sheet,
                row,
                col,
            } => {
                let scalar = ctx
                    .workbook
                    .sheet(*sheet)
                    .map_or(EvalScalar::Error(CalcError::Ref), |s| {
                        EvalScalar::from_sheet_value(s, s.value_by_id(*row, *col))
                    });
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.fill_scalar(&scalar, len, text_pool);
                }
            }
            Instr::LoadStrideCell {
                dst, stride_idx, ..
            } => {
                let target_sheet = ctx
                    .program
                    .stride_cols
                    .get(*stride_idx)
                    .and_then(|spec| ctx.workbook.sheet(spec.sheet));
                if let (Some(sheet), Some(words), Some(dst_reg)) = (
                    target_sheet,
                    ctx.stride_words.get(*stride_idx).and_then(|v| v.get(..len)),
                    regs.get_mut(*dst as usize),
                ) {
                    dst_reg.load_packed_slice(sheet, words, text_pool);
                } else if let Some(dst_reg) = regs.get_mut(*dst as usize) {
                    dst_reg.fill_scalar(&EvalScalar::Error(CalcError::Ref), len, text_pool);
                }
            }
            Instr::RowZeroArgs { dst } => {
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.all_num = true;
                    if let Some(dst_nums) = r.nums.get_mut(..len) {
                        for (i, d) in dst_nums.iter_mut().enumerate() {
                            let row_1based = ctx
                                .host_base_pos
                                .saturating_add(u32::try_from(i).unwrap_or(0))
                                .saturating_add(1);
                            *d = f64::from(row_1based);
                        }
                    }
                }
            }
            Instr::Unary { dst, src, op } => {
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                if let (Some(src_reg), Some(dst_reg)) =
                    (before.get(*src as usize), after.first_mut())
                {
                    eval_unary_vec(dst_reg, src_reg, *op, len, text_pool);
                }
            }
            Instr::Binary {
                dst,
                lhs,
                rhs,
                op,
                snap_zero,
            } => {
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                if let (Some(lhs_reg), Some(rhs_reg), Some(dst_reg)) = (
                    before.get(*lhs as usize),
                    before.get(*rhs as usize),
                    after.first_mut(),
                ) {
                    eval_binary_vec(dst_reg, lhs_reg, rhs_reg, *op, *snap_zero, len, text_pool);
                }
            }
            Instr::Abs { dst, src } => {
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                if let (Some(src_reg), Some(dst_reg)) =
                    (before.get(*src as usize), after.first_mut())
                {
                    if src_reg.all_num {
                        dst_reg.all_num = true;
                        if let (Some(d_slice), Some(s_slice)) =
                            (dst_reg.nums.get_mut(..len), src_reg.nums.get(..len))
                        {
                            for (d, &s) in d_slice.iter_mut().zip(s_slice.iter()) {
                                *d = s.abs();
                            }
                        }
                    } else {
                        for lane in 0..len {
                            let s = src_reg.read_scalar(lane, text_pool);
                            let res = s
                                .to_number()
                                .map_or_else(EvalScalar::Error, |n| EvalScalar::number(n.abs()));
                            dst_reg.write_scalar(lane, res, text_pool);
                        }
                        dst_reg.refresh_all_num(len);
                    }
                }
            }
            Instr::Not { dst, src } => {
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                if let (Some(src_reg), Some(dst_reg)) =
                    (before.get(*src as usize), after.first_mut())
                {
                    dst_reg.all_num = false;
                    if src_reg.all_num {
                        if let (Some(d_tags), Some(d_pay), Some(s_nums)) = (
                            dst_reg.tags.get_mut(..len),
                            dst_reg.payloads.get_mut(..len),
                            src_reg.nums.get(..len),
                        ) {
                            d_tags.fill(TAG_BOOL);
                            for (dp, &sn) in d_pay.iter_mut().zip(s_nums.iter()) {
                                *dp = u32::from(sn == 0.0);
                            }
                        }
                    } else {
                        for lane in 0..len {
                            let s = src_reg.read_scalar(lane, text_pool);
                            let res = s
                                .to_bool()
                                .map_or_else(EvalScalar::Error, |b| EvalScalar::Bool(!b));
                            dst_reg.write_scalar(lane, res, text_pool);
                        }
                    }
                }
            }
            Instr::Round {
                dst,
                val,
                digits,
                mode,
            } => {
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                if let (Some(v_reg), Some(d_reg), Some(dst_reg)) = (
                    before.get(*val as usize),
                    before.get(*digits as usize),
                    after.first_mut(),
                ) {
                    for lane in 0..len {
                        let vs = v_reg.read_scalar(lane, text_pool);
                        let ds = d_reg.read_scalar(lane, text_pool);
                        let res = eval_round_scalar(&vs, &ds, *mode);
                        dst_reg.write_scalar(lane, res, text_pool);
                    }
                    dst_reg.refresh_all_num(len);
                }
            }
            Instr::SumFixedArea { dst, area_idx } => {
                let s = eval_fixed_area_op(
                    ctx.program.fixed_areas.get(*area_idx),
                    ctx.workbook,
                    ctx.spills,
                    FixedAreaOp::Sum,
                );
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.fill_scalar(&s, len, text_pool);
                }
            }
            Instr::AvgFixedArea { dst, area_idx } => {
                let s = eval_fixed_area_op(
                    ctx.program.fixed_areas.get(*area_idx),
                    ctx.workbook,
                    ctx.spills,
                    FixedAreaOp::Avg,
                );
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.fill_scalar(&s, len, text_pool);
                }
            }
            Instr::MinMaxFixedArea {
                dst,
                area_idx,
                is_max,
            } => {
                let s = eval_fixed_area_op(
                    ctx.program.fixed_areas.get(*area_idx),
                    ctx.workbook,
                    ctx.spills,
                    FixedAreaOp::MinMax(*is_max),
                );
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.fill_scalar(&s, len, text_pool);
                }
            }
            Instr::CountFixedArea {
                dst,
                area_idx,
                count_all,
            } => {
                let s = eval_fixed_area_op(
                    ctx.program.fixed_areas.get(*area_idx),
                    ctx.workbook,
                    ctx.spills,
                    FixedAreaOp::Count(*count_all),
                );
                if let Some(r) = regs.get_mut(*dst as usize) {
                    r.fill_scalar(&s, len, text_pool);
                }
            }
            Instr::AndOr { dst, args, is_or } => {
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                let Some(dst_reg) = after.first_mut() else {
                    continue;
                };
                for lane in 0..len {
                    let res = eval_and_or_lane(before, args, *is_or, lane, text_pool);
                    dst_reg.write_scalar(lane, res, text_pool);
                }
            }
            Instr::IfSelect {
                dst,
                cond,
                then_instrs,
                then_reg,
                else_instrs,
                else_reg,
            } => {
                let mut any_true = false;
                let mut any_false = false;
                if let Some(c_reg) = regs.get(*cond as usize) {
                    if c_reg.all_num {
                        if let Some(slice) = c_reg.nums.get(..len) {
                            for &n in slice {
                                if n != 0.0 {
                                    any_true = true;
                                } else {
                                    any_false = true;
                                }
                                if any_true && any_false {
                                    break;
                                }
                            }
                        }
                    } else {
                        for lane in 0..len {
                            match read_reg_bool(c_reg, lane, text_pool) {
                                Ok(true) => any_true = true,
                                Ok(false) => any_false = true,
                                Err(_) => {}
                            }
                            if any_true && any_false {
                                break;
                            }
                        }
                    }
                }
                if any_true {
                    exec_vec_instrs(then_instrs, ctx, regs, text_pool);
                }
                if any_false {
                    exec_vec_instrs(else_instrs, ctx, regs, text_pool);
                }
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                let (Some(c_reg), Some(t_reg), Some(e_reg), Some(dst_reg)) = (
                    before.get(*cond as usize),
                    before.get(*then_reg as usize),
                    before.get(*else_reg as usize),
                    after.first_mut(),
                ) else {
                    continue;
                };
                if c_reg.all_num {
                    for lane in 0..len {
                        let cond_val = c_reg.nums.get(lane).copied().unwrap_or(0.0) != 0.0;
                        let picked = if cond_val { t_reg } else { e_reg };
                        dst_reg.copy_lane_from(lane, picked, false);
                    }
                } else {
                    for lane in 0..len {
                        match read_reg_bool(c_reg, lane, text_pool) {
                            Ok(true) => dst_reg.copy_lane_from(lane, t_reg, false),
                            Ok(false) => dst_reg.copy_lane_from(lane, e_reg, false),
                            Err(err) => {
                                if let (Some(dt), Some(dp)) =
                                    (dst_reg.tags.get_mut(lane), dst_reg.payloads.get_mut(lane))
                                {
                                    *dt = TAG_ERROR;
                                    *dp = err_to_u32(err);
                                }
                            }
                        }
                    }
                }
                dst_reg.refresh_all_num(len);
            }
            Instr::IfErrorSelect {
                dst,
                primary,
                fallback_instrs,
                fallback_reg,
                only_na,
            } => {
                let need_fb = regs.get(*primary as usize).is_some_and(|p_reg| {
                    if p_reg.all_num {
                        return false;
                    }
                    (0..len).any(|lane| {
                        let tag = p_reg.tags.get(lane).copied().unwrap_or(TAG_EMPTY);
                        tag == TAG_ERROR && {
                            let err = err_from_u32(p_reg.payloads.get(lane).copied().unwrap_or(0));
                            !*only_na || err == CalcError::Na
                        }
                    })
                });
                if need_fb {
                    exec_vec_instrs(fallback_instrs, ctx, regs, text_pool);
                }
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                let (Some(p_reg), Some(fb_reg), Some(dst_reg)) = (
                    before.get(*primary as usize),
                    before.get(*fallback_reg as usize),
                    after.first_mut(),
                ) else {
                    continue;
                };
                if p_reg.all_num {
                    dst_reg.all_num = true;
                    if let (Some(dn), Some(pn)) =
                        (dst_reg.nums.get_mut(..len), p_reg.nums.get(..len))
                    {
                        dn.copy_from_slice(pn);
                    }
                } else {
                    for lane in 0..len {
                        let tag = p_reg.tags.get(lane).copied().unwrap_or(TAG_EMPTY);
                        let is_caught = tag == TAG_ERROR && {
                            let err = err_from_u32(p_reg.payloads.get(lane).copied().unwrap_or(0));
                            !*only_na || err == CalcError::Na
                        };
                        if is_caught {
                            dst_reg.copy_lane_from(lane, fb_reg, false);
                        } else {
                            dst_reg.copy_lane_from(lane, p_reg, true);
                        }
                    }
                    dst_reg.refresh_all_num(len);
                }
            }
            Instr::VlookupExact {
                dst,
                needle,
                area_idx,
                col_index,
            } => {
                let Some(area) = ctx.program.fixed_areas.get(*area_idx) else {
                    continue;
                };
                let area_ref = fixed_area_resolved(*area);
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                let (Some(n_reg), Some(dst_reg)) =
                    (before.get(*needle as usize), after.first_mut())
                else {
                    continue;
                };
                for lane in 0..len {
                    let needle_s = n_reg.read_scalar(lane, text_pool);
                    let res = lookup::vlookup_exact_on_ref(
                        &needle_s,
                        area_ref.clone(),
                        *col_index,
                        ctx.workbook,
                        ctx.spills,
                    );
                    dst_reg.write_scalar(lane, res, text_pool);
                }
                dst_reg.refresh_all_num(len);
            }
            Instr::MatchExact {
                dst,
                needle,
                area_idx,
            } => {
                let Some(area) = ctx.program.fixed_areas.get(*area_idx) else {
                    continue;
                };
                let area_ref = fixed_area_resolved(*area);
                let dst_idx = *dst as usize;
                let (before, after) = regs.split_at_mut(dst_idx);
                let (Some(n_reg), Some(dst_reg)) =
                    (before.get(*needle as usize), after.first_mut())
                else {
                    continue;
                };
                for lane in 0..len {
                    let needle_s = n_reg.read_scalar(lane, text_pool);
                    let res = lookup::match_exact_on_ref(
                        &needle_s,
                        area_ref.clone(),
                        ctx.workbook,
                        ctx.spills,
                    );
                    dst_reg.write_scalar(lane, res, text_pool);
                }
                dst_reg.refresh_all_num(len);
            }
        }
    }
}

struct LaneEvalCtx<'a> {
    program: &'a CompiledStripProgram,
    workbook: &'a Workbook<BoundFormula>,
    host_sheet_id: SheetId,
    spills: &'a SpillRegistry,
    stride_words: &'a [Vec<u64>],
    host_base_pos: u32,
    lane: usize,
    max_back: usize,
    self_words: &'a [u64],
    self_texts: &'a [Option<u32>],
    fixed_area_cache: &'a mut Vec<((usize, u8), EvalScalar)>,
    lane_nums: &'a mut [f64],
    lane_tags: &'a mut [u8],
    lane_payloads: &'a mut [u32],
    text_pool: &'a mut Vec<String>,
}

impl LaneEvalCtx<'_> {
    fn set_scalar(&mut self, reg: usize, scalar: EvalScalar) {
        let Some(tag_slot) = self.lane_tags.get_mut(reg) else {
            return;
        };
        match scalar {
            EvalScalar::Empty => *tag_slot = TAG_EMPTY,
            EvalScalar::Number(n) => {
                if !n.is_finite() {
                    *tag_slot = TAG_ERROR;
                    if let Some(p) = self.lane_payloads.get_mut(reg) {
                        *p = err_to_u32(CalcError::Num);
                    }
                } else {
                    *tag_slot = TAG_NUM;
                    if let Some(slot) = self.lane_nums.get_mut(reg) {
                        *slot = if n == 0.0 { 0.0 } else { n };
                    }
                }
            }
            EvalScalar::Bool(b) => {
                *tag_slot = TAG_BOOL;
                if let Some(p) = self.lane_payloads.get_mut(reg) {
                    *p = u32::from(b);
                }
            }
            EvalScalar::Error(e) => {
                *tag_slot = TAG_ERROR;
                if let Some(p) = self.lane_payloads.get_mut(reg) {
                    *p = err_to_u32(e);
                }
            }
            EvalScalar::Text(s) => {
                let idx = intern_pool_string(self.text_pool, &s);
                *tag_slot = TAG_TEXT;
                if let Some(p) = self.lane_payloads.get_mut(reg) {
                    *p = idx;
                }
            }
        }
    }

    fn read_scalar(&self, reg: usize) -> EvalScalar {
        match self.lane_tags.get(reg).copied().unwrap_or(TAG_EMPTY) {
            TAG_NUM => EvalScalar::number(self.lane_nums.get(reg).copied().unwrap_or(0.0)),
            TAG_EMPTY => EvalScalar::Empty,
            TAG_BOOL => EvalScalar::Bool(self.lane_payloads.get(reg).copied().unwrap_or(0) != 0),
            TAG_ERROR => EvalScalar::Error(err_from_u32(
                self.lane_payloads.get(reg).copied().unwrap_or(0),
            )),
            TAG_TEXT => {
                let idx = self.lane_payloads.get(reg).copied().unwrap_or(0) as usize;
                EvalScalar::Text(self.text_pool.get(idx).cloned().unwrap_or_default())
            }
            _ => EvalScalar::Empty,
        }
    }

    fn load_word(&mut self, reg: usize, sheet: Option<&Sheet>, word: u64) {
        if (word & EXP_ALL_ONES) != EXP_ALL_ONES {
            let n = f64::from_bits(if word == NEG_ZERO_BITS { 0 } else { word });
            if let (Some(t), Some(ns)) = (self.lane_tags.get_mut(reg), self.lane_nums.get_mut(reg))
            {
                *t = TAG_NUM;
                *ns = n;
            }
            return;
        }
        let s = match (sheet, Value::unpack(word)) {
            (_, Value::Empty) => EvalScalar::Empty,
            (_, Value::Number(n)) => EvalScalar::number(n),
            (_, Value::Bool(b)) => EvalScalar::Bool(b),
            (_, Value::Error(e)) => EvalScalar::Error(e),
            (Some(sh), Value::Text(id)) => EvalScalar::Text(String::from(sh.strings().get(id))),
            (None, Value::Text(_)) => EvalScalar::Error(CalcError::Ref),
            (_, Value::Array(_)) => EvalScalar::Error(CalcError::Spill),
        };
        self.set_scalar(reg, s);
    }

    fn exec_instrs(&mut self, instrs: &[Instr]) {
        for instr in instrs {
            match instr {
                Instr::ConstNum { dst, val } => {
                    let r = *dst as usize;
                    if let (Some(t), Some(ns)) =
                        (self.lane_tags.get_mut(r), self.lane_nums.get_mut(r))
                    {
                        *t = TAG_NUM;
                        *ns = *val;
                    }
                }
                Instr::ConstBool { dst, val } => {
                    self.set_scalar(*dst as usize, EvalScalar::Bool(*val));
                }
                Instr::ConstText { dst, const_str_idx } => {
                    let r = *dst as usize;
                    if let (Some(t), Some(p)) =
                        (self.lane_tags.get_mut(r), self.lane_payloads.get_mut(r))
                    {
                        *t = TAG_TEXT;
                        *p = u32::try_from(*const_str_idx).unwrap_or(0);
                    }
                }
                Instr::ConstError { dst, err } => {
                    self.set_scalar(*dst as usize, EvalScalar::Error(*err));
                }
                Instr::ConstEmpty { dst } => {
                    if let Some(t) = self.lane_tags.get_mut(*dst as usize) {
                        *t = TAG_EMPTY;
                    }
                }
                Instr::LoadFixedCell {
                    dst,
                    sheet,
                    row,
                    col,
                } => {
                    let s = self
                        .workbook
                        .sheet(*sheet)
                        .map_or(EvalScalar::Error(CalcError::Ref), |sh| {
                            EvalScalar::from_sheet_value(sh, sh.value_by_id(*row, *col))
                        });
                    self.set_scalar(*dst as usize, s);
                }
                Instr::LoadStrideCell {
                    dst,
                    stride_idx,
                    self_back_offset,
                } => {
                    let dst_r = *dst as usize;
                    if let Some(back) = self_back_offset {
                        let win_idx = self
                            .max_back
                            .saturating_add(self.lane)
                            .saturating_sub(*back as usize);
                        if let Some(Some(pool_idx)) = self.self_texts.get(win_idx) {
                            if let (Some(t), Some(p)) = (
                                self.lane_tags.get_mut(dst_r),
                                self.lane_payloads.get_mut(dst_r),
                            ) {
                                *t = TAG_TEXT;
                                *p = *pool_idx;
                            }
                        } else {
                            let w = self
                                .self_words
                                .get(win_idx)
                                .copied()
                                .unwrap_or(Value::EMPTY_BITS);
                            let sh = self.workbook.sheet(self.host_sheet_id);
                            self.load_word(dst_r, sh, w);
                        }
                    } else {
                        let w = self
                            .stride_words
                            .get(*stride_idx)
                            .and_then(|v| v.get(self.lane))
                            .copied()
                            .unwrap_or(Value::EMPTY_BITS);
                        let sh = self
                            .program
                            .stride_cols
                            .get(*stride_idx)
                            .and_then(|spec| self.workbook.sheet(spec.sheet));
                        self.load_word(dst_r, sh, w);
                    }
                }
                Instr::RowZeroArgs { dst } => {
                    let row_1based = self
                        .host_base_pos
                        .saturating_add(u32::try_from(self.lane).unwrap_or(0))
                        .saturating_add(1);
                    let r = *dst as usize;
                    if let (Some(t), Some(ns)) =
                        (self.lane_tags.get_mut(r), self.lane_nums.get_mut(r))
                    {
                        *t = TAG_NUM;
                        *ns = f64::from(row_1based);
                    }
                }
                Instr::Unary { dst, src, op } => {
                    let s = self.read_scalar(*src as usize);
                    let res = apply_unary_scalar(byte_unary_to_ast(*op), &s);
                    self.set_scalar(*dst as usize, res);
                }
                Instr::Binary {
                    dst,
                    lhs,
                    rhs,
                    op,
                    snap_zero,
                } => {
                    let lr = *lhs as usize;
                    let rr = *rhs as usize;
                    let dr = *dst as usize;
                    let lt = self.lane_tags.get(lr).copied().unwrap_or(TAG_EMPTY);
                    let rt = self.lane_tags.get(rr).copied().unwrap_or(TAG_EMPTY);
                    if lt == TAG_NUM && rt == TAG_NUM {
                        let x = self.lane_nums.get(lr).copied().unwrap_or(0.0);
                        let y = self.lane_nums.get(rr).copied().unwrap_or(0.0);
                        if let Some(res_n) = fast_binary_num(*op, x, y, *snap_zero) {
                            if let (Some(t), Some(ns)) =
                                (self.lane_tags.get_mut(dr), self.lane_nums.get_mut(dr))
                            {
                                *t = TAG_NUM;
                                *ns = res_n;
                            }
                            continue;
                        }
                    }
                    let a = self.read_scalar(lr);
                    let b = self.read_scalar(rr);
                    let res = apply_binary_scalar(*op, &a, &b, *snap_zero);
                    self.set_scalar(dr, res);
                }
                Instr::Abs { dst, src } => {
                    let s = self.read_scalar(*src as usize);
                    let res = s
                        .to_number()
                        .map_or_else(EvalScalar::Error, |n| EvalScalar::number(n.abs()));
                    self.set_scalar(*dst as usize, res);
                }
                Instr::Not { dst, src } => {
                    let s = self.read_scalar(*src as usize);
                    let res = s
                        .to_bool()
                        .map_or_else(EvalScalar::Error, |b| EvalScalar::Bool(!b));
                    self.set_scalar(*dst as usize, res);
                }
                Instr::Round {
                    dst,
                    val,
                    digits,
                    mode,
                } => {
                    let vs = self.read_scalar(*val as usize);
                    let ds = self.read_scalar(*digits as usize);
                    self.set_scalar(*dst as usize, eval_round_scalar(&vs, &ds, *mode));
                }
                Instr::SumFixedArea { dst, area_idx } => {
                    let s = self.cached_fixed_area(*area_idx, FixedAreaOp::Sum);
                    self.set_scalar(*dst as usize, s);
                }
                Instr::AvgFixedArea { dst, area_idx } => {
                    let s = self.cached_fixed_area(*area_idx, FixedAreaOp::Avg);
                    self.set_scalar(*dst as usize, s);
                }
                Instr::MinMaxFixedArea {
                    dst,
                    area_idx,
                    is_max,
                } => {
                    let s = self.cached_fixed_area(*area_idx, FixedAreaOp::MinMax(*is_max));
                    self.set_scalar(*dst as usize, s);
                }
                Instr::CountFixedArea {
                    dst,
                    area_idx,
                    count_all,
                } => {
                    let s = self.cached_fixed_area(*area_idx, FixedAreaOp::Count(*count_all));
                    self.set_scalar(*dst as usize, s);
                }
                Instr::AndOr { dst, args, is_or } => {
                    let mut acc = !*is_or;
                    let mut seen_any = false;
                    let mut out_err: Option<CalcError> = None;
                    for &(arg_reg, is_cell_ref) in args {
                        let s = self.read_scalar(arg_reg as usize);
                        match eval_and_or_step(&s, is_cell_ref) {
                            Ok(Some(b)) => {
                                seen_any = true;
                                acc = if *is_or { acc || b } else { acc && b };
                            }
                            Ok(None) => {}
                            Err(e) => {
                                out_err = Some(e);
                                break;
                            }
                        }
                    }
                    let res = if let Some(e) = out_err {
                        EvalScalar::Error(e)
                    } else if !seen_any {
                        EvalScalar::Error(CalcError::Value)
                    } else {
                        EvalScalar::Bool(acc)
                    };
                    self.set_scalar(*dst as usize, res);
                }
                Instr::IfSelect {
                    dst,
                    cond,
                    then_instrs,
                    then_reg,
                    else_instrs,
                    else_reg,
                } => {
                    let cs = self.read_scalar(*cond as usize);
                    match cs.to_bool() {
                        Ok(true) => {
                            self.exec_instrs(then_instrs);
                            self.copy_lane_reg(*dst as usize, *then_reg as usize, false);
                        }
                        Ok(false) => {
                            self.exec_instrs(else_instrs);
                            self.copy_lane_reg(*dst as usize, *else_reg as usize, false);
                        }
                        Err(e) => self.set_scalar(*dst as usize, EvalScalar::Error(e)),
                    }
                }
                Instr::IfErrorSelect {
                    dst,
                    primary,
                    fallback_instrs,
                    fallback_reg,
                    only_na,
                } => {
                    let p_idx = *primary as usize;
                    let tag = self.lane_tags.get(p_idx).copied().unwrap_or(TAG_EMPTY);
                    let is_caught = tag == TAG_ERROR && {
                        let e = err_from_u32(self.lane_payloads.get(p_idx).copied().unwrap_or(0));
                        !*only_na || e == CalcError::Na
                    };
                    if is_caught {
                        self.exec_instrs(fallback_instrs);
                        self.copy_lane_reg(*dst as usize, *fallback_reg as usize, false);
                    } else {
                        self.copy_lane_reg(*dst as usize, p_idx, true);
                    }
                }
                Instr::VlookupExact {
                    dst,
                    needle,
                    area_idx,
                    col_index,
                } => {
                    let Some(area) = self.program.fixed_areas.get(*area_idx) else {
                        continue;
                    };
                    let n = self.read_scalar(*needle as usize);
                    let res = lookup::vlookup_exact_on_ref(
                        &n,
                        fixed_area_resolved(*area),
                        *col_index,
                        self.workbook,
                        self.spills,
                    );
                    self.set_scalar(*dst as usize, res);
                }
                Instr::MatchExact {
                    dst,
                    needle,
                    area_idx,
                } => {
                    let Some(area) = self.program.fixed_areas.get(*area_idx) else {
                        continue;
                    };
                    let n = self.read_scalar(*needle as usize);
                    let res = lookup::match_exact_on_ref(
                        &n,
                        fixed_area_resolved(*area),
                        self.workbook,
                        self.spills,
                    );
                    self.set_scalar(*dst as usize, res);
                }
            }
        }
    }

    fn copy_lane_reg(&mut self, dst: usize, src: usize, zero_empty: bool) {
        let st = self.lane_tags.get(src).copied().unwrap_or(TAG_EMPTY);
        if zero_empty && st == TAG_EMPTY {
            if let (Some(dt), Some(dn)) = (self.lane_tags.get_mut(dst), self.lane_nums.get_mut(dst))
            {
                *dt = TAG_NUM;
                *dn = 0.0;
            }
            return;
        }
        let sn = self.lane_nums.get(src).copied().unwrap_or(0.0);
        let sp = self.lane_payloads.get(src).copied().unwrap_or(0);
        if let (Some(dt), Some(dn), Some(dp)) = (
            self.lane_tags.get_mut(dst),
            self.lane_nums.get_mut(dst),
            self.lane_payloads.get_mut(dst),
        ) {
            *dt = st;
            *dn = sn;
            *dp = sp;
        }
    }

    fn cached_fixed_area(&mut self, area_idx: usize, op: FixedAreaOp) -> EvalScalar {
        let key = (area_idx, op.discriminant());
        if let Some((_, cached)) = self.fixed_area_cache.iter().find(|(k, _)| *k == key) {
            return cached.clone();
        }
        let res = eval_fixed_area_op(
            self.program.fixed_areas.get(area_idx),
            self.workbook,
            self.spills,
            op,
        );
        self.fixed_area_cache.push((key, res.clone()));
        res
    }
}

#[derive(Clone, Copy)]
enum FixedAreaOp {
    Sum,
    Avg,
    MinMax(bool),
    Count(bool),
}

impl FixedAreaOp {
    const fn discriminant(self) -> u8 {
        match self {
            Self::Sum => 0,
            Self::Avg => 1,
            Self::MinMax(false) => 2,
            Self::MinMax(true) => 3,
            Self::Count(false) => 4,
            Self::Count(true) => 5,
        }
    }
}

fn eval_fixed_area_op(
    spec: Option<&FixedAreaSpec>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    op: FixedAreaOp,
) -> EvalScalar {
    let Some(area) = spec else {
        return EvalScalar::Error(CalcError::Ref);
    };
    let val = EvalValue::Reference(fixed_area_resolved(*area));
    match op {
        FixedAreaOp::Sum => eval_sum(core::slice::from_ref(&val), workbook, spills),
        FixedAreaOp::Avg => eval_average(core::slice::from_ref(&val), workbook, spills),
        FixedAreaOp::MinMax(is_max) => {
            eval_min_max(core::slice::from_ref(&val), workbook, spills, !is_max)
        }
        FixedAreaOp::Count(count_all) => {
            if count_all {
                eval_counta(core::slice::from_ref(&val), workbook, spills)
            } else {
                eval_count(core::slice::from_ref(&val), workbook, spills)
            }
        }
    }
}

const fn fixed_area_resolved(area: FixedAreaSpec) -> ResolvedRef {
    ResolvedRef::Area {
        sheet: area.sheet,
        first_row: area.first_row,
        last_row: area.last_row,
        first_col: area.first_col,
        last_col: area.last_col,
    }
}

fn eval_unary_vec(
    dst: &mut RegVec,
    src: &RegVec,
    op: UnaryByteOp,
    len: usize,
    text_pool: &mut Vec<String>,
) {
    if src.all_num {
        dst.all_num = true;
        if let (Some(d_slice), Some(s_slice)) = (dst.nums.get_mut(..len), src.nums.get(..len)) {
            match op {
                UnaryByteOp::Plus => d_slice.copy_from_slice(s_slice),
                UnaryByteOp::Neg => {
                    for (d, &x) in d_slice.iter_mut().zip(s_slice.iter()) {
                        let v = -x;
                        *d = if v == 0.0 { 0.0 } else { v };
                    }
                }
                UnaryByteOp::Percent => {
                    for (d, &x) in d_slice.iter_mut().zip(s_slice.iter()) {
                        let v = x / 100.0;
                        *d = if v == 0.0 { 0.0 } else { v };
                    }
                }
            }
        }
        return;
    }
    let ast_op = byte_unary_to_ast(op);
    for lane in 0..len {
        let s = src.read_scalar(lane, text_pool);
        dst.write_scalar(lane, apply_unary_scalar(ast_op, &s), text_pool);
    }
    dst.refresh_all_num(len);
}

#[expect(
    clippy::too_many_lines,
    reason = "numeric vector fast paths + exact scalar fallback"
)]
fn eval_binary_vec(
    dst: &mut RegVec,
    lhs: &RegVec,
    rhs: &RegVec,
    op: BinaryOp,
    snap_zero: bool,
    len: usize,
    text_pool: &mut Vec<String>,
) {
    if lhs.all_num && rhs.all_num {
        match op {
            BinaryOp::Add => {
                if let (Some(dn), Some(ln), Some(rn)) = (
                    dst.nums.get_mut(..len),
                    lhs.nums.get(..len),
                    rhs.nums.get(..len),
                ) {
                    let mut bad = false;
                    for ((d, &x), &y) in dn.iter_mut().zip(ln.iter()).zip(rn.iter()) {
                        let raw = x + y;
                        bad |= !raw.is_finite();
                        *d = if snap_zero {
                            fast_snap_cancelled_zero(x, raw)
                        } else if raw == 0.0 {
                            0.0
                        } else {
                            raw
                        };
                    }
                    if !bad {
                        dst.all_num = true;
                        return;
                    }
                }
            }
            BinaryOp::Sub => {
                if let (Some(dn), Some(ln), Some(rn)) = (
                    dst.nums.get_mut(..len),
                    lhs.nums.get(..len),
                    rhs.nums.get(..len),
                ) {
                    let mut bad = false;
                    for ((d, &x), &y) in dn.iter_mut().zip(ln.iter()).zip(rn.iter()) {
                        let raw = x - y;
                        bad |= !raw.is_finite();
                        *d = if snap_zero {
                            fast_snap_cancelled_zero(x, raw)
                        } else if raw == 0.0 {
                            0.0
                        } else {
                            raw
                        };
                    }
                    if !bad {
                        dst.all_num = true;
                        return;
                    }
                }
            }
            BinaryOp::Mul => {
                if let (Some(dn), Some(ln), Some(rn)) = (
                    dst.nums.get_mut(..len),
                    lhs.nums.get(..len),
                    rhs.nums.get(..len),
                ) {
                    let mut bad = false;
                    for ((d, &x), &y) in dn.iter_mut().zip(ln.iter()).zip(rn.iter()) {
                        let raw = x * y;
                        bad |= !raw.is_finite();
                        *d = if raw == 0.0 { 0.0 } else { raw };
                    }
                    if !bad {
                        dst.all_num = true;
                        return;
                    }
                }
            }
            BinaryOp::Div => {
                if let (Some(dn), Some(ln), Some(rn)) = (
                    dst.nums.get_mut(..len),
                    lhs.nums.get(..len),
                    rhs.nums.get(..len),
                ) {
                    let mut bad = false;
                    for ((d, &x), &y) in dn.iter_mut().zip(ln.iter()).zip(rn.iter()) {
                        let raw = x / y;
                        bad |= y == 0.0 || !raw.is_finite();
                        *d = if raw == 0.0 { 0.0 } else { raw };
                    }
                    if !bad {
                        dst.all_num = true;
                        return;
                    }
                }
            }
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Le
            | BinaryOp::Ge => {
                dst.all_num = false;
                if let (Some(dt), Some(dp), Some(ln), Some(rn)) = (
                    dst.tags.get_mut(..len),
                    dst.payloads.get_mut(..len),
                    lhs.nums.get(..len),
                    rhs.nums.get(..len),
                ) {
                    dt.fill(TAG_BOOL);
                    for ((p, &x), &y) in dp.iter_mut().zip(ln.iter()).zip(rn.iter()) {
                        let ord = if x == y {
                            Ordering::Equal
                        } else {
                            cmp_excel_f64(x, y).unwrap_or(Ordering::Equal)
                        };
                        let flag = match op {
                            BinaryOp::Eq => ord == Ordering::Equal,
                            BinaryOp::Ne => ord != Ordering::Equal,
                            BinaryOp::Lt => ord == Ordering::Less,
                            BinaryOp::Gt => ord == Ordering::Greater,
                            BinaryOp::Le => ord != Ordering::Greater,
                            BinaryOp::Ge => ord != Ordering::Less,
                            _ => false,
                        };
                        *p = u32::from(flag);
                    }
                    return;
                }
            }
            BinaryOp::Pow | BinaryOp::Concat => {}
        }
    }

    for lane in 0..len {
        let a = lhs.read_scalar(lane, text_pool);
        let b = rhs.read_scalar(lane, text_pool);
        dst.write_scalar(lane, apply_binary_scalar(op, &a, &b, snap_zero), text_pool);
    }
    dst.refresh_all_num(len);
}

#[inline]
fn fast_binary_num(op: BinaryOp, x: f64, y: f64, snap_zero: bool) -> Option<f64> {
    match op {
        BinaryOp::Add => {
            let raw = x + y;
            raw.is_finite().then(|| {
                if snap_zero {
                    fast_snap_cancelled_zero(x, raw)
                } else if raw == 0.0 {
                    0.0
                } else {
                    raw
                }
            })
        }
        BinaryOp::Sub => {
            let raw = x - y;
            raw.is_finite().then(|| {
                if snap_zero {
                    fast_snap_cancelled_zero(x, raw)
                } else if raw == 0.0 {
                    0.0
                } else {
                    raw
                }
            })
        }
        BinaryOp::Mul => {
            let raw = x * y;
            raw.is_finite()
                .then_some(if raw == 0.0 { 0.0 } else { raw })
        }
        BinaryOp::Div => {
            if y == 0.0 {
                return None;
            }
            let raw = x / y;
            raw.is_finite()
                .then_some(if raw == 0.0 { 0.0 } else { raw })
        }
        _ => None,
    }
}

#[inline]
fn fast_snap_cancelled_zero(left: f64, raw: f64) -> f64 {
    let scaled = left.abs() * CANCEL_GUARD_MULT;
    if raw.abs() > scaled && raw.abs() >= f64::MIN_POSITIVE {
        if raw == 0.0 { 0.0 } else { raw }
    } else {
        snap_cancelled_zero(left, raw)
    }
}

fn eval_and_or_lane(
    regs: &[RegVec],
    args: &[(u16, bool)],
    is_or: bool,
    lane: usize,
    text_pool: &[String],
) -> EvalScalar {
    let mut acc = !is_or;
    let mut seen_any = false;
    for &(arg_reg, is_cell_ref) in args {
        let Some(r) = regs.get(arg_reg as usize) else {
            return EvalScalar::Error(CalcError::Value);
        };
        let s = r.read_scalar(lane, text_pool);
        match eval_and_or_step(&s, is_cell_ref) {
            Ok(Some(b)) => {
                seen_any = true;
                acc = if is_or { acc || b } else { acc && b };
            }
            Ok(None) => {}
            Err(e) => return EvalScalar::Error(e),
        }
    }
    if seen_any {
        EvalScalar::Bool(acc)
    } else {
        EvalScalar::Error(CalcError::Value)
    }
}

fn eval_and_or_step(scalar: &EvalScalar, is_cell_ref: bool) -> Result<Option<bool>, CalcError> {
    if is_cell_ref {
        match scalar {
            EvalScalar::Error(e) => Err(*e),
            EvalScalar::Bool(b) => Ok(Some(*b)),
            EvalScalar::Number(n) => Ok(Some(*n != 0.0)),
            EvalScalar::Empty | EvalScalar::Text(_) => Ok(None),
        }
    } else {
        match scalar {
            EvalScalar::Empty => Ok(None),
            other => other.to_bool().map(Some),
        }
    }
}

fn read_reg_bool(reg: &RegVec, lane: usize, text_pool: &[String]) -> Result<bool, CalcError> {
    if reg.all_num {
        return Ok(reg.nums.get(lane).copied().unwrap_or(0.0) != 0.0);
    }
    match reg.tags.get(lane).copied().unwrap_or(TAG_EMPTY) {
        TAG_NUM => Ok(reg.nums.get(lane).copied().unwrap_or(0.0) != 0.0),
        TAG_EMPTY => Ok(false),
        TAG_BOOL => Ok(reg.payloads.get(lane).copied().unwrap_or(0) != 0),
        TAG_ERROR => Err(err_from_u32(reg.payloads.get(lane).copied().unwrap_or(0))),
        TAG_TEXT => {
            let idx = reg.payloads.get(lane).copied().unwrap_or(0) as usize;
            let s = text_pool.get(idx).map_or("", String::as_str);
            if s.eq_ignore_ascii_case("TRUE") {
                Ok(true)
            } else if s.eq_ignore_ascii_case("FALSE") {
                Ok(false)
            } else {
                Err(CalcError::Value)
            }
        }
        _ => Ok(false),
    }
}

fn pack_root_vec(
    root: &RegVec,
    len: usize,
    text_pool: &[String],
    out_words: &mut [u64],
    text_fixups: &mut Vec<(usize, String)>,
) {
    let Some(dst) = out_words.get_mut(..len) else {
        return;
    };
    if root.all_num {
        if let Some(nums) = root.nums.get(..len) {
            for (d, &n) in dst.iter_mut().zip(nums.iter()) {
                *d = if !n.is_finite() {
                    Value::Error(CalcError::Num).pack()
                } else if n == 0.0 {
                    0
                } else {
                    n.to_bits()
                };
            }
        }
        return;
    }

    for (lane, d) in dst.iter_mut().enumerate() {
        let tag = root.tags.get(lane).copied().unwrap_or(TAG_EMPTY);
        *d = match tag {
            TAG_NUM => Value::number(root.nums.get(lane).copied().unwrap_or(0.0)).pack(),
            TAG_EMPTY => Value::Number(0.0).pack(),
            TAG_BOOL => Value::Bool(root.payloads.get(lane).copied().unwrap_or(0) != 0).pack(),
            TAG_ERROR => {
                Value::Error(err_from_u32(root.payloads.get(lane).copied().unwrap_or(0))).pack()
            }
            TAG_TEXT => {
                let idx = root.payloads.get(lane).copied().unwrap_or(0) as usize;
                let s = text_pool.get(idx).cloned().unwrap_or_default();
                text_fixups.push((lane, s));
                Value::EMPTY_BITS
            }
            _ => Value::Number(0.0).pack(),
        };
    }
}

const fn byte_unary_to_ast(op: UnaryByteOp) -> UnaryOp {
    match op {
        UnaryByteOp::Plus => UnaryOp::Plus,
        UnaryByteOp::Neg => UnaryOp::Neg,
        UnaryByteOp::Percent => UnaryOp::Percent,
    }
}

fn intern_pool_string(pool: &mut Vec<String>, s: &str) -> u32 {
    let idx = pool.len();
    pool.push(String::from(s));
    u32::try_from(idx).unwrap_or(u32::MAX)
}

const fn err_to_u32(e: CalcError) -> u32 {
    match e {
        CalcError::Div0 => 1,
        CalcError::Value => 2,
        CalcError::Ref => 3,
        CalcError::Name => 4,
        CalcError::Num => 5,
        CalcError::Na => 6,
        CalcError::Null => 7,
        CalcError::Spill => 8,
        CalcError::Calc => 9,
        CalcError::Cycle => 10,
    }
}

const fn err_from_u32(v: u32) -> CalcError {
    match v {
        1 => CalcError::Div0,
        2 => CalcError::Value,
        3 => CalcError::Ref,
        4 => CalcError::Name,
        5 => CalcError::Num,
        6 => CalcError::Na,
        7 => CalcError::Null,
        8 => CalcError::Spill,
        9 => CalcError::Calc,
        _ => CalcError::Cycle,
    }
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

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    use reticula_core::id::{ColId, IdRun, RowId, SheetId};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, FormulaMode, Op, Workbook};
    use reticula_core::sheet::Sheet;
    use reticula_core::value::{CalcError, Value};

    use crate::binder::{BoundFormula, bind_expr};
    use crate::engine::{FormulaEngine, RecalcStatus};
    use crate::parser::parse_formula;
    use crate::value_eval::EvalScalar;

    fn make_formula_edit(
        row: RowId,
        col: ColId,
        raw: &str,
        wb: &Workbook<BoundFormula>,
    ) -> CellEdit<BoundFormula> {
        let ast = parse_formula(raw).unwrap();
        let formula = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: bind_expr(&ast, wb, SheetId(0)),
        };
        CellEdit {
            row,
            col,
            input: CellInput::Formula {
                formula,
                cached: None,
            },
            format: FormatPatch::Keep,
        }
    }

    fn assert_strip_matches_single_step(
        eng_batch: &mut FormulaEngine,
        eng_ast: &mut FormulaEngine,
        sheet: SheetId,
        rows: u32,
        cols: u32,
    ) {
        eng_batch.recalc_all();
        loop {
            match eng_ast.recalc_step(1) {
                RecalcStatus::Complete { .. } => break,
                RecalcStatus::InProgress { .. } => {}
            }
        }
        let s_batch = eng_batch.workbook().sheet(sheet).unwrap();
        let s_ast = eng_ast.workbook().sheet(sheet).unwrap();
        for r in 0..rows {
            let rid = RowId(s_batch.row_axis().pos_to_id(r).unwrap());
            for c in 0..cols {
                let cid = ColId(s_batch.col_axis().pos_to_id(c).unwrap());
                let vb = s_batch.value_by_id(rid, cid);
                let va = s_ast.value_by_id(rid, cid);
                assert_eq!(
                    EvalScalar::from_sheet_value(s_batch, vb),
                    EvalScalar::from_sheet_value(s_ast, va),
                    "mismatch at pos ({r}, {c}): batch={vb:?}, ast={va:?}"
                );
                if matches!(
                    vb,
                    Value::Number(_) | Value::Empty | Value::Bool(_) | Value::Error(_)
                ) {
                    assert_eq!(
                        vb.pack(),
                        va.pack(),
                        "bit mismatch at pos ({r}, {c}): {:#x} vs {:#x}",
                        vb.pack(),
                        va.pack()
                    );
                }
            }
        }
    }

    #[test]
    fn batch_cross_1024_b3_cancellation_paren_and_mixed_types() {
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("Sheet1"), 2600, 14);
        sheet.set_value_by_id(RowId(0), ColId(10), Value::Number(0.3));
        for r in 0..2500_u32 {
            let rid = RowId(u64::from(r));
            match r % 7 {
                0 => {
                    sheet.set_value_by_id(rid, ColId(0), Value::Number(0.1));
                    sheet.set_value_by_id(rid, ColId(1), Value::Number(0.2));
                }
                1 => {
                    sheet.set_value_by_id(rid, ColId(0), Value::Number(-f64::from(r) * 0.25));
                    sheet.set_value_by_id(rid, ColId(1), Value::Number(0.0));
                }
                2 => {
                    sheet.set_value_by_id(rid, ColId(0), Value::Bool(r % 2 == 0));
                    let sid = sheet.add_string("12.5").unwrap();
                    sheet.set_value_by_id(rid, ColId(1), Value::Text(sid));
                }
                3 => {
                    let sid = sheet.add_string("bad").unwrap();
                    sheet.set_value_by_id(rid, ColId(0), Value::Text(sid));
                    sheet.set_value_by_id(rid, ColId(1), Value::Number(3.0));
                }
                4 => {
                    sheet.set_value_by_id(rid, ColId(0), Value::Error(CalcError::Na));
                    sheet.set_value_by_id(rid, ColId(1), Value::Number(5.0));
                }
                5 => {
                    // Leave empty!
                }
                _ => {
                    sheet.set_value_by_id(rid, ColId(0), Value::Number(1.0e308));
                    sheet.set_value_by_id(rid, ColId(1), Value::Number(1.0e308));
                }
            }
        }
        let mut wb = Workbook::<BoundFormula>::from_sheet(sheet, None);
        let formulas = [
            (ColId(2), "=(A{r}+B{r})-$K$1"),
            (ColId(3), "=((A{r}+B{r})-$K$1)"),
            (
                ColId(4),
                "=IF(A{r}=B{r}, \"same\", IFERROR(A{r}/B{r}, \"div\"))",
            ),
            (ColId(5), "=IFERROR(1/B{r}, B{r})=\"\""),
            (ColId(6), "=AND(A{r}, B{r}>0, NOT(A{r}=0))"),
            (
                ColId(7),
                "=ROUND(ABS(A{r})*0.125, 2)+SUM($A$1:$B$10)+COUNT($A$1:$B$10)",
            ),
            (ColId(8), "=IF(A{r}>0, A{r}*2, )"),
            (ColId(9), "=@A{r}"),
        ];
        for (col, pat) in formulas {
            let edits: Vec<CellEdit<BoundFormula>> = (1..=2500_u32)
                .map(|r| {
                    let raw = pat.replace("{r}", &format!("{r}"));
                    make_formula_edit(RowId(u64::from(r - 1)), col, &raw, &wb)
                })
                .collect();
            wb.apply(Op::SetCells {
                sheet: SheetId(0),
                edits,
            })
            .unwrap();
        }

        let mut eng_batch = FormulaEngine::from_workbook_recalc_all(wb.clone());
        let mut eng_ast = FormulaEngine::from_workbook_recalc_all(wb);
        assert_strip_matches_single_step(&mut eng_batch, &mut eng_ast, SheetId(0), 2500, 10);

        // Verify outermost `Sub` snapped `0.1+0.2-0.3` to exact `0.0`, while `Paren` did not.
        let sb = eng_batch.workbook().sheet(SheetId(0)).unwrap();
        assert_eq!(sb.value_by_id(RowId(0), ColId(2)), Value::Number(0.0));
        assert!(matches!(sb.value_by_id(RowId(0), ColId(3)), Value::Number(v) if v != 0.0));
        assert_eq!(sb.value_by_id(RowId(5), ColId(9)), Value::Empty);
        // `IFERROR(1/0, EmptyBlank) = ""` must preserve `Empty` on caught fallback -> TRUE.
        assert_eq!(sb.value_by_id(RowId(5), ColId(5)), Value::Bool(true));
    }

    #[test]
    fn batch_recurrence_lookup_and_row_after_insert() {
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("Sheet1"), 2600, 8);
        for r in 0..2500_u32 {
            let rid = RowId(u64::from(r));
            sheet.set_value_by_id(rid, ColId(0), Value::Number(f64::from(r * 2)));
            sheet.set_value_by_id(rid, ColId(1), Value::Number(f64::from(r + 1) * 0.5));
        }
        // Duplicate key check + miss check in VLOOKUP/MATCH table A1:B100.
        sheet.set_value_by_id(RowId(2), ColId(0), Value::Number(2.0));
        let mut wb = Workbook::<BoundFormula>::from_sheet(sheet, None);
        // Recurrence seed C1 = B1, C2..C2500 = C{r-1} + B{r}
        let seed_edit = make_formula_edit(RowId(0), ColId(2), "=B1", &wb);
        wb.apply(Op::SetCells {
            sheet: SheetId(0),
            edits: alloc::vec![seed_edit],
        })
        .unwrap();
        for (col, pat) in [
            (ColId(2), "=C{prev}+B{r}"),
            (ColId(3), "=ROW()+B{r}"),
            (ColId(4), "=VLOOKUP(A{r}, $A$1:$B$100, 2, FALSE)"),
            (ColId(5), "=IFERROR(MATCH(A{r}, $A$1:$A$100, 0), -1)"),
        ] {
            let start = if col == ColId(2) { 2_u32 } else { 1_u32 };
            let edits: Vec<CellEdit<BoundFormula>> = (start..=2500_u32)
                .map(|r| {
                    let raw = pat
                        .replace("{r}", &format!("{r}"))
                        .replace("{prev}", &format!("{}", r - 1));
                    make_formula_edit(RowId(u64::from(r - 1)), col, &raw, &wb)
                })
                .collect();
            wb.apply(Op::SetCells {
                sheet: SheetId(0),
                edits,
            })
            .unwrap();
        }

        let mut eng_batch = FormulaEngine::from_workbook_recalc_all(wb.clone());
        let mut eng_ast = FormulaEngine::from_workbook_recalc_all(wb);
        assert_strip_matches_single_step(&mut eng_batch, &mut eng_ast, SheetId(0), 2500, 6);

        // Insert 3 rows at the top (B7a Group 3 `=ROW()+B{r}` must update live row positions cleanly).
        let run = IdRun::new(0x1000_0000_0000_0000, 3).unwrap();
        let op = Op::InsertRows {
            sheet: SheetId(0),
            after: None,
            ids: run,
        };
        eng_batch.apply_op(op.clone()).unwrap();
        eng_ast.apply_op(op).unwrap();
        assert_strip_matches_single_step(&mut eng_batch, &mut eng_ast, SheetId(0), 2503, 6);

        // Mid-recurrence input edit at row_id 1200 updates downstream chunk cleanly.
        eng_batch
            .edit_cell_raw(SheetId(0), RowId(1200), ColId(1), "999.25")
            .unwrap();
        eng_ast
            .edit_cell_raw(SheetId(0), RowId(1200), ColId(1), "999.25")
            .unwrap();
        assert_strip_matches_single_step(&mut eng_batch, &mut eng_ast, SheetId(0), 2503, 6);
    }
}

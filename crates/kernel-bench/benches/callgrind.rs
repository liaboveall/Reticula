//! Deterministic Callgrind instruction-count benchmarks for Linux CI gating
//! (`cargo bench --package reticula-kernel-bench --bench callgrind`).

mod fixtures {
    use std::hint::black_box;

    use reticula_core::id::{ColId, IdRun, RowId, SheetId};
    use reticula_core::ops::Op;
    use reticula_formula::engine::FormulaEngine;

    pub fn setup_b3_10k() -> FormulaEngine {
        let mut eng = FormulaEngine::new();
        let s = SheetId(0);
        eng.edit_cell_raw(s, RowId(0), ColId(25), "1.5").unwrap();
        for r in 1_u32..=10_000 {
            let row = RowId(u64::from(r));
            eng.edit_cell_raw(s, row, ColId(1), &format!("{r}"))
                .unwrap();
            eng.edit_cell_raw(s, row, ColId(2), "2").unwrap();
            let excel_r = r + 1;
            let expr = format!("=IF(B{excel_r}>500, B{excel_r}*C{excel_r}+$Z$1, 0)");
            eng.edit_cell_raw(s, row, ColId(3), &expr).unwrap();
        }
        eng.recalc_all();
        eng.edit_cell_raw(s, RowId(0), ColId(25), "2.25").unwrap();
        eng
    }

    pub fn run_b3_10k(mut eng: FormulaEngine) -> u32 {
        black_box(eng.recalc_all())
    }

    pub fn setup_b5_10k() -> FormulaEngine {
        let mut eng = FormulaEngine::new();
        let s = SheetId(0);
        eng.edit_cell_raw(s, RowId(0), ColId(1), "1").unwrap();
        eng.edit_cell_raw(s, RowId(0), ColId(2), "=B1").unwrap();
        for r in 1_u32..10_000 {
            let row = RowId(u64::from(r));
            eng.edit_cell_raw(s, row, ColId(1), "1").unwrap();
            let expr = format!("=C{}+B{}", r, r + 1);
            eng.edit_cell_raw(s, row, ColId(2), &expr).unwrap();
        }
        eng.recalc_all();
        eng.edit_cell_raw(s, RowId(0), ColId(1), "3.5").unwrap();
        eng
    }

    pub fn run_b5_10k(mut eng: FormulaEngine) -> u32 {
        black_box(eng.recalc_all())
    }

    pub fn setup_b6_1k() -> FormulaEngine {
        let mut eng = FormulaEngine::new();
        let s = SheetId(0);
        for r in 0_u32..1_000 {
            let row = RowId(u64::from(r));
            eng.edit_cell_raw(s, row, ColId(0), &format!("{}", r + 1))
                .unwrap();
            eng.edit_cell_raw(s, row, ColId(1), &format!("{}", (r + 1) * 10))
                .unwrap();
            let key = ((r.wrapping_mul(7919)) % 1_000) + 1;
            eng.edit_cell_raw(s, row, ColId(3), &format!("{key}"))
                .unwrap();
            let excel_r = r + 1;
            let expr = format!("=VLOOKUP(D{excel_r}, $A$1:$B$1000, 2, FALSE)");
            eng.edit_cell_raw(s, row, ColId(4), &expr).unwrap();
        }
        // Warm LookupIndexCache via initial recalc and one value-column edit.
        eng.recalc_all();
        eng.edit_cell_raw(s, RowId(0), ColId(1), "20").unwrap();
        eng.recalc_all();
        eng.edit_cell_raw(s, RowId(0), ColId(1), "42.5").unwrap();
        eng
    }

    pub fn run_b6_1k(mut eng: FormulaEngine) -> u32 {
        black_box(eng.recalc_all())
    }

    pub fn setup_early_cutoff_1k() -> FormulaEngine {
        let mut eng = FormulaEngine::new();
        let s = SheetId(0);
        eng.edit_cell_raw(s, RowId(0), ColId(0), "5").unwrap();
        eng.edit_cell_raw(s, RowId(0), ColId(1), "=IF(A1>0, 100, 0)")
            .unwrap();
        for r in 0_u32..1_000 {
            eng.edit_cell_raw(s, RowId(u64::from(r)), ColId(2), "=B$1+ROW()")
                .unwrap();
        }
        eng.recalc_all();
        eng.edit_cell_raw(s, RowId(0), ColId(0), "10").unwrap();
        eng
    }

    pub fn run_early_cutoff_1k(mut eng: FormulaEngine) -> u32 {
        black_box(eng.recalc_all())
    }

    pub fn setup_b7a_10k() -> (FormulaEngine, IdRun) {
        let mut eng = FormulaEngine::new();
        let s = SheetId(0);
        eng.edit_cell_raw(s, RowId(0), ColId(25), "3").unwrap();
        for r in 1_u32..=10_000 {
            let row = RowId(u64::from(r));
            eng.edit_cell_raw(s, row, ColId(1), &format!("{r}"))
                .unwrap();
            let expr = format!("=B{}*2+$Z$1", r + 1);
            eng.edit_cell_raw(s, row, ColId(2), &expr).unwrap();
        }
        eng.recalc_all();
        let run = eng.allocator_mut().alloc_run(1).unwrap();
        (eng, run)
    }

    pub fn run_b7a_10k((mut eng, run): (FormulaEngine, IdRun)) -> u32 {
        let _ = eng.apply_op(Op::InsertRows {
            sheet: SheetId(0),
            after: None,
            ids: run,
        });
        black_box(eng.recalc_all())
    }
}

#[cfg(target_os = "linux")]
#[gungraun::library_benchmark]
#[bench::run(fixtures::setup_b3_10k())]
fn b3_strip_eval_10k(eng: reticula_formula::engine::FormulaEngine) -> u32 {
    fixtures::run_b3_10k(eng)
}

#[cfg(target_os = "linux")]
#[gungraun::library_benchmark]
#[bench::run(fixtures::setup_b5_10k())]
fn b5_recurrence_10k(eng: reticula_formula::engine::FormulaEngine) -> u32 {
    fixtures::run_b5_10k(eng)
}

#[cfg(target_os = "linux")]
#[gungraun::library_benchmark]
#[bench::run(fixtures::setup_b6_1k())]
fn b6_vlookup_1k(eng: reticula_formula::engine::FormulaEngine) -> u32 {
    fixtures::run_b6_1k(eng)
}

#[cfg(target_os = "linux")]
#[gungraun::library_benchmark]
#[bench::run(fixtures::setup_early_cutoff_1k())]
fn early_cutoff_1k(eng: reticula_formula::engine::FormulaEngine) -> u32 {
    fixtures::run_early_cutoff_1k(eng)
}

#[cfg(target_os = "linux")]
#[gungraun::library_benchmark]
#[bench::run(fixtures::setup_b7a_10k())]
fn b7a_insert_top_row(
    input: (
        reticula_formula::engine::FormulaEngine,
        reticula_core::id::IdRun,
    ),
) -> u32 {
    fixtures::run_b7a_10k(input)
}

#[cfg(target_os = "linux")]
gungraun::library_benchmark_group!(
    name = kernel_callgrind,
    benchmarks = [
        b3_strip_eval_10k,
        b5_recurrence_10k,
        b6_vlookup_1k,
        early_cutoff_1k,
        b7a_insert_top_row,
    ]
);

#[cfg(target_os = "linux")]
gungraun::main!(library_benchmark_groups = kernel_callgrind);

#[cfg(not(target_os = "linux"))]
fn main() {
    if std::env::args().any(|arg| arg == "--smoke") {
        assert_eq!(fixtures::run_b3_10k(fixtures::setup_b3_10k()), 10_000);
        assert_eq!(fixtures::run_b5_10k(fixtures::setup_b5_10k()), 10_000);
        assert_eq!(fixtures::run_b6_1k(fixtures::setup_b6_1k()), 1_000);
        assert_eq!(
            fixtures::run_early_cutoff_1k(fixtures::setup_early_cutoff_1k()),
            1
        );
        assert_eq!(fixtures::run_b7a_10k(fixtures::setup_b7a_10k()), 0);
    }
}

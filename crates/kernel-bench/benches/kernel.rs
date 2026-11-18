//! Criterion micro-benchmarks for the Reticula kernel (`cargo bench -p reticula-kernel-bench`).

use criterion::{Criterion, criterion_group, criterion_main};
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::Op;
use reticula_formula::engine::FormulaEngine;

fn bench_sample_recalc_and_insert(c: &mut Criterion) {
    let mut eng = FormulaEngine::open_sample(42, 100_001);
    let mut rate = 0.01_f64;

    c.bench_function("sample_100k_incremental_recalc", |b| {
        b.iter(|| {
            rate = if rate > 0.15 { 0.01 } else { rate + 0.01 };
            eng.edit_cell_raw(SheetId(0), RowId(0), ColId(21), &format!("{rate:.2}"))
                .unwrap();
            eng.recalc_all();
        });
    });

    c.bench_function("sample_100k_insert_row_top", |b| {
        b.iter(|| {
            let run = eng.allocator_mut().alloc_run(1).unwrap();
            let inv = eng
                .apply_op(Op::InsertRows {
                    sheet: SheetId(0),
                    after: None,
                    ids: run,
                })
                .unwrap();
            eng.recalc_all();
            eng.apply_inverse(inv).unwrap();
        });
    });
}

criterion_group!(benches, bench_sample_recalc_and_insert);
criterion_main!(benches);

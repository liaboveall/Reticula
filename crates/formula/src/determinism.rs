//! Cross-target determinism self-check for the full kernel (`core` + `formula`).
//!
//! Folds [`reticula_core::determinism::digest`] together with all Layer-0
//! functions, 15-digit comparison/rounding edge cases, dynamic-array
//! broadcasting, structural (`ROW`/`COLUMN`) recalculation, time-sliced
//! recalculation on the upgraded seeded sample, and `RTCLSNAP` v0 round-trip.

use reticula_core::determinism as core_det;
use reticula_core::digest::Digest64;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::Op;

use crate::engine::{FormulaEngine, RecalcStatus};

pub use reticula_core::determinism::to_hex;

/// Runs the full kernel self-check workload and returns its 64-bit digest.
#[must_use]
pub fn digest() -> u64 {
    let mut d = Digest64::new();
    d.write_u64(core_det::digest());
    layer0_and_numeric_boundaries(&mut d);
    upgraded_sample_and_slicing(&mut d);
    d.finish()
}

fn layer0_and_numeric_boundaries(d: &mut Digest64) {
    let mut eng = FormulaEngine::new();
    let s = SheetId(0);

    let inputs: &[(u64, u64, &str)] = &[
        (0, 0, "0.1"),
        (1, 0, "0.2"),
        (2, 0, "0.3"),
        (3, 0, "1.005"),
        (4, 0, "-2.675"),
        (5, 0, "  Hello   Reticula  "),
        (6, 0, "TRUE"),
        (7, 0, "2026-11-18"),
        // Formulas exercising all 39 Layer-0 functions, D-027 and D-028:
        (0, 1, "=A1+A2=A3"),
        (1, 1, "=A3-(A1+A2)"),
        (2, 1, "=(A3-(A1+A2))"),
        (3, 1, "=ROUND(A4,2)"),
        (4, 1, "=ROUNDDOWN(A5,2)"),
        (5, 1, "=ROUNDUP(A5,2)"),
        (6, 1, "=INT(A5)"),
        (7, 1, "=TRUNC(A5,1)"),
        (8, 1, "=MOD(-7,3)"),
        (9, 1, "=SQRT(POWER(3,2)+POWER(4,2))"),
        (10, 1, "=ABS(-42.5)"),
        (11, 1, "=SUM(A1:A5,10,\"5\",TRUE)"),
        (12, 1, "=AVERAGE(A1:A5)"),
        (13, 1, "=COUNT(A1:A8)"),
        (14, 1, "=COUNTA(A1:A8)"),
        (15, 1, "=MIN(A1:A5)"),
        (16, 1, "=MAX(A1:A5)"),
        (
            17,
            1,
            "=IF(AND(A7,OR(FALSE(),NOT(FALSE()))),IFERROR(1/0,IFNA(NA(),77)),0)",
        ),
        (
            18,
            1,
            "=CONCAT(UPPER(TRIM(A6)),\"-\",LOWER(\"XYZ\"),\"-\",LEFT(\"abcdef\",2),RIGHT(\"abcdef\",2),MID(\"abcdef\",2,3))",
        ),
        (19, 1, "=LEN(TRIM(A6))"),
        (20, 1, "=SUM({1,2;3,4}*{10;100})"),
        (
            21,
            1,
            "=ISBLANK(Z99)&ISNUMBER(A1)&ISTEXT(A6)&ISLOGICAL(A7)&ISERROR(NA())",
        ),
        (22, 1, "=ROW()*100+COLUMN()"),
        (23, 1, "=-2^2+2^3^2"),
    ];

    for &(r, c, raw) in inputs {
        let _ = eng.edit_cell_raw(s, RowId(r), ColId(c), raw);
    }
    eng.recalc_all();
    d.write_u64(eng.workbook().logical_digest());

    // Insert a row at top and delete row A4 (`RowId(3)`), then recalculate.
    if let Some(ins) = eng.allocator_mut().alloc_run(1) {
        let _ = eng.apply_op(Op::InsertRows {
            sheet: s,
            after: None,
            ids: ins,
        });
    }
    if let Some(del) = IdRun::new(3, 1) {
        let _ = eng.apply_op(Op::DeleteRows {
            sheet: s,
            ids: alloc::vec![del],
        });
    }
    eng.recalc_all();
    d.write_u64(eng.workbook().logical_digest());

    // Snapshot round-trip must preserve the logical digest identically across
    // 64-bit and 32-bit targets (note: LZ4 compressed byte length can vary
    // slightly with target pointer width while decoding to the identical state).
    if let Ok(bytes) = eng.encode_snapshot()
        && let Ok((decoded, _)) = FormulaEngine::decode_snapshot(&bytes)
    {
        d.write_u64(decoded.workbook().logical_digest());
    }
}

fn upgraded_sample_and_slicing(d: &mut Digest64) {
    let mut eng = FormulaEngine::open_sample(0x5EED_2026, 600);
    let s = SheetId(0);
    d.write_u64(eng.workbook().logical_digest());

    // Edit promo discount `V1` to `5%` and run in small 97-cell slices.
    let _ = eng.edit_cell_raw(s, RowId(0), ColId(21), "5%");
    while matches!(eng.recalc_step(97), RecalcStatus::InProgress { .. }) {}
    d.write_u64(eng.workbook().logical_digest());
}

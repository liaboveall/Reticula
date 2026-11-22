//! Multi-threaded mutation & `arbitrary` fuzz runner (`14-m1b.md` 8.1).
//!
//! Runs all six shared harnesses (`fuzz_one_parser`, `fuzz_one_snapshot`,
//! `fuzz_one_wal`, `fuzz_one_ops`, `fuzz_one_eval`, `fuzz_one_sync`) across
//! `threads` worker threads until cumulative CPU time exceeds `target_cpu_seconds`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
use reticula_core::rng::Rng;
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_wire::snapshot::{encode_seed_stub_v1, encode_snapshot_v1};
use reticula_wire::sync_frame::encode_sync_frame;
use reticula_wire::v1::{WireOpV1, WireWalRecordV1};
use reticula_wire::v2::{
    SYNC_PROTOCOL_VERSION, WireCatchupV1, WireOpV2, WirePendingTxnV2, WireSyncFrameV1, WireTxnV2,
    WireWalRecordV2,
};
use reticula_wire::wal::{
    encode_pending_wal_complete, encode_wal_complete, encode_wal_complete_v2,
};
use serde::Serialize;

use crate::fuzz_harnesses::{
    fuzz_one_eval, fuzz_one_ops, fuzz_one_parser, fuzz_one_snapshot, fuzz_one_sync, fuzz_one_wal,
};

/// Summary of a completed fuzzing campaign.
#[derive(Clone, Debug, Serialize)]
pub struct FuzzReport {
    /// The target the whole budget went to, or `all`.
    pub target: &'static str,
    /// Worker threads used.
    pub threads: usize,
    /// Wall-clock duration in seconds.
    pub wall_seconds: f64,
    /// Cumulative CPU-seconds (`wall_seconds * threads`).
    pub cpu_seconds: f64,
    /// Total parser/lexer/E30/CSV executions.
    pub parser_execs: u64,
    /// Total `RTCLSNAP` v0 & v1 snapshot decode executions.
    pub snapshot_execs: u64,
    /// Total `RTCLWAL1` WAL scan & replay executions.
    pub wal_execs: u64,
    /// Total structured operation-sequence executions.
    pub ops_execs: u64,
    /// Total `BoundExpr` evaluator executions.
    pub eval_execs: u64,
    /// Total collaborative sync frame & state-machine executions.
    pub sync_execs: u64,
    /// Number of seed corpus entries constructed.
    pub seed_corpus_count: usize,
}

const DICT_TOKENS: [&[u8]; 22] = [
    b"=SUM(A1:B10)",
    b"=ROUND(J2*K2*(1-L2-$V$1),2)",
    b"='Sheet 1'!$A$1:$XFD$1048576",
    b"={1,-2.5,\"a\"\"b\";TRUE,FALSE,#DIV/0!}",
    b"=@A1:A100+50%",
    b"=-2^3^2",
    b"=IF(AND(A1>0,OR(B1,NOT(C1))),IFERROR(1/0,IFNA(#N/A,42)),0)",
    b"=ROW()*100+COLUMN(B:D)+SUM(1:5)",
    b"=VLOOKUP(\"SKU-1\",Data!A1:B10,2,FALSE)+SUMIF(A1:A5,\"alpha*\",B1:B5)",
    b"1900-02-29",
    b"9999-12-31",
    b"-0.0%",
    b"#SPILL!",
    b"#CYCLE!",
    b"RTCLSNAP\x01\x00\x00\x00",
    b"RTCLWAL1\x01\x00\x00\x00",
    b"RTCLWAL2\x01\x00\x00\x00",
    b"RTCLPWAL\x01\x00\x00\x00",
    b"\xff\xff\xff\xff\x7f\xfc\x00\x04\x00\x00\x00\xff",
    b"((((((((((((((((1))))))))))))))))",
    b"=CONCAT(TRIM(\"  a  b \"),MID(\"hello\",2,3))",
    b"a,\"b\"\"c\nline2\",=not_formula\r\n1,2,3\r\n",
];

/// Builds initial seed corpus buffers (including real `RTCLSNAP` v0/v1
/// snapshots and `RTCLWAL1` framed logs so mutations explore deep decoder paths).
#[must_use]
pub fn build_seed_corpus() -> Vec<Vec<u8>> {
    let mut seeds: Vec<Vec<u8>> = DICT_TOKENS.iter().map(|s| s.to_vec()).collect();

    let mut eng = FormulaEngine::new();
    let _ = eng.edit_cell_raw(SheetId(0), RowId(0), ColId(0), "42.5");
    let _ = eng.edit_cell_raw(SheetId(0), RowId(1), ColId(0), "hello");
    let _ = eng.edit_cell_raw(SheetId(0), RowId(0), ColId(1), "=SUM(A1:A2)");
    eng.recalc_all();
    if let Ok(snap_v0) = eng.encode_snapshot() {
        seeds.push(snap_v0);
    }
    if let Ok(snap_v1) = encode_snapshot_v1(eng.workbook(), &[]) {
        seeds.push(snap_v1);
    }

    let sample_eng = FormulaEngine::open_sample(7, 25);
    if let Ok(snap2) = encode_snapshot_v1(sample_eng.workbook(), &[]) {
        seeds.push(snap2);
    }
    if let Some(origin) = sample_eng.workbook().seed_origin()
        && let Ok(stub) = encode_seed_stub_v1(origin, &[])
    {
        seeds.push(stub);
    }

    let wal_op: Op<BoundFormula> = Op::SetCells {
        sheet: SheetId(0),
        edits: vec![CellEdit {
            row: RowId(0),
            col: ColId(0),
            input: CellInput::Scalar(ScalarValue::Number(123.0)),
            format: FormatPatch::Keep,
        }],
    };
    let wire_op = WireOpV1::try_from(&wal_op).expect("wal_op fits WireOpV1");
    let wal_records = vec![
        WireWalRecordV1 {
            seq: 1,
            timestamp_ms: 1_760_000_000_001,
            label: String::from("Edit A1"),
            ops: vec![wire_op.clone()],
        },
        WireWalRecordV1 {
            seq: 2,
            timestamp_ms: 1_760_000_000_002,
            label: String::from("Edit A1 again"),
            ops: vec![wire_op],
        },
    ];
    if let Ok(wal_bytes) = encode_wal_complete(0, &wal_records) {
        seeds.push(wal_bytes);
    }

    let pending_txns = WireOpV2::try_from(&wal_op).map_or_else(
        |_| Vec::new(),
        |wire_op_v2| {
            let wal_v2 = vec![WireWalRecordV2 {
                seq: 1,
                timestamp_ms: 1_760_000_000_001,
                label: String::from("v2"),
                ops: vec![wire_op_v2.clone()],
            }];
            if let Ok(wal2) = encode_wal_complete_v2(0, &wal_v2) {
                seeds.push(wal2);
            }
            let txns = vec![WirePendingTxnV2 {
                replica: 1,
                client_seq: 1,
                base_server_seq: 0,
                timestamp_ms: 1_760_000_000_001,
                txn: WireTxnV2 {
                    label: String::from("pend"),
                    ops: vec![wire_op_v2],
                },
            }];
            if let Ok(pwal) = encode_pending_wal_complete(&txns) {
                seeds.push(pwal);
            }
            txns
        },
    );
    if let Ok(f1) = encode_sync_frame(&WireSyncFrameV1::Hello {
        protocol_version: SYNC_PROTOCOL_VERSION,
        doc_id: [0x11; 16],
        branch_id: [0x22; 16],
        replica: None,
        last_applied_seq: 0,
        known_epoch: 0,
    }) {
        seeds.push(f1);
    }
    if let Ok(f2) = encode_sync_frame(&WireSyncFrameV1::Welcome {
        epoch: 0,
        head_seq: 0,
        replica: 1,
        last_client_seq: 0,
        max_id_counter: 0,
        catchup: WireCatchupV1::Txns {
            from_seq: 1,
            txns: Vec::new(),
        },
    }) {
        seeds.push(f2);
    }
    if let Ok(f3) = encode_sync_frame(&WireSyncFrameV1::Push { txns: pending_txns }) {
        seeds.push(f3);
    }

    seeds
}

/// The names `--target` accepts, in the order the campaign cycles through them.
pub const FUZZ_TARGETS: [&str; 6] = ["parser", "snapshot", "wal", "ops", "eval", "sync"];

/// Runs the six fuzz targets in parallel across `threads` threads until
/// `threads * wall_elapsed >= target_cpu_seconds`, cycling through all of
/// them, or running only `FUZZ_TARGETS[only]` so that the whole budget goes
/// to one target.
pub fn run_fuzz_campaign(
    target_cpu_seconds: u64,
    threads: usize,
    only: Option<usize>,
) -> FuzzReport {
    let worker_count = threads.max(1);
    let wall_budget =
        Duration::from_secs_f64(target_cpu_seconds as f64 / worker_count as f64 + 1.0);
    let seeds = build_seed_corpus();
    let seed_corpus_count = seeds.len();

    let parser_execs = AtomicU64::new(0);
    let snapshot_execs = AtomicU64::new(0);
    let wal_execs = AtomicU64::new(0);
    let ops_execs = AtomicU64::new(0);
    let eval_execs = AtomicU64::new(0);
    let sync_execs = AtomicU64::new(0);

    let started = Instant::now();
    std::thread::scope(|scope| {
        for tid in 0..worker_count {
            let seeds_ref = &seeds;
            let p_cnt = &parser_execs;
            let s_cnt = &snapshot_execs;
            let w_cnt = &wal_execs;
            let o_cnt = &ops_execs;
            let e_cnt = &eval_execs;
            let y_cnt = &sync_execs;

            scope.spawn(move || {
                let mut rng =
                    Rng::from_seed(0xF022_2026_u64 ^ (tid as u64).wrapping_mul(0x9E37_79B9));
                let mut buf = Vec::with_capacity(512);
                let mut local_p = 0_u64;
                let mut local_s = 0_u64;
                let mut local_w = 0_u64;
                let mut local_o = 0_u64;
                let mut local_e = 0_u64;
                let mut local_y = 0_u64;
                let mut iter = 0_u64;

                while started.elapsed() < wall_budget {
                    mutate_input(&mut rng, seeds_ref, &mut buf);
                    match only.unwrap_or((iter % 6) as usize) {
                        0 => {
                            fuzz_one_parser(&buf);
                            local_p += 1;
                        }
                        1 => {
                            fuzz_one_snapshot(&buf);
                            local_s += 1;
                        }
                        2 => {
                            fuzz_one_wal(&buf);
                            local_w += 1;
                        }
                        3 => {
                            fuzz_one_ops(&buf);
                            local_o += 1;
                        }
                        4 => {
                            fuzz_one_eval(&buf);
                            local_e += 1;
                        }
                        _ => {
                            fuzz_one_sync(&buf);
                            local_y += 1;
                        }
                    }
                    iter += 1;
                    // Periodically flush counters and check wall clock every 64 iterations.
                    if iter & 63 == 0 && started.elapsed() >= wall_budget {
                        break;
                    }
                }

                p_cnt.fetch_add(local_p, Ordering::Relaxed);
                s_cnt.fetch_add(local_s, Ordering::Relaxed);
                w_cnt.fetch_add(local_w, Ordering::Relaxed);
                o_cnt.fetch_add(local_o, Ordering::Relaxed);
                e_cnt.fetch_add(local_e, Ordering::Relaxed);
                y_cnt.fetch_add(local_y, Ordering::Relaxed);
            });
        }
    });

    let wall_seconds = started.elapsed().as_secs_f64();
    FuzzReport {
        target: only
            .and_then(|i| FUZZ_TARGETS.get(i).copied())
            .unwrap_or("all"),
        threads: worker_count,
        wall_seconds,
        cpu_seconds: wall_seconds * (worker_count as f64),
        parser_execs: parser_execs.load(Ordering::Relaxed),
        snapshot_execs: snapshot_execs.load(Ordering::Relaxed),
        wal_execs: wal_execs.load(Ordering::Relaxed),
        ops_execs: ops_execs.load(Ordering::Relaxed),
        eval_execs: eval_execs.load(Ordering::Relaxed),
        sync_execs: sync_execs.load(Ordering::Relaxed),
        seed_corpus_count,
    }
}

fn mutate_input(rng: &mut Rng, seeds: &[Vec<u8>], out: &mut Vec<u8>) {
    out.clear();
    if rng.next_below(5) == 0 {
        // Fresh pseudo-random byte slice (`0..=256` bytes).
        let len = rng.next_below(257) as usize;
        let mut i = 0;
        while i < len {
            let word = rng.next_u64().to_le_bytes();
            let take = (len - i).min(8);
            out.extend_from_slice(&word[..take]);
            i += take;
        }
        return;
    }

    let base = &seeds[rng.next_below(seeds.len() as u64) as usize];
    out.extend_from_slice(base);
    let mutations = (rng.next_below(6) + 1) as usize;
    for _ in 0..mutations {
        match rng.next_below(5) {
            0 if !out.is_empty() => {
                // Bit flip
                let idx = rng.next_below(out.len() as u64) as usize;
                let bit = 1_u8 << rng.next_below(8);
                out[idx] ^= bit;
            }
            1 if !out.is_empty() => {
                // Byte overwrite with boundary value
                let idx = rng.next_below(out.len() as u64) as usize;
                out[idx] = rng.next_u64() as u8;
            }
            2 if out.len() > 2 => {
                // Delete slice
                let start = rng.next_below((out.len() - 1) as u64) as usize;
                let max_del = (out.len() - start).min(16);
                let del = (rng.next_below(max_del as u64) + 1) as usize;
                out.drain(start..start + del);
            }
            3 if out.len() < 4096 => {
                // Splice a dictionary token
                let tok = DICT_TOKENS[rng.next_below(DICT_TOKENS.len() as u64) as usize];
                let idx = rng.next_below((out.len() + 1) as u64) as usize;
                out.splice(idx..idx, tok.iter().copied());
            }
            _ if out.len() < 4096 => {
                // Splice tail of another seed
                let other = &seeds[rng.next_below(seeds.len() as u64) as usize];
                if !other.is_empty() {
                    let cut = rng.next_below(other.len() as u64) as usize;
                    let take = (other.len() - cut).min(64);
                    out.extend_from_slice(&other[cut..cut + take]);
                }
            }
            _ => {}
        }
    }
}

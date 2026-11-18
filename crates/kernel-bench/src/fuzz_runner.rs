//! Multi-threaded mutation & `arbitrary` fuzz runner (`13-m1a.md` 7.3).
//!
//! Runs the four shared harnesses (`fuzz_one_parser`, `fuzz_one_snapshot`,
//! `fuzz_one_ops`, `fuzz_one_eval`) across `threads` worker threads until
//! cumulative CPU time exceeds `target_cpu_seconds`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::rng::Rng;
use reticula_formula::engine::FormulaEngine;
use serde::Serialize;

use crate::fuzz_harnesses::{fuzz_one_eval, fuzz_one_ops, fuzz_one_parser, fuzz_one_snapshot};

/// Summary of a completed fuzzing campaign.
#[derive(Clone, Debug, Serialize)]
pub struct FuzzReport {
    /// Worker threads used.
    pub threads: usize,
    /// Wall-clock duration in seconds.
    pub wall_seconds: f64,
    /// Cumulative CPU-seconds (`wall_seconds * threads`).
    pub cpu_seconds: f64,
    /// Total parser/lexer/E30 executions.
    pub parser_execs: u64,
    /// Total `RTCLSNAP` snapshot decode executions.
    pub snapshot_execs: u64,
    /// Total structured operation-sequence executions.
    pub ops_execs: u64,
    /// Total `BoundExpr` evaluator executions.
    pub eval_execs: u64,
    /// Number of seed corpus entries constructed.
    pub seed_corpus_count: usize,
}

const DICT_TOKENS: [&[u8]; 18] = [
    b"=SUM(A1:B10)",
    b"=ROUND(J2*K2*(1-L2-$V$1),2)",
    b"='Sheet 1'!$A$1:$XFD$1048576",
    b"={1,-2.5,\"a\"\"b\";TRUE,FALSE,#DIV/0!}",
    b"=@A1:A100+50%",
    b"=-2^3^2",
    b"=IF(AND(A1>0,OR(B1,NOT(C1))),IFERROR(1/0,IFNA(#N/A,42)),0)",
    b"=ROW()*100+COLUMN(B:D)+SUM(1:5)",
    b"1900-02-29",
    b"9999-12-31",
    b"-0.0%",
    b"#SPILL!",
    b"#CYCLE!",
    b"RTCLSNAP\x00\x00\x00\x00",
    b"\xff\xff\xff\xff\x7f\xfc\x00\x04\x00\x00\x00\xff",
    b"((((((((((((((((1))))))))))))))))",
    b"=CONCAT(TRIM(\"  a  b \"),MID(\"hello\",2,3))",
    b"=A1+B1+A1",
];

/// Builds initial seed corpus buffers (including real `RTCLSNAP` snapshots so
/// byte mutations explore deep decoder paths).
#[must_use]
pub fn build_seed_corpus() -> Vec<Vec<u8>> {
    let mut seeds: Vec<Vec<u8>> = DICT_TOKENS.iter().map(|s| s.to_vec()).collect();

    let mut eng = FormulaEngine::new();
    let _ = eng.edit_cell_raw(SheetId(0), RowId(0), ColId(0), "42.5");
    let _ = eng.edit_cell_raw(SheetId(0), RowId(1), ColId(0), "hello");
    let _ = eng.edit_cell_raw(SheetId(0), RowId(0), ColId(1), "=SUM(A1:A2)");
    eng.recalc_all();
    if let Ok(snap) = eng.encode_snapshot() {
        seeds.push(snap);
    }

    let sample_eng = FormulaEngine::open_sample(7, 25);
    if let Ok(snap2) = sample_eng.encode_snapshot() {
        seeds.push(snap2);
    }
    seeds
}

/// Runs all four fuzz targets in parallel across `threads` threads until
/// `threads * wall_elapsed >= target_cpu_seconds`.
pub fn run_fuzz_campaign(target_cpu_seconds: u64, threads: usize) -> FuzzReport {
    let worker_count = threads.max(1);
    let wall_budget =
        Duration::from_secs_f64(target_cpu_seconds as f64 / worker_count as f64 + 1.0);
    let seeds = build_seed_corpus();
    let seed_corpus_count = seeds.len();

    let parser_execs = AtomicU64::new(0);
    let snapshot_execs = AtomicU64::new(0);
    let ops_execs = AtomicU64::new(0);
    let eval_execs = AtomicU64::new(0);

    let started = Instant::now();
    std::thread::scope(|scope| {
        for tid in 0..worker_count {
            let seeds_ref = &seeds;
            let p_cnt = &parser_execs;
            let s_cnt = &snapshot_execs;
            let o_cnt = &ops_execs;
            let e_cnt = &eval_execs;

            scope.spawn(move || {
                let mut rng =
                    Rng::from_seed(0xF022_2026_u64 ^ (tid as u64).wrapping_mul(0x9E37_79B9));
                let mut buf = Vec::with_capacity(512);
                let mut local_p = 0_u64;
                let mut local_s = 0_u64;
                let mut local_o = 0_u64;
                let mut local_e = 0_u64;
                let mut iter = 0_u64;

                while started.elapsed() < wall_budget {
                    mutate_input(&mut rng, seeds_ref, &mut buf);
                    match iter & 3 {
                        0 => {
                            fuzz_one_parser(&buf);
                            local_p += 1;
                        }
                        1 => {
                            fuzz_one_snapshot(&buf);
                            local_s += 1;
                        }
                        2 => {
                            fuzz_one_ops(&buf);
                            local_o += 1;
                        }
                        _ => {
                            fuzz_one_eval(&buf);
                            local_e += 1;
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
                o_cnt.fetch_add(local_o, Ordering::Relaxed);
                e_cnt.fetch_add(local_e, Ordering::Relaxed);
            });
        }
    });

    let wall_seconds = started.elapsed().as_secs_f64();
    FuzzReport {
        threads: worker_count,
        wall_seconds,
        cpu_seconds: wall_seconds * (worker_count as f64),
        parser_execs: parser_execs.load(Ordering::Relaxed),
        snapshot_execs: snapshot_execs.load(Ordering::Relaxed),
        ops_execs: ops_execs.load(Ordering::Relaxed),
        eval_execs: eval_execs.load(Ordering::Relaxed),
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

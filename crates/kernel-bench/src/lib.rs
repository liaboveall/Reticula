//! Native testing, differential evaluation, fuzzing and micro-benchmarks for
//! the Reticula kernel.

pub mod bench_workloads;
#[cfg(feature = "diff")]
pub mod diff;
pub mod fuzz_harnesses;
pub mod fuzz_runner;
pub mod fuzz_tables;
#[cfg(feature = "diff")]
pub mod lo_diff;
pub mod sync_bench;
#[cfg(feature = "table-diff")]
pub mod table_diff;
pub mod tables;

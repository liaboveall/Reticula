#![no_main]

use libfuzzer_sys::fuzz_target;
use reticula_kernel_bench::fuzz_tables::fuzz_one_csv;

fuzz_target!(|data: &[u8]| {
    fuzz_one_csv(data);
});

#![no_main]

use libfuzzer_sys::fuzz_target;
use reticula_kernel_bench::fuzz_harnesses::fuzz_one_parser;

fuzz_target!(|data: &[u8]| {
    fuzz_one_parser(data);
});

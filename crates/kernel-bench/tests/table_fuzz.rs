//! The fuzz harnesses of tables, over their seeds and a few thousand damaged
//! ones: that the harnesses themselves are right is not left to a campaign.

use reticula_core::rng::Rng;
use reticula_kernel_bench::fuzz_tables::{
    csv_seeds, fuzz_one_csv, fuzz_one_parquet, fuzz_one_table, parquet_templates, table_templates,
};

/// Calls a harness with the seeds, some random bytes, and seeds with other bytes.
fn each(seeds: &[Vec<u8>], rounds: u64, harness: fn(&[u8])) {
    assert!(!seeds.is_empty());
    let mut rng = Rng::from_seed(0x5EED);
    for seed in seeds {
        harness(seed);
    }
    for _ in 0..rounds {
        let mut bytes: Vec<u8> = (0..rng.next_below(400))
            .map(|_| rng.next_u64().to_le_bytes()[0])
            .collect();
        harness(&bytes);
        bytes.clone_from(&seeds[rng.next_below(seeds.len() as u64) as usize]);
        bytes.truncate(8_192);
        for _ in 0..=rng.next_below(4) {
            let at = rng.next_below(bytes.len() as u64) as usize;
            bytes[at] = rng.next_u64().to_le_bytes()[0];
        }
        harness(&bytes);
    }
}

#[test]
fn files_that_claim_to_be_tables() {
    each(table_templates(), 150, fuzz_one_table);
}

#[test]
fn files_that_claim_to_be_parquet() {
    each(parquet_templates(), 400, fuzz_one_parquet);
}

#[test]
fn files_that_claim_to_be_delimited_text() {
    each(&csv_seeds(), 400, fuzz_one_csv);
}

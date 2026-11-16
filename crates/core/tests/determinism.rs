//! The determinism self-check must give the recorded value on this target.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use reticula_core::determinism::{digest, to_hex};

#[test]
fn digest_matches_recorded_value() {
    let expected = include_str!("../../../tests/determinism/expected.txt").trim();
    let actual = to_hex(digest());
    assert_eq!(
        core::str::from_utf8(&actual).unwrap(),
        expected,
        "the determinism digest differs from tests/determinism/expected.txt; if the \
         workload changed on purpose, run `cargo xtask digest --update`"
    );
}

//! Cross-target determinism test of tables.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use reticula_table::determinism::{app, digest, to_hex};

#[test]
fn digest_matches_recorded_value() {
    let expected = include_str!("../../../tests/determinism/table.txt").trim();
    let actual = to_hex(digest());
    assert_eq!(
        core::str::from_utf8(&actual).unwrap(),
        expected,
        "the digest of tables differs from tests/determinism/table.txt; if the \
         workload changed on purpose, run `cargo xtask digest --update`"
    );
}

#[test]
fn what_the_app_makes_matches_what_is_recorded() {
    assert_eq!(
        app(include_str!("../../../tests/determinism/table.csv")),
        include_str!("../../../tests/determinism/table-app.txt"),
        "what the app makes of tables differs from tests/determinism/table-app.txt; \
         if the workload changed on purpose, run `cargo xtask digest --update`"
    );
}

//! Prints the report. Run with `--nocapture`, or through `cargo xtask spike determinism`.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

#[test]
fn print_report() {
    let report = spike_determinism::report();
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::console_log!("{report}");
    #[cfg(not(target_arch = "wasm32"))]
    println!("{report}");
    assert!(report.contains("SmallRng"));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "a timing run; `cargo xtask spike determinism` runs it in release mode"]
fn print_rounding_cost() {
    println!("{}", spike_determinism::rounding_cost());
}

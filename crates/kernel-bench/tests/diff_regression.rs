//! CI regression suite running >= 10,000 random Layer-0 + Layer-1 formulas
//! against both `ironcalc_base` and `formualizer-eval` and asserting >= 90%
//! non-error ratio and zero unclassified divergences (`14-m1b.md` 8.1).

use reticula_kernel_bench::diff::run_differential;

#[test]
fn differential_10000_random_formulas_have_zero_unclassified_divergences() {
    let report = run_differential(0xD1FF_2026, 10_000);
    assert_eq!(report.total_formulas, 10_000);
    assert!(
        report.non_error_ratio >= 0.90,
        "expected >= 90% non-error formulas, got {:.1}%",
        report.non_error_ratio * 100.0
    );

    if report.unclassified_count() > 0 {
        let mut details = String::new();
        for c in &report.clusters {
            if c.disposition == "unclassified" {
                details.push_str(&format!(
                    "\n  [{}] {} ({}x): `{}` -> reticula={:?} vs ref={:?}",
                    c.reference_engine,
                    c.category,
                    c.count,
                    c.sample_formula,
                    c.reticula_result,
                    c.reference_result
                ));
            }
        }
        panic!(
            "found {} unclassified differential divergences:{details}",
            report.unclassified_count()
        );
    }
}

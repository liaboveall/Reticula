//! CI regression suite running >= 2,000 random formulas against both
//! `ironcalc_base` and `formualizer-eval` and asserting zero unclassified
//! divergences (`13-m1a.md` 7.2).

use reticula_kernel_bench::diff::run_differential;

#[test]
fn differential_2000_random_formulas_have_zero_unclassified_divergences() {
    let report = run_differential(0xD1FF_2026, 2_000);
    assert_eq!(report.total_formulas, 2_000);
    assert!(
        report.non_error_ratio >= 0.80,
        "expected >= 80% non-error formulas, got {:.1}%",
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

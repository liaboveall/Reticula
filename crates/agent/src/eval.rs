//! SpreadsheetBench Verified headless evaluation runner, dual-path XLSX export
//! (`patch_xlsx` vs `export_xlsx`), Wilson 95% CI statistics, token/cache cost
//! estimator, and S6 multi-step tool loop probe.

use std::path::Path;

use reticula_core::id::{ColId, RowId, SheetId, pack_replica_id};
use reticula_formula::engine::FormulaEngine;
use reticula_xlsx::{
    ChangedCell, ExportOptions, export_xlsx_with_options, import_xlsx, patch_xlsx,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::llm::{LlmProvider, ScriptedMockProvider, ScriptedTurn};
use crate::runner::{MemoryStepStore, run_agent_loop};
use crate::tools::{AgentWorkbookSink, HeadlessEvalSink};
use crate::{AgentError, AgentRunConfig};

/// Export fidelity path selected for an evaluation case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalExportPath {
    PatchXlsx,
    ExportXlsx,
}

/// Three mutually exclusive failure root-cause categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    KernelGap,
    AgentStrategy,
    ModelCapability,
}

/// Single task outcome recorded in an evaluation run report.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalCaseResult {
    pub task_id: String,
    pub passed: bool,
    pub export_path: EvalExportPath,
    pub preserved_opaque_formulas: u32,
    pub steps: u32,
    pub prompt_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregated statistics with Wilson 95% confidence interval.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalSummaryStats {
    pub total_cases: u64,
    pub passed_cases: u64,
    pub pass_rate: f64,
    pub wilson_ci_low: f64,
    pub wilson_ci_high: f64,
    pub patch_xlsx_count: u64,
    pub export_xlsx_count: u64,
    pub kernel_gap_failures: u64,
    pub agent_strategy_failures: u64,
    pub model_capability_failures: u64,
    pub total_prompt_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_completion_tokens: u64,
}

/// Complete serializable evaluation report archived for `/benchmarks`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalRunReport {
    pub benchmark: String,
    pub dataset_license: String,
    pub model: String,
    pub provider: String,
    pub grader_image: String,
    pub libreoffice_version: String,
    pub run_index: u32,
    pub summary: EvalSummaryStats,
    pub cases: Vec<EvalCaseResult>,
}

/// Computes the Wilson score 95% confidence interval (`z = 1.959963984540054`)
/// for `passed` successes out of `total` trials.
pub fn wilson_ci_95(passed: u64, total: u64) -> (f64, f64) {
    if total == 0 {
        return (0.0, 0.0);
    }
    let n = total as f64;
    let p = (passed as f64) / n;
    let z = 1.959_963_984_540_054_f64;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = (z * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt()) / denom;
    (
        (center - margin).clamp(0.0, 1.0),
        (center + margin).clamp(0.0, 1.0),
    )
}

/// Token & cache-hit driven evaluation cost formula.
/// Prices are per 1,000,000 tokens in USD.
#[allow(clippy::too_many_arguments)]
pub fn estimate_eval_cost_usd(
    n_cases: u64,
    avg_steps: f64,
    avg_prompt_tokens: f64,
    cache_hit_rate: f64,
    avg_completion_tokens: f64,
    price_in_per_m: f64,
    price_cached_per_m: f64,
    price_out_per_m: f64,
) -> f64 {
    let r = cache_hit_rate.clamp(0.0, 1.0);
    let per_step = (avg_prompt_tokens * (1.0 - r) * price_in_per_m
        + avg_prompt_tokens * r * price_cached_per_m
        + avg_completion_tokens * price_out_per_m)
        / 1_000_000.0;
    (n_cases as f64) * avg_steps * per_step
}

/// Exports the agent-modified workbook to `.xlsx` using `patch_xlsx` by default
/// (preserving original charts, merged cells, styles, conditional formatting,
/// and macros) and falling back automatically to `export_xlsx_with_options`
/// whenever structural sheet/row/column mutations occurred.
pub fn export_eval_workbook(
    orig_xlsx: &[u8],
    sink: &HeadlessEvalSink,
) -> Result<(Vec<u8>, EvalExportPath), String> {
    if !sink.has_structural_ops {
        let changed: Vec<ChangedCell> = sink
            .touched_cells
            .iter()
            .map(|(sheet_name, row, col)| ChangedCell {
                sheet_name: sheet_name.clone(),
                row: *row,
                col: *col,
            })
            .collect();
        if let Ok(patched) = patch_xlsx(orig_xlsx, sink.engine(), &changed) {
            return Ok((patched.bytes, EvalExportPath::PatchXlsx));
        }
    }
    let exported = export_xlsx_with_options(
        sink.engine(),
        ExportOptions {
            omit_cached_values: true,
            full_calc_on_load: true,
        },
    )
    .map_err(|e| format!("export_xlsx failed: {e}"))?;
    Ok((exported.bytes, EvalExportPath::ExportXlsx))
}

/// Runs a single SpreadsheetBench `.xlsx` task end-to-end headlessly and writes
/// the modified workbook to `output_xlsx_path`.
pub async fn run_headless_xlsx_task(
    task_id: &str,
    prompt: &str,
    input_xlsx_path: &Path,
    output_xlsx_path: &Path,
    provider: &dyn LlmProvider,
    config: AgentRunConfig,
) -> Result<EvalCaseResult, AgentError> {
    let started = std::time::Instant::now();
    let orig_bytes = std::fs::read(input_xlsx_path)
        .map_err(|e| AgentError::Workbook(format!("read {}: {e}", input_xlsx_path.display())))?;
    let imported = import_xlsx(&orig_bytes)
        .map_err(|e| AgentError::Workbook(format!("import {}: {e}", input_xlsx_path.display())))?;
    let preserved = imported.report.preserved_formula_count;

    let rep = pack_replica_id(0, 1).ok_or_else(|| AgentError::Workbook("replica".to_owned()))?;
    let mut sink =
        HeadlessEvalSink::from_engine(imported.engine, 0, rep).map_err(AgentError::Workbook)?;
    let store = MemoryStepStore::default();

    let run_res = run_agent_loop(prompt, provider, &mut sink, &store, config, None, |_| {}).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match run_res {
        Ok(outcome) => {
            let (out_bytes, export_path) =
                export_eval_workbook(&orig_bytes, &sink).map_err(AgentError::Workbook)?;
            if let Some(parent) = output_xlsx_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AgentError::Store(format!("mkdir {}: {e}", parent.display())))?;
            }
            std::fs::write(output_xlsx_path, out_bytes).map_err(|e| {
                AgentError::Store(format!("write {}: {e}", output_xlsx_path.display()))
            })?;
            Ok(EvalCaseResult {
                task_id: task_id.to_owned(),
                passed: false, // Graded externally by fixed LibreOffice 26.8.2.2 container
                export_path,
                preserved_opaque_formulas: preserved,
                steps: outcome.steps_executed,
                prompt_tokens: outcome.total_usage.prompt_tokens,
                cached_prompt_tokens: outcome.total_usage.cached_prompt_tokens,
                completion_tokens: outcome.total_usage.completion_tokens,
                latency_ms: elapsed_ms,
                failure_category: None,
                error: None,
            })
        }
        Err(err) => Ok(EvalCaseResult {
            task_id: task_id.to_owned(),
            passed: false,
            export_path: EvalExportPath::PatchXlsx,
            preserved_opaque_formulas: preserved,
            steps: store.snapshot().len() as u32,
            prompt_tokens: 0,
            cached_prompt_tokens: 0,
            completion_tokens: 0,
            latency_ms: elapsed_ms,
            failure_category: Some(FailureCategory::AgentStrategy),
            error: Some(err.to_string()),
        }),
    }
}

/// Aggregates case outcomes into [`EvalSummaryStats`] including Wilson 95% CI.
pub fn summarize_eval_cases(cases: &[EvalCaseResult]) -> EvalSummaryStats {
    let total = cases.len() as u64;
    let passed = cases.iter().filter(|c| c.passed).count() as u64;
    let (ci_lo, ci_hi) = wilson_ci_95(passed, total);
    let mut stats = EvalSummaryStats {
        total_cases: total,
        passed_cases: passed,
        pass_rate: if total > 0 {
            passed as f64 / total as f64
        } else {
            0.0
        },
        wilson_ci_low: ci_lo,
        wilson_ci_high: ci_hi,
        patch_xlsx_count: 0,
        export_xlsx_count: 0,
        kernel_gap_failures: 0,
        agent_strategy_failures: 0,
        model_capability_failures: 0,
        total_prompt_tokens: 0,
        total_cached_tokens: 0,
        total_completion_tokens: 0,
    };
    for c in cases {
        match c.export_path {
            EvalExportPath::PatchXlsx => stats.patch_xlsx_count += 1,
            EvalExportPath::ExportXlsx => stats.export_xlsx_count += 1,
        }
        if !c.passed {
            match c.failure_category {
                Some(FailureCategory::KernelGap) => stats.kernel_gap_failures += 1,
                Some(FailureCategory::AgentStrategy) => stats.agent_strategy_failures += 1,
                Some(FailureCategory::ModelCapability) | None => {
                    stats.model_capability_failures += 1;
                }
            }
        }
        stats.total_prompt_tokens += c.prompt_tokens;
        stats.total_cached_tokens += c.cached_prompt_tokens;
        stats.total_completion_tokens += c.completion_tokens;
    }
    stats
}

/// Per-scenario telemetry collected by `cargo xtask spike s6`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S6ScenarioResult {
    pub id: String,
    pub completed: bool,
    pub steps: u32,
    pub commits: u32,
    pub prompt_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub completion_tokens: u64,
    pub schema_violations: u32,
    pub retries: u32,
    pub rate_limited_hits: u32,
    pub ttft_samples_ms: Vec<u64>,
    pub step_latencies_ms: Vec<u64>,
    pub remaining_errors: u32,
    pub error: Option<String>,
}

/// Aggregate S6 multi-step tool loop probe report.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S6ProbeReport {
    pub provider: String,
    pub model: String,
    pub resolved_model: Option<String>,
    pub scenarios: Vec<S6ScenarioResult>,
    pub completion_rate: f64,
    pub avg_steps: f64,
    pub avg_prompt_tokens_per_step: f64,
    pub avg_completion_tokens_per_step: f64,
    pub cache_hit_rate: f64,
    pub ttft_p50_ms: u64,
    pub ttft_p95_ms: u64,
    pub step_latency_p50_ms: u64,
    pub step_latency_p95_ms: u64,
    pub step_latency_max_ms: u64,
    pub total_rate_limited_hits: u32,
    pub total_schema_violations: u32,
}

/// Runs 5 representative multi-step spreadsheet scenarios to measure S6 tool-loop
/// reliability, latency distribution, token consumption, and cache hit rate.
pub async fn run_s6_probe(provider: &dyn LlmProvider) -> S6ProbeReport {
    let tasks: Vec<(&str, &str, FormulaEngine)> = vec![
        (
            "s6-1-multi-condition-summary",
            "Inspect the ledger, dry-run SUMIFS formulas by region, write a summary table in E1:F5, sort descending, format as currency, check errors, and finish.",
            seed_s6_ledger(),
        ),
        (
            "s6-2-error-audit-repair",
            "Find all errors using list_errors, read their precedents, dry-run IFERROR fixes, apply fixes, re-check list_errors, and finish.",
            seed_s6_errors(),
        ),
        (
            "s6-3-dynamic-spill-dashboard",
            "Inspect data, dry-run SORT and TAKE dynamic array formulas, write Top-3 leaderboard and total with # operator, check errors, and finish.",
            seed_s6_ledger(),
        ),
        (
            "s6-4-structure-insert-subtotal",
            "Read rows, insert a subtotal row via edit_structure, populate SUM formulas and percentage share, format, verify with list_errors, and finish.",
            seed_s6_ledger(),
        ),
        (
            "s6-5-cross-sheet-lookup",
            "Inspect sheets, dry-run XLOOKUP across sheets, fill_formula across rows, sort summary, verify zero errors, and finish.",
            seed_s6_ledger(),
        ),
    ];

    let mut scenarios = Vec::with_capacity(tasks.len());
    let mut all_ttft = Vec::new();
    let mut all_lat = Vec::new();
    let mut total_steps = 0_u64;
    let mut total_in = 0_u64;
    let mut total_cached = 0_u64;
    let mut total_out = 0_u64;
    let mut total_429 = 0_u32;
    let mut total_schema = 0_u32;
    let mut resolved_model = None;

    for (id, prompt, engine) in tasks {
        let rep = pack_replica_id(0, 1).expect("replica");
        let mut sink = HeadlessEvalSink::from_engine(engine, 0, rep).expect("sink");
        let store = MemoryStepStore::default();
        let cfg = AgentRunConfig::default();
        match run_agent_loop(prompt, provider, &mut sink, &store, cfg, None, |_| {}).await {
            Ok(outcome) => {
                if resolved_model.is_none() {
                    resolved_model = outcome.resolved_model.clone();
                }
                total_steps += u64::from(outcome.steps_executed);
                total_in += outcome.total_usage.prompt_tokens;
                total_cached += outcome.total_usage.cached_prompt_tokens;
                total_out += outcome.total_usage.completion_tokens;
                total_429 += outcome.rate_limited_hits;
                total_schema += outcome.schema_violations;
                all_ttft.extend(&outcome.ttft_samples_ms);
                all_lat.extend(&outcome.step_latencies_ms);
                let remaining_errors = sink.engine().list_error_cells(None, 20).len() as u32;
                scenarios.push(S6ScenarioResult {
                    id: id.to_owned(),
                    completed: true,
                    steps: outcome.steps_executed,
                    commits: outcome.commits_written,
                    prompt_tokens: outcome.total_usage.prompt_tokens,
                    cached_prompt_tokens: outcome.total_usage.cached_prompt_tokens,
                    completion_tokens: outcome.total_usage.completion_tokens,
                    schema_violations: outcome.schema_violations,
                    retries: outcome.retries,
                    rate_limited_hits: outcome.rate_limited_hits,
                    ttft_samples_ms: outcome.ttft_samples_ms,
                    step_latencies_ms: outcome.step_latencies_ms,
                    remaining_errors,
                    error: None,
                });
            }
            Err(err) => {
                scenarios.push(S6ScenarioResult {
                    id: id.to_owned(),
                    completed: false,
                    steps: store.snapshot().len() as u32,
                    commits: sink.committed_txns.len() as u32,
                    prompt_tokens: 0,
                    cached_prompt_tokens: 0,
                    completion_tokens: 0,
                    schema_violations: 0,
                    retries: 0,
                    rate_limited_hits: 0,
                    ttft_samples_ms: Vec::new(),
                    step_latencies_ms: Vec::new(),
                    remaining_errors: 0,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    let completed = scenarios.iter().filter(|s| s.completed).count();
    let step_denom = total_steps.max(1) as f64;
    S6ProbeReport {
        provider: provider.provider_name().to_owned(),
        model: provider.model_name().to_owned(),
        resolved_model,
        completion_rate: completed as f64 / scenarios.len().max(1) as f64,
        avg_steps: total_steps as f64 / scenarios.len().max(1) as f64,
        avg_prompt_tokens_per_step: total_in as f64 / step_denom,
        avg_completion_tokens_per_step: total_out as f64 / step_denom,
        cache_hit_rate: if total_in > 0 {
            total_cached as f64 / total_in as f64
        } else {
            0.0
        },
        ttft_p50_ms: percentile(&mut all_ttft, 50),
        ttft_p95_ms: percentile(&mut all_ttft, 95),
        step_latency_p50_ms: percentile(&mut all_lat, 50),
        step_latency_p95_ms: percentile(&mut all_lat, 95),
        step_latency_max_ms: all_lat.iter().copied().max().unwrap_or(0),
        total_rate_limited_hits: total_429,
        total_schema_violations: total_schema,
        scenarios,
    }
}

/// Deterministic 10-step mock provider for verifying the `spike s6` pipeline offline.
pub fn s6_mock_provider() -> ScriptedMockProvider {
    let mut turns = Vec::new();
    for _ in 0..5 {
        for step in 1..=9 {
            let tool = match step % 3 {
                1 => ("workbook_overview", json!({})),
                2 => ("read_range", json!({ "range": "A1:C6" })),
                _ => (
                    "evaluate_formula",
                    json!({ "cell": "D2", "formula": "=SUM(C2:C6)" }),
                ),
            };
            turns.push(ScriptedTurn::tool(
                format!("Step {step}"),
                format!("s6_{step}"),
                tool.0,
                tool.1,
            ));
        }
        turns.push(ScriptedTurn::tool(
            "Finishing 10-step probe.",
            "s6_fin",
            "finish",
            json!({ "summary": "Mock 10-step S6 probe completed." }),
        ));
    }
    ScriptedMockProvider::new(turns)
}

fn percentile(samples: &mut [u64], pct: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() - 1) * pct) / 100;
    samples[idx]
}

fn seed_s6_ledger() -> FormulaEngine {
    let mut e = FormulaEngine::new();
    let s0 = SheetId(0);
    for &(r, c, v) in &[
        (0, 0, "Order"),
        (0, 1, "Region"),
        (0, 2, "Sales"),
        (1, 0, "O1"),
        (1, 1, "North"),
        (1, 2, "120"),
        (2, 0, "O2"),
        (2, 1, "South"),
        (2, 2, "340"),
        (3, 0, "O3"),
        (3, 1, "East"),
        (3, 2, "210"),
        (4, 0, "O4"),
        (4, 1, "North"),
        (4, 2, "180"),
        (5, 0, "O5"),
        (5, 1, "West"),
        (5, 2, "450"),
    ] {
        e.edit_cell_raw(s0, RowId(r), ColId(c), v).unwrap();
    }
    e.recalc_all();
    e
}

fn seed_s6_errors() -> FormulaEngine {
    let mut e = FormulaEngine::new();
    let s0 = SheetId(0);
    for &(r, c, v) in &[
        (0, 0, "A"),
        (0, 1, "B"),
        (0, 2, "Ratio"),
        (1, 0, "10"),
        (1, 1, "0"),
        (1, 2, "=A2/B2"),
        (2, 0, "20"),
        (2, 1, "4"),
        (2, 2, "=A3/B3"),
    ] {
        e.edit_cell_raw(s0, RowId(r), ColId(c), v).unwrap();
    }
    e.recalc_all();
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_ci_and_cost_formula_match_expected_bounds() {
        let (lo, hi) = wilson_ci_95(200, 400);
        assert!((lo - 0.4512).abs() < 0.01);
        assert!((hi - 0.5488).abs() < 0.01);

        let cost = estimate_eval_cost_usd(400, 12.0, 4000.0, 0.75, 400.0, 2.0, 0.5, 8.0);
        // 4800 steps * ((1000*2 + 3000*0.5 + 400*8) / 1e6) = 4800 * 0.0067 = $32.16
        assert!((cost - 32.16).abs() < 1e-6);
    }

    #[tokio::test]
    async fn s6_mock_probe_completes_five_ten_step_scenarios() {
        let provider = s6_mock_provider();
        let report = run_s6_probe(&provider).await;
        assert_eq!(report.scenarios.len(), 5);
        assert!((report.completion_rate - 1.0).abs() < 1e-9);
        assert!((report.avg_steps - 10.0).abs() < 1e-9);
    }
}

//! SpreadsheetBench Verified evaluation CLI & deterministic 10-task replay
//! generator.

use std::path::{Path, PathBuf};

use reticula_agent::eval::{
    EvalCaseResult, EvalRunReport, FailureCategory, run_headless_xlsx_task, summarize_eval_cases,
};
use reticula_agent::llm::{LlmProvider, OpenAiCompatProvider, ScriptedMockProvider};
use reticula_agent::replay::write_demo_replays_to_dir;
use reticula_agent::{AgentRunConfig, LlmConfig};

use crate::util::{self, Flags, Result};

const DEFAULT_DATASET_DIR: &str = "target/eval/spreadsheetbench-verified";
const DEFAULT_OUT_DIR: &str = "target/eval/runs";
const DEFAULT_GRADER_IMAGE: &str = "reticula-libreoffice:26.8.2";
const DEFAULT_LO_VERSION: &str = "26.8.2";

pub fn run(args: &[String]) -> Result {
    let Some((sub, rest)) = args.split_first() else {
        return Err(
            "usage: cargo xtask eval <replays|download|run|grade|report> [options]".to_owned(),
        );
    };
    match sub.as_str() {
        "replays" => {
            let flags = Flags::parse(rest, &[], &["out"])?;
            let out_dir = flags.get("out").map_or_else(
                || util::root().join("apps/web/public/data/ai-replays"),
                PathBuf::from,
            );
            util::step(&format!(
                "generating 10 self-contained AI replay bundles -> {}",
                out_dir.display()
            ));
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            let index = rt
                .block_on(write_demo_replays_to_dir(&out_dir))
                .map_err(|e| format!("failed to generate demo replays: {e}"))?;
            println!(
                "generated {} replay bundles in {}",
                index.items.len(),
                out_dir.display()
            );
            for item in &index.items {
                println!(
                    "  - {:<28} ({} steps{}{})",
                    item.id,
                    item.step_count,
                    if item.has_conflict_demo {
                        ", conflict"
                    } else {
                        ""
                    },
                    if item.has_closure_demo {
                        ", closure"
                    } else {
                        ""
                    },
                );
            }
            Ok(())
        }
        "download" => {
            let flags = Flags::parse(rest, &[], &["dir"])?;
            let dir = flags
                .get("dir")
                .map_or_else(|| util::root().join(DEFAULT_DATASET_DIR), PathBuf::from);
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            println!("SpreadsheetBench Verified (400 tasks, CC BY-SA 4.0)");
            println!("Local cache directory (gitignored): {}", dir.display());
            println!("Dataset source: https://huggingface.co/datasets/KAKA22/SpreadsheetBench");
            println!(
                "Place the verified dataset JSON (`dataset.json`) and input workbooks (`spreadsheet/<id>/...`) under {} before running `cargo xtask eval run`.",
                dir.display()
            );
            Ok(())
        }
        "run" => run_eval(&Flags::parse(
            rest,
            &["mock"],
            &["dataset", "out", "limit", "max-steps", "run-index"],
        )?),
        "grade" => grade_eval(&Flags::parse(rest, &[], &["dir", "image"])?),
        "report" => report_eval(&Flags::parse(rest, &[], &["in", "out"])?),
        other => Err(format!(
            "unknown eval subcommand `{other}`; expected replays, download, run, grade, or report"
        )),
    }
}

fn run_eval(flags: &Flags) -> Result {
    let dataset_dir = flags
        .get("dataset")
        .map_or_else(|| util::root().join(DEFAULT_DATASET_DIR), PathBuf::from);
    let out_dir = flags
        .get("out")
        .map_or_else(|| util::root().join(DEFAULT_OUT_DIR), PathBuf::from);
    let limit: usize = flags
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let max_steps: u32 = flags
        .get("max-steps")
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    let run_index: u32 = flags
        .get("run-index")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let manifest_path = dataset_dir.join("dataset.json");
    if !manifest_path.exists() {
        return Err(format!(
            "dataset manifest not found at {}; run `cargo xtask eval download` first",
            manifest_path.display()
        ));
    }

    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("failed to read {}: {e}", manifest_path.display()))?;
    let tasks: Vec<serde_json::Value> =
        serde_json::from_str(&raw).map_err(|e| format!("invalid dataset.json: {e}"))?;

    let provider: Box<dyn LlmProvider> = if flags.has("mock") {
        Box::new(ScriptedMockProvider::server_mock())
    } else {
        let dotenv = util::root().join(".env.local");
        let cfg = LlmConfig::from_env_and_dotenv(Some(&dotenv))
            .map_err(|e| format!("LLM config error: {e}"))?
            .ok_or_else(|| {
                "live LLM is not configured (set RETICULA_LLM_ENABLED=true and RETICULA_LLM_API_KEY in .env.local, or pass --mock)"
                    .to_owned()
            })?;
        Box::new(
            OpenAiCompatProvider::new(cfg)
                .map_err(|e| format!("failed to build OpenAI-compatible client: {e}"))?,
        )
    };

    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;
    let cfg = AgentRunConfig {
        max_steps,
        ..AgentRunConfig::default()
    };

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    for task in tasks.into_iter().take(limit) {
        let task_id = task["id"]
            .as_str()
            .or_else(|| task["task_id"].as_str())
            .unwrap_or("unknown")
            .to_owned();
        let instruction = task["instruction"].as_str().unwrap_or("").to_owned();
        let input_rel = task["spreadsheet_path"]
            .as_str()
            .or_else(|| task["input_path"].as_str())
            .unwrap_or("");
        let input_path = dataset_dir.join(input_rel);
        let output_path = out_dir.join(format!("{task_id}_output.xlsx"));
        let res = rt.block_on(run_headless_xlsx_task(
            &task_id,
            &instruction,
            &input_path,
            &output_path,
            provider.as_ref(),
            cfg,
        ));
        match res {
            Ok(mut case) => {
                // Pending external LibreOffice container grading unless marked.
                case.passed = false;
                results.push(case);
            }
            Err(err) => {
                results.push(EvalCaseResult {
                    task_id,
                    passed: false,
                    export_path: reticula_agent::eval::EvalExportPath::ExportXlsx,
                    preserved_opaque_formulas: 0,
                    steps: 0,
                    prompt_tokens: 0,
                    cached_prompt_tokens: 0,
                    completion_tokens: 0,
                    latency_ms: 0,
                    failure_category: Some(FailureCategory::KernelGap),
                    error: Some(err.to_string()),
                });
            }
        }
    }

    let summary = summarize_eval_cases(&results);
    let report = EvalRunReport {
        benchmark: "SpreadsheetBench Verified".to_owned(),
        dataset_license: "CC BY-SA 4.0".to_owned(),
        model: provider.model_name().to_owned(),
        provider: provider.provider_name().to_owned(),
        grader_image: DEFAULT_GRADER_IMAGE.to_owned(),
        libreoffice_version: DEFAULT_LO_VERSION.to_owned(),
        run_index,
        summary,
        cases: results,
    };
    let report_path = out_dir.join(format!("run-{run_index}.json"));
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(&report_path, json)
        .map_err(|e| format!("failed to write {}: {e}", report_path.display()))?;
    println!("wrote raw eval run to {}", report_path.display());
    Ok(())
}

fn grade_eval(flags: &Flags) -> Result {
    let dir = flags
        .get("dir")
        .map_or_else(|| util::root().join(DEFAULT_OUT_DIR), PathBuf::from);
    let image = flags.get("image").unwrap_or(DEFAULT_GRADER_IMAGE);
    println!("LibreOffice grading target: {image}");
    println!("Output workbook directory: {}", dir.display());
    if !Path::new(&dir).exists() {
        return Err(format!("directory {} does not exist", dir.display()));
    }
    println!(
        "Run LibreOffice Calc inside `{image}` against `*_output.xlsx` and expected answer workbooks to populate `passed` and `failureCategory` fields before running `cargo xtask eval report`."
    );
    Ok(())
}

fn report_eval(flags: &Flags) -> Result {
    let input = flags.get("in").ok_or_else(|| {
        "usage: cargo xtask eval report --in <run.json> [--out <summary.json>]".to_owned()
    })?;
    let raw = std::fs::read_to_string(input).map_err(|e| format!("cannot read {input}: {e}"))?;
    let mut report: EvalRunReport =
        serde_json::from_str(&raw).map_err(|e| format!("invalid eval report JSON: {e}"))?;
    report.summary = summarize_eval_cases(&report.cases);
    let s = &report.summary;
    println!(
        "{} ({} / {}): {}/{} ({:.2}%, 95% Wilson CI [{:.2}%, {:.2}%])",
        report.benchmark,
        report.provider,
        report.model,
        s.passed_cases,
        s.total_cases,
        s.pass_rate * 100.0,
        s.wilson_ci_low * 100.0,
        s.wilson_ci_high * 100.0
    );
    println!(
        "Export path: patch_xlsx={}, export_xlsx={} | Failures: kernel_gap={}, agent_strategy={}, model_capability={}",
        s.patch_xlsx_count,
        s.export_xlsx_count,
        s.kernel_gap_failures,
        s.agent_strategy_failures,
        s.model_capability_failures
    );
    if let Some(out) = flags.get("out") {
        let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(out, json).map_err(|e| format!("cannot write {out}: {e}"))?;
        println!("wrote report to {out}");
    }
    Ok(())
}

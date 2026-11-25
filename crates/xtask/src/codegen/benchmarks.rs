//! What the archived measurements in `bench/results/` say: the numbers of the
//! landing page, each with where it comes from, and the history for the
//! dashboard.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use reticula_formula::functions::catalog::FUNCTION_CATALOG;
use serde_json::Value;

use super::{BANNER, quoted};
use crate::util::{self, Result};

const RESULTS: &str = "bench/results";

/// The milestones in the order they were finished. A suite is named after the
/// one it was measured for: `m6-startup`.
const MILESTONES: [&str; 8] = ["m1a", "m1b", "m2a", "m2b", "m3a", "m3b", "m4", "m6"];

/// The functions of the catalog that are not another name of one that is there
/// as well (`crates/formula/src/functions/catalog.rs`).
const UNIQUE_FUNCTIONS: u64 = 193;
/// The functions whose result depends on their arguments alone.
const DETERMINISTIC_FUNCTIONS: u64 = 195;
/// What `crates/sim/src/world.rs` checks, which numbers it.
const SIM_INVARIANTS: u64 = 11;

/// Where a suite stands among the ones of the same day: the later milestone's
/// counts, and a report that names no suite is older than the ones that do.
fn suite_rank(report: &Value) -> usize {
    let suite = report["suite"].as_str().unwrap_or("");
    let milestone = suite.split('-').next().unwrap_or("");
    MILESTONES
        .iter()
        .position(|known| *known == milestone)
        .map_or(0, |position| position + 1)
}

fn commit_subrank(commit: &str) -> u8 {
    match commit {
        "c515330" => 1,
        "0429bfd" => 2,
        "9eb1390" => 3,
        _ => 50,
    }
}

fn short_date(val: &Value) -> String {
    let raw = val["date"].as_str().unwrap_or("");
    raw.chars().take(10).collect()
}

/// A report and the name of its file.
struct Archived {
    name: String,
    report: Value,
}

/// Puts reports in the order they were made, the latest last.
fn sort_by_age(rows: &mut [Archived]) {
    rows.sort_by_cached_key(|Archived { name, report }| {
        let text = |key: &str| report[key].as_str().unwrap_or("").to_owned();
        (
            short_date(report),
            suite_rank(report),
            commit_subrank(&text("commit")),
            // Some reports have the time as well.
            text("date"),
            name.clone(),
        )
    });
}

/// The kind of a report, which its name ends with: `2026-11-26-abc1234-gpu.json`.
/// Not all kinds are shown anywhere.
fn kind_of(name: &str) -> Option<&str> {
    let (_, kind) = name.strip_suffix(".json")?.rsplit_once('-')?;
    Some(kind)
}

fn load_all_results(kind: &str) -> Result<Vec<Archived>> {
    let dir = util::root().join(RESULTS);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut rows = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if kind_of(name) != Some(kind) {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let report: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("invalid JSON {}: {e}", path.display()))?;
        rows.push(Archived {
            name: name.to_owned(),
            report,
        });
    }
    sort_by_age(&mut rows);
    Ok(rows)
}

fn latest_result(kind: &str) -> Result<Option<Archived>> {
    Ok(load_all_results(kind)?.pop())
}

/// The processor's name without the trademarks, which nobody looks it up by.
fn cpu_name(raw: &str) -> String {
    let plain = raw.replace("(R)", "").replace("(TM)", "");
    let words: Vec<&str> = plain.split_whitespace().collect();
    match words.as_slice() {
        [_, "Gen", rest @ ..] if !rest.is_empty() => rest.join(" "),
        _ => words.join(" "),
    }
}

/// The browser a report names, as it calls itself.
fn browser_name(browser: &Value) -> Option<String> {
    let version = browser["version"].as_str()?;
    let name = browser["name"].as_str().unwrap_or("chrome");
    let mut letters = name.chars();
    let first = letters.next()?.to_uppercase();
    Some(format!("{first}{} {version}", letters.as_str()))
}

fn opt_text(value: Option<&str>) -> String {
    value.map_or_else(|| String::from("null"), quoted)
}

fn opt_bool(value: Option<bool>) -> String {
    value.map_or_else(|| String::from("null"), |value| value.to_string())
}

/// A number, or a null that the type still allows to be one.
fn opt_number(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(
        || String::from("null as number | null"),
        |value| format!("{value:.decimals$}"),
    )
}

/// Where the numbers of a report come from. The machine and the browser are
/// named by the report itself or by the part of it that was measured in the
/// browser, under names that have changed over time.
fn provenance(archived: Option<&Archived>) -> String {
    let Some(Archived { name, report }) = archived else {
        return String::from("null");
    };
    let part = [&report["wasmChrome"], report]
        .into_iter()
        .find(|part| part["machine"].is_object())
        .unwrap_or(report);
    let (machine, browser) = (&part["machine"], &part["browser"]);
    let first = |values: [&Value; 2]| {
        values
            .into_iter()
            .find_map(Value::as_str)
            .map(str::to_owned)
    };
    let cpu = first([&machine["cpus"], &machine["cpu"]]).map(|raw| cpu_name(&raw));
    let affinity = [
        &report["affinity"],
        &report["options"]["affinity"],
        &machine["system"]["affinity"],
    ]
    .into_iter()
    .find_map(Value::as_str);
    let memory = [&machine["totalMemGiB"], &machine["memoryGB"]]
        .into_iter()
        .find_map(Value::as_f64);

    let mut out = String::from("{\n");
    for (key, value) in [
        ("report", quoted(&format!("{RESULTS}/{name}"))),
        (
            "commit",
            quoted(report["commit"].as_str().unwrap_or("unknown")),
        ),
        ("date", quoted(&short_date(report))),
        (
            "uncommittedChanges",
            opt_bool(report["uncommittedChanges"].as_bool()),
        ),
        ("cpu", opt_text(cpu.as_deref())),
        (
            "logicalProcessors",
            machine["logicalProcessors"]
                .as_u64()
                .map_or_else(|| String::from("null"), |count| count.to_string()),
        ),
        (
            "memoryGb",
            memory.map_or_else(|| String::from("null"), |size| format!("{size:.0}")),
        ),
        (
            "os",
            opt_text(first([&machine["platform"], &machine["os"]]).as_deref()),
        ),
        ("browser", opt_text(browser_name(browser).as_deref())),
        ("headless", opt_bool(browser["headless"].as_bool())),
        ("affinity", opt_text(affinity)),
    ] {
        let _ = writeln!(out, "    {key}: {value},");
    }
    out.push_str("  }");
    out
}

const TYPES: &str = "\
/** Where a group of numbers comes from. What a report does not say is null. */
export interface Provenance {
  /** The archived report, from the root of the repository. */
  readonly report: string;
  readonly commit: string;
  readonly date: string;
  /** Whether the files measured differed from the commit's. */
  readonly uncommittedChanges: boolean | null;
  readonly cpu: string | null;
  readonly logicalProcessors: number | null;
  readonly memoryGb: number | null;
  readonly os: string | null;
  readonly browser: string | null;
  readonly headless: boolean | null;
  /** The logical processors the measurement was held to, as a mask. */
  readonly affinity: string | null;
}

/** The kinds of reports, each of which has a provenance of its own. */
export type Measurement = \"engine\" | \"startup\" | \"render\" | \"sync\" | \"load\" | \"diff\";

/** What one renderer did under the load both were given. */
export interface RendererResult {
  readonly renderer: \"canvas2d\" | \"webgpu\";
  /** What a frame took on the main thread. */
  readonly workP50Ms: number | null;
  readonly workP99Ms: number | null;
  /** The share of frames that came more than 17.2 ms after the one before. */
  readonly lateFrames: number | null;
}

/** A run in which the renderers drew the same thing in turns. */
export interface RendererComparison {
  readonly provenance: Provenance;
  readonly visibleCells: number;
  readonly width: number;
  readonly height: number;
  readonly refreshHz: number | null;
  readonly results: readonly RendererResult[];
}

/** One workload of a renderer, as `public/data/benchmarks-history.json` has it. */
export interface GpuResult {
  /** The app with its engine, or the renderer by itself. */
  readonly part: \"app\" | \"renderer\";
  /** About how many cells are in view where the zoom stays the same. */
  readonly size: \"1k\" | \"10k\" | null;
  readonly motion: \"scroll\" | \"jump\" | \"zoom\" | \"edit\";
  readonly content: \"distinct\" | \"repeating\";
  readonly renderer: RendererResult[\"renderer\"];
  readonly cells: number | null;
  readonly frames: number | null;
  readonly workP50Ms: number | null;
  readonly workP99Ms: number | null;
  readonly workMaxMs: number | null;
  readonly intervalP50Ms: number | null;
  readonly intervalP99Ms: number | null;
  /** The share of frames that came more than 17.2 ms after the one before. */
  readonly shareOver60Hz: number | null;
  readonly placeholderFrames: number | null;
}

/** A run of the renderers in one browser, with its conditions. */
export interface GpuRun {
  readonly report: string;
  readonly date: string;
  readonly commit: string;
  readonly suite: string;
  readonly cpu: string | null;
  readonly browser: string | null;
  readonly headless: boolean | null;
  /** How many times slower than it is the main thread was made. */
  readonly slowdown: number | null;
  readonly dpr: number | null;
  readonly refreshHz: number | null;
  readonly affinity: string | null;
  readonly adapter: string | null;
  readonly results: readonly GpuResult[];
}
";

/// The constants of the landing page and of the page for small screens.
pub fn constants() -> Result<String> {
    let engine = latest_result("engine")?;
    let startup = latest_result("startup")?;
    let render = latest_result("render")?;
    let sync = latest_result("sync")?;
    let load = latest_result("load")?;
    let diff = latest_result("diff")?;

    let report = |archived: &Option<Archived>| {
        archived
            .as_ref()
            .map_or(Value::Null, |found| found.report.clone())
    };
    let wasm = report(&engine)["wasmChrome"].clone();
    let (first, scroll) = (report(&startup), report(&render));
    let (replay, clients) = (report(&sync)["wasmChrome"].clone(), report(&load));
    let mib = |bytes: &Value| bytes.as_f64().map(|bytes| bytes / (1024.0 * 1024.0));
    let times = |value: &Value, factor: f64| value.as_f64().map(|number| number * factor);

    let open = &wasm["b1_b11_20m_with_1m_formulas"];
    let limit = &wasm["b12_50m_limit"];
    let single = &replay["b9a_1000_single_cells"];
    let scrolled = &scroll["chromeS1"]["scrollMargin2"];
    let visible = &clients["remoteVisibilityLoopbackMs"];
    let delayed = &clients["remoteVisibilityRtt50Ms"];
    let numbers: &[(&str, Option<f64>, usize)] = &[
        ("b1Load20m1mP50Ms", open["load_ms"]["p50_ms"].as_f64(), 2),
        ("b11Save20m1mP50Ms", open["save_ms"]["p50_ms"].as_f64(), 2),
        ("b11ProbeP95Ms", open["save_probe_ms"]["p95_ms"].as_f64(), 2),
        ("b12Load50mP50Ms", limit["load_ms"]["p50_ms"].as_f64(), 2),
        ("b12Save50mP50Ms", limit["save_ms"]["p50_ms"].as_f64(), 2),
        (
            "b12Recalc100kP50Ms",
            limit["recalc_100k_ms"]["p50_ms"].as_f64(),
            2,
        ),
        ("b12PeakWasmMiB", mib(&limit["peak_wasm_memory_bytes"]), 2),
        (
            "b4Incremental100kP50Ms",
            wasm["b4_incremental_100k"]["p50_ms"].as_f64(),
            2,
        ),
        (
            "b4Incremental100kP95Ms",
            wasm["b4_incremental_100k"]["p95_ms"].as_f64(),
            2,
        ),
        (
            "b7aInsertTopMeanUs",
            times(
                &wasm["b7a_insert_top_row_local"]["batch100_mean_ms"],
                1000.0,
            ),
            1,
        ),
        (
            "b7aInsertCrossRangeP50Ms",
            wasm["b7a_insert_cross_range"]["p50_ms"].as_f64(),
            2,
        ),
        ("b2Sum1mP50Ms", wasm["b2_sum_1m"]["p50_ms"].as_f64(), 2),
        (
            "b3FullRecalc1mP50Ms",
            wasm["b3_full_recalc_1m"]["p50_ms"].as_f64(),
            1,
        ),
        ("b5Chain1mP50Ms", wasm["b5_chain_1m"]["p50_ms"].as_f64(), 1),
        (
            "startupShellP50Ms",
            first["results"]["shell"]["p50"].as_f64(),
            2,
        ),
        (
            "startupFirstScreenP50Ms",
            first["results"]["firstScreen"]["p50"].as_f64(),
            2,
        ),
        ("startupVisits", first["options"]["runs"].as_f64(), 0),
        (
            "startupDownMbps",
            times(&first["network"]["downBitsPerSecond"], 1e-6),
            0,
        ),
        (
            "startupRoundTripMs",
            first["network"]["roundTripMs"].as_f64(),
            0,
        ),
        (
            "renderScrollWorkP99Ms",
            scroll["scrollWorkP99Ms"].as_f64(),
            2,
        ),
        ("renderScrollFrames", scrolled["frames"].as_f64(), 0),
        (
            "b9aFrameP50Ms",
            single["engine_frame_ms"]["p50_ms"].as_f64(),
            2,
        ),
        (
            "b9aTotalP50Ms",
            single["engine_total_ms"]["p50_ms"].as_f64(),
            2,
        ),
        (
            "b9aTotalP95Ms",
            single["engine_total_ms"]["p95_ms"].as_f64(),
            2,
        ),
        (
            "b9bTotalP50Ms",
            replay["b9b_100k_cell_block"]["engine_total_ms"]["p50_ms"].as_f64(),
            2,
        ),
        ("loadClients", clients["clients"].as_f64(), 0),
        ("loadTxnsPerSec", clients["achievedTxnsPerSec"].as_f64(), 2),
        ("loadSeconds", clients["durationSeconds"].as_f64(), 0),
        ("loadLoopbackP50Ms", visible["p50_ms"].as_f64(), 2),
        ("loadRtt50P95Ms", delayed["p95_ms"].as_f64(), 2),
        (
            "loadAddedRoundTripMs",
            clients["referenceRttMs"].as_f64(),
            0,
        ),
        (
            "loadServerPeakMiB",
            clients["resources"]["serverPeakMiB"].as_f64(),
            2,
        ),
        (
            "loadCellsM",
            times(&clients["document"]["populatedCells"], 1e-6),
            2,
        ),
    ];

    let mut out = String::from(BANNER);
    out.push_str("//\n// What the latest archived reports say. A number that is in none of them is null.\n\n");
    out.push_str(TYPES);
    out.push_str("\nexport const BENCHMARKS = {\n");
    for (key, value, decimals) in numbers {
        let _ = writeln!(out, "  {key}: {},", opt_number(*value, *decimals));
    }
    out.push_str("} as const;\n\n");

    out.push_str("export const PROVENANCE: Readonly<Record<Measurement, Provenance | null>> = {\n");
    for (key, archived) in [
        ("engine", &engine),
        ("startup", &startup),
        ("render", &render),
        ("sync", &sync),
        ("load", &load),
        ("diff", &diff),
    ] {
        let _ = writeln!(out, "  {key}: {},", provenance(archived.as_ref()));
    }
    out.push_str("};\n\n");

    out.push_str(&quality_constants(&report(&diff))?);
    let comparison = load_all_results("gpu")?
        .iter()
        .rev()
        .find_map(renderer_comparison)
        .unwrap_or_else(|| String::from("null"));
    let _ = write!(
        out,
        "\n/** The latest comparison of the renderers, once there is one. */\n\
         export const RENDERER_COMPARISON: RendererComparison | null = {comparison};\n",
    );
    Ok(out)
}

/// The runs a report of the renderers is made of, the one it is about first. The
/// others are of other browsers, or on a main thread that is held back.
fn runs(report: &Value) -> &[Value] {
    report["runs"].as_array().map_or(&[], Vec::as_slice)
}

/// Both renderers in the app, under the load of the latest run in Chrome.
fn renderer_comparison(archived: &Archived) -> Option<String> {
    let run = runs(&archived.report).iter().find(|run| {
        run["browser"]["name"] == "chrome"
            && ["slowdown", "dpr"]
                .iter()
                .all(|key| run[key].as_f64() == Some(1.0))
    })?;
    let product = run["product"].as_array()?;
    let of = |renderer: &str| product.iter().find(|one| one["renderer"] == renderer);
    let (canvas, webgpu) = (of("canvas2d")?, of("webgpu")?);
    let cells = [canvas, webgpu]
        .into_iter()
        .filter_map(|one| one["cells"]["p50"].as_f64())
        .reduce(f64::min)?;
    let source = Archived {
        name: archived.name.clone(),
        report: run.clone(),
    };

    let mut out = String::from("{\n");
    let _ = writeln!(out, "  provenance: {},", provenance(Some(&source)));
    let _ = writeln!(out, "  visibleCells: {cells:.0},");
    for key in ["width", "height"] {
        let _ = writeln!(out, "  {key}: {:.0},", canvas[key].as_f64()?);
    }
    let rate = opt_number(run["refreshHz"].as_f64(), 0);
    let _ = writeln!(out, "  refreshHz: {rate},\n  results: [");
    for (renderer, one) in [("canvas2d", canvas), ("webgpu", webgpu)] {
        let _ = writeln!(
            out,
            "    {{ renderer: {}, workP50Ms: {}, workP99Ms: {}, lateFrames: {} }},",
            quoted(renderer),
            opt_number(one["work"]["p50"].as_f64(), 2),
            opt_number(one["work"]["p99"].as_f64(), 2),
            opt_number(one["shareOver60Hz"].as_f64(), 4),
        );
    }
    out.push_str("  ],\n}");
    Some(out)
}

/// A run of the renderers as the dashboard shows it.
fn gpu_run(archived: &Archived, run: &Value) -> Value {
    let workload = |part: &str, one: &Value| {
        serde_json::json!({
            // The app has an engine to wait for, a toolbar and a size of its own.
            "part": part,
            "size": one["size"],
            "motion": one["motion"].as_str().unwrap_or("scroll"),
            "content": one["content"].as_str().unwrap_or("distinct"),
            "renderer": one["renderer"],
            "cells": one["cells"]["p50"],
            "frames": one["frames"],
            "workP50Ms": opt_round2(one["work"]["p50"].as_f64()),
            "workP99Ms": opt_round2(one["work"]["p99"].as_f64()),
            "workMaxMs": opt_round2(one["work"]["max"].as_f64()),
            "intervalP50Ms": opt_round2(one["interval"]["p50"].as_f64()),
            "intervalP99Ms": opt_round2(one["interval"]["p99"].as_f64()),
            "shareOver60Hz": one["shareOver60Hz"].as_f64().map(|share| (share * 1e4).round() / 1e4),
            "placeholderFrames": one["placeholderFrames"],
        })
    };
    let each = |part: &'static str, key: &str| -> Vec<Value> {
        let all = run[key].as_array().map_or(&[][..], Vec::as_slice);
        all.iter().map(|one| workload(part, one)).collect()
    };
    let results = [each("app", "product"), each("renderer", "synthetic")].concat();
    serde_json::json!({
        "report": format!("{RESULTS}/{}", archived.name),
        "date": short_date(run),
        "commit": run["commit"].as_str().unwrap_or("unknown"),
        "suite": archived.report["suite"].as_str().unwrap_or("gpu"),
        "cpu": run["machine"]["cpu"].as_str().map(cpu_name),
        "browser": browser_name(&run["browser"]),
        "headless": run["browser"]["headless"],
        "slowdown": run["slowdown"],
        "dpr": run["dpr"],
        "refreshHz": run["refreshHz"],
        "affinity": run["affinity"],
        "adapter": run["adapter"]["vendor"],
        "results": results,
    })
}

fn gpu_runs(archives: &[Archived]) -> Vec<Value> {
    archives
        .iter()
        .flat_map(|archived| {
            runs(&archived.report)
                .iter()
                .map(|run| gpu_run(archived, run))
        })
        .collect()
}

fn read(path: &str) -> Result<String> {
    std::fs::read_to_string(util::root().join(path)).map_err(|e| format!("cannot read {path}: {e}"))
}

/// The number that follows a text in a file.
fn number_after(path: &str, text: &str) -> Result<u64> {
    number_in(&read(path)?, text).ok_or_else(|| format!("no number after `{text}` in {path}"))
}

fn number_in(source: &str, text: &str) -> Option<u64> {
    let (_, rest) = source.split_once(text)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// A count of what is there for certain: none of it means that it has moved or
/// is written in another way, and the pages must not say that there is none.
fn some(what: &str, count: usize) -> Result<usize> {
    if count == 0 {
        return Err(format!("found no {what}"));
    }
    Ok(count)
}

/// How many different reasons the differences from LibreOffice have. A cluster
/// is the ones of one function, and several functions may have one reason.
fn kinds_of_differences(diff: &Value) -> Option<usize> {
    let reasons: BTreeSet<&str> = diff["clusters"]
        .as_array()?
        .iter()
        .filter_map(|cluster| cluster["disposition"].as_str())
        .collect();
    Some(reasons.len())
}

/// How the tests are set up, read from what sets them up.
struct Quality {
    sim_ci_seeds: u64,
    sim_nightly_seeds: u64,
    fuzz_targets: usize,
    excel_numeric_cases: usize,
    determinism_digest: String,
    compat_files: usize,
    compat_generations: Vec<String>,
    visual_cases: usize,
}

impl Quality {
    fn read() -> Result<Self> {
        let fuzz = util::root().join("fuzz/fuzz_targets");
        let fuzz_targets = std::fs::read_dir(&fuzz)
            .map_err(|e| format!("cannot read {}: {e}", fuzz.display()))?
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
            .count();
        let excel = read("spikes/excel/results/numeric-excel-16.0.20711.tsv")?;
        let hashes = read("tests/compat/hashes.txt")?;
        let compat: Vec<&str> = hashes
            .lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .nth(1)?
                    .strip_prefix("tests/compat/")
            })
            .collect();
        let generations: BTreeSet<&str> = compat
            .iter()
            .filter_map(|path| Path::new(path).iter().next()?.to_str())
            .collect();
        let fuzz_targets = some("fuzz targets", fuzz_targets)?;
        let excel_numeric_cases = some(
            "cases recorded in Excel",
            excel.lines().filter(|line| is_content(line)).count(),
        )?;
        let compat_files = some("frozen files", compat.len())?;
        // Every case is a line of its own.
        let visual_cases = some(
            "visual cases",
            read("apps/web/src/visual/names.ts")?
                .lines()
                .filter(|line| line.contains(": { scale: "))
                .count(),
        )?;
        Ok(Self {
            sim_ci_seeds: number_after(".github/workflows/ci.yml", "xtask sim --seeds ")?,
            sim_nightly_seeds: number_after(
                ".github/workflows/sim-nightly.yml",
                "inputs.seeds || '",
            )?,
            fuzz_targets,
            excel_numeric_cases,
            determinism_digest: read("tests/determinism/expected.txt")?.trim().to_owned(),
            compat_files,
            compat_generations: generations.into_iter().map(str::to_owned).collect(),
            visual_cases,
        })
    }
}

fn is_content(line: &str) -> bool {
    !line.is_empty() && !line.starts_with('#')
}

fn quality_constants(diff: &Value) -> Result<String> {
    let quality = Quality::read()?;
    let mut out =
        String::from("/** What the code is checked against. */\nexport const QUALITY = {\n");
    let count = |value: &Value| opt_number(value.as_f64(), 0);
    let libreoffice = diff["libreoffice_version"].as_str();
    // "LibreOffice 26.8.2.2 680(Build:2)"
    let libreoffice =
        libreoffice.map(|version| version.split(' ').take(2).collect::<Vec<_>>().join(" "));
    let rust = &diff["rust_reference"];
    for (key, value) in [
        ("builtInFunctions", UNIQUE_FUNCTIONS.to_string()),
        (
            "libreOffice",
            libreoffice.map_or_else(
                || String::from("null as string | null"),
                |text| quoted(&text),
            ),
        ),
        ("diffFormulas", count(&diff["total_formulas"])),
        ("diffCells", count(&diff["total_cells_compared"])),
        (
            "diffAgreement",
            opt_number(diff["agreement_ratio"].as_f64(), 4),
        ),
        (
            "diffClasses",
            opt_number(kinds_of_differences(diff).map(|count| count as f64), 0),
        ),
        ("diffUnexplained", count(&diff["unclassified_count"])),
        ("rustFormulas", count(&rust["total_formulas"])),
        ("ironcalcMatches", count(&rust["ironcalc_matches"])),
        ("formualizerMatches", count(&rust["formualizer_matches"])),
        ("excelNumericCases", quality.excel_numeric_cases.to_string()),
        ("simInvariants", SIM_INVARIANTS.to_string()),
        ("simCiSeeds", quality.sim_ci_seeds.to_string()),
        ("simNightlySeeds", quality.sim_nightly_seeds.to_string()),
        ("fuzzTargets", quality.fuzz_targets.to_string()),
        ("determinismDigest", quoted(&quality.determinism_digest)),
        ("compatFiles", quality.compat_files.to_string()),
        (
            "compatGenerations",
            quality.compat_generations.len().to_string(),
        ),
        ("visualCases", quality.visual_cases.to_string()),
    ] {
        let _ = writeln!(out, "  {key}: {value},");
    }
    out.push_str("} as const;\n");
    Ok(out)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn opt_round2(value: Option<f64>) -> Value {
    value.map_or(Value::Null, |n| serde_json::json!(round2(n)))
}

fn extract_engine_metrics(block: &Value) -> Value {
    let b7a_top_us = block["b7a_insert_top_row_local"]["batch100_mean_ms"]
        .as_f64()
        .or_else(|| block["b7a_insert_top_row_local"]["mean_ms"].as_f64())
        .or_else(|| block["b7a_insert_top_row_local"]["p50_ms"].as_f64())
        .map(|ms| ms * 1000.0);
    let b12_peak_mib = block["b12_50m_limit"]["peak_wasm_memory_bytes"]
        .as_f64()
        .map(|bytes| bytes / (1024.0 * 1024.0));
    serde_json::json!({
        "b1Load20mMs": opt_round2(block["b1_b11_20m_with_1m_formulas"]["load_ms"]["p50_ms"].as_f64()),
        "b11Save20mMs": opt_round2(block["b1_b11_20m_with_1m_formulas"]["save_ms"]["p50_ms"].as_f64()),
        "b11ProbeP95Ms": opt_round2(block["b1_b11_20m_with_1m_formulas"]["save_probe_ms"]["p95_ms"].as_f64()),
        "b2Sum1mMs": opt_round2(block["b2_sum_1m"]["p50_ms"].as_f64()),
        "b3FullRecalc1mMs": opt_round2(block["b3_full_recalc_1m"]["p50_ms"].as_f64()),
        "b4Incremental100kP50Ms": opt_round2(block["b4_incremental_100k"]["p50_ms"].as_f64()),
        "b4Incremental100kP95Ms": opt_round2(block["b4_incremental_100k"]["p95_ms"].as_f64()),
        "b5Chain1mMs": opt_round2(block["b5_chain_1m"]["p50_ms"].as_f64()),
        "b7aInsertTopUs": opt_round2(b7a_top_us),
        "b7aInsertCrossP50Ms": opt_round2(block["b7a_insert_cross_range"]["p50_ms"].as_f64()),
        "b12Load50mMs": opt_round2(block["b12_50m_limit"]["load_ms"]["p50_ms"].as_f64()),
        "b12Save50mMs": opt_round2(block["b12_50m_limit"]["save_ms"]["p50_ms"].as_f64()),
        "b12Recalc100kMs": opt_round2(block["b12_50m_limit"]["recalc_100k_ms"]["p50_ms"].as_f64()),
        "b12PeakWasmMiB": opt_round2(b12_peak_mib),
    })
}

fn sync_stage_label(commit: &str, suite: &str) -> &'static str {
    match commit {
        "c515330" => "M2a clone",
        "0429bfd" => "M2a in-place",
        "9eb1390" => "M2b worker incremental",
        _ if suite.starts_with("m3a") => "M3a wire v3 + permute",
        _ if suite.starts_with("m6") => "M6",
        _ => "Sync replay",
    }
}

fn format_diff_value(val: &Value) -> String {
    if let Some(n) = val["Number"].as_f64() {
        return format!("{n}");
    }
    if let Some(b) = val["Bool"].as_bool() {
        return if b { "TRUE" } else { "FALSE" }.to_owned();
    }
    if let Some(s) = val["Text"].as_str() {
        return format!("\"{s}\"");
    }
    if let Some(e) = val["Error"].as_str() {
        return e.to_owned();
    }
    if let Some(s) = val.as_str() {
        return s.to_owned();
    }
    val.to_string()
}

fn build_diff_summary(val: &Value) -> Value {
    let by_family: Vec<Value> = val["by_family"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|entry| {
                    serde_json::json!({
                        "family": entry["family"].as_str().unwrap_or(""),
                        "total": entry["total"].as_u64().unwrap_or(0),
                        "nonError": entry["non_error"].as_u64().unwrap_or(0),
                        "matches": entry["matches"].as_u64().unwrap_or(0),
                        "agreementRatio": entry["agreement_ratio"].as_f64().unwrap_or(0.0),
                        "unclassified": entry["unclassified"].as_u64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let clusters: Vec<Value> = val["clusters"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    serde_json::json!({
                        "referenceEngine": c["reference_engine"].as_str().unwrap_or(""),
                        "category": c["category"].as_str().unwrap_or(""),
                        "family": c["family"].as_str().unwrap_or(""),
                        "disposition": c["disposition"].as_str().unwrap_or(""),
                        "reason": c["reason"].as_str().unwrap_or(""),
                        "count": c["count"].as_u64().unwrap_or(0),
                        "sampleFormula": c["sample_formula"].as_str().unwrap_or(""),
                        "reticulaResult": format_diff_value(&c["reticula_result"]),
                        "libreofficeResult": format_diff_value(&c["libreoffice_result"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let rust_clusters: Vec<Value> = val["rust_reference"]["clusters"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    serde_json::json!({
                        "referenceEngine": c["reference_engine"].as_str().unwrap_or(""),
                        "category": c["category"].as_str().unwrap_or(""),
                        "disposition": c["disposition"].as_str().unwrap_or(""),
                        "count": c["count"].as_u64().unwrap_or(0),
                        "sampleFormula": c["sample_formula"].as_str().unwrap_or(""),
                        "reticulaResult": format_diff_value(&c["reticula_result"]),
                        "referenceResult": format_diff_value(&c["reference_result"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    serde_json::json!({
        "date": short_date(val),
        "commit": val["commit"].as_str().unwrap_or("unknown"),
        "libreofficeVersion": val["libreoffice_version"].as_str().unwrap_or("unknown"),
        "containerImage": val["container_image"].as_str().unwrap_or("unknown"),
        "totalFormulas": val["total_formulas"].as_u64().unwrap_or(0),
        "totalCellsCompared": val["total_cells_compared"].as_u64().unwrap_or(0),
        "deterministicFunctionsCovered": val["deterministic_functions_covered"].as_u64().unwrap_or(0),
        "uniqueFunctionsCovered": val["unique_functions_covered"].as_u64().unwrap_or(0),
        "minSamplesPerFunction": val["min_samples_per_function"].as_u64().unwrap_or(0),
        "nonErrorCount": val["non_error_count"].as_u64().unwrap_or(0),
        "nonErrorRatio": val["non_error_ratio"].as_f64().unwrap_or(0.0),
        "matches": val["matches"].as_u64().unwrap_or(0),
        "agreementRatio": val["agreement_ratio"].as_f64().unwrap_or(0.0),
        "unclassifiedCount": val["unclassified_count"].as_u64().unwrap_or(0),
        "byFamily": by_family,
        "clusters": clusters,
        "rustReference": {
            "totalFormulas": val["rust_reference"]["total_formulas"].as_u64().unwrap_or(0),
            "nonErrorCount": val["rust_reference"]["non_error_count"].as_u64().unwrap_or(0),
            "nonErrorRatio": val["rust_reference"]["non_error_ratio"].as_f64().unwrap_or(0.0),
            "ironcalcMatches": val["rust_reference"]["ironcalc_matches"].as_u64().unwrap_or(0),
            "formualizerMatches": val["rust_reference"]["formualizer_matches"].as_u64().unwrap_or(0),
            "clusters": rust_clusters,
        },
    })
}

/// Generates static historical benchmark & correctness JSON for the lazy
/// `/benchmarks` dashboard.
pub fn history() -> Result<String> {
    let engine_runs: Vec<Value> = load_all_results("engine")?
        .into_iter()
        .map(|Archived { report: val, .. }| {
            let wasm = &val["wasmChrome"];
            let browser_name = wasm["browser"]["name"].as_str().unwrap_or("chrome");
            let browser_ver = wasm["browser"]["version"].as_str().unwrap_or("unknown");
            serde_json::json!({
                "date": short_date(&val),
                "commit": val["commit"].as_str().unwrap_or("unknown"),
                "suite": val["suite"].as_str().unwrap_or("engine"),
                "cpu": wasm["machine"]["cpus"].as_str().unwrap_or("unknown"),
                "browser": format!("{browser_name} {browser_ver}"),
                "native": extract_engine_metrics(&val["native"]),
                "wasmChrome": extract_engine_metrics(wasm),
            })
        })
        .collect();

    let startup_runs: Vec<Value> = load_all_results("startup")?
        .into_iter()
        .map(|Archived { report: val, .. }| {
            let down_mbps = val["network"]["downBitsPerSecond"]
                .as_f64()
                .map_or(20.0, |bps| bps / 1_000_000.0);
            let rtt_ms = val["network"]["roundTripMs"].as_u64().unwrap_or(50);
            let suite = val["suite"]
                .as_str()
                .or_else(|| val["spike"].as_str())
                .unwrap_or("startup");
            serde_json::json!({
                "date": short_date(&val),
                "commit": val["commit"].as_str().unwrap_or("unknown"),
                "suite": suite,
                "networkLabel": format!("{down_mbps:.0} Mbps / {rtt_ms} ms RTT"),
                "shellP50Ms": opt_round2(val["results"]["shell"]["p50"].as_f64()),
                "firstScreenP50Ms": opt_round2(val["results"]["firstScreen"]["p50"].as_f64()),
            })
        })
        .collect();

    let render_runs: Vec<Value> = load_all_results("render")?
        .into_iter()
        .map(|Archived { report: val, .. }| {
            serde_json::json!({
                "date": short_date(&val),
                "commit": val["commit"].as_str().unwrap_or("unknown"),
                "suite": val["suite"].as_str().unwrap_or("render"),
                "browser": browser_name(&val["browser"]),
                "scrollWorkP99Ms": opt_round2(val["scrollWorkP99Ms"].as_f64()),
                "chromeMargin2P99Ms": opt_round2(val["chromeS1"]["scrollMargin2"]["work"]["p99"].as_f64()),
                "firefoxJumpP99Ms": opt_round2(val["firefoxHeadedJump"]["jumpCompleteFrameWorkMs"]["p99"].as_f64()),
                "firefoxJumpMaxMs": opt_round2(val["firefoxHeadedJump"]["jumpCompleteFrameWorkMs"]["max"].as_f64()),
            })
        })
        .collect();

    let sync_runs: Vec<Value> = load_all_results("sync")?
        .into_iter()
        .map(|Archived { report: val, .. }| {
            let commit = val["commit"].as_str().unwrap_or("unknown");
            let suite = val["suite"].as_str().unwrap_or("sync");
            let nat = &val["native"];
            let wasm = &val["wasmChrome"];
            serde_json::json!({
                "date": short_date(&val),
                "commit": commit,
                "suite": suite,
                "stage": sync_stage_label(commit, suite),
                "nativeB9aFrameP50Ms": opt_round2(nat["b9a_1000_single_cells"]["engine_frame_ms"]["p50_ms"].as_f64()),
                "nativeB9aTotalP50Ms": opt_round2(nat["b9a_1000_single_cells"]["engine_total_ms"]["p50_ms"].as_f64()),
                "nativeB9bTotalP50Ms": opt_round2(nat["b9b_100k_cell_block"]["engine_total_ms"]["p50_ms"].as_f64()),
                "wasmB9aFrameP50Ms": opt_round2(wasm["b9a_1000_single_cells"]["engine_frame_ms"]["p50_ms"].as_f64()),
                "wasmB9aTotalP50Ms": opt_round2(wasm["b9a_1000_single_cells"]["engine_total_ms"]["p50_ms"].as_f64()),
                "wasmB9aTotalP95Ms": opt_round2(wasm["b9a_1000_single_cells"]["engine_total_ms"]["p95_ms"].as_f64()),
                "wasmB9bTotalP50Ms": opt_round2(wasm["b9b_100k_cell_block"]["engine_total_ms"]["p50_ms"].as_f64()),
            })
        })
        .collect();

    let load_runs: Vec<Value> = load_all_results("load")?
        .into_iter()
        .map(|Archived { report: val, .. }| {
            serde_json::json!({
                "date": short_date(&val),
                "commit": val["commit"].as_str().unwrap_or("unknown"),
                "suite": val["suite"].as_str().unwrap_or("load"),
                "clients": val["clients"].as_u64().unwrap_or(0),
                "targetTxnsPerSec": round2(val["targetTxnsPerSec"].as_f64().unwrap_or(0.0)),
                "achievedTxnsPerSec": round2(val["achievedTxnsPerSec"].as_f64().unwrap_or(0.0)),
                "populatedCells": val["document"]["populatedCells"].as_u64().unwrap_or(0),
                "allDigestsMatch": val["allDigestsMatch"].as_bool().unwrap_or(false),
                "digestMismatches": val["digestMismatches"].as_u64().unwrap_or(0),
                "lostTxns": val["lostTxns"].as_u64().unwrap_or(0),
                "loopbackP50Ms": opt_round2(val["remoteVisibilityLoopbackMs"]["p50_ms"].as_f64()),
                "loopbackP95Ms": opt_round2(val["remoteVisibilityLoopbackMs"]["p95_ms"].as_f64()),
                "rtt50P50Ms": opt_round2(val["remoteVisibilityRtt50Ms"]["p50_ms"].as_f64()),
                "rtt50P95Ms": opt_round2(val["remoteVisibilityRtt50Ms"]["p95_ms"].as_f64()),
                "serverPeakMiB": opt_round2(val["resources"]["serverPeakMiB"].as_f64()),
                "postgresPeakMiB": opt_round2(val["resources"]["postgresPeakMiB"].as_f64()),
            })
        })
        .collect();

    let latest_diff = latest_result("diff")?.map(|found| build_diff_summary(&found.report));
    let quality = Quality::read()?;

    let history = serde_json::json!({
        "engineRuns": engine_runs,
        "startupRuns": startup_runs,
        "renderRuns": render_runs,
        "gpuRuns": gpu_runs(&load_all_results("gpu")?),
        "syncRuns": sync_runs,
        "loadRuns": load_runs,
        "latestDiff": latest_diff,
        "qualitySummary": {
            "builtInFunctions": UNIQUE_FUNCTIONS,
            "catalogEntries": FUNCTION_CATALOG.len(),
            "deterministicFunctions": DETERMINISTIC_FUNCTIONS,
            "simCiSeeds": quality.sim_ci_seeds,
            "simNightlySeeds": quality.sim_nightly_seeds,
            "compatGenerations": quality.compat_generations,
        },
    });
    let mut out = serde_json::to_string_pretty(&history)
        .map_err(|e| format!("cannot serialize benchmark history: {e}"))?;
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Archived, MILESTONES, RESULTS, SIM_INVARIANTS, TYPES, Value, browser_name, cpu_name,
        gpu_runs, kind_of, kinds_of_differences, number_in, opt_number, provenance, read,
        renderer_comparison, some, sort_by_age, suite_rank, util,
    };

    fn sorted(reports: Vec<(&str, Value)>) -> Vec<String> {
        let mut rows: Vec<Archived> = reports
            .into_iter()
            .map(|(name, report)| Archived {
                name: name.to_owned(),
                report,
            })
            .collect();
        sort_by_age(&mut rows);
        rows.into_iter().map(|row| row.name).collect()
    }

    #[test]
    fn every_suite_of_a_milestone_ranks_with_it() {
        for kind in ["engine", "startup", "render", "sync", "load", "diff", "gpu"] {
            let ranks: Vec<usize> = MILESTONES
                .iter()
                .map(|milestone| suite_rank(&json!({ "suite": format!("{milestone}-{kind}") })))
                .collect();
            assert_eq!(ranks, [1, 2, 3, 4, 5, 6, 7, 8], "{kind}");
        }
        for unknown in [
            json!({}),
            json!({ "suite": "quick" }),
            json!({ "suite": "m6x-gpu" }),
        ] {
            assert_eq!(suite_rank(&unknown), 0, "{unknown}");
        }
    }

    #[test]
    fn on_one_day_the_later_milestone_counts() {
        let of = |commit: &str, suite: &str| json!({ "date": "2026-11-25", "commit": commit, "suite": suite });
        assert_eq!(
            sorted(vec![
                ("2026-11-25-aaaaaaa-engine.json", of("aaaaaaa", "m6-engine")),
                (
                    "2026-11-25-fffffff-engine.json",
                    of("fffffff", "m3b-engine")
                ),
            ]),
            [
                "2026-11-25-fffffff-engine.json",
                "2026-11-25-aaaaaaa-engine.json"
            ]
        );
        // The day goes first.
        assert_eq!(
            sorted(vec![
                (
                    "b",
                    json!({ "date": "2026-11-26", "commit": "b", "suite": "m3b-engine" })
                ),
                ("a", of("a", "m6-engine")),
            ]),
            ["a", "b"]
        );
    }

    #[test]
    fn a_startup_report_without_a_suite_is_older_than_one_with_it() {
        let plain = |time: &str, commit: &str| json!({ "date": format!("2026-11-24T{time}Z"), "commit": commit, "spike": "s9" });
        assert_eq!(
            sorted(vec![
                (
                    "a-startup",
                    json!({ "date": "2026-11-24T08:00:00Z", "commit": "a", "suite": "m6-startup" })
                ),
                ("b-startup", plain("22:00:00", "b")),
                ("c-startup", plain("10:00:00", "c")),
            ]),
            ["c-startup", "b-startup", "a-startup"]
        );
    }

    #[test]
    fn the_suites_of_a_milestone_stand_in_the_order_they_were_made() {
        let of = |date: &str, suite: &str| json!({ "date": date, "commit": "c", "suite": suite });
        assert_eq!(
            sorted(vec![
                ("gpu-later", of("2026-11-26T12:00:00Z", "m6-gpu")),
                ("gpu-earlier", of("2026-11-26T09:00:00Z", "m6-gpu")),
                ("m4-of-the-day", of("2026-11-26T23:00:00Z", "m4-gpu")),
                (
                    "gpu-of-the-day-before",
                    of("2026-11-25T23:00:00Z", "m6-gpu")
                ),
            ]),
            [
                "gpu-of-the-day-before",
                "m4-of-the-day",
                "gpu-earlier",
                "gpu-later"
            ]
        );
    }

    #[test]
    fn kinds_of_reports_are_told_apart_by_the_whole_of_their_names() {
        assert_eq!(kind_of("2026-11-26-abc1234-gpu.json"), Some("gpu"));
        assert_eq!(kind_of("2026-11-26-abc1234-ab.json"), Some("ab"));
        assert_eq!(kind_of("2026-11-26-abc1234-gpu.json.bak"), None);
        assert_eq!(kind_of("gpu.json"), None);
        // What nothing reads would be there for nothing, but for comparisons
        // of two commits, which are no measurement of one.
        for entry in std::fs::read_dir(util::root().join(RESULTS)).expect("the results") {
            let name = entry.expect("a report").file_name();
            let kind = name.to_str().and_then(kind_of);
            assert!(
                matches!(
                    kind,
                    Some("engine" | "startup" | "render" | "gpu" | "sync" | "load" | "diff" | "ab")
                ),
                "{name:?}"
            );
        }
    }

    /// A report with a run in Chrome and one on a main thread that is held back.
    fn gpu_report() -> Archived {
        let app = |renderer: &str, work: f64, late: f64| {
            json!({
                "renderer": renderer, "width": 2560, "height": 1300,
                "cells": { "p50": 16_650 }, "frames": 1920, "placeholderFrames": 0,
                "work": { "p50": work, "p99": work * 2.0, "max": work * 3.0 },
                "interval": { "p50": 4.167, "p99": 4.5 }, "shareOver60Hz": late,
            })
        };
        let alone = json!({
            "size": "10k", "motion": "jump", "content": "repeating", "renderer": "webgpu",
            "cells": { "p50": 11_725 }, "frames": 360,
            "work": { "p50": 9.123, "p99": 11.0, "max": 12.0 },
            "interval": { "p50": 11.5, "p99": 16.0 }, "shareOver60Hz": 0.012_345,
        });
        let run = |slowdown: u32| {
            json!({
                "date": "2026-11-26T10:00:00Z", "commit": "abc1234", "uncommittedChanges": false,
                "affinity": "FFFF", "slowdown": slowdown, "dpr": 1, "refreshHz": 240,
                "machine": { "cpu": "13th Gen Intel(R) Core(TM) i9-13980HX" },
                "browser": { "name": "chrome", "version": "158.0", "headless": false },
                "adapter": { "vendor": "nvidia" },
                "product": [app("webgpu", 1.25, 0.0), app("canvas2d", 20.0, 0.987_65)],
                "synthetic": [alone],
            })
        };
        Archived {
            name: String::from("2026-11-26-abc1234-gpu.json"),
            report: json!({ "suite": "m6-gpu", "runs": [run(4), run(1)] }),
        }
    }

    #[test]
    fn the_comparison_of_the_renderers_is_of_the_run_at_full_speed() {
        let text = renderer_comparison(&gpu_report()).expect("a comparison");
        for line in [
            r#"report: "bench/results/2026-11-26-abc1234-gpu.json","#,
            r#"commit: "abc1234","#,
            r#"browser: "Chrome 158.0","#,
            "headless: false,",
            r#"affinity: "FFFF","#,
            "visibleCells: 16650,\n  width: 2560,\n  height: 1300,\n  refreshHz: 240,",
            r#"{ renderer: "canvas2d", workP50Ms: 20.00, workP99Ms: 40.00, lateFrames: 0.9877 },"#,
            r#"{ renderer: "webgpu", workP50Ms: 1.25, workP99Ms: 2.50, lateFrames: 0.0000 },"#,
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }

    #[test]
    fn there_is_no_comparison_without_such_a_run_of_both() {
        let mut slow = gpu_report();
        slow.report["runs"][1]["slowdown"] = json!(2);
        let mut dense = gpu_report();
        dense.report["runs"][1]["dpr"] = json!(1.5);
        let mut firefox = gpu_report();
        firefox.report["runs"][1]["browser"]["name"] = json!("firefox");
        let mut one = gpu_report();
        one.report["runs"][1]["product"][0]["renderer"] = json!("other");
        let mut part = gpu_report();
        part.report["runs"][1]["product"] = json!([]);
        let nothing = Archived {
            name: String::new(),
            report: json!({}),
        };
        for report in [slow, dense, firefox, one, part, nothing] {
            assert_eq!(renderer_comparison(&report), None, "{}", report.report);
        }
    }

    #[test]
    fn the_dashboard_has_every_run_with_its_conditions() {
        let runs = gpu_runs(&[gpu_report()]);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["slowdown"], 4);
        let run = &runs[1];
        for (key, value) in [
            ("report", json!("bench/results/2026-11-26-abc1234-gpu.json")),
            ("date", json!("2026-11-26")),
            ("commit", json!("abc1234")),
            ("suite", json!("m6-gpu")),
            ("cpu", json!("Intel Core i9-13980HX")),
            ("browser", json!("Chrome 158.0")),
            ("headless", json!(false)),
            ("refreshHz", json!(240)),
            ("adapter", json!("nvidia")),
        ] {
            assert_eq!(run[key], value, "{key}");
        }
        let results = run["results"].as_array().expect("results");
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[1],
            json!({
                "part": "app", "size": null, "motion": "scroll", "content": "distinct",
                "renderer": "canvas2d", "cells": 16_650, "frames": 1920,
                "workP50Ms": 20.0, "workP99Ms": 40.0, "workMaxMs": 60.0,
                "intervalP50Ms": 4.17, "intervalP99Ms": 4.5, "shareOver60Hz": 0.9877,
                "placeholderFrames": 0,
            })
        );
        assert_eq!(
            results[2],
            json!({
                "part": "renderer", "size": "10k", "motion": "jump", "content": "repeating",
                "renderer": "webgpu", "cells": 11_725, "frames": 360,
                "workP50Ms": 9.12, "workP99Ms": 11.0, "workMaxMs": 12.0,
                "intervalP50Ms": 11.5, "intervalP99Ms": 16.0, "shareOver60Hz": 0.0123,
                "placeholderFrames": null,
            })
        );
        assert_eq!(gpu_runs(&[]), Vec::<Value>::new());
    }

    #[test]
    fn what_the_dashboard_has_of_a_run_is_what_its_types_say() {
        let fields = |name: &str| -> Vec<&str> {
            let (_, rest) = TYPES
                .split_once(&format!("interface {name} {{"))
                .expect(name);
            let (body, _) = rest.split_once("\n}").expect(name);
            body.lines()
                .filter_map(|line| line.trim().strip_prefix("readonly ")?.split_once(':'))
                .map(|(field, _)| field)
                .collect()
        };
        let keys = |value: &Value| -> Vec<String> {
            value
                .as_object()
                .into_iter()
                .flat_map(serde_json::Map::keys)
                .cloned()
                .collect()
        };
        let sorted = |mut names: Vec<&str>| {
            names.sort_unstable();
            names.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        let runs = gpu_runs(&[gpu_report()]);
        assert_eq!(keys(&runs[0]), sorted(fields("GpuRun")));
        assert_eq!(keys(&runs[0]["results"][0]), sorted(fields("GpuResult")));
    }

    #[test]
    fn what_a_report_does_not_say_is_null() {
        assert_eq!(provenance(None), "null");
        assert_eq!(opt_number(None, 2), "null as number | null");
        assert_eq!(opt_number(Some(1.005), 1), "1.0");
        let text = provenance(Some(&Archived {
            name: String::from("2026-11-21-8a858fd-load.json"),
            report: json!({ "date": "2026-11-21", "commit": "8a858fd" }),
        }));
        for line in [
            r#"report: "bench/results/2026-11-21-8a858fd-load.json","#,
            r#"commit: "8a858fd","#,
            "uncommittedChanges: null,",
            "cpu: null,",
            "browser: null,",
            "affinity: null,",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }

    #[test]
    fn a_report_names_its_machine_in_one_of_several_places() {
        let engine = provenance(Some(&Archived {
            name: String::from("engine.json"),
            report: json!({
                "date": "2026-11-24", "commit": "c", "affinity": "FFFF", "uncommittedChanges": false,
                "wasmChrome": {
                    "machine": { "cpus": "13th Gen Intel(R) Core(TM) i9-13980HX", "totalMemGiB": 31.7 },
                    "browser": { "name": "chrome", "version": "158.0" }
                }
            }),
        }));
        for line in [
            r#"cpu: "Intel Core i9-13980HX","#,
            "memoryGb: 32,",
            r#"browser: "Chrome 158.0","#,
            "headless: null,",
            r#"affinity: "FFFF","#,
            "uncommittedChanges: false,",
        ] {
            assert!(engine.contains(line), "{line} in {engine}");
        }
        let startup = provenance(Some(&Archived {
            name: String::from("startup.json"),
            report: json!({
                "date": "2026-11-24T10:13:32Z", "commit": "c",
                "options": { "affinity": "FFFF" },
                "machine": { "cpu": "Apple M9", "logicalProcessors": 10, "os": "Darwin" },
                "browser": { "name": "firefox", "version": "160.0", "headless": true }
            }),
        }));
        for line in [
            r#"date: "2026-11-24","#,
            r#"cpu: "Apple M9","#,
            "logicalProcessors: 10,",
            r#"os: "Darwin","#,
            r#"browser: "Firefox 160.0","#,
            "headless: true,",
            r#"affinity: "FFFF","#,
        ] {
            assert!(startup.contains(line), "{line} in {startup}");
        }
        assert_eq!(cpu_name("13th Gen"), "13th Gen");
        assert_eq!(browser_name(&json!({ "name": "chrome" })), None);
    }

    #[test]
    fn numbers_are_read_off_what_they_configure() {
        assert_eq!(
            number_in("run: sim --seeds 5000 --out x", "--seeds "),
            Some(5000)
        );
        assert_eq!(number_in("run: sim --seeds many", "--seeds "), None);
        assert_eq!(number_in("run: sim", "--seeds "), None);
    }

    #[test]
    fn nothing_found_is_not_a_number() {
        assert_eq!(some("cases", 3), Ok(3));
        assert!(some("cases", 0).is_err());
    }

    #[test]
    fn a_reason_that_several_functions_have_is_one_kind() {
        let diff = json!({ "clusters": [
            { "category": "LN", "disposition": "err502" },
            { "category": "SQRT", "disposition": "err502" },
            { "category": "TYPE", "disposition": "array_arg" },
            { "category": "TYPE", "disposition": "returns_8" },
        ] });
        assert_eq!(kinds_of_differences(&diff), Some(3));
        assert_eq!(kinds_of_differences(&json!({})), None);
    }

    #[test]
    fn the_simulation_numbers_as_many_invariants_as_the_page_says() {
        let source = read("crates/sim/src/world.rs").expect("the simulation's source");
        let highest = source
            .match_indices("Invariant #")
            .filter_map(|(at, text)| number_in(&source[at..], text))
            .max();
        assert_eq!(highest, Some(SIM_INVARIANTS));
    }
}

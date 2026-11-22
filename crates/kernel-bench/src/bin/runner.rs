//! Native CLI runner for `cargo xtask diff`, `cargo xtask fuzz` and
//! `cargo xtask bench` (`13-m1a.md` 7.2, 7.3 & 7.4).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use reticula_kernel_bench::bench_workloads::run_native_suite;
use reticula_kernel_bench::diff::run_differential;
use reticula_kernel_bench::fuzz_runner::{FUZZ_TARGETS, run_fuzz_campaign};
use reticula_kernel_bench::lo_diff::{
    DEFAULT_LIBREOFFICE_IMAGE, LibreOfficeDiffConfig, run_libreoffice_differential,
};
use reticula_kernel_bench::sync_bench::run_b9;

fn pin_current_process(mask: &str) {
    assert!(
        !mask.is_empty() && mask.chars().all(|c| c.is_ascii_hexdigit()),
        "invalid hex affinity mask `{mask}`"
    );
    let pid = std::process::id();
    let status = if cfg!(windows) {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).ProcessorAffinity = 0x{mask}"),
            ])
            .status()
    } else {
        Command::new("taskset")
            .args(["--all-tasks", "--pid", mask, &pid.to_string()])
            .status()
    };
    let ok = status.map(|s| s.success()).unwrap_or(false);
    assert!(ok, "failed to pin process {pid} to affinity mask 0x{mask}");
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some((sub, rest)) = args.split_first() else {
        eprintln!("usage: runner <diff|fuzz|native-bench|sync-bench|hdb-bench> [options]");
        std::process::exit(2);
    };

    match sub.as_str() {
        "diff" => run_diff_cmd(rest),
        "fuzz" => run_fuzz_cmd(rest),
        "native-bench" => run_bench_cmd(rest),
        "sync-bench" => run_sync_bench_cmd(rest),
        "hdb-bench" => run_hdb_cmd(rest),
        other => {
            eprintln!("unknown subcommand `{other}`");
            std::process::exit(2);
        }
    }
}

fn run_diff_cmd(args: &[String]) {
    let mut engine = String::from("rust");
    let mut count = 10_000_usize;
    let mut quota_per_fn = 250_usize;
    let mut seed = 0xD1FF_2026_u64;
    let mut image = String::from(DEFAULT_LIBREOFFICE_IMAGE);
    let mut skip_image = false;
    let mut dump_xlsx: Option<PathBuf> = None;
    let mut commit = String::from("local");
    let mut date = String::from("2026-11-22");
    let mut out: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--engine" => {
                i += 1;
                engine = args[i].clone();
            }
            "--count" => {
                i += 1;
                count = args[i].parse().expect("valid --count");
            }
            "--quota-per-fn" => {
                i += 1;
                quota_per_fn = args[i].parse().expect("valid --quota-per-fn");
            }
            "--seed" => {
                i += 1;
                seed = args[i].parse().expect("valid --seed");
            }
            "--image" => {
                i += 1;
                image = args[i].clone();
            }
            "--skip-image" => {
                skip_image = true;
            }
            "--dump-xlsx" => {
                i += 1;
                dump_xlsx = Some(PathBuf::from(&args[i]));
            }
            "--commit" => {
                i += 1;
                commit = args[i].clone();
            }
            "--date" => {
                i += 1;
                date = args[i].clone();
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            flag => panic!("unknown diff flag `{flag}`"),
        }
        i += 1;
    }

    let run_rust = match engine.as_str() {
        "rust" | "all" => true,
        "libreoffice" => false,
        other => panic!("unknown --engine `{other}` (expected rust, libreoffice, or all)"),
    };
    let run_lo = matches!(engine.as_str(), "libreoffice" | "all");

    let mut rust_report = None;
    if run_rust {
        let report = run_differential(seed, count);
        println!(
            "Differential report (seed={:#x}, total={} formulas, non-error={:.1}%):",
            report.seed,
            report.total_formulas,
            report.non_error_ratio * 100.0
        );
        println!(
            "  ironcalc_base    exact/15-digit matches: {} / {}",
            report.ironcalc_matches, report.total_formulas
        );
        println!(
            "  formualizer-eval exact/15-digit matches: {} / {}",
            report.formualizer_matches, report.total_formulas
        );
        for c in &report.clusters {
            println!(
                "  cluster [{}] {} ({}) x{}: `{}` -> reticula={:?}, ref={:?}",
                c.reference_engine,
                c.category,
                c.disposition,
                c.count,
                c.sample_formula,
                c.reticula_result,
                c.reference_result
            );
        }

        if report.unclassified_count() > 0 {
            eprintln!(
                "error: {} unclassified Rust differential divergences!",
                report.unclassified_count()
            );
            std::process::exit(1);
        }

        if !run_lo {
            if let Some(path) = &out {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).expect("mkdir");
                }
                fs::write(
                    path,
                    format!("{}\n", serde_json::to_string_pretty(&report).unwrap()),
                )
                .expect("write diff report");
            }
            return;
        }
        rust_report = Some(report);
    }

    if run_lo {
        let lo_cfg = LibreOfficeDiffConfig {
            seed,
            quota_per_fn,
            image,
            skip_image_build: skip_image,
            dump_xlsx_dir: dump_xlsx,
            commit,
            date,
            rust_reference: rust_report,
        };
        let lo_report = match run_libreoffice_differential(&lo_cfg) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: LibreOffice differential failed: {e}");
                std::process::exit(1);
            }
        };

        println!(
            "LibreOffice differential ({}, image={}, formulas={}, cells={}, funcs={}/{} unique):",
            lo_report.libreoffice_version,
            lo_report.container_image,
            lo_report.total_formulas,
            lo_report.total_cells_compared,
            lo_report.deterministic_functions_covered,
            lo_report.unique_functions_covered
        );
        println!(
            "  non-error: {} / {} ({:.2}%)",
            lo_report.non_error_count,
            lo_report.total_formulas,
            lo_report.non_error_ratio * 100.0
        );
        println!(
            "  agreement: {} / {} ({:.2}%), unclassified={}",
            lo_report.matches,
            lo_report.total_formulas,
            lo_report.agreement_ratio * 100.0,
            lo_report.unclassified_count
        );
        for fam in &lo_report.by_family {
            println!(
                "  family {:<14} total={:<6} non_err={:<6} agree={:.2}% unclassified={}",
                fam.family,
                fam.total,
                fam.non_error,
                fam.agreement_ratio * 100.0,
                fam.unclassified
            );
        }
        for c in &lo_report.clusters {
            println!(
                "  cluster [{}] {} ({}) x{}: `{}` -> reticula={:?}, lo={:?} ({})",
                c.family,
                c.category,
                c.disposition,
                c.count,
                c.sample_formula,
                c.reticula_result,
                c.libreoffice_result,
                c.reason
            );
        }

        if let Some(path) = &out {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(
                path,
                format!("{}\n", serde_json::to_string_pretty(&lo_report).unwrap()),
            )
            .expect("write LibreOffice diff report");
        }

        if quota_per_fn >= 250 && lo_report.total_formulas < 50_000 {
            eprintln!(
                "error: expected >= 50,000 formulas for LibreOffice differential, got {}",
                lo_report.total_formulas
            );
            std::process::exit(1);
        }
        if lo_report.min_samples_per_function < quota_per_fn {
            eprintln!(
                "error: minimum samples per deterministic function ({}) < quota_per_fn ({quota_per_fn})",
                lo_report.min_samples_per_function
            );
            std::process::exit(1);
        }
        if lo_report.non_error_ratio < 0.80 {
            eprintln!(
                "error: non-error ratio {:.2}% < 80.0% gate",
                lo_report.non_error_ratio * 100.0
            );
            std::process::exit(1);
        }
        if lo_report.agreement_ratio < 0.995 {
            eprintln!(
                "error: LibreOffice agreement ratio {:.2}% < 99.5% gate",
                lo_report.agreement_ratio * 100.0
            );
            std::process::exit(1);
        }
        if lo_report.unclassified_count > 0 {
            eprintln!(
                "error: {} unclassified LibreOffice divergence(s)!",
                lo_report.unclassified_count
            );
            std::process::exit(1);
        }
    }
}

fn run_fuzz_cmd(args: &[String]) {
    let mut cpu_seconds = 3_600_u64;
    let mut threads = 16_usize;
    let mut affinity: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut only: Option<usize> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--target" => {
                i += 1;
                only = Some(
                    FUZZ_TARGETS
                        .iter()
                        .position(|name| *name == args[i])
                        .unwrap_or_else(|| panic!("unknown fuzz target `{}`", args[i])),
                );
            }
            "--cpu-seconds" => {
                i += 1;
                cpu_seconds = args[i].parse().expect("valid --cpu-seconds");
            }
            "--threads" => {
                i += 1;
                threads = args[i].parse().expect("valid --threads");
            }
            "--affinity" => {
                i += 1;
                affinity = Some(args[i].clone());
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            flag => panic!("unknown fuzz flag `{flag}`"),
        }
        i += 1;
    }

    if let Some(mask) = &affinity {
        pin_current_process(mask);
    }

    let report = run_fuzz_campaign(cpu_seconds, threads, only);
    println!(
        "Fuzzing complete: {:.1} wall s x {} threads = {:.1} CPU-s ({:.2} CPU-hours)",
        report.wall_seconds,
        report.threads,
        report.cpu_seconds,
        report.cpu_seconds / 3600.0
    );
    println!(
        "  executions: parser={}, snapshot={}, wal={}, ops={}, eval={}, sync={} (total={})",
        report.parser_execs,
        report.snapshot_execs,
        report.wal_execs,
        report.ops_execs,
        report.eval_execs,
        report.sync_execs,
        report.parser_execs
            + report.snapshot_execs
            + report.wal_execs
            + report.ops_execs
            + report.eval_execs
            + report.sync_execs
    );

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&report).unwrap()),
        )
        .expect("write fuzz report");
    }
}

fn run_bench_cmd(args: &[String]) {
    let mut quick = false;
    let mut affinity: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--quick" => quick = true,
            "--affinity" => {
                i += 1;
                affinity = Some(args[i].clone());
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            flag => panic!("unknown native-bench flag `{flag}`"),
        }
        i += 1;
    }

    if let Some(mask) = &affinity {
        pin_current_process(mask);
    }

    let suite = run_native_suite(quick);
    let json = serde_json::to_string_pretty(&suite).unwrap();
    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, format!("{json}\n")).expect("write native bench");
    }
    println!("{json}");
}

/// `runner sync-bench [--quick] [--affinity <mask>] [--out <file>]`: B9.
fn run_sync_bench_cmd(args: &[String]) {
    let mut quick = false;
    let mut affinity: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--quick" => quick = true,
            "--affinity" => {
                i += 1;
                affinity = Some(args[i].clone());
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            flag => panic!("unknown sync-bench flag `{flag}`"),
        }
        i += 1;
    }
    if let Some(mask) = &affinity {
        pin_current_process(mask);
    }
    let report = run_b9(quick);
    let json = serde_json::to_string_pretty(&report).unwrap();
    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(
            &path,
            format!(
                "{json}
"
            ),
        )
        .expect("write sync bench");
    }
    println!("{json}");
}

fn parse_csv_line<'a>(line: &'a str, out: &mut Vec<std::borrow::Cow<'a, str>>) {
    out.clear();
    let bytes = line.trim_end_matches(['\r', '\n']).as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        if i == bytes.len() {
            if !bytes.is_empty() && bytes[bytes.len() - 1] == b',' {
                out.push(std::borrow::Cow::Borrowed(""));
            }
            break;
        }
        if bytes[i] == b'"' {
            i += 1;
            let mut s = String::new();
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        s.push('"');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    let ch = line[i..].chars().next().unwrap();
                    s.push(ch);
                    i += ch.len_utf8();
                }
            }
            out.push(std::borrow::Cow::Owned(s));
            if i < bytes.len() && bytes[i] == b',' {
                i += 1;
                if i == bytes.len() {
                    out.push(std::borrow::Cow::Borrowed(""));
                    break;
                }
            }
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            out.push(std::borrow::Cow::Borrowed(&line[start..i]));
            if i < bytes.len() && bytes[i] == b',' {
                i += 1;
                if i == bytes.len() {
                    out.push(std::borrow::Cow::Borrowed(""));
                    break;
                }
            } else {
                break;
            }
        }
    }
}

fn run_hdb_cmd(args: &[String]) {
    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::input::{InferredInput, infer_input};
    use reticula_core::ops::{ScalarValue, Workbook};
    use reticula_core::sheet::Sheet;
    use reticula_core::value::Value;
    use reticula_formula::binder::BoundFormula;
    use reticula_formula::engine::FormulaEngine;
    use std::io::Write as _;
    use std::time::Instant;

    let mut dir = PathBuf::from("target/hdb");
    let mut affinity: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                dir = PathBuf::from(&args[i]);
            }
            "--affinity" => {
                i += 1;
                affinity = Some(args[i].clone());
            }
            other => panic!("unknown hdb-bench flag `{other}`"),
        }
        i += 1;
    }
    if let Some(mask) = &affinity {
        pin_current_process(mask);
    }

    // Chronological order from 1990 to present (`data.gov.sg` Collection 189).
    let files = [
        ("d_ebc5ab87086db484f88045b47411ebc5.csv", 10_usize), // 1990 - 1999
        ("d_43f493c6c50d54243cc1eab0df142d6a.csv", 10_usize), // 2000 - Feb 2012
        ("d_2d5ff9ea31397b66239f245f57751537.csv", 10_usize), // Mar 2012 - Dec 2014
        ("d_ea9ed51da2787afaf8e51f827c304208.csv", 11_usize), // Jan 2015 - Dec 2016
        ("d_8b84c4ee58e3cfc0ece0d773c8ca6abc.csv", 11_usize), // Jan 2017 onwards
    ];

    let header_cols = [
        "month",
        "town",
        "flat_type",
        "block",
        "street_name",
        "storey_range",
        "floor_area_sqm",
        "flat_model",
        "lease_commence_date",
        "remaining_lease",
        "resale_price",
    ];

    let mut raw_texts = Vec::new();
    let mut total_data_rows: u32 = 0;
    for (fname, _) in files {
        let content = fs::read_to_string(dir.join(fname)).expect("read HDB CSV");
        let rows_in_file = content.lines().count().saturating_sub(1);
        total_data_rows += u32::try_from(rows_in_file).unwrap();
        raw_texts.push((fname, content));
    }
    let total_rows = total_data_rows + 1;

    let combined_csv_path = dir.join("combined.csv");
    let mut csv_out = std::io::BufWriter::new(fs::File::create(&combined_csv_path).unwrap());
    writeln!(csv_out, "{}", header_cols.join(",")).unwrap();

    let t_ingest = Instant::now();
    let mut sheet = Sheet::with_axes(SheetId(0), String::from("HDB Resale"), total_rows, 11);
    for (c, title) in header_cols.iter().enumerate() {
        let sid = sheet.add_string(title).unwrap();
        sheet.set_value_by_id(RowId(0), ColId(c as u64), Value::Text(sid));
    }

    let mut row_idx: u32 = 1;
    let mut fields = Vec::with_capacity(12);
    let mut text_cells: u64 = 11;
    let mut num_cells: u64 = 0;

    for ((_, content), (_, expected_cols)) in raw_texts.iter().zip(files.iter()) {
        for line in content.lines().skip(1) {
            if line.trim().is_empty() {
                continue;
            }
            parse_csv_line(line, &mut fields);
            if *expected_cols == 10 && fields.len() == 10 {
                // Insert empty `remaining_lease` before `resale_price`.
                writeln!(
                    csv_out,
                    "{},{},{},{},{},{},{},{},{},,{}",
                    fields[0],
                    fields[1],
                    fields[2],
                    fields[3],
                    fields[4],
                    fields[5],
                    fields[6],
                    fields[7],
                    fields[8],
                    fields[9]
                )
                .unwrap();
            } else {
                writeln!(csv_out, "{line}").unwrap();
            }

            for col_out in 0..11_usize {
                let raw_val: &str = if *expected_cols == 10 {
                    if col_out < 9 {
                        fields.get(col_out).map_or("", |s| s.as_ref())
                    } else if col_out == 9 {
                        ""
                    } else {
                        fields.get(9).map_or("", |s| s.as_ref())
                    }
                } else {
                    fields.get(col_out).map_or("", |s| s.as_ref())
                };
                if raw_val.is_empty() {
                    continue;
                }
                // Columns 0..=5 and 7 (`month`, `town`, `flat_type`, `block`, `street_name`,
                // `storey_range`, `flat_model`) include codes like `block = "1"` or `"102A"`,
                // while 6, 8, 9, 10 are numeric or mixed (`remaining_lease`). Use E30 inference!
                let val = match infer_input(raw_val) {
                    Ok(InferredInput::Scalar {
                        value: ScalarValue::Number(n),
                        ..
                    }) => {
                        num_cells += 1;
                        Value::number(n)
                    }
                    _ => {
                        text_cells += 1;
                        let sid = sheet.add_string(raw_val).unwrap();
                        Value::Text(sid)
                    }
                };
                sheet.set_value_by_id(RowId(u64::from(row_idx)), ColId(col_out as u64), val);
            }
            row_idx += 1;
        }
    }
    csv_out.flush().unwrap();
    drop(csv_out);
    let ingest_ms = t_ingest.elapsed().as_secs_f64() * 1000.0;

    let unique_strings = sheet.strings().len();
    let wb: Workbook<BoundFormula> = Workbook::from_sheet(sheet, None);
    let estimated_mem_bytes = wb.estimated_bytes();
    let engine = FormulaEngine::from_workbook_clean(wb);

    let mut encode_times = Vec::new();
    let mut snap_bytes = Vec::new();
    for turn in 0..5 {
        let t0 = Instant::now();
        let encoded = engine.encode_snapshot().expect("encode HDB snapshot");
        encode_times.push(t0.elapsed().as_secs_f64() * 1000.0);
        if turn == 0 {
            snap_bytes = encoded;
        }
    }
    encode_times.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let snap_path = dir.join("combined.rtclsnap");
    fs::write(&snap_path, &snap_bytes).unwrap();

    let mut decode_times = Vec::new();
    for _ in 0..5 {
        let t0 = Instant::now();
        let (decoded, _) =
            FormulaEngine::decode_snapshot(&snap_bytes).expect("decode HDB snapshot");
        assert_eq!(
            decoded.workbook().sheet(SheetId(0)).unwrap().rows(),
            row_idx
        );
        decode_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    decode_times.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let summary = serde_json::json!({
        "rows_with_header": row_idx,
        "data_rows": row_idx - 1,
        "cols": 11,
        "numeric_cells": num_cells,
        "text_cells": text_cells,
        "unique_interned_strings": unique_strings,
        "estimated_workbook_bytes": estimated_mem_bytes,
        "combined_csv_bytes": fs::metadata(&combined_csv_path).unwrap().len(),
        "rtclsnap_bytes": snap_bytes.len(),
        "native_csv_ingest_ms": (ingest_ms * 10.0).round() / 10.0,
        "native_encode_p50_ms": (encode_times[2] * 10.0).round() / 10.0,
        "native_decode_p50_ms": (decode_times[2] * 10.0).round() / 10.0,
    });
    fs::write(
        dir.join("native-hdb-summary.json"),
        format!("{}\n", serde_json::to_string_pretty(&summary).unwrap()),
    )
    .unwrap();
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}

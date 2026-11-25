//! Files that other programs wrote, compared with what another of them reads.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use reticula_core::rng::Rng;

use super::tests::{cells, import, rounds};
use super::{Options, SkipReason};
use crate::tests::Model;
use crate::text;
use crate::types::{CellValue, ColumnType};

macro_rules! sample {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!(
                "../../../../tests/conformance/parquet/",
                $name,
                ".parquet"
            ))
            .as_slice(),
        )
    };
}

macro_rules! values {
    ($name:literal) => {
        include_str!(concat!(
            "../../../../tests/conformance/parquet/",
            $name,
            ".tsv"
        ))
    };
}

const INT: ColumnType = ColumnType::Integer;
const FLOAT: ColumnType = ColumnType::Float;
const TEXT: ColumnType = ColumnType::Text;
const DATE: ColumnType = ColumnType::Date;
const TIME: ColumnType = ColumnType::DateTime;
const TRUTH: ColumnType = ColumnType::Boolean;

const fn decimal(scale: u8) -> ColumnType {
    ColumnType::Decimal { scale, fixed: true }
}

/// What the cells show, the way the values are written.
fn shown(kind: ColumnType, cell: &Model) -> String {
    let mut out = String::new();
    match cell {
        Model::Blank => {}
        Model::Int(value) => text::write_value(kind, CellValue::Int(*value), &mut out),
        Model::Float(value) => out = value.to_string(),
        Model::Text(text) => {
            for char in text.chars() {
                match char {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    other => out.push(other),
                }
            }
        }
    }
    out
}

/// Imports files and compares them all with the same values.
fn compare(
    files: &[(&str, &[u8])],
    values: &str,
    kinds: &[ColumnType],
    skipped: &[(&str, SkipReason)],
) {
    let mut lines = values
        .lines()
        .map(|line| line.split('\t').collect::<Vec<_>>());
    let header = lines.next().unwrap();
    let rows: Vec<Vec<&str>> = lines.collect();
    assert_eq!(header.len(), kinds.len());
    for (name, file) in files {
        let imported = import(file, &Options::default()).unwrap();
        let left_out: Vec<(&str, SkipReason)> = imported
            .report
            .skipped
            .iter()
            .map(|column| (column.name.as_str(), column.reason))
            .collect();
        assert_eq!(left_out, skipped, "{name}");
        let columns = cells(&imported.table);
        let names: Vec<&str> = columns.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, header, "{name}");
        for (index, (column, kind, cells)) in columns.iter().enumerate() {
            assert_eq!(*kind, kinds[index], "{name}, {column}");
            assert_eq!(cells.len(), rows.len(), "{name}");
            for (row, (cell, expected)) in cells.iter().zip(&rows).enumerate() {
                let expected = expected[index];
                let same = match cell {
                    Model::Float(value) => expected.parse::<f64>().is_ok_and(|e| e == *value),
                    _ => shown(*kind, cell) == expected,
                };
                assert!(
                    same,
                    "{name}, {column}, row {row}: {cell:?}, not {expected}"
                );
            }
        }
    }
}

#[test]
fn every_type_that_arrow_writes() {
    let kinds = [
        TRUTH,
        INT,
        INT,
        INT,
        INT,
        INT,
        INT,
        INT,
        INT,
        FLOAT,
        FLOAT,
        decimal(2),
        decimal(10),
        TEXT,
        DATE,
        TIME,
        TIME,
        TIME,
        TEXT,
        TEXT,
        TEXT,
        TEXT,
        TEXT,
        TEXT,
        TEXT,
        INT,
    ];
    let files = [
        sample!("arrow-old"),
        sample!("arrow-snappy"),
        sample!("arrow-gzip"),
        sample!("arrow-zstd"),
        sample!("arrow-lz4"),
        sample!("arrow-checksums"),
    ];
    let values = values!("arrow-types");
    compare(&files, values, &kinds, &[]);
    // The same columns without any rows.
    let header = values.lines().next().unwrap();
    compare(&[sample!("arrow-empty")], header, &kinds, &[]);
}

#[test]
fn every_encoding_that_arrow_writes() {
    compare(
        &[sample!("arrow-encodings"), sample!("arrow-encodings-zstd")],
        values!("arrow-encodings"),
        &[INT, INT, TIME, FLOAT, FLOAT, TEXT, TEXT, TRUTH, FLOAT, TEXT],
        &[],
    );
    compare(
        &[sample!("arrow-split")],
        values!("arrow-split"),
        &[INT, INT, decimal(3), TEXT, FLOAT],
        &[],
    );
    compare(
        &[sample!("arrow-fallback")],
        values!("arrow-fallback"),
        &[TEXT],
        &[],
    );
}

#[test]
fn times_that_arrow_writes() {
    compare(
        &[sample!("arrow-int96")],
        values!("arrow-int96"),
        &[TIME],
        &[],
    );
    let (_, file) = sample!("arrow-nanos");
    compare(
        &[sample!("arrow-nanos")],
        values!("arrow-nanos"),
        &[TIME, TEXT],
        &[],
    );
    let report = import(file, &Options::default()).unwrap().report;
    assert!(report.columns[0].cut_nanos > 10);
    assert_eq!(report.columns[1].cut_nanos, 0);
}

#[test]
fn numbers_that_arrow_writes() {
    compare(
        &[sample!("arrow-decimals")],
        values!("arrow-decimals"),
        &[decimal(3), decimal(4), TEXT],
        &[],
    );
    compare(
        &[sample!("arrow-digits")],
        values!("arrow-digits"),
        &[TEXT],
        &[],
    );
    // One column has numbers that are too large, another bytes that are not
    // text.
    compare(
        &[sample!("arrow-unsigned")],
        values!("arrow-unsigned"),
        &[TEXT, TEXT],
        &[("bytes", SkipReason::NotText)],
    );
}

#[test]
fn nested_columns_are_left_out() {
    compare(
        &[sample!("arrow-nested")],
        values!("arrow-nested"),
        &[INT, TEXT],
        &[
            ("list", SkipReason::Nested),
            ("struct", SkipReason::Nested),
            ("map", SkipReason::Nested),
        ],
    );
    compare(
        &[sample!("duckdb-others")],
        values!("duckdb-others"),
        &[INT, TEXT],
        &[
            ("gap", SkipReason::Interval),
            ("list", SkipReason::Nested),
            ("struct", SkipReason::Nested),
            ("map", SkipReason::Nested),
            ("bytes", SkipReason::NotText),
        ],
    );
}

#[test]
fn damaged_samples_are_survived() {
    // Their pages are packed better than those of the other tests.
    let mut rng = Rng::from_seed(1);
    let files = [
        sample!("arrow-snappy"),
        sample!("arrow-gzip"),
        sample!("arrow-zstd"),
        sample!("arrow-lz4"),
        sample!("arrow-encodings-zstd"),
        sample!("arrow-fallback"),
        sample!("arrow-split"),
        sample!("duckdb-gzip"),
        sample!("duckdb-zstd"),
        sample!("duckdb-lz4_raw"),
        sample!("duckdb-v2"),
    ];
    for (_, file) in files {
        for _ in 0..rounds(25) {
            let mut damaged = file.to_vec();
            for _ in 0..=rng.next_below(3) {
                let at = rng.next_below(file.len() as u64) as usize;
                damaged[at] = rng.next_u64().to_le_bytes()[0];
            }
            if let Ok(imported) = import(&damaged, &Options::default()) {
                assert!(crate::tests::exercise(&imported.table).is_ok());
            }
        }
    }
}

#[test]
fn every_type_that_duckdb_writes() {
    let kinds = [
        TRUTH,
        INT,
        INT,
        INT,
        INT,
        INT,
        INT,
        INT,
        INT,
        FLOAT,
        FLOAT,
        decimal(1),
        decimal(3),
        decimal(6),
        TEXT,
        DATE,
        TIME,
        TIME,
        TIME,
        TIME,
        TIME,
        TEXT,
        TEXT,
        TEXT,
        TEXT,
        TEXT,
        INT,
    ];
    let files = [
        sample!("duckdb-snappy"),
        sample!("duckdb-gzip"),
        sample!("duckdb-zstd"),
        sample!("duckdb-lz4_raw"),
        sample!("duckdb-v2"),
    ];
    compare(&files, values!("duckdb-types"), &kinds, &[]);
}

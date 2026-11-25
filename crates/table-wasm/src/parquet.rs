//! Importing Parquet files.

use reticula_table::parquet::{Import, ParquetFile};

use crate::wire::{ImportReport, ImportedColumn, ParquetColumn, ParquetInfo};

/// What is in a file.
pub fn info(file: &ParquetFile) -> ParquetInfo {
    ParquetInfo {
        rows: file.rows(),
        columns: file
            .columns()
            .map(|column| ParquetColumn {
                name: String::from(column.name),
                kind: column.kind.into(),
                bytes: column.bytes,
            })
            .collect(),
        skipped: file.skipped().iter().map(Into::into).collect(),
    }
}

/// What became of a file, once the table is written.
pub fn report(import: &Import) -> ImportReport {
    let report = import.report();
    ImportReport {
        columns: report
            .columns
            .into_iter()
            .map(|column| ImportedColumn {
                name: column.name,
                kind: column.kind.into(),
                blanks: column.blanks,
                misfits: 0,
                cut_texts: column.cut_texts,
                cut_nanos: column.cut_nanos,
                not_finite: column.not_finite,
                out_of_range: column.out_of_range,
            })
            .collect(),
        skipped: report.skipped.iter().map(Into::into).collect(),
        ..ImportReport::default()
    }
}

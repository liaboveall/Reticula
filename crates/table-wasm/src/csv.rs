//! Importing text files.

use reticula_table::csv::{self, Import, Options};
use wasm_bindgen::prelude::*;

use crate::json::{Value, json};
use crate::wire::{CsvSniffed, CsvSniffedColumn, ImportReport, ImportedColumn, type_named};

/// How much of the start of a file is to be looked at.
#[must_use]
#[wasm_bindgen(js_name = csvHeadBytes)]
pub fn head_bytes() -> usize {
    csv::HEAD_BYTES
}

/// What `CsvOptions` say.
pub fn options(asked: &str) -> Options {
    let asked = Value::read(asked);
    let mut options = Options {
        delimiter: asked.of("delimiter").bytes().next(),
        header: match asked.of("header") {
            "" => None,
            header => Some(header == "true"),
        },
        blank_markers: asked.of("blankMarkers") == "true",
        kinds: Vec::new(),
    };
    for (_, column) in asked.field("types").items() {
        let kind = column.field("type");
        if let Ok(at) = column.of("column").parse()
            && let Some(kind) = type_named(kind.of("kind"), kind.of("scale").parse().unwrap_or(0))
        {
            options.kinds.push((at, kind));
        }
    }
    options
}

/// Looks at the start of a file, or at the whole of it.
#[must_use]
#[wasm_bindgen(js_name = csvSniff, unchecked_return_type = "Json<CsvSniffed>")]
pub fn sniff(head: &[u8], whole: bool, settings: &str) -> String {
    let found = csv::sniff(head, whole, &options(settings));
    let found = CsvSniffed {
        delimiter: char::from(found.delimiter).to_string(),
        header: found.header,
        columns: found
            .columns
            .into_iter()
            .map(|column| CsvSniffedColumn {
                name: column.name,
                kind: column.kind.map(Into::into),
            })
            .collect(),
        preview: found.preview,
        valid_utf8: found.valid_utf8,
        row_bytes: u32::try_from(found.row_bytes).unwrap_or(u32::MAX),
    };
    json(&found)
}

/// What became of a file, once the table is written.
pub fn report(import: &Import) -> Option<ImportReport> {
    import.report().map(|report| ImportReport {
        columns: report
            .columns
            .into_iter()
            .map(|column| ImportedColumn {
                name: column.name,
                kind: column.kind.into(),
                blanks: column.blanks,
                misfits: column.misfits,
                cut_texts: column.cut_texts,
                cut_nanos: 0,
                not_finite: 0,
                out_of_range: 0,
            })
            .collect(),
        skipped: Vec::new(),
        extra_fields: report.extra_fields,
        short_rows: report.short_rows,
        empty_lines: report.empty_lines,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use reticula_table::types::ColumnType;

    use super::*;

    #[test]
    fn options_are_what_they_say() {
        for nothing in ["{}", "", r#"{"types":[]}"#] {
            let found = options(nothing);
            assert_eq!((found.delimiter, found.header), (None, None));
            assert!(!found.blank_markers && found.kinds.is_empty());
        }
        let found = options(concat!(
            r#"{"delimiter":";","header":false,"blankMarkers":true,"types":["#,
            r#"{"column":2,"type":{"kind":"text"}},"#,
            r#"{"column":0,"type":{"kind":"decimal","scale":3}}]}"#
        ));
        assert_eq!((found.delimiter, found.header), (Some(b';'), Some(false)));
        assert!(found.blank_markers);
        let decimal = ColumnType::Decimal {
            scale: 3,
            fixed: false,
        };
        assert_eq!(found.kinds, [(2, ColumnType::Text), (0, decimal)]);
        // What is none of them.
        let found = options(concat!(
            r#"{"delimiter":"\t","header":true,"types":["#,
            r#"{"column":"x","type":{"kind":"text"}},"#,
            r#"{"column":1,"type":{"kind":"words"}},{"column":3}]}"#
        ));
        assert_eq!((found.delimiter, found.header), (Some(b'\t'), Some(true)));
        assert!(found.kinds.is_empty());
    }
}

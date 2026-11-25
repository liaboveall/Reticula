//! Cross-target determinism self-check of tables.
//!
//! Makes tables in three ways, asks them all that can be asked, and folds the
//! bytes of the files and the answers into one number. It has to be the same
//! on every target.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use reticula_core::digest::Digest64;
use reticula_core::rng::Rng;
use reticula_core::viewport::{PAGE_COLS, PAGE_ROWS, PageFrame};
use sha2::{Digest, Sha256};

use crate::calendar::DAY_MICROS;
use crate::csv::{self, HEAD_BYTES, Import, ImportError, Options, Report, Step};
use crate::query::{
    ColumnSummary, Condition, Figure, NumberOp, Progress, Session, SortKey, Spec, TimeOp, Value,
    ValueList,
};
use crate::reader::{Opened, Reader};
use crate::sample::Sample;
use crate::types::{CellValue, ColumnType};
use crate::writer::{ColumnSpec, TableWriter};

pub use reticula_core::determinism::to_hex;

/// How the work is cut, which changes nothing about what comes of it.
#[derive(Clone, Copy)]
struct Pieces {
    /// Cells that one step works on, about.
    cells: u32,
    /// Bytes of a file that are given at a time.
    bytes: usize,
}

/// Runs the self-check and returns its 64-bit digest.
#[must_use]
pub fn digest() -> u64 {
    // Pieces of text that end anywhere, also within a character.
    digest_in(Pieces {
        cells: 3_000,
        bytes: 40_961,
    })
}

/// What the app comes to with a text to import, and with the example it makes up:
/// SHA-256 of the files, and of what is copied out of an ordered view of each.
/// A browser is asked for the same (`tests/e2e/table-digest.ts`).
#[must_use]
pub fn app(text: &str) -> String {
    let mut example = Vec::new();
    if let Ok(mut sample) = Sample::new(70_000, 2026) {
        while sample.step(&mut example) == Ok(true) {}
    }
    let mut imported = Vec::new();
    // What went wrong shows in the file.
    let _ = import(
        &Options::default(),
        text.as_bytes(),
        HEAD_BYTES,
        &mut imported,
    );
    let lamps = spec(
        vec![(6, Condition::TextContains(String::from("LAMP")))],
        &[(6, true), (11, false)],
    );
    let words = spec(
        vec![(1, Condition::NotBlank)],
        &[(7, false), (3, true), (0, false)],
    );
    let mut out = String::new();
    for (name, file, view, copied) in [
        ("example", &example, lamps, 13),
        ("text", &imported, words, 7),
    ] {
        let _ = writeln!(out, "{name} {}", sha256(file));
        let cells = copy(file, view, copied);
        let _ = writeln!(out, "{name} view {}", sha256(cells.as_bytes()));
    }
    out
}

fn sha256(bytes: &[u8]) -> String {
    let mut hex = String::new();
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The first columns of a view of a table, as they are copied.
fn copy(file: &[u8], view: Spec, columns: usize) -> String {
    let mut out = String::new();
    let mut source = file;
    let length = u64::try_from(file.len()).unwrap_or(0);
    let Ok(Opened::Ready(reader)) = Reader::open(&mut source, length) else {
        return out;
    };
    let mut session = Session::new(*reader);
    if session.begin(view).is_err() {
        return out;
    }
    run(&mut Digest64::new(), &mut session, &mut source, 65_536);
    for position in 0..session.view_len() {
        out.push_str(if position == 0 { "" } else { "\n" });
        for column in 0..columns {
            out.push_str(if column == 0 { "" } else { "\t" });
            let text = session.cell_text(&mut source, position, column);
            out.push_str(&text.unwrap_or_default());
        }
    }
    out
}

/// A digest of what a table holds: names, types and every cell. `None` when not
/// all of it can be read.
#[must_use]
pub fn content_digest(file: &[u8]) -> Option<u64> {
    let mut source = file;
    let length = u64::try_from(file.len()).ok()?;
    let Ok(Opened::Ready(mut reader)) = Reader::open(&mut source, length) else {
        return None;
    };
    let mut d = Digest64::new();
    d.write_u32(reader.rows());
    let mut text = String::new();
    for column in 0..reader.columns().len() {
        let of = reader.columns().get(column)?;
        d.write_bytes(of.name.as_bytes());
        d.write_bytes(format!("{:?}", of.kind).as_bytes());
        for row in 0..reader.rows() {
            text.clear();
            let cell = reader.cell(&mut source, column, row, usize::MAX, &mut text);
            match cell.ok()? {
                CellValue::Blank => d.write_u64(0),
                CellValue::Int(value) => d.write_u64(value.cast_unsigned()),
                CellValue::Float(value) => d.write_f64(value),
                CellValue::Text => d.write_bytes(text.as_bytes()),
            }
            d.write_u64(u64::from(cell == Ok(CellValue::Blank)));
        }
    }
    Some(d.finish())
}

fn digest_in(pieces: Pieces) -> u64 {
    let mut d = Digest64::new();
    made_up(&mut d, pieces);
    imported(&mut d, pieces);
    written(&mut d, pieces);
    d.finish()
}

/// The example that comes with the app.
fn made_up(d: &mut Digest64, pieces: Pieces) {
    let mut file = Vec::new();
    if let Ok(mut sample) = Sample::new(20_000, 0x5EED_2026) {
        while sample.step(&mut file) == Ok(true) {}
    }
    d.write_bytes(&file);
    let number = |op, a: &str, b: Option<&str>| Condition::Number {
        op,
        a: String::from(a),
        b: b.map(String::from),
    };
    let after = DAY_MICROS.saturating_mul(19_000);
    let views = [
        spec(Vec::new(), &[(11, true)]),
        spec(Vec::new(), &[(3, false), (11, true), (7, false)]),
        spec(
            vec![(6, Condition::TextContains(String::from("LAMP")))],
            &[(6, true), (1, false)],
        ),
        spec(
            vec![
                (11, number(NumberOp::Between, "900.005", Some("100"))),
                (10, Condition::Blank),
            ],
            &[(7, true)],
        ),
        spec(
            vec![
                (
                    9,
                    Condition::Top {
                        n: 500,
                        bottom: false,
                    },
                ),
                (
                    1,
                    Condition::Time {
                        op: TimeOp::After,
                        a: after,
                        b: None,
                    },
                ),
            ],
            &[(9, false)],
        ),
        spec(
            vec![
                (
                    3,
                    Condition::OneOf {
                        values: vec![
                            Value::Text(String::from("north")),
                            Value::Text(String::from("AIRPORT")),
                        ],
                    },
                ),
                (
                    12,
                    Condition::OneOf {
                        values: vec![Value::Int(1)],
                    },
                ),
            ],
            &[],
        ),
    ];
    ask(d, &file, &views, &["", "1", "lamp"], pieces);
}

/// Delimited text with values of every type, some of which are awkward.
fn imported(d: &mut Digest64, pieces: Pieces) {
    let text = delimited();
    let head = text.as_bytes().get(..HEAD_BYTES).unwrap_or(text.as_bytes());
    let sniffed = csv::sniff(head, false, &Options::default());
    d.write_bytes(format!("{sniffed:?}").as_bytes());
    let mut file = Vec::new();
    for options in [
        Options {
            blank_markers: true,
            kinds: vec![(2, ColumnType::Text), (13, ColumnType::Integer)],
            ..named()
        },
        named(),
    ] {
        file.clear();
        let report = import(&options, text.as_bytes(), pieces.bytes, &mut file);
        d.write_bytes(format!("{report:?}").as_bytes());
        d.write_bytes(&file);
    }
    let views = [
        spec(Vec::new(), &[(0, true)]),
        spec(Vec::new(), &[(3, false), (1, true)]),
        spec(Vec::new(), &[(4, true), (5, false)]),
        spec(Vec::new(), &[(8, false), (9, true)]),
        spec(
            vec![
                (
                    3,
                    Condition::Number {
                        op: NumberOp::NotEqual,
                        a: String::from("1e-1"),
                        b: None,
                    },
                ),
                (8, Condition::TextEndsWith(String::from("Σ"))),
            ],
            &[(5, true)],
        ),
        spec(
            vec![
                (
                    5,
                    Condition::Time {
                        op: TimeOp::On,
                        a: after_midnight(),
                        b: None,
                    },
                ),
                (13, Condition::NotBlank),
            ],
            &[(13, false)],
        ),
    ];
    ask(d, &file, &views, &["", "0", "Σ"], pieces);
}

/// Two of the columns have the same name, so they do not look like names.
fn named() -> Options {
    Options {
        header: Some(true),
        ..Options::default()
    }
}

fn after_midnight() -> i64 {
    DAY_MICROS.saturating_mul(20_000).saturating_add(1)
}

fn import(
    options: &Options,
    bytes: &[u8],
    piece: usize,
    file: &mut Vec<u8>,
) -> Result<Option<Report>, ImportError> {
    let head = bytes.get(..HEAD_BYTES).unwrap_or(bytes);
    let mut import = Import::new(options, head)?;
    let mut pieces = bytes.chunks(piece);
    loop {
        match import.step(file)? {
            Step::NeedInput => match pieces.next() {
                Some(piece) => import.feed(piece)?,
                None => import.end_of_input(),
            },
            Step::Working => {}
            Step::Restart => {
                file.clear();
                pieces = bytes.chunks(piece);
            }
            Step::Done => return Ok(import.report()),
        }
    }
}

/// More than is looked at to find out the types, which change after that.
fn delimited() -> String {
    const WORDS: [&str; 12] = [
        "north",
        "NORTH",
        "ΟΔΟΣ",
        "οδος",
        "İzmir",
        "straße",
        "STRASSE",
        "漢字",
        "e\u{301}",
        "é",
        "\"quoted\"",
        "two\nlines",
    ];
    let mut rng = Rng::from_seed(0x7AB1_E000);
    let mut out = String::from(
        "\u{feff}whole,fixed,padded,loose,float,at,day,truth,word,long,nothing,late,\"a, b\",late\r\n",
    );
    for row in 0..1_500_u32 {
        let wide = rng.next_u64().cast_signed();
        let whole = match row.checked_rem(50) {
            Some(0) => i64::MIN,
            Some(1) => i64::MAX,
            Some(2) => 0,
            Some(3..10) => wide,
            _ => wide.checked_rem(1_000).unwrap_or(0),
        };
        let cents = i64::try_from(rng.next_below(2_000_000))
            .unwrap_or(0)
            .saturating_sub(1_000_000);
        let (units, rest) = (
            cents.saturating_div(100),
            cents.unsigned_abs().checked_rem(100).unwrap_or(0),
        );
        let sign = if cents < 0 && units == 0 { "-" } else { "" };
        let _ = write!(
            out,
            "{whole},{sign}{units}.{rest:02},{:05},",
            rng.next_below(90)
        );
        match rng.next_below(5) {
            0 => {}
            1 => out.push_str("0.1"),
            2 => {
                let _ = write!(out, "{}.{}", rng.next_below(100), rng.next_below(1_000));
            }
            _ => {
                let _ = write!(out, "{}", rng.next_below(50));
            }
        }
        out.push(',');
        match rng.next_below(6) {
            0 => out.push_str("5e-324"),
            1 => out.push_str("1.7976931348623157e308"),
            2 => out.push_str("-1.7976931348623157e308"),
            3 => out.push_str("1e-3"),
            _ => {
                let _ = write!(out, "{:e}", (rng.next_f64() - 0.5) * 1e9);
            }
        }
        let _ = match rng.next_below(4) {
            0 => write!(out, ",2024-10-04T{:02}:30", rng.next_below(24)),
            1 => write!(
                out,
                ",2024-10-04 00:00:{:02}.{}Z",
                rng.next_below(60),
                rng.next_below(1_000)
            ),
            2 => write!(
                out,
                ",{:04}-02-28 23:59:59",
                rng.next_below(9_999).saturating_add(1)
            ),
            _ => write!(out, ",2024/10/4 12:00"),
        };
        let _ = match rng.next_below(3) {
            0 => write!(out, ",2024-02-29"),
            1 => write!(out, ",0001-01-01"),
            _ => write!(out, ",9999/12/{}", rng.next_below(31).saturating_add(1)),
        };
        out.push_str(if rng.next_below(2) == 0 {
            ",true,"
        } else {
            ",False,"
        });
        let word = usize::try_from(rng.next_below(14)).unwrap_or(0);
        let word = WORDS.get(word).copied().unwrap_or("");
        out.push('"');
        out.push_str(&word.replace('"', "\"\""));
        out.push_str("\",");
        for _ in 0..40 {
            let _ = write!(out, "{:016x} ", rng.next_u64());
        }
        let _ = write!(out, ",,{}", if row < 900 { "" } else { "NA" });
        if row == 1_400 {
            out.push_str(",0.5,7,extra");
        } else if row.checked_rem(7) != Some(0) {
            let _ = write!(out, ",{},{}", rng.next_below(9), rng.next_below(90));
        }
        out.push_str(if row.checked_rem(3) == Some(0) {
            "\n"
        } else {
            "\r\n"
        });
    }
    out
}

/// Many small groups, and columns that are coded in different ways.
fn written(d: &mut Digest64, pieces: Pieces) {
    let rows = 1_100_u32;
    let names = [
        "step", "wide", "half", "float", "few", "long", "late", "none",
    ];
    let kinds = [
        Some(ColumnType::DateTime),
        Some(ColumnType::Integer),
        Some(ColumnType::Decimal {
            scale: 3,
            fixed: false,
        }),
        Some(ColumnType::Float),
        Some(ColumnType::Text),
        Some(ColumnType::Text),
        None,
        None,
    ];
    let specs = names
        .iter()
        .zip(kinds)
        .enumerate()
        .map(|(index, (name, kind))| ColumnSpec {
            name: String::from(*name),
            kind,
            dictionary: index != 5,
        })
        .collect();
    let mut file = Vec::new();
    let mut rng = Rng::from_seed(0x7AB1_E001);
    if let Ok(mut writer) = TableWriter::new(specs, 256) {
        for row in 0..rows {
            for (index, column) in writer.columns().iter_mut().enumerate() {
                let random = rng.next_u64();
                let small = i64::try_from(random >> 52).unwrap_or(0);
                match index {
                    0 => column.push_int(i64::from(row).saturating_mul(60_000_000)),
                    1 => column.push_int(random.cast_signed()),
                    2 if small < 2_000 => column.push_blank(),
                    2 => column.push_int(small.saturating_sub(3_000)),
                    3 if small < 100 => column.push_float(-0.0),
                    3 => column.push_float(rng.next_f64() / 3.0 - 0.1),
                    4 => {
                        column.push_text(&format!("Value {}", random & 15));
                    }
                    5 => {
                        column.push_text(&format!("{random:016X}"));
                    }
                    6 if row > 600 => {
                        column.set_kind(ColumnType::Text);
                        column.push_text(if random & 1 == 0 { "Ja" } else { "JA" });
                    }
                    _ => column.push_blank(),
                }
            }
            if writer.check_group() == Ok(true) || row.saturating_add(1) == rows {
                d.write_u64(u64::from(writer.seal_group().is_ok()));
                while writer.step(&mut file) == Ok(true) {}
            }
        }
        writer.finish();
        while writer.step(&mut file) == Ok(true) {}
    }
    d.write_bytes(&file);
    let views = [
        spec(Vec::new(), &[(1, false)]),
        spec(Vec::new(), &[(2, true), (5, false)]),
        spec(Vec::new(), &[(6, false), (3, true)]),
        spec(
            vec![
                (
                    2,
                    Condition::Top {
                        n: 50,
                        bottom: true,
                    },
                ),
                (4, Condition::TextStartsWith(String::from("value 1"))),
            ],
            &[(5, true)],
        ),
    ];
    ask(d, &file, &views, &["", "a"], pieces);
}

fn spec(filters: Vec<(usize, Condition)>, order: &[(usize, bool)]) -> Spec {
    Spec {
        filters,
        order: order
            .iter()
            .map(|(column, descending)| SortKey {
                column: *column,
                descending: *descending,
            })
            .collect(),
    }
}

/// Shows the views of a table, adds up its columns and lists their values.
fn ask(d: &mut Digest64, file: &[u8], views: &[Spec], searches: &[&str], pieces: Pieces) {
    let mut source = file;
    let length = u64::try_from(file.len()).unwrap_or(0);
    let Ok(Opened::Ready(reader)) = Reader::open(&mut source, length) else {
        d.write_u64(u64::MAX);
        return;
    };
    let mut session = Session::new(*reader);
    let width = session.reader().columns().len();
    for view in views {
        d.write_u64(u64::from(session.begin(view.clone()).is_ok()));
        run(d, &mut session, &mut source, pieces.cells);
        d.write_u32(session.view_len());
        for position in 0..session.view_len() {
            d.write_u32(session.view_row(position).unwrap_or(u32::MAX));
        }
        pages(d, &mut session, &mut source, width);
        let all = (0..width).collect();
        d.write_u64(u64::from(session.begin_summary(all).is_ok()));
        run(d, &mut session, &mut source, pieces.cells);
        for summary in session.take_summary().unwrap_or_default() {
            write_summary(d, &summary);
        }
        for column in 0..width {
            let search = searches
                .get(column.checked_rem(searches.len()).unwrap_or(0))
                .copied()
                .unwrap_or("");
            let begun = session.begin_values(column, String::from(search), 40);
            d.write_u64(u64::from(begun.is_ok()));
            run(d, &mut session, &mut source, pieces.cells);
            if let Some(values) = session.take_values() {
                write_values(d, &values);
            }
        }
    }
}

/// Does what was asked for. Only what comes of it counts, not in how many steps.
fn run(d: &mut Digest64, session: &mut Session, source: &mut &[u8], budget: u32) {
    while let Some(job) = session.running() {
        match session.step(source, budget) {
            Ok(Progress::Working { .. } | Progress::Done) => {}
            // All of the file is here.
            Ok(Progress::Need(_)) => session.cancel_job(job),
            Err(error) => d.write_bytes(format!("{error:?}").as_bytes()),
        }
    }
}

/// The first, a middle and the last pages.
fn pages(d: &mut Digest64, session: &mut Session, source: &mut &[u8], width: usize) {
    let mut scratch = String::new();
    let rows = session.view_len();
    let last = rows.saturating_sub(rows.checked_rem(PAGE_ROWS).unwrap_or(0));
    let cols = u32::try_from(width).unwrap_or(0);
    for first_row in [0, (rows >> 1) & !63, last] {
        let mut first_col = 0;
        while first_col < cols {
            let frame = PageFrame {
                sheet: 0,
                first_row,
                first_col,
                epoch: 1,
            };
            match session.page(source, frame, &mut scratch) {
                Ok(page) => d.write_bytes(&page),
                Err(error) => d.write_bytes(format!("{error:?}").as_bytes()),
            }
            first_col = first_col.saturating_add(PAGE_COLS);
        }
    }
    for (column, position) in (0..width).zip([0, rows >> 1, last].into_iter().cycle()) {
        let text = session.cell_text(source, position, column);
        d.write_bytes(text.unwrap_or_default().as_bytes());
    }
}

fn write_summary(d: &mut Digest64, summary: &ColumnSummary) {
    let mut figure = |figure: &Option<Figure>| match figure {
        Some(figure) => {
            d.write_bytes(figure.text.as_bytes());
            d.write_u64(u64::from(figure.exact));
        }
        None => d.write_u64(2),
    };
    for one in [&summary.sum, &summary.mean, &summary.min, &summary.max] {
        figure(one);
    }
    d.write_u64(summary.rows);
    d.write_u64(summary.blanks);
    d.write_u64(summary.distinct.unwrap_or(u64::MAX));
}

fn write_values(d: &mut Digest64, list: &ValueList) {
    for one in &list.values {
        d.write_bytes(one.text.as_bytes());
        match &one.value {
            Value::Blank => d.write_u64(0),
            Value::Int(value) => d.write_u64(value.cast_unsigned()),
            Value::Float(bits) => d.write_u64(*bits),
            Value::Text(text) => d.write_bytes(text.as_bytes()),
        }
        d.write_u64(one.count);
    }
    d.write_u64(list.blanks);
    d.write_u64(u64::from(list.complete));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn how_the_work_is_cut_changes_nothing() {
        let expected = digest();
        for (cells, bytes) in [(1, 1 << 20), (7, 997), (100, 65_536), (u32::MAX, 1 << 22)] {
            assert_eq!(
                digest_in(Pieces { cells, bytes }),
                expected,
                "{cells} cells, {bytes} bytes"
            );
        }
    }

    #[test]
    fn the_digest_of_a_table_is_of_what_it_holds() {
        let make = |seed| {
            let mut file = Vec::new();
            let mut sample = Sample::new(300, seed).unwrap();
            while sample.step(&mut file).unwrap() {}
            file
        };
        let mut file = make(1);
        assert_eq!(content_digest(&file), content_digest(&make(1)));
        assert_ne!(content_digest(&file), content_digest(&make(2)));
        if let Some(byte) = file.get_mut(5_000) {
            *byte ^= 1;
        }
        assert_eq!(content_digest(&file), None);
        assert_eq!(content_digest(&[]), None);
    }

    /// A digest of nothing would be the same everywhere too.
    #[test]
    fn there_is_something_to_compare() {
        let mut file = Vec::new();
        let text = delimited();
        assert!(text.len() > HEAD_BYTES);
        let report = import(&named(), text.as_bytes(), 1 << 20, &mut file)
            .unwrap()
            .unwrap();
        assert_eq!(
            (report.rows, report.passes, report.extra_fields),
            (1_500, 2, 1)
        );
        assert!(report.short_rows > 200);
        let kinds: Vec<ColumnType> = report.columns.iter().map(|column| column.kind).collect();
        assert_eq!(
            kinds,
            [
                ColumnType::Integer,
                ColumnType::Decimal {
                    scale: 2,
                    fixed: true
                },
                ColumnType::Text,
                ColumnType::Decimal {
                    scale: 3,
                    fixed: false
                },
                ColumnType::Float,
                ColumnType::DateTime,
                ColumnType::Date,
                ColumnType::Boolean,
                ColumnType::Text,
                ColumnType::Text,
                ColumnType::Text,
                ColumnType::Text,
                ColumnType::Decimal {
                    scale: 1,
                    fixed: false
                },
                ColumnType::Integer,
            ]
        );
    }
}

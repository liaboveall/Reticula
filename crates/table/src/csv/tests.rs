//! Files imported, and compared with what a plain reading of them gives.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use std::format;

use reticula_core::rng::Rng;

use super::import::Limits;
use super::*;
#[cfg(not(target_arch = "wasm32"))]
use crate::source::{Sink, SinkFailed};
use crate::tests::{Model, open};
use crate::types::{CellValue, MAX_COLUMNS, MAX_NAME_BYTES, MAX_TEXT_BYTES};

/// How a file is given to an import.
enum Pieces<'a> {
    Whole,
    Bytes(usize),
    Random(&'a mut Rng),
}

/// The bytes of the table, and what was reported.
type Outcome = Result<(Vec<u8>, Report), ImportError>;

fn run(import: &mut Import, file: &[u8], mut pieces: Pieces<'_>) -> Outcome {
    let mut sink = Vec::new();
    let mut given = 0;
    let mut steps = 0_usize;
    loop {
        steps += 1;
        assert!(steps < 10_000_000, "no progress");
        let wants = match import.step(&mut sink)? {
            Step::NeedInput => true,
            // More can also be given before it is asked for.
            Step::Working => match &mut pieces {
                Pieces::Random(rng) => rng.next_below(4) == 0 && given < file.len(),
                _ => false,
            },
            Step::Restart => {
                sink.clear();
                given = 0;
                false
            }
            Step::Done => break,
        };
        if wants && given < file.len() {
            let len = match &mut pieces {
                Pieces::Whole => file.len(),
                Pieces::Bytes(len) => *len,
                Pieces::Random(rng) => match rng.next_below(4) {
                    0 => 1,
                    1 => rng.next_below(4) as usize,
                    2 => rng.next_below(40) as usize,
                    _ => rng.next_below(2_000) as usize,
                },
            };
            let end = file.len().min(given + len);
            if import.feed(&file[given..end]).is_ok() {
                given = end;
            }
        } else if wants {
            import.end_of_input();
        }
    }
    let report = import.report().unwrap();
    assert_eq!(report.bytes, sink.len() as u64);
    assert_eq!(import.progress().bytes, file.len() as u64);
    assert_eq!(import.progress().rows, report.rows);
    Ok((sink, report))
}

fn import_with(options: &Options, head: &[u8], file: &[u8], pieces: Pieces<'_>) -> Outcome {
    run(&mut Import::new(options, head)?, file, pieces)
}

fn head_of(file: &[u8]) -> &[u8] {
    &file[..file.len().min(HEAD_BYTES)]
}

fn import(options: &Options, file: &[u8]) -> Outcome {
    import_with(options, head_of(file), file, Pieces::Whole)
}

/// Imports a text of a few rows.
fn table(text: &str) -> (Vec<(String, ColumnType)>, Vec<Vec<Model>>, Report) {
    table_with(&Options::default(), text)
}

fn table_with(
    options: &Options,
    text: &str,
) -> (Vec<(String, ColumnType)>, Vec<Vec<Model>>, Report) {
    let (bytes, report) = import(options, text.as_bytes()).unwrap();
    let (columns, rows) = cells(&bytes);
    (columns, rows, report)
}

/// Checks that two imports came to the same.
fn assert_same(one: &Outcome, other: &Outcome, shown: impl Fn() -> String) {
    match (one, other) {
        (Ok(one), Ok(other)) => {
            assert_eq!(one.1, other.1, "{}", shown());
            assert!(one.0 == other.0, "{}", shown());
        }
        (Err(one), Err(other)) => assert_eq!(one, other, "{}", shown()),
        _ => panic!(
            "{:?}, {:?}: {}",
            one.as_ref().map(|one| &one.1),
            other.as_ref().map(|other| &other.1),
            shown()
        ),
    }
}

/// What is in a table, row by row.
fn cells(file: &[u8]) -> (Vec<(String, ColumnType)>, Vec<Vec<Model>>) {
    let mut reader = open(file);
    let mut source = file;
    let columns: Vec<(String, ColumnType)> = reader
        .columns()
        .iter()
        .map(|column| (column.name.clone(), column.kind))
        .collect();
    let rows = (0..reader.rows())
        .map(|row| {
            (0..columns.len())
                .map(|column| {
                    let mut text = String::new();
                    match reader
                        .cell(&mut source, column, row, usize::MAX, &mut text)
                        .unwrap()
                    {
                        CellValue::Blank => Model::Blank,
                        CellValue::Int(value) => Model::Int(value),
                        CellValue::Float(value) => Model::Float(value),
                        CellValue::Text => Model::Text(text),
                    }
                })
                .collect()
        })
        .collect();
    (columns, rows)
}

fn kinds_of(text: &str) -> Vec<ColumnType> {
    table(text).0.into_iter().map(|(_, kind)| kind).collect()
}

/// The type of a column of these values, the cells of which are compared
/// with those of the plain reading.
fn kind_of(values: &[&str]) -> ColumnType {
    let options = Options {
        header: Some(true),
        delimiter: Some(b','),
        ..Options::default()
    };
    let text = format!(
        "x,y\n{}",
        values.iter().map(|v| format!("{v},\n")).collect::<String>()
    );
    agree(&options, text.as_bytes(), text.as_bytes()).unwrap();
    table_with(&options, &text).0[0].1
}

const fn decimal(scale: u8, fixed: bool) -> ColumnType {
    ColumnType::Decimal { scale, fixed }
}

// The plain reading.

/// One line of a file.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    fields: Vec<String>,
    empty: bool,
}

/// Cuts a text into lines. Gives the line a quote is open in at the end.
fn plain_lines(text: &str, delimiter: char) -> (Vec<Line>, Option<u64>) {
    let chars: Vec<char> = text.chars().collect();
    let ends = |c: char| c == delimiter || c == '\n' || c == '\r';
    let mut lines = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let mut line = Line {
            fields: Vec::new(),
            empty: chars[at] == '\n' || chars[at] == '\r',
        };
        loop {
            let mut field = String::new();
            if chars.get(at) == Some(&'"') {
                at += 1;
                loop {
                    match (chars.get(at), chars.get(at + 1)) {
                        (None, _) => return (lines.clone(), Some(lines.len() as u64 + 1)),
                        (Some('"'), Some('"')) => {
                            field.push('"');
                            at += 2;
                        }
                        (Some('"'), _) => {
                            at += 1;
                            break;
                        }
                        (Some(c), _) => {
                            field.push(*c);
                            at += 1;
                        }
                    }
                }
            }
            while at < chars.len() && !ends(chars[at]) {
                field.push(chars[at]);
                at += 1;
            }
            line.fields.push(field);
            if chars.get(at) == Some(&delimiter) {
                at += 1;
            } else {
                break;
            }
        }
        if chars.get(at) == Some(&'\r') && chars.get(at + 1) == Some(&'\n') {
            at += 1;
        }
        at += 1;
        lines.push(line);
    }
    (lines, None)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Plain {
    Integer,
    Decimal { whole: usize, fraction: usize },
    Exponent,
    Date(i64),
    DateTime(i64),
    Boolean(bool),
    Text,
}

fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// Whether the digits of a number come back from floating point.
fn plain_lossless(text: &str) -> bool {
    let digits = |text: &str| -> String {
        let mantissa = text.split(['e', 'E']).next().unwrap();
        let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
        digits.trim_matches('0').to_string()
    };
    let value: f64 = text.parse().unwrap();
    let (written, shortest) = (digits(text), digits(&format!("{value:e}")));
    if !value.is_finite() || written == shortest || written.len() != shortest.len() {
        return value.is_finite() && written == shortest;
    }
    // As many digits, but others: that is right when both are as close.
    let exact = digits(&format!("{value:.800e}"));
    let half = exact
        .strip_suffix('5')
        .filter(|half| half.len() == written.len());
    half.is_some_and(|half| {
        let below: u64 = half.parse().unwrap();
        (below..=below + 1).contains(&written.parse().unwrap())
    })
}

fn plain_date(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split(['-', '/']).collect();
    let dashes = text.matches('-').count();
    let lens: Vec<usize> = parts.iter().map(|part| part.len()).collect();
    let fits = match dashes {
        2 => lens == [4, 2, 2],
        0 => {
            lens.len() == 3
                && lens[0] == 4
                && (1..=2).contains(&lens[1])
                && (1..=2).contains(&lens[2])
        }
        _ => false,
    };
    if !fits || !parts.iter().all(|part| all_digits(part)) {
        return None;
    }
    let number = |index: usize| parts[index].parse::<i64>().unwrap();
    crate::calendar::days_from_civil(number(0), number(1), number(2))
}

fn plain_date_time(text: &str) -> Option<i64> {
    let (day, time) = text.split_once([' ', 'T'])?;
    let day = plain_date(day)?;
    let time = time.strip_suffix('Z').unwrap_or(time);
    let (time, fraction) = match time.split_once('.') {
        Some((time, fraction)) if all_digits(fraction) && fraction.len() <= 6 => {
            (time, format!("{fraction:0<6}").parse::<i64>().unwrap())
        }
        Some(_) => return None,
        None => (time, 0),
    };
    let parts: Vec<&str> = time.split(':').collect();
    if !(parts.len() == 3 || (parts.len() == 2 && !text.contains('.'))) {
        return None;
    }
    if !parts.iter().all(|part| part.len() == 2 && all_digits(part)) {
        return None;
    }
    let number = |index: usize| {
        parts
            .get(index)
            .map_or(0, |part| part.parse::<i64>().unwrap())
    };
    let (hour, minute, second) = (number(0), number(1), number(2));
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((day * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000_000 + fraction)
}

fn plain_class(text: &str) -> Plain {
    if text.len() > MAX_TEXT_BYTES {
        return Plain::Text;
    }
    match text.to_lowercase().as_str() {
        "true" => return Plain::Boolean(true),
        "false" => return Plain::Boolean(false),
        _ => {}
    }
    if let Some(days) = plain_date(text) {
        return Plain::Date(days);
    }
    if let Some(micros) = plain_date_time(text) {
        return Plain::DateTime(micros);
    }
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let exponent_fits = exponent
        .is_none_or(|exponent| all_digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)));
    if !all_digits(whole)
        || !fraction.is_none_or(all_digits)
        || !exponent_fits
        || (whole.len() > 1 && whole.starts_with('0'))
        || (text.starts_with('-') && !mantissa.contains(|c| ('1'..='9').contains(&c)))
    {
        return Plain::Text;
    }
    match (fraction, exponent) {
        (_, Some(_)) => Plain::Exponent,
        (Some(fraction), None) => Plain::Decimal {
            whole: if whole == "0" { 0 } else { whole.len() },
            fraction: fraction.len(),
        },
        (None, None) if text.parse::<i64>().is_ok() => Plain::Integer,
        (None, None) => Plain::Text,
    }
}

/// The narrowest type of some texts that are not blank.
fn plain_kind(texts: &[&str]) -> Option<ColumnType> {
    let classes: Vec<Plain> = texts.iter().map(|text| plain_class(text)).collect();
    let any = |of: &dyn Fn(&Plain) -> bool| classes.iter().any(of);
    let dates = any(&|class| matches!(class, Plain::Date(_)));
    let times = any(&|class| matches!(class, Plain::DateTime(_)));
    let booleans = any(&|class| matches!(class, Plain::Boolean(_)));
    let exponents = any(&|class| matches!(class, Plain::Exponent));
    let decimals = any(&|class| matches!(class, Plain::Decimal { .. }));
    let numbers = exponents || decimals || any(&|class| matches!(class, Plain::Integer));
    if classes.is_empty() {
        return None;
    }
    let text = any(&|class| matches!(class, Plain::Text));
    if text || u8::from(dates || times) + u8::from(booleans) + u8::from(numbers) > 1 {
        return Some(ColumnType::Text);
    }
    if booleans {
        return Some(ColumnType::Boolean);
    }
    if dates || times {
        return Some(if times {
            ColumnType::DateTime
        } else {
            ColumnType::Date
        });
    }
    if !exponents {
        if !decimals {
            return Some(ColumnType::Integer);
        }
        let shapes: Vec<(usize, usize)> = classes
            .iter()
            .zip(texts)
            .map(|(class, text)| match class {
                Plain::Decimal { whole, fraction } => (*whole, *fraction),
                _ => (text.trim_start_matches(['-', '0']).len(), 0),
            })
            .collect();
        let whole = shapes.iter().map(|shape| shape.0).max().unwrap();
        let fraction = shapes.iter().map(|shape| shape.1).max().unwrap();
        if whole + fraction <= 18 {
            let fixed = shapes.iter().all(|shape| shape.1 == fraction);
            return Some(decimal(fraction as u8, fixed));
        }
    }
    Some(if texts.iter().all(|text| plain_lossless(text)) {
        ColumnType::Float
    } else {
        ColumnType::Text
    })
}

fn plain_cell(kind: ColumnType, text: &str) -> Model {
    match (kind, plain_class(text)) {
        (ColumnType::Text, _) => Model::Text(text.to_string()),
        (ColumnType::Integer, _) => Model::Int(text.parse().unwrap()),
        (ColumnType::Decimal { scale, .. }, _) => {
            let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
            let digits = format!("{whole}{fraction:0<width$}", width = usize::from(scale));
            Model::Int(digits.parse().unwrap())
        }
        (ColumnType::Float, _) => Model::Float(text.parse().unwrap()),
        (ColumnType::Date, Plain::Date(days)) => Model::Int(days),
        (ColumnType::DateTime, Plain::DateTime(micros)) => Model::Int(micros),
        (ColumnType::DateTime, Plain::Date(days)) => Model::Int(days * 86_400_000_000),
        (ColumnType::Boolean, Plain::Boolean(truth)) => Model::Int(i64::from(truth)),
        other => panic!("{other:?} of {text:?}"),
    }
}

const MARKERS: [&str; 9] = [
    "NA", "na", "N/A", "n/a", "NULL", "null", "NaN", "nan", "NAN",
];

fn plain_year(text: &str) -> Option<i64> {
    let year = text.parse::<i64>().ok()?;
    (text.len() == 4 && (1_800..=2_100).contains(&year)).then_some(year)
}

/// Whether the first of some lines is one of names.
fn plain_header(sample: &[&Line], markers: bool) -> bool {
    let (first, others) = sample.split_first().unwrap();
    let mut lens: Vec<usize> = others.iter().map(|line| line.fields.len()).collect();
    lens.sort_unstable();
    let count = |len: usize| lens.iter().filter(|other| **other == len).count();
    let common = lens.iter().copied().max_by_key(|len| (count(*len), *len));
    if first.fields.len() < common.unwrap_or(0) {
        return false;
    }
    let (mut over_narrower, mut missing, mut again) = (0, 0, 0);
    let (mut texts, mut years) = (Vec::new(), Vec::new());
    for (column, name) in first.fields.iter().enumerate() {
        let values: Vec<&str> = others
            .iter()
            .filter_map(|line| line.fields.get(column))
            .map(String::as_str)
            .filter(|text| !text.is_empty() && !(markers && MARKERS.contains(text)))
            .collect();
        if MARKERS.contains(&name.as_str()) || name.chars().all(|c| " -.?*_".contains(c)) {
            missing += usize::from(!values.is_empty() || others.is_empty());
            continue;
        }
        let kind = plain_kind(&values);
        let mut with = values.clone();
        with.push(name);
        let is_text = plain_class(name) == Plain::Text;
        if is_text {
            again += usize::from(texts.contains(name));
            texts.push(name.clone());
        }
        if kind.is_some_and(|kind| kind != ColumnType::Text)
            && plain_kind(&with) == Some(ColumnType::Text)
        {
            over_narrower += 1;
        } else if !is_text {
            match plain_year(name) {
                Some(year) if values.iter().any(|value| plain_year(value).is_none()) => {
                    years.push(year);
                }
                _ => return false,
            }
        }
    }
    let width = first.fields.len();
    if over_narrower > 0 {
        return missing * 2 <= width && again * 2 <= width;
    }
    if years.is_empty() {
        // The numbers of the rows often have no name.
        let numbers = first.fields[0].is_empty()
            && others
                .first()
                .is_some_and(|line| matches!(line.fields[0].as_str(), "0" | "1"));
        return again == 0 && missing == usize::from(numbers);
    }
    (missing == 1 || !texts.is_empty())
        && missing <= 1
        && years.len() > 1
        && years
            .windows(2)
            .all(|pair| (1..=10).contains(&(pair[1] - pair[0]).abs()))
}

/// What the plain reading finds in a file.
struct PlainTable {
    names: Vec<String>,
    kinds: Vec<Option<ColumnType>>,
    rows: Vec<Vec<String>>,
    extra: u64,
    short: u64,
    empty: u64,
}

/// Reads a file whose delimiter is given. What is decided by the start of it
/// is decided by `head`.
fn plain_table(options: &Options, head: &str, text: &str) -> Result<PlainTable, ImportError> {
    let delimiter = char::from(options.delimiter.unwrap());
    let blank = |text: &str| text.is_empty() || (options.blank_markers && MARKERS.contains(&text));
    // A mark at the start is not part of the text.
    let [head, text] = [head, text].map(|text| text.strip_prefix('\u{feff}').unwrap_or(text));
    let (sample, open) = plain_lines(head, delimiter);
    let sample: Vec<&Line> = sample.iter().filter(|line| !line.empty).collect();
    let first = sample.first().ok_or(match open {
        Some(row) => ImportError::UnclosedQuote { row },
        None => ImportError::Empty,
    })?;
    let header = options
        .header
        .unwrap_or_else(|| plain_header(&sample, options.blank_markers));
    let width = if header {
        first.fields.len()
    } else {
        sample.iter().map(|line| line.fields.len()).max().unwrap()
    };
    let mut names: Vec<String> = Vec::new();
    for index in 0..width {
        let given = first.fields.get(index).filter(|_| header);
        let mut name = given.cloned().unwrap_or_default();
        while name.len() > MAX_NAME_BYTES - 8 {
            name.pop();
        }
        if name.is_empty() {
            name = format!("Column {}", index + 1);
        }
        let free = (2..).map(|number| format!("{name} ({number})"));
        let mut choices = core::iter::once(name.clone()).chain(free);
        let unique = choices.find(|choice| !names.contains(choice)).unwrap();
        names.push(unique);
    }
    let (lines, unclosed) = plain_lines(text, delimiter);
    if let Some(row) = unclosed {
        return Err(ImportError::UnclosedQuote { row });
    }
    let mut out = PlainTable {
        names,
        kinds: Vec::new(),
        rows: Vec::new(),
        extra: 0,
        short: 0,
        empty: 0,
    };
    let mut names_come = header;
    let mut started = false;
    for line in lines {
        if line.empty && !(width == 1 && started) {
            out.empty += 1;
            continue;
        }
        started = true;
        if names_come {
            names_come = false;
            continue;
        }
        let mut fields = line.fields;
        if fields.len() < width {
            out.short += 1;
        }
        out.extra += fields.len().saturating_sub(width) as u64;
        fields.resize(width, String::new());
        for field in &mut fields {
            if blank(field) {
                field.clear();
            }
        }
        out.rows.push(fields);
    }
    for column in 0..width {
        let texts: Vec<&str> = out
            .rows
            .iter()
            .map(|row| row[column].as_str())
            .filter(|text| !text.is_empty())
            .collect();
        out.kinds.push(plain_kind(&texts));
    }
    Ok(out)
}

/// Checks an import against the plain reading. Gives the number of passes.
fn agree(options: &Options, head: &[u8], file: &[u8]) -> Result<u32, ImportError> {
    let outcome = import_with(options, head, file, Pieces::Whole);
    agree_on(options, head, file, outcome)
}

fn agree_on(
    options: &Options,
    head: &[u8],
    file: &[u8],
    outcome: Outcome,
) -> Result<u32, ImportError> {
    let text = core::str::from_utf8(file).unwrap();
    let shown = || format!("{options:?}\n{}\n{text}", head.len());
    let plain = plain_table(options, core::str::from_utf8(head).unwrap(), text);
    let (outcome, plain) = match (outcome, plain) {
        (Ok(outcome), Ok(plain)) => (outcome, plain),
        (Err(error), Err(plain)) => {
            assert_eq!(error, plain, "{}", shown());
            return Err(error);
        }
        (outcome, plain) => panic!(
            "{:?} but {:?}: {}",
            outcome.map(|_| ()),
            plain.map(|_| ()),
            shown()
        ),
    };
    let (columns, rows) = cells(&outcome.0);
    let report = outcome.1;
    assert_eq!(columns.len(), plain.kinds.len(), "{}", shown());
    assert_eq!(rows.len(), plain.rows.len(), "{}", shown());
    for (index, ((name, kind), expected)) in columns.iter().zip(&plain.kinds).enumerate() {
        assert_eq!(
            *kind,
            expected.unwrap_or(ColumnType::Text),
            "column {index} of {}",
            shown()
        );
        assert_eq!(*name, plain.names[index]);
        assert_eq!(report.columns[index].kind, *kind);
        assert_eq!(report.columns[index].name, *name);
        assert_eq!(report.columns[index].misfits, 0);
        let mut blanks = 0;
        for (row, expected) in rows.iter().zip(&plain.rows) {
            let text = &expected[index];
            let expected = if text.is_empty() {
                blanks += 1;
                Model::Blank
            } else {
                plain_cell(*kind, text)
            };
            assert_eq!(row[index], expected, "{}", shown());
        }
        assert_eq!(report.columns[index].blanks, blanks, "{}", shown());
    }
    assert_eq!(
        (report.extra_fields, report.short_rows, report.empty_lines),
        (plain.extra, plain.short, plain.empty),
        "{}",
        shown()
    );
    assert!(report.passes <= 2);
    Ok(report.passes)
}

// Random files.

fn pick<'a>(rng: &mut Rng, from: &[&'a str]) -> &'a str {
    from[rng.next_below(from.len() as u64) as usize]
}

fn random_digits(rng: &mut Rng, most: u64) -> String {
    (0..=rng.next_below(most))
        .map(|_| char::from(b'0' + rng.next_below(10) as u8))
        .collect()
}

/// A value the way a column of some sort has it.
fn random_value(rng: &mut Rng, sort: u64) -> String {
    let sign = if rng.next_below(4) == 0 { "-" } else { "" };
    match sort {
        0 => rng.next_below(200).to_string(),
        1 => format!("{}", rng.next_u64().cast_signed() >> rng.next_below(64)),
        2 => format!(
            "{sign}{}.{:02}",
            rng.next_below(100_000),
            rng.next_below(100)
        ),
        3 => {
            let size = 1 << rng.next_below(60);
            let whole = rng.next_below(size);
            format!("{sign}{whole}.{}", random_digits(rng, 12))
        }
        4 => format!("{:e}", (rng.next_f64() - 0.5) * 1e10),
        5 => format!("{}", f64::from_bits(rng.next_u64() >> 2)),
        6 => format!(
            "{:04}-{:02}-{:02}",
            1 + rng.next_below(9_999),
            1 + rng.next_below(12),
            1 + rng.next_below(28)
        ),
        7 => format!(
            "{:04}/{}/{}",
            1 + rng.next_below(9_999),
            1 + rng.next_below(12),
            1 + rng.next_below(28)
        ),
        8 => {
            let sort = 6 + rng.next_below(2);
            let day = random_value(rng, sort);
            let time = format!("{:02}:{:02}", rng.next_below(24), rng.next_below(60));
            let more = match rng.next_below(4) {
                0 => String::new(),
                1 => format!(":{:02}.{}", rng.next_below(60), random_digits(rng, 6)),
                _ => format!(":{:02}", rng.next_below(60)),
            };
            let zone = if rng.next_below(3) == 0 { "Z" } else { "" };
            format!("{day}{}{time}{more}{zone}", pick(rng, &[" ", "T"]))
        }
        9 => pick(rng, &["true", "false", "TRUE", "False", "tRuE"]).to_string(),
        10 => pick(
            rng,
            &["red", "green", "blue", "Größe", "漢字 и", "a b", " x"],
        )
        .to_string(),
        11 => {
            // Digits and points and signs in any order.
            (0..=rng.next_below(8))
                .map(|_| {
                    pick(
                        rng,
                        &["0", "1", "9", "00", ".", "-", "+", "e", "E", "/", ":"],
                    )
                })
                .collect()
        }
        _ => pick(
            rng,
            &[
                "00123",
                "+5",
                "-0",
                "-0.0",
                "0.0",
                "-0e1",
                "0e0",
                "1e5",
                "1E+5",
                "1e",
                "1.",
                ".5",
                "inf",
                "nan",
                "NaN",
                "NA",
                "n/a",
                "NULL",
                "null",
                "na",
                "N/a",
                "True ",
                "truth",
                "2023-02-29",
                "2024-02-29",
                "2023-1-5",
                "2023/13/1",
                "0000-01-01",
                "9999-12-31",
                "2023-01-01T10:00+02:00",
                "2023-01-01 24:00",
                "2023-01-01 10:00:60",
                "2023-01-01 10:00:00.1234567",
                "2023-01-01  10:00",
                "9223372036854775807",
                "9223372036854775808",
                "-9223372036854775808",
                "-9223372036854775809",
                "12345678901234567890",
                "0.1234567890123456",
                "123456789012345678.5",
                "0.123456789012345678",
                "0.1234567890123456789",
                "1e400",
                "1e-400",
                "5e-324",
                "4.9e-324",
                "1.7976931348623157e308",
                "9007199254740993",
                "9007199254740992",
                "1e22",
                "1e23",
                "9999999999999999e22",
                "0.30000000000000004",
                "0.1e1",
                "1e+05",
                "100000000000000000000",
                "1.000000000000000000000000",
                "\u{feff}x",
                "  ",
                " 1",
                "1 ",
                "x\"y",
                "--1",
                "1-",
                "1,5",
                "a;b",
                "a|b",
                "a\tb",
                "line\nbreak",
                "cr\rlf\r\n",
            ],
        )
        .to_string(),
    }
}

/// Writes a value into a file.
fn write_field(rng: &mut Rng, value: &str, delimiter: char, out: &mut String) {
    let special = value.contains([delimiter, '\n', '\r']) || value.starts_with('"');
    if special || rng.next_below(8) == 0 {
        out.push('"');
        out.push_str(&value.replace('"', "\"\""));
        out.push('"');
        if rng.next_below(40) == 0 {
            // What some programs do.
            out.push_str(pick(rng, &[" ", "x", "\"", "x\"y\""]));
        }
    } else {
        out.push_str(value);
    }
}

fn random_file(rng: &mut Rng, delimiter: char) -> String {
    let width = 1 + rng.next_below(5) as usize;
    let rows = match rng.next_below(10) {
        0 => rng.next_below(3),
        1 => 250 + rng.next_below(300),
        _ => rng.next_below(30),
    };
    let sorts: Vec<u64> = (0..width).map(|_| rng.next_below(13)).collect();
    let blanks = rng.next_below(5);
    let odd = [0, 0, 2, 20][rng.next_below(4) as usize];
    let newline = pick(rng, &["\n", "\r\n", "\r", "\n", "\n"]);
    let mut out = String::new();
    if rng.next_below(10) == 0 {
        out.push('\u{feff}');
    }
    for row in 0..rows {
        if rng.next_below(40) == 0 {
            out.push_str(newline);
        }
        let len = match rng.next_below(30) {
            0 => rng.next_below(width as u64 + 1) as usize,
            1 => width + rng.next_below(4) as usize,
            _ => width,
        };
        for column in 0..len {
            if column > 0 {
                out.push(delimiter);
            }
            let sort = sorts.get(column).copied().unwrap_or(10);
            let sort = if row == 0 && rng.next_below(2) == 0 {
                10
            } else if rng.next_below(100) < odd {
                rng.next_below(13)
            } else {
                sort
            };
            if rng.next_below(8) < blanks && row > 0 {
                out.push_str(pick(rng, &["", "", "\"\""]));
            } else {
                let value = random_value(rng, sort);
                write_field(rng, &value, delimiter, &mut out);
            }
        }
        if row + 1 < rows || rng.next_below(2) == 0 {
            out.push_str(newline);
        }
    }
    if rng.next_below(30) == 0 {
        out.push_str("\"never closed");
    }
    out
}

/// Compares random files. Gives how many were tables, and for how many of
/// them the first guess at the types was wrong.
fn compare_files(seed: u64, files: usize) -> (u32, u32) {
    let mut rng = Rng::from_seed(seed);
    let (mut imported, mut twice) = (0, 0);
    for _ in 0..files {
        let delimiter = [',', ',', '\t', ';', '|', ' '][rng.next_below(6) as usize];
        let file = random_file(&mut rng, delimiter);
        let options = Options {
            delimiter: Some(delimiter as u8),
            header: [None, None, Some(true), Some(false)][rng.next_below(4) as usize],
            blank_markers: rng.next_below(2) == 0,
            kinds: Vec::new(),
        };
        // Sometimes the types are guessed from too little.
        let text = file.as_str();
        let mut head = text.len();
        if rng.next_below(3) == 0 {
            head = rng.next_below(text.len() as u64 + 1) as usize;
            while !text.is_char_boundary(head) {
                head -= 1;
            }
        }
        let head = &text.as_bytes()[..head];
        let outcome = import_with(&options, head, text.as_bytes(), Pieces::Whole);
        // Other pieces, other steps, other groups.
        if let Ok(mut import) = Import::new(&options, head) {
            import.limits.step_bytes = 1 + rng.next_below(100);
            import.limits.step_fields = 1 + rng.next_below(30) as usize;
            let pieces = match rng.next_below(3) {
                0 => Pieces::Whole,
                1 => Pieces::Bytes(1 + rng.next_below(9) as usize),
                _ => Pieces::Random(&mut rng),
            };
            let other = run(&mut import, text.as_bytes(), pieces);
            assert_same(&other, &outcome, || format!("{options:?}\n{text}"));
            let limits = Limits {
                step_bytes: 1 + rng.next_below(100),
                step_fields: 1 + rng.next_below(100) as usize,
                group_bytes: 2_000 + rng.next_below(20_000),
            };
            let with_groups = |pieces: Pieces<'_>| {
                let mut import = Import::new(&options, head)?;
                import.limits = limits;
                run(&mut import, text.as_bytes(), pieces)
            };
            let whole = with_groups(Pieces::Whole);
            let pieces = with_groups(Pieces::Random(&mut rng));
            assert_same(&whole, &pieces, || {
                format!("{options:?} {limits:?}\n{text}")
            });
            if let (Ok(whole), Ok(outcome)) = (whole, &outcome) {
                assert!(cells(&whole.0) == cells(&outcome.0));
            }
        }
        if let Ok(passes) = agree_on(&options, head, text.as_bytes(), outcome) {
            imported += 1;
            twice += u32::from(passes == 2);
        }
    }
    (imported, twice)
}

#[test]
fn files_are_read_the_plain_way() {
    let files = if cfg!(target_arch = "wasm32") {
        500
    } else {
        3_000
    };
    let (imported, twice) = compare_files(91, files);
    // Both ways are taken often.
    assert!(imported as usize > files / 2, "{imported}");
    assert!(twice as usize > files / 40, "{twice}");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "takes long"]
fn many_more_files_are_read_the_plain_way() {
    for seed in 1_000..1_040 {
        std::println!("{:?}", compare_files(seed, 5_000));
        compare_floats(seed, 20_000);
    }
}

#[test]
fn types_take_the_values_they_show_and_no_others() {
    use ColumnType::{Boolean, Date, DateTime, Float, Integer, Text};
    for (values, kind) in [
        (
            &["0", "-1", "9223372036854775807", "-9223372036854775808"][..],
            Integer,
        ),
        (&["1", "9223372036854775808"], Text),
        (&["-9223372036854775809"], Text),
        (&["100000000000000000000"], Text),
        (&["1", "00123"], Text),
        (&["1", "+2"], Text),
        (&["0", "-0"], Text),
        (&["1.5", "-0.0"], Text),
        (&["1e3", "-0e3"], Text),
        (&["1", "2 "], Text),
        (&["1", " 2"], Text),
        (&["84.93", "0.10", "-5.00"], decimal(2, true)),
        (&["84.93", "0.1"], decimal(2, false)),
        (&["84.93", "7"], decimal(2, false)),
        (&["0.123456789012345678"], decimal(18, true)),
        (&["123456789012345678", "0.5"], Text),
        (&["12345678901234567", "0.5"], decimal(1, false)),
        (&["0.1234567890123456789"], Text),
        (&["0.0000000000000000001"], Float),
        (&["1000000000000000000", "0.5"], Float),
        (&["1234567890123456789", "0.5"], Text),
        (&["1.5", "1e5", "1E+5", "2e-3", "1e05"], Float),
        (&["1", "1e5"], Float),
        (&["9007199254740993", "1e5"], Text),
        (&["9007199254740992", "1e5"], Float),
        (&["9007199254740993", "1.5"], decimal(1, false)),
        (&["0.30000000000000004", "1e5"], Float),
        (&["0.10000000000000001", "1e5"], Text),
        // Between those two, and as close to both.
        (&["1000000000000000.2", "1e5"], Float),
        (&["1000000000000000.3", "1e5"], Float),
        (&["1000000000000000.4", "1e5"], Float),
        (&["1000000000000000.1", "1e5"], Float),
        (&["1000000000000000.15", "1e5"], Text),
        (&["0.10000000000000001"], decimal(17, true)),
        (&["1e400"], Text),
        (&["1e-400"], Text),
        (&["0e400"], Float),
        (&["5e-324"], Float),
        (&["4.9e-324"], Text),
        (&["1.7976931348623157e308"], Float),
        (&["1.7976931348623159e308"], Text),
        (&["1e", "2"], Text),
        (&["1e+"], Text),
        (&["1."], Text),
        (&[".5"], Text),
        (&["5.e1"], Text),
        (&["inf"], Text),
        (&["nan"], Text),
        (&["1_000"], Text),
        (&["1.2.3"], Text),
        (&["0x10"], Text),
        (&["-"], Text),
        (&["."], Text),
        (&["true", "FALSE", "tRuE"], Boolean),
        (&["true", "1"], Text),
        (&["true", "yes"], Text),
        (
            &[
                "2023-01-31",
                "2023/1/5",
                "2023/01/05",
                "0001-01-01",
                "9999-12-31",
            ],
            Date,
        ),
        (&["2023-1-5"], Text),
        (&["2023-02-29"], Text),
        (&["0000-01-01"], Text),
        (&["10000-01-01"], Text),
        (&["2023/13/1"], Text),
        (&["2023/1/005"], Text),
        (&["2023-01/05"], Text),
        (&["20230105", "2023-01-05"], Text),
        (&["2023-01-31", "2023-01-31 10:00"], DateTime),
        (&["2023-01-31T23:59:59.999999Z", "2023/1/5 0:00"], Text),
        (&["2023-01-31T23:59:59.999999Z", "2023/1/5 00:00"], DateTime),
        (&["2023-01-31 10:00:00.1"], DateTime),
        (&["2023-01-31 10:00:00.1234567"], Text),
        (&["2023-01-31 10:00:00."], Text),
        (&["2023-01-31 10:00.5"], Text),
        (&["2023-01-31 10:00+02:00"], Text),
        (&["2023-01-31 24:00"], Text),
        (&["2023-01-31 10:60"], Text),
        (&["2023-01-31 10:00:60"], Text),
        (&["2023-01-31 1000"], Text),
        (&["2023-01-31 "], Text),
        (&["2023-01-31z10:00"], Text),
        (&["2023-01-31 10:00", "5"], Text),
        (&["", ""], Text),
        (&["", "5", ""], Integer),
        (&["\"5\"", "6"], Integer),
    ] {
        assert_eq!(kind_of(values), kind, "{values:?}");
    }
}

#[test]
fn a_surprise_in_the_last_row_is_met_in_a_second_pass() {
    let mut text = String::from("a,b,c,d\n");
    while text.len() < 3 * HEAD_BYTES {
        text.push_str("1,2.50,2023-01-01,\n");
    }
    let plain = import(&Options::default(), text.as_bytes()).unwrap().1;
    assert_eq!(plain.passes, 1);
    // A value of the kind that was expected, in the last row and in a column
    // that was empty up to there, is none.
    text.push_str("-7,1.25,2024-02-29,x\n");
    let (bytes, report) = import(&Options::default(), text.as_bytes()).unwrap();
    assert_eq!(report.passes, 1);
    assert_eq!(report.rows, plain.rows + 1);
    let last = cells(&bytes).1.pop().unwrap();
    assert_eq!(last[3], Model::Text(String::from("x")));
    for (row, kinds) in [
        (
            "1.5,2.505,2024-02-29 10:00,true",
            [
                decimal(1, false),
                decimal(3, false),
                ColumnType::DateTime,
                ColumnType::Text,
            ],
        ),
        (
            "007,1e3,2023-02-29,x",
            [
                ColumnType::Text,
                ColumnType::Float,
                ColumnType::Text,
                ColumnType::Text,
            ],
        ),
        (
            "x,12345678901234567.1,1,",
            [
                ColumnType::Text,
                ColumnType::Text,
                ColumnType::Text,
                ColumnType::Text,
            ],
        ),
    ] {
        let mut text = text.clone();
        text.push_str(row);
        let mut import = Import::new(&Options::default(), head_of(text.as_bytes())).unwrap();
        let first: Vec<_> = import.kinds();
        assert_eq!(
            first,
            [
                Some(ColumnType::Integer),
                Some(decimal(2, true)),
                Some(ColumnType::Date),
                None
            ]
        );
        let (bytes, report) = run(&mut import, text.as_bytes(), Pieces::Bytes(1 << 20)).unwrap();
        assert_eq!(report.passes, 2);
        let found: Vec<ColumnType> = report.columns.iter().map(|column| column.kind).collect();
        assert_eq!(found, kinds);
        let last = cells(&bytes).1.pop().unwrap();
        let expected: Vec<Model> = row
            .split(',')
            .zip(kinds)
            .map(|(text, kind)| match text {
                "" => Model::Blank,
                text => plain_cell(kind, text),
            })
            .collect();
        assert_eq!(last, expected);
        // With the types known beforehand, the same file comes of one pass.
        if kinds[0] == ColumnType::Text {
            let options = Options {
                kinds: kinds.into_iter().enumerate().collect(),
                ..Options::default()
            };
            let (forced, report) = import_with(
                &options,
                head_of(text.as_bytes()),
                text.as_bytes(),
                Pieces::Whole,
            )
            .unwrap();
            assert_eq!(report.passes, 1);
            assert_eq!(forced, bytes);
        }
    }
}

#[test]
fn a_column_is_not_text_because_its_sample_seemed_to_be() {
    // The sample ends in the middle of a date.
    let text = "day,n\n2023-01-01,1.5\n2023-01-02,2.5\n";
    let head = &text.as_bytes()[..text.len() - 8];
    let mut import = Import::new(&Options::default(), head).unwrap();
    assert_eq!(
        import.kinds(),
        [Some(ColumnType::Text), Some(decimal(1, true))]
    );
    let (bytes, report) = run(&mut import, text.as_bytes(), Pieces::Whole).unwrap();
    assert_eq!(report.passes, 2);
    assert_eq!(cells(&bytes).0[0].1, ColumnType::Date);
    // Nor is a scale larger than it has to be, or a number one with decimals.
    let text = "a,b\n1.25,1.5\n2.5,2\n3.5,3\n";
    let head = text.as_bytes();
    let other = "a,b\n1.5,15\n2.5,2\n3.5,3\n";
    let mut import = Import::new(&Options::default(), head).unwrap();
    let (bytes, report) = run(&mut import, other.as_bytes(), Pieces::Whole).unwrap();
    assert_eq!(report.passes, 2);
    let columns = cells(&bytes).0;
    assert_eq!(columns[0].1, decimal(1, true));
    assert_eq!(columns[1].1, ColumnType::Integer);
}

#[test]
fn numbers_that_would_lose_digits_are_remembered() {
    // The sample does not show them, and what makes them matter comes later.
    for (sample, later) in [
        ("1\n", "9007199254740993\n1e5\n"),
        ("1.500\n", "123456789012345.671\n1e5\n"),
        ("1e5\n", "9007199254740993\n"),
    ] {
        let head = format!("n\n{sample}");
        let text = format!("{head}{later}");
        let options = Options {
            header: Some(true),
            delimiter: Some(b','),
            ..Options::default()
        };
        assert_eq!(agree(&options, head.as_bytes(), text.as_bytes()), Ok(2));
        let (_, report) =
            import_with(&options, head.as_bytes(), text.as_bytes(), Pieces::Whole).unwrap();
        assert_eq!(report.columns[0].kind, ColumnType::Text, "{text}");
        let fine = text.replace("40993", "40992").replace("5.671", "5.5");
        let (_, report) =
            import_with(&options, head.as_bytes(), fine.as_bytes(), Pieces::Whole).unwrap();
        assert_eq!(report.columns[0].kind, ColumnType::Float, "{fine}");
    }
}

#[test]
fn a_file_that_changes_between_two_passes_is_refused() {
    let first = "a,b\n1,2\n3,4\n";
    for (second, third) in [
        ("a,b\n1.5,2\n", "a,b\nx,2\n"),
        ("a,b\n1.5,2\n", "a,b\n1,2\n"),
    ] {
        let mut import = Import::new(&Options::default(), first.as_bytes()).unwrap();
        let mut sink = Vec::new();
        let mut give = |import: &mut Import, text: &str| {
            import.feed(text.as_bytes()).unwrap();
            import.end_of_input();
            loop {
                match import.step(&mut sink) {
                    Ok(Step::Working) => {}
                    other => break other,
                }
            }
        };
        assert_eq!(give(&mut import, second), Ok(Step::Restart));
        assert_eq!(give(&mut import, third), Err(ImportError::Changed));
        assert_eq!(import.step(&mut Vec::new()), Err(ImportError::Changed));
        assert!(import.report().is_none());
    }
}

#[test]
fn groups_become_smaller_where_rows_are_long() {
    let mut rng = Rng::from_seed(95);
    let mut text = String::from("a,b\n");
    for row in 0..3_000 {
        let len = match row {
            0..100 => 1,
            100..1_500 => 200,
            _ => 450,
        };
        text.push_str(&format!("{row},{}\n", random_digits(&mut rng, len)));
    }
    let options = Options {
        kinds: vec![(1, ColumnType::Text)],
        ..Options::default()
    };
    let group_rows = |bytes: &[u8]| open(bytes).directory().group_rows;
    let with_limits = |step_bytes: u64, group_bytes: u64, pieces: Pieces<'_>| {
        let mut import = Import::new(&options, text.as_bytes()).unwrap();
        import.limits.step_bytes = step_bytes;
        import.limits.group_bytes = group_bytes;
        run(&mut import, text.as_bytes(), pieces)
    };
    let (usual, report) = import(&options, text.as_bytes()).unwrap();
    assert_eq!((report.passes, group_rows(&usual)), (1, 65_536));
    // Bytes of the longest group of so many rows. The first has the header.
    let longest = |rows: usize| {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let groups = lines[1..].chunks(rows).enumerate();
        let len = |group: &[&str]| group.iter().map(|line| line.len()).sum::<usize>();
        let lens = groups.map(|(index, group)| len(group) + if index == 0 { 4 } else { 0 });
        lens.max().unwrap() as u64
    };
    for (step, limit) in [
        (1_024, 300_000),
        (100, 300_000),
        (100, 70_000),
        (20_000, 150_000),
    ] {
        let (bytes, report) = with_limits(step, limit, Pieces::Whole).unwrap();
        assert_eq!(report.passes, 2);
        // As many as fit, but for what lies within a step or two.
        let rows = group_rows(&bytes) as usize;
        assert!(longest(rows) <= limit, "{step} {limit} {rows}");
        assert!(
            longest(2 * rows) + 2 * step > limit,
            "{step} {limit} {rows}"
        );
        assert_eq!(cells(&bytes), cells(&usual));
        for len in [1, 10, 1_000] {
            let pieces = with_limits(step, limit, Pieces::Bytes(len)).unwrap();
            assert!(pieces.0 == bytes && pieces.1 == report);
        }
        let pieces = with_limits(step, limit, Pieces::Random(&mut rng)).unwrap();
        assert!(pieces.0 == bytes && pieces.1 == report);
    }
    assert_eq!(
        with_limits(100, 10_000, Pieces::Whole).map(|_| ()),
        Err(ImportError::Write(WriteError::Limit(
            "the rows are too long"
        )))
    );
}

#[test]
fn rows_that_get_longer_and_longer_take_two_passes() {
    let mut text = String::from("a,b\n");
    let mut row = 0;
    for len in [1, 10, 20, 40, 80, 160, 320, 600] {
        for _ in 0..150_000 / (len + 7) {
            row += 1;
            text.push_str(&format!("{row},{}\n", "y".repeat(len)));
        }
    }
    let options = Options::default();
    let (usual, _) = import(&options, text.as_bytes()).unwrap();
    for end in ["", "x,\"a value of another type\"\n"] {
        let text = format!("{text}{end}");
        for pieces in [1 << 20, 777] {
            let mut import = Import::new(&options, head_of(text.as_bytes())).unwrap();
            import.limits.step_bytes = 1_000;
            import.limits.group_bytes = 200_000;
            let (bytes, report) = run(&mut import, text.as_bytes(), Pieces::Bytes(pieces)).unwrap();
            assert_eq!(report.passes, 2);
            assert!(open(&bytes).directory().group_rows <= 512);
            let rows = cells(&bytes).1;
            assert_eq!(rows.len(), row + usize::from(!end.is_empty()));
            if end.is_empty() {
                assert_eq!(rows, cells(&usual).1);
            }
        }
    }
}

#[test]
fn groups_become_as_small_as_can_be_when_they_are_too_large_again() {
    let text = format!("a,b\n{}", "12345,some text\n".repeat(2_000));
    let mut import = Import::new(&Options::default(), text.as_bytes()).unwrap();
    import.limits.step_bytes = 100;
    import.limits.group_bytes = 20_000;
    let mut sink = Vec::new();
    let mut pass = |import: &mut Import| {
        sink.clear();
        import.feed(text.as_bytes()).unwrap();
        import.end_of_input();
        loop {
            match import.step(&mut sink) {
                Ok(Step::Working) => {}
                Ok(Step::Done) => break Ok(open(&sink).directory().group_rows),
                Ok(_) => break Ok(0),
                Err(error) => break Err(error),
            }
        }
    };
    assert_eq!(pass(&mut import), Ok(0));
    // What was found out does not hold any more.
    import.limits.group_bytes = 10_000;
    assert_eq!(pass(&mut import), Ok(0));
    assert_eq!(pass(&mut import), Ok(256));
    assert_eq!(import.report().unwrap().passes, 3);
    let mut import = Import::new(&Options::default(), text.as_bytes()).unwrap();
    import.limits.step_bytes = 100;
    for group_bytes in [20_000, 10_000] {
        import.limits.group_bytes = group_bytes;
        assert_eq!(pass(&mut import), Ok(0));
    }
    import.limits.group_bytes = 3_000;
    let limit = WriteError::Limit("the rows are too long");
    assert_eq!(pass(&mut import), Err(ImportError::Write(limit)));
}

#[test]
fn both_reasons_to_start_over_can_come_together() {
    let mut rng = Rng::from_seed(96);
    let mut text = String::from("a,b\n");
    for row in 0..2_000 {
        text.push_str(&format!("{row},{}\n", random_digits(&mut rng, 100)));
    }
    let late = text.clone() + "x,y\n";
    let early = text.replacen("\n500,", "\nx,", 1);
    for text in [late, early] {
        let head = &text.as_bytes()[..1_000];
        let mut import = Import::new(&Options::default(), head).unwrap();
        import.limits.step_bytes = 512;
        import.limits.group_bytes = 50_000;
        let (bytes, report) = run(&mut import, text.as_bytes(), Pieces::Whole).unwrap();
        assert_eq!(report.passes, 2);
        let (usual, _) =
            import_with(&Options::default(), head, text.as_bytes(), Pieces::Whole).unwrap();
        assert_eq!(cells(&bytes), cells(&usual));
    }
}

#[test]
fn a_field_without_quotes_is_cut_when_it_is_too_long() {
    let long = "é".repeat(MAX_TEXT_BYTES / 2 + 100_000);
    let most = "x".repeat(MAX_TEXT_BYTES);
    let digits = "7".repeat(MAX_TEXT_BYTES + 1);
    let text = format!("a,b\n1,z{long}\n2,{most}\n3,{digits},4\n4,{long}");
    let options = Options {
        header: Some(true),
        delimiter: Some(b','),
        ..Options::default()
    };
    let head = &text.as_bytes()[..4];
    let (bytes, report) = import_with(&options, head, text.as_bytes(), Pieces::Whole).unwrap();
    assert_eq!(report.columns[1].cut_texts, 3);
    assert_eq!((report.rows, report.extra_fields), (4, 1));
    let rows = cells(&bytes).1;
    let cut = |model: &Model| match model {
        Model::Text(text) => text.clone(),
        other => panic!("{other:?}"),
    };
    // Not in the middle of a character.
    assert_eq!(
        cut(&rows[0][1]),
        format!("z{}", &long[..MAX_TEXT_BYTES - 2])
    );
    assert_eq!(cut(&rows[1][1]), most);
    assert_eq!(cut(&rows[2][1]), digits[1..]);
    assert_eq!(cut(&rows[3][1]), long[..MAX_TEXT_BYTES]);
    for len in [100_001, 1 << 20, (3 << 20) + 1] {
        let other = import_with(&options, head, text.as_bytes(), Pieces::Bytes(len)).unwrap();
        assert!(other.0 == bytes && other.1 == report);
    }
    // What is too long is no number, whatever is in it.
    let zeros = format!("a,b\n1.{},1\n", "0".repeat(MAX_TEXT_BYTES - 1));
    let kinds =
        |text: &str| import(&options, text.as_bytes()).map(|(_, report)| report.columns[0].kind);
    assert_eq!(kinds(&zeros), Ok(ColumnType::Text));
    let zeros = format!("a,b\n1.{},1\n", "0".repeat(MAX_TEXT_BYTES - 2));
    assert_eq!(kinds(&zeros), Ok(ColumnType::Float));
}

#[test]
fn a_field_in_quotes_fails_when_it_is_too_long() {
    let options = Options {
        header: Some(true),
        delimiter: Some(b','),
        ..Options::default()
    };
    let try_with = |field: &str, after: &str, pieces: usize| {
        let text = format!("a,b\n1,2\n\n3,{field}{after}");
        let outcome = import_with(
            &options,
            &text.as_bytes()[..8],
            text.as_bytes(),
            Pieces::Bytes(pieces),
        );
        outcome.map(|(bytes, _)| match cells(&bytes).1.pop().unwrap().pop() {
            Some(Model::Text(text)) => text.len(),
            other => panic!("{other:?}"),
        })
    };
    let quoted = |text: &str| format!("\"{text}\"");
    let most = "x".repeat(MAX_TEXT_BYTES);
    let doubled = "\"\"".repeat(MAX_TEXT_BYTES);
    for pieces in [99_991, 1 << 20, 3 << 20] {
        for after in ["\n", "", "\r\n"] {
            assert_eq!(try_with(&quoted(&most), after, pieces), Ok(MAX_TEXT_BYTES));
            assert_eq!(
                try_with(&quoted(&doubled), after, pieces),
                Ok(MAX_TEXT_BYTES)
            );
            let long = Err(ImportError::FieldTooLong { row: 4 });
            assert_eq!(try_with(&quoted(&format!("{most}y")), after, pieces), long);
            assert_eq!(
                try_with(&quoted(&format!("{doubled}\"\"")), after, pieces),
                long
            );
            // What comes after the quote counts as well.
            assert_eq!(
                try_with(&format!("{}y", quoted(&most)), after, pieces),
                long
            );
            assert_eq!(try_with(&format!("\"{most}y"), after, pieces), long);
            assert_eq!(
                try_with(&format!("\"{}", &most[1..]), "", pieces),
                Err(ImportError::UnclosedQuote { row: 4 })
            );
        }
    }
}

#[test]
fn what_is_not_a_table_is_refused() {
    let usual = Options::default();
    let error = |file: &[u8]| import(&usual, file).map(|_| ()).unwrap_err();
    for file in [
        &b""[..],
        b"\xef\xbb\xbf",
        b"\n",
        b"\r\n\n\r",
        b"\xef\xbb\xbf\n\n",
    ] {
        assert_eq!(error(file), ImportError::Empty, "{file:?}");
    }
    assert_eq!(
        error(b"a,b\n1,\"x\n"),
        ImportError::UnclosedQuote { row: 2 }
    );
    assert_eq!(
        error(b"\"a,b\n1,x\n"),
        ImportError::UnclosedQuote { row: 1 }
    );
    assert_eq!(
        error(b"a,b\n\n\n\"1\n2\",\"x"),
        ImportError::UnclosedQuote { row: 4 }
    );
    // Not text: in the head, after it, at the end of it, and at the end of the file.
    assert_eq!(error(b"\xff\xfea\0"), ImportError::NotUtf8 { row: 1 });
    assert_eq!(
        error(b"\xef\xbb\xbf\"a\nb\",c\n1,\xff"),
        ImportError::NotUtf8 { row: 2 }
    );
    assert_eq!(error(b"a,b\n1,\x80\n"), ImportError::NotUtf8 { row: 2 });
    assert_eq!(
        error(b"a,b\n1,2\n\xe6\xbc"),
        ImportError::NotUtf8 { row: 3 }
    );
    assert_eq!(
        error(
            "a\u{fffd},b\n1,2\n\u{fffd}"
                .as_bytes()
                .split_last()
                .unwrap()
                .1
        ),
        ImportError::NotUtf8 { row: 3 }
    );
    for tail in [
        &b"\x80"[..],
        b"1,\xe6\xbc",
        b"1,\xe6\xbc,2\n",
        b"\xe6\xbcz",
        b"\xf8\x80\x80\x80\x80",
    ] {
        let mut file = b"a,b\n1,2\n\n".to_vec();
        let head = file.len();
        file.extend_from_slice(tail);
        for len in [1, 2, 3, 100] {
            assert_eq!(
                import_with(&usual, &file[..head], &file, Pieces::Bytes(len)).map(|_| ()),
                Err(ImportError::NotUtf8 { row: 4 }),
                "{tail:?}"
            );
        }
    }
    let names = |count: usize| vec!["x"; count].join(",").into_bytes();
    assert_eq!(error(&names(MAX_COLUMNS + 1)), ImportError::TooManyColumns);
    assert_eq!(error(&names(5 * MAX_COLUMNS)), ImportError::TooManyColumns);
    assert_eq!(
        import(&usual, &names(MAX_COLUMNS)).unwrap().1.columns.len(),
        MAX_COLUMNS
    );
    let long = vec![b'x'; HEAD_BYTES];
    assert_eq!(error(&long), ImportError::FirstRowTooLong);
    let mut open = vec![b'x'; HEAD_BYTES + 5];
    open[0] = b'"';
    assert_eq!(error(&open), ImportError::FirstRowTooLong);
    // What fits exactly does.
    assert!(import(&usual, &vec![b'x'; 1 << 20]).is_ok());
    assert!(import(&usual, &vec![b'x'; (1 << 20) - 1]).is_ok());
    assert_eq!(
        error(&vec![b'x'; (1 << 20) + 1]),
        ImportError::FirstRowTooLong
    );
}

#[test]
fn more_than_can_wait_is_refused() {
    let mut import = Import::new(&Options::default(), b"a,b\n1,2\n").unwrap();
    let mut sink = Vec::new();
    assert_eq!(import.step(&mut sink), Ok(Step::NeedInput));
    let rows = b"1,2\n".repeat(MAX_WAITING / 4);
    assert_eq!(import.feed(&rows[..1]), Ok(()));
    assert_eq!(import.feed(&rows[1..]), Ok(()));
    assert_eq!(import.feed(b"1"), Err(ImportError::Refused));
    assert_eq!(import.feed(b""), Ok(()));
    // Nothing of that was lost, and what was parsed makes room.
    let mut steps = 0;
    while import.step(&mut sink).unwrap() == Step::Working {
        steps += 1;
        let parsed = import.progress().bytes;
        assert!(parsed <= steps * (256 << 10));
    }
    assert_eq!(import.feed(&rows), Ok(()));
    import.end_of_input();
    assert_eq!(import.feed(b""), Err(ImportError::Refused));
    while import.step(&mut sink).unwrap() == Step::Working {}
    assert_eq!(import.step(&mut sink), Ok(Step::Done));
    assert_eq!(import.feed(b""), Err(ImportError::Refused));
    assert_eq!(import.report().unwrap().rows, (MAX_WAITING / 2) as u64 - 1);
}

#[test]
fn a_step_does_not_take_long_where_rows_are_filled_up() {
    // Two bytes for a row of a thousand cells.
    let mut text = vec!["x"; MAX_COLUMNS].join(",");
    text.push('\n');
    text.push_str(&"1\n".repeat(20_000));
    let mut import = Import::new(&Options::default(), text.as_bytes()).unwrap();
    import.feed(text.as_bytes()).unwrap();
    import.end_of_input();
    let mut sink = Vec::new();
    let mut before = 0;
    loop {
        let step = import.step(&mut sink).unwrap();
        let rows = import.progress().rows;
        assert!(rows - before <= 129);
        before = rows;
        if step == Step::Done {
            break;
        }
    }
    let report = import.report().unwrap();
    assert_eq!((report.rows, report.short_rows), (20_001, 20_000));
    assert_eq!(report.columns[1].blanks, 20_000);
}

#[test]
fn rows_of_the_wrong_length_and_empty_lines() {
    let options = Options {
        header: Some(true),
        ..Options::default()
    };
    let (columns, rows, report) = table_with(&options, "\n\r\na,b,c\n1,2\n\n3,4,5,6,\n\"\"\n,,\n");
    assert_eq!(columns.len(), 3);
    let int = Model::Int;
    assert_eq!(rows[0], [int(1), int(2), Model::Blank]);
    assert_eq!(rows[1], [int(3), int(4), int(5)]);
    assert_eq!(rows[2], vec![Model::Blank; 3]);
    assert_eq!(rows[3], vec![Model::Blank; 3]);
    assert_eq!(rows.len(), 4);
    assert_eq!(
        (report.short_rows, report.extra_fields, report.empty_lines),
        (2, 2, 3)
    );
    // Where there is one column, an empty line is a blank cell.
    let (_, rows, report) = table("\nname\nx\n\ny\n\n");
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[1], [Model::Blank]);
    assert_eq!(report.empty_lines, 1);
    assert_eq!(report.columns[0].blanks, 2);
    // Without a header, the longest row of the sample tells the columns.
    let options = Options {
        header: Some(false),
        ..Options::default()
    };
    let (columns, rows, report) = table_with(&options, "1,2\n3,4,5\n6\n");
    assert_eq!(
        columns
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Column 1", "Column 2", "Column 3"]
    );
    assert_eq!(rows[0], [int(1), int(2), Model::Blank]);
    assert_eq!((report.short_rows, report.extra_fields), (2, 0));
}

#[test]
fn quotes() {
    let text = "a,b\n\"x, \"\"y\"\"\nz\",1\n\"\"\"\",2\nin \"the\" middle,3\n\"after\" it \"s\",4\n\"\",5\n";
    let (_, rows, _) = table(text);
    let texts: Vec<Model> = rows.into_iter().map(|mut row| row.swap_remove(0)).collect();
    let text = |text: &str| Model::Text(text.to_string());
    assert_eq!(
        texts,
        [
            text("x, \"y\"\nz"),
            text("\""),
            text("in \"the\" middle"),
            text("after it \"s\""),
            Model::Blank
        ]
    );
}

#[test]
fn markers_are_blank_only_when_asked_for() {
    let text = "n,t\n1,NA\nNA,x\nn/a,nan\nNULL,Na\nnull,N/a\n\"NaN\",\nNAN,Null\n2,na\n";
    assert_eq!(kinds_of(text), [ColumnType::Text, ColumnType::Text]);
    let options = Options {
        blank_markers: true,
        ..Options::default()
    };
    let (columns, rows, report) = table_with(&options, text);
    assert_eq!(columns[0].1, ColumnType::Integer);
    assert_eq!(report.columns[0].blanks, 6);
    let texts: Vec<&Model> = rows
        .iter()
        .map(|row| &row[1])
        .filter(|cell| **cell != Model::Blank)
        .collect();
    let text = |text: &str| Model::Text(text.to_string());
    assert_eq!(
        texts,
        [&text("x"), &text("Na"), &text("N/a"), &text("Null")]
    );
}

#[test]
fn columns_that_have_to_hold_something() {
    use ColumnType::{Boolean, Date, DateTime, Float, Integer, Text};
    let values = [
        "5",
        "-0",
        "007",
        "+5",
        "5.00",
        "5.250",
        "5.257",
        "1e2",
        "9223372036854775807",
        "9223372036854775808",
        "12345678901234567890123",
        "0.10000000000000001",
        "1e999",
        "x",
        " 5",
        "true",
        "2023-01-05",
        "2023/1/5",
        "2023-01-05 00:00",
        "NA",
        "",
    ];
    let text = format!(
        "a,b\n{}",
        values.iter().map(|v| format!("{v},\n")).collect::<String>()
    );
    let read = |kind: ColumnType, markers: bool| {
        let options = Options {
            header: Some(true),
            blank_markers: markers,
            kinds: vec![(7, Integer), (0, Text), (0, kind)],
            ..Options::default()
        };
        let mut import = Import::new(&options, text.as_bytes()).unwrap();
        let (bytes, report) = run(&mut import, text.as_bytes(), Pieces::Bytes(5)).unwrap();
        assert_eq!(report.passes, 1);
        let column = report.columns[0].clone();
        let (columns, rows) = cells(&bytes);
        assert_eq!(columns[0].1, column.kind);
        let found: Vec<(&str, Model)> = values
            .iter()
            .zip(rows)
            .filter(|(_, row)| row[0] != Model::Blank)
            .map(|(value, row)| (*value, row[0].clone()))
            .collect();
        assert_eq!(found.len() as u64 + column.blanks, values.len() as u64);
        (found, column)
    };
    let int = Model::Int;
    let (found, column) = read(Integer, false);
    assert_eq!(
        found,
        [
            ("5", int(5)),
            ("-0", int(0)),
            ("007", int(7)),
            ("+5", int(5)),
            ("5.00", int(5)),
            ("9223372036854775807", int(i64::MAX)),
        ]
    );
    assert_eq!((column.misfits, column.blanks), (14, 15));
    // What is blank anyway is no misfit.
    assert_eq!(read(Integer, true).1.misfits, 13);
    let (found, column) = read(decimal(2, true), false);
    assert_eq!(
        found,
        [
            ("5", int(500)),
            ("-0", int(0)),
            ("007", int(700)),
            ("+5", int(500)),
            ("5.00", int(500)),
            ("5.250", int(525)),
        ]
    );
    assert_eq!(column.kind, decimal(2, false));
    let (found, _) = read(Float, false);
    let float = |value: f64| Model::Float(value);
    assert_eq!(found.len(), 12);
    assert_eq!(found[4], ("5.00", float(5.0)));
    assert_eq!(found[7], ("1e2", float(100.0)));
    assert_eq!(
        found[9],
        ("9223372036854775808", float(9_223_372_036_854_775_808.0))
    );
    assert_eq!(found[11], ("0.10000000000000001", float(0.1)));
    let days = plain_date("2023-01-05").unwrap();
    let (found, _) = read(Date, false);
    assert_eq!(found, [("2023-01-05", int(days)), ("2023/1/5", int(days))]);
    let (found, _) = read(DateTime, false);
    assert_eq!(found.len(), 3);
    assert_eq!(found[2], ("2023-01-05 00:00", int(days * 86_400_000_000)));
    assert_eq!(read(Boolean, false).0, [("true", int(1))]);
    let (found, column) = read(Text, false);
    assert_eq!((found.len(), column.misfits), (values.len() - 1, 0));
    // A column that is asked for is of that type even when nothing fits.
    let all = |kind: ColumnType, text: &str| {
        let options = Options {
            header: Some(true),
            kinds: vec![(0, kind)],
            ..Options::default()
        };
        table_with(&options, text).0[0].1
    };
    assert_eq!(all(Date, "a\nx\n"), Date);
    assert_eq!(all(decimal(2, false), "a\n1.50\n2.25\n"), decimal(2, true));
    assert_eq!(all(decimal(99, false), "a\n1.50\n"), decimal(18, false));
}

#[test]
fn the_kind_of_an_empty_column_waits_for_a_value() {
    // Long enough for groups before the first value, and a step that ends
    // within the row of it.
    let mut text = String::from("a,b,c\n");
    for _ in 0..1_000 {
        text.push_str(",,\n");
    }
    let blank = text.clone();
    text.push_str("1.50,2023-01-01,x\n,,\n2.25,2023-01-02,y\n");
    for step_bytes in [1, 7, 256 << 10] {
        let mut import = Import::new(&Options::default(), blank.as_bytes()).unwrap();
        import.limits.step_bytes = step_bytes;
        assert_eq!(import.kinds(), [None; 3]);
        let (bytes, report) = run(&mut import, text.as_bytes(), Pieces::Whole).unwrap();
        assert_eq!(report.passes, 1);
        let (columns, rows) = cells(&bytes);
        let kinds: Vec<ColumnType> = columns.iter().map(|column| column.1).collect();
        assert_eq!(
            kinds,
            [decimal(2, true), ColumnType::Date, ColumnType::Text]
        );
        assert_eq!(rows[1_000][0], Model::Int(150));
        assert_eq!(rows[1_002][2], Model::Text(String::from("y")));
        assert_eq!(rows[999], vec![Model::Blank; 3]);
    }
    // A column that stays so is one of text.
    assert_eq!(kinds_of(&blank), [ColumnType::Text; 3]);
}

#[test]
fn names_of_columns() {
    let names = |text: &str| {
        let options = Options {
            header: Some(true),
            ..Options::default()
        };
        let sniffed = sniff(text.as_bytes(), true, &options);
        let names: Vec<String> = sniffed
            .columns
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(
            names,
            table_with(&options, text)
                .0
                .into_iter()
                .map(|column| column.0)
                .collect::<Vec<_>>()
        );
        names
    };
    assert_eq!(names("a,b\n"), ["a", "b"]);
    assert_eq!(names("a,,a,\n"), ["a", "Column 2", "a (2)", "Column 4"]);
    assert_eq!(names("a,a,a (2),a\n"), ["a", "a (2)", "a (2) (2)", "a (3)"]);
    assert_eq!(
        names(",Column 1,Column 1 (2)\n"),
        ["Column 1", "Column 1 (2)", "Column 1 (2) (2)"]
    );
    assert_eq!(names("\" x\ny\",2\n"), [" x\ny", "2"]);
    let long = "é".repeat(MAX_NAME_BYTES);
    let cut = names(&format!("{long},{long},{long}x\n"));
    assert_eq!(cut[0], long[..MAX_NAME_BYTES - 8]);
    assert_eq!(cut[1], format!("{} (2)", &long[..MAX_NAME_BYTES - 8]));
    assert_eq!(cut[2], format!("{} (3)", &long[..MAX_NAME_BYTES - 8]));
    let same = vec!["x"; MAX_COLUMNS].join(",");
    let many = names(&same);
    assert_eq!(many[MAX_COLUMNS - 1], format!("x ({MAX_COLUMNS})"));
}

#[test]
fn headers_are_recognized() {
    let header = |text: &str| sniff(text.as_bytes(), true, &Options::default()).header;
    assert!(header("id,name\n1,x\n"));
    assert!(header("name,city\nx,y\n"));
    assert!(header("name,city"));
    assert!(!header("1,x\n2,y\n"));
    assert!(!header("2023-01-01,x\n"));
    assert!(!header("x,true\ny,false\n"));
    // A title above the table.
    assert!(!header("Report\n1,2,3\n4,5,6\n"));
    // One row that is too long says nothing.
    assert!(header("a,b\n1,2\n3,4\n5,6,7\n"));
    let markers = Options {
        blank_markers: true,
        ..Options::default()
    };
    assert!(header("a,NA\n1,2\n"));
    assert!(sniff(b"a,NA\n1,2\n", true, &markers).header);
    // Some names may be missing, or be there twice.
    for text in [
        ",price,qty\n0,1.5,3\n1,2.5,4\n",
        ",name,city\n0,x,y\n",
        "\"\",name\n1,x\n2,y\n",
        "x,\n1,2\n",
        "x,x\n1,2\n",
        "id,name,name,price\n1,a,b,1.5\n",
        "a,b,\n1,2,\n3,4,\n",
        "a,b,\nx,y,\nz,w,\n",
        "a,b,\n1,2,3\n",
        "a,b,\n1,2\n",
        "id,?,name\n1,x,y\n",
        "a,a,a,b\n1,2,3,4\n",
    ] {
        assert!(header(text), "{text}");
    }
    // Years are names where not all values are years, and so are dates.
    for text in [
        "country,2019,2020,2021\nFR,1.5,2.5,3.5\n",
        "country,2019,2020\nFR,100,200\nDE,150,300\n",
        "country,2020,2010,2000\nFR,100,200,300\n",
        ",2019,2020\n0,1.5,3\n",
        "id,name,2019\n1,x,100\n",
        "country,1900,2023-01-01\nFR,1,2\n",
        "country,2023-01-01,2023-02-01\nFR,1,2\n",
    ] {
        assert!(header(text), "{text}");
    }
    for text in [
        "1,2\n3,4\n",
        "0.5,1,2019\n0.7,2,2020\n",
        "2019,2020\n100,200\n",
        "2019,1.5\n2020,2.5\n",
        "FR,2019,2020\nDE,2015,2018\n",
        "A,1999\nB,2500\n",
        "A,1999,5.0\nB,2500,6.1\n",
        "A,1999,2050\nB,2500,3000\n",
        "country,2019,2019\nFR,100,200\n",
        "1,N/A,3\n2,5,6\n",
        "1,x,3\n2,5,6\n",
        "NA,NA\n1,2\n",
        "?,-\n1,2\n",
        "x,,y\nz,w,v\n",
        "x,,y\n",
        ",x,y\n",
        ",x,y\n17,z,w\n",
        ",x,x\n0,y,z\n",
        ",,,x\n1,2,3,4\n",
        "a,a\nb,b\n",
        "x,5\ny,z\n",
        "x,2019\ny,\n",
    ] {
        assert!(!header(text), "{text}");
    }
    for header in [true, false] {
        let options = Options {
            header: Some(header),
            ..Options::default()
        };
        let sniffed = sniff(b"a,b\nc,d\n", true, &options);
        assert_eq!(sniffed.header, header);
        assert_eq!(sniffed.preview.len(), if header { 1 } else { 2 });
    }
}

#[test]
fn delimiters_are_recognized() {
    let found =
        |text: &str| char::from(sniff(text.as_bytes(), true, &Options::default()).delimiter);
    assert_eq!(found("a,b\n1,2\n"), ',');
    assert_eq!(found("a;b\n1,5;2,5\n3,5;4\n"), ';');
    assert_eq!(found("a\tb\tc\nx, y\tz\t1\nx, y\tz\t2\n"), '\t');
    assert_eq!(found("a|b\n1|2\n"), '|');
    assert_eq!(found("a,b,c\n\"x;y\",\"1;2\",3\n\"x;y;z\",\"1\",3\n"), ',');
    assert_eq!(found("one\ntwo\nthree\n"), ',');
    assert_eq!(found(""), ',');
    // Rows that are broken here and there do not hide it.
    let mut text = String::new();
    for row in 0..100 {
        text.push_str(&format!(
            "{row}|x{}|z{}\n",
            if row % 2 == 0 { ";" } else { "" },
            if row % 10 == 0 { "|" } else { "" }
        ));
    }
    assert_eq!(found(&text), '|');
    // A quote that opens by mistake takes the rows after it away.
    let mut text = String::from("a;b,x\nc;d,x\n");
    for _ in 0..100 {
        text.push_str("e;\"f,x\n");
    }
    assert_eq!(found(&text), ',');
    let options = Options {
        delimiter: Some(b' '),
        ..Options::default()
    };
    assert_eq!(sniff(b"a b,c\n", true, &options).columns.len(), 2);
    for useless in [b'"', b'\n', b'\r', 0, 0x80] {
        let options = Options {
            delimiter: Some(useless),
            ..Options::default()
        };
        assert_eq!(sniff(b"a;b\n", true, &options).delimiter, b';');
    }
}

#[test]
fn what_is_shown_before_an_import() {
    let mut text = String::from("\u{feff}id,price,note\n");
    for row in 0..60 {
        text.push_str(&format!("{row},{row}.50,\"größe, {row}\"\n"));
    }
    let sniffed = sniff(text.as_bytes(), true, &Options::default());
    assert!(sniffed.valid_utf8 && sniffed.header);
    let kinds: Vec<_> = sniffed.columns.iter().map(|column| column.kind).collect();
    assert_eq!(
        kinds,
        [
            Some(ColumnType::Integer),
            Some(decimal(2, true)),
            Some(ColumnType::Text)
        ]
    );
    assert_eq!(sniffed.preview.len(), 50);
    assert_eq!(sniffed.preview[49], ["49", "49.50", "größe, 49"]);
    assert_eq!(sniffed.row_bytes, (text.len() - 3) / 61);
    // It may end anywhere.
    for end in 0..text.len() {
        let part = sniff(&text.as_bytes()[..end], false, &Options::default());
        assert!(part.valid_utf8, "{end}");
        assert!(part.preview.len() <= sniffed.preview.len());
        for (row, whole) in part.preview.iter().zip(&sniffed.preview) {
            if part.header {
                assert_eq!(row, whole, "{end}");
            }
        }
        if end > 100 {
            assert_eq!(
                part.columns
                    .iter()
                    .map(|column| column.kind)
                    .collect::<Vec<_>>(),
                kinds
            );
        }
    }
    // As the whole file it may not.
    let cut = text.find('ö').unwrap() + 1;
    assert!(!sniff(&text.as_bytes()[..cut], true, &Options::default()).valid_utf8);
    let broken = sniff(b"a,b\n1,\xe9\n", false, &Options::default());
    assert!(!broken.valid_utf8);
    assert_eq!(broken.preview, [["1", "\u{fffd}"]]);
    // No more is looked at than the start.
    let long = "x,1\n".repeat(HEAD_BYTES);
    let sniffed = sniff(long.as_bytes(), true, &Options::default());
    assert_eq!((sniffed.row_bytes, sniffed.columns.len()), (4, 2));
    let empty = sniff(b"", true, &Options::default());
    assert!(empty.columns.is_empty() && empty.preview.is_empty() && empty.valid_utf8);
    // Columns without a value have no type yet, short rows are filled up.
    let sniffed = sniff(b"a,b\n1\n2,\n", true, &Options::default());
    assert_eq!(sniffed.columns[1].kind, None);
    assert_eq!(sniffed.preview, [["1", ""], ["2", ""]]);
}

#[test]
fn texts_that_differ_have_no_dictionary_when_they_are_long() {
    let dictionaries = |cell: &dyn Fn(usize) -> String| {
        let mut text = String::from("a,b\n");
        for row in 0..1_000 {
            text.push_str(&format!("{row},{}\n", cell(row)));
        }
        let (bytes, _) = import(&Options::default(), text.as_bytes()).unwrap();
        open(&bytes).columns()[1].dictionary.is_some()
    };
    assert!(dictionaries(&|row| format!(
        "the same text again and again {}",
        row % 400
    )));
    assert!(!dictionaries(&|row| format!(
        "another text in every one of the rows {row}"
    )));
    assert!(dictionaries(&|row| format!("C{row:07}")));
}

#[test]
fn any_bytes_are_survived() {
    let mut rng = Rng::from_seed(97);
    let rounds = if cfg!(target_arch = "wasm32") {
        300
    } else {
        3_000
    };
    for round in 0..rounds {
        let len = rng.next_below(200) as usize;
        let file: Vec<u8> = (0..len)
            .map(|_| {
                if round % 3 == 0 {
                    rng.next_u64() as u8
                } else {
                    b",,;\"\"\n\r\n 1-.e\xc3\xa9\xff\0x"[rng.next_below(18) as usize]
                }
            })
            .collect();
        let options = Options {
            delimiter: [None, Some(rng.next_u64() as u8)][rng.next_below(2) as usize],
            header: [None, Some(true), Some(false)][rng.next_below(3) as usize],
            blank_markers: rng.next_below(2) == 0,
            kinds: vec![(
                rng.next_below(3) as usize,
                [ColumnType::Text, ColumnType::Float, decimal(200, true)]
                    [rng.next_below(3) as usize],
            )],
        };
        let head = &file[..rng.next_below(len as u64 + 1) as usize];
        let _ = sniff(head, rng.next_below(2) == 0, &options);
        let whole = import_with(&options, head, &file, Pieces::Whole);
        if let Ok(mut import) = Import::new(&options, head) {
            import.limits.step_bytes = 1 + rng.next_below(50);
            import.limits.step_fields = rng.next_below(5) as usize;
            let other = run(&mut import, &file, Pieces::Random(&mut rng));
            assert_same(&other, &whole, || format!("{options:?} {file:?}"));
        }
        if let Ok((bytes, _)) = whole {
            crate::tests::exercise(&bytes).unwrap();
        }
    }
}

#[test]
fn every_float_comes_back_from_its_shortest_digits() {
    let rounds = if cfg!(target_arch = "wasm32") {
        1_000
    } else {
        5_000
    };
    compare_floats(99, rounds);
}

fn compare_floats(seed: u64, rounds: usize) {
    use super::value::Number;
    let mut rng = Rng::from_seed(seed);
    let lossless = |text: &str| {
        Number::read(text.as_bytes())
            .unwrap()
            .is_lossless(text.as_bytes())
    };
    for round in 0..rounds {
        let bits = match round % 4 {
            0 => rng.next_u64(),
            // Not too large and not too small.
            1 => ((rng.next_f64() - 0.5) * 1e6).to_bits(),
            // Few bits, powers of two among them.
            2 => rng.next_u64() & 0x7ff0_0000_0000_0000 | rng.next_below(4) << rng.next_below(50),
            // The smallest ones.
            _ => rng.next_u64() >> (11 + rng.next_below(52)),
        };
        let value = f64::from_bits(bits);
        if !value.is_finite() || value == 0.0 {
            continue;
        }
        let shortest = format!("{value:e}");
        assert!(lossless(&shortest), "{shortest}");
        // Another last digit, and one more.
        let (mantissa, exponent) = shortest.split_once('e').unwrap();
        for digit in '0'..='9' {
            let last = mantissa.len() - 1;
            let other = format!("{}{digit}e{exponent}", &mantissa[..last]);
            let more = if mantissa.contains('.') { "" } else { "." };
            let longer = format!("{mantissa}{more}{digit}e{exponent}");
            for text in [other, longer] {
                let finite = text.parse::<f64>().unwrap().is_finite();
                if finite && Number::read(text.as_bytes()).is_some() {
                    assert_eq!(lossless(&text), plain_lossless(&text), "{text}");
                }
            }
        }
    }
}

#[test]
fn values_are_read_the_plain_way() {
    use super::value::{self, Class, Number, classify};
    let mut rng = Rng::from_seed(98);
    let rounds = if cfg!(target_arch = "wasm32") {
        20_000
    } else {
        200_000
    };
    for round in 0..rounds {
        let text: String = match round % 4 {
            0 => {
                let sort = rng.next_below(13);
                random_value(&mut rng, sort)
            }
            1 => random_digits(&mut rng, 22),
            2 => {
                let whole = random_digits(&mut rng, 19);
                format!(
                    "{}{whole}.{}",
                    pick(&mut rng, &["", "-"]),
                    random_digits(&mut rng, 19)
                )
            }
            // One character of something that is a value.
            _ => {
                let sort = rng.next_below(9);
                let mut chars: Vec<char> = random_value(&mut rng, sort).chars().collect();
                let at = rng.next_below(chars.len() as u64) as usize;
                chars[at] = pick(
                    &mut rng,
                    &["0", "9", "/", ":", "-", ".", "\u{80}", "T", " "],
                )
                .chars()
                .next()
                .unwrap();
                chars.into_iter().collect()
            }
        };
        let bytes = text.as_bytes();
        let plain = plain_class(&text);
        let class = classify(bytes);
        match (plain, class) {
            (Plain::Integer, Class::Integer { value, lossless }) => {
                assert_eq!(Ok(value), text.parse());
                assert_eq!(lossless, plain_lossless(&text), "{text}");
            }
            (
                Plain::Decimal { whole, fraction },
                Class::Decimal {
                    whole: w,
                    fraction: f,
                    lossless,
                },
            ) => {
                assert_eq!((whole, fraction), (w as usize, f as usize), "{text}");
                assert_eq!(lossless, plain_lossless(&text), "{text}");
            }
            (Plain::Exponent, Class::Exponent { lossless }) => {
                assert_eq!(lossless, plain_lossless(&text), "{text}");
            }
            (Plain::Date(days), Class::Date) => assert_eq!(value::date(bytes), Some(days)),
            (Plain::DateTime(micros), Class::DateTime) => {
                assert_eq!(value::date_time(bytes), Some(micros), "{text}");
            }
            (Plain::Boolean(_), Class::Boolean) | (Plain::Text, Class::Text) => {}
            other => panic!("{other:?}: {text}"),
        }
        // What one kind of column takes, all take for what it is.
        let integer = (plain == Plain::Integer).then(|| text.parse().unwrap());
        assert_eq!(value::integer(bytes), integer, "{text}");
        let scale = rng.next_below(19) as u32;
        let expected = match plain {
            Plain::Integer | Plain::Decimal { .. } => {
                let whole = text.split('.').next().unwrap().trim_start_matches('-');
                let digits = if whole == "0" { 0 } else { whole.len() };
                let places = text
                    .split_once('.')
                    .map_or(0, |(_, fraction)| fraction.len());
                let fits = places as u32 <= scale && digits as u32 + scale <= 18;
                let kind = decimal(scale as u8, false);
                fits.then(|| (plain_cell(kind, &text), places as u32))
            }
            _ => None,
        };
        let found = value::decimal(bytes, scale).map(|(value, places)| (Model::Int(value), places));
        assert_eq!(found, expected, "{text} with {scale} decimals");
        assert_eq!(value::date(bytes), plain_date(&text), "{text}");
        assert_eq!(value::date_time(bytes), plain_date_time(&text), "{text}");
        if let Some(number) = Number::read(bytes) {
            let parsed: f64 = text.parse().unwrap();
            let float = number.to_float(bytes).map(f64::to_bits);
            assert_eq!(
                float,
                parsed.is_finite().then(|| parsed.to_bits()),
                "{text}"
            );
        }
    }
}

/// Rows like those of the table the measurements are made with.
#[cfg(not(target_arch = "wasm32"))]
fn orders(rows: usize) -> Vec<u8> {
    use core::fmt::Write;
    const REGIONS: [&str; 8] = [
        "North", "South", "East", "West", "Central", "Harbour", "Airport", "Online",
    ];
    const WORDS: [&str; 10] = [
        "late", "gift", "return", "exchange", "fragile", "priority", "repeat", "bulk", "online",
        "counter",
    ];
    let mut rng = Rng::from_seed(7);
    let mut below = move |limit: u64| rng.next_below(limit);
    let mut out = String::from(
        "id,ordered_at,day,store,region,product,customer,quantity,unit_price,discount,amount,paid,note\n",
    );
    let mut at = 1_672_531_200_000_u64;
    for id in 1..=rows {
        at += below(16_000);
        let days = (at / 86_400_000) as i64;
        let (y, m, d) = crate::calendar::civil_from_days(days);
        let of_day = at / 1_000 % 86_400;
        let quantity = 1 + below(12);
        let cents = 99 + below(50_000);
        let discount = if below(5) == 0 { below(40) } else { 0 };
        let amount = (quantity * cents * (100 - discount) + 50) / 100;
        let product = below(2_000);
        let _ = write!(
            out,
            "{id},{y}-{m:02}-{d:02} {:02}:{:02}:{:02},{y}-{m:02}-{d:02},{},{},P-{product:04} {},C{:07},{quantity},{}.{:02},",
            of_day / 3_600,
            of_day / 60 % 60,
            of_day % 60,
            below(5_000),
            REGIONS[below(8) as usize],
            WORDS[product as usize % 10],
            below(1_200_000),
            cents / 100,
            cents % 100,
        );
        if discount > 0 {
            let _ = write!(out, "0.{discount:02}");
        }
        let _ = write!(
            out,
            ",{}.{:02},{},",
            amount / 100,
            amount % 100,
            below(20) > 0
        );
        if below(10) == 0 {
            let word = WORDS[below(10) as usize];
            if below(3) == 0 {
                let _ = write!(out, "\"{word}, {}\"", WORDS[below(10) as usize]);
            } else {
                let _ = write!(out, "{word} {}", below(1_000));
            }
        }
        out.push('\n');
    }
    out.into_bytes()
}

/// Takes bytes and only counts them.
#[cfg(not(target_arch = "wasm32"))]
struct Count(u64);

#[cfg(not(target_arch = "wasm32"))]
impl Sink for Count {
    fn append(&mut self, bytes: &[u8]) -> Result<(), SinkFailed> {
        self.0 += bytes.len() as u64;
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn time_import(name: &str, file: &[u8], options: &Options) {
    use std::time::Instant;
    for _ in 0..3 {
        let start = Instant::now();
        let mut longest = 0.0_f64;
        let mut import = Import::new(options, &file[..file.len().min(HEAD_BYTES)]).unwrap();
        let mut sink = Count(0);
        let mut chunks = file.chunks(4 << 20);
        loop {
            let step = Instant::now();
            let came = import.step(&mut sink).unwrap();
            longest = longest.max(step.elapsed().as_secs_f64());
            match came {
                Step::NeedInput => match chunks.next() {
                    Some(chunk) => import.feed(chunk).unwrap(),
                    None => import.end_of_input(),
                },
                Step::Working => {}
                Step::Restart => {
                    chunks = file.chunks(4 << 20);
                    sink = Count(0);
                }
                Step::Done => break,
            }
        }
        let seconds = start.elapsed().as_secs_f64();
        let report = import.report().unwrap();
        std::println!(
            "{name}: {:.0} MB/s, {:.1} ns/cell, {:.2} s, {} passes, longest step {:.2} ms, {} -> {} bytes",
            file.len() as f64 / 1e6 / seconds,
            seconds * 1e9 / (report.rows as f64 * report.columns.len() as f64),
            seconds,
            report.passes,
            longest * 1e3,
            file.len(),
            sink.0,
        );
    }
}

/// Rows of so many cells, from a function.
#[cfg(not(target_arch = "wasm32"))]
fn rows_of(
    rows: usize,
    columns: usize,
    cell: &mut dyn FnMut(&mut Rng, usize) -> String,
) -> Vec<u8> {
    let mut rng = Rng::from_seed(8);
    let mut out = String::new();
    for row in 0..=rows {
        for column in 0..columns {
            if column > 0 {
                out.push(',');
            }
            if row == 0 {
                out.push_str(&format!("c{column}"));
            } else {
                out.push_str(&cell(&mut rng, column));
            }
        }
        out.push('\n');
    }
    out.into_bytes()
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "a measurement"]
fn speed() {
    let usual = Options::default();
    let mut file = orders(1_000_000);
    time_import("orders", &file, &usual);
    file.extend_from_slice(b"0,,,,,,,1.5,,,,,\n");
    time_import("orders, a surprise at the end", &file, &usual);
    let at = 2 * HEAD_BYTES
        + file[2 * HEAD_BYTES..]
            .iter()
            .position(|b| *b == b'\n')
            .unwrap()
        + 1;
    file.splice(at..at, *b"0,,,,,,,1.5,,,,,\n");
    time_import("orders, a surprise soon", &file, &usual);
    let uuid = &mut |rng: &mut Rng, column: usize| match column {
        0 => format!(
            "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
            rng.next_below(1 << 32),
            rng.next_below(1 << 16),
            rng.next_below(1 << 12),
            rng.next_below(1 << 12),
            rng.next_below(1 << 48)
        ),
        _ => rng.next_below(1_000_000).to_string(),
    };
    time_import(
        "long texts that differ",
        &rows_of(1_000_000, 3, uuid),
        &usual,
    );
    let floats = &mut |rng: &mut Rng, _: usize| format!("{:e}", (rng.next_f64() - 0.5) * 1e10);
    time_import("floats", &rows_of(300_000, 10, floats), &usual);
    let numbers = &mut |rng: &mut Rng, _: usize| rng.next_below(1_000_000).to_string();
    time_import(
        "a thousand columns",
        &rows_of(10_000, 1_000, numbers),
        &usual,
    );
    let quoted = &mut |rng: &mut Rng, _: usize| format!("\"{}\"", rng.next_below(1_000_000));
    time_import("all in quotes", &rows_of(1_000_000, 10, quoted), &usual);
    let mixed = &mut |rng: &mut Rng, _: usize| {
        format!("{}", rng.next_u64().cast_signed() >> rng.next_below(64))
    };
    time_import(
        "whole numbers of all sizes",
        &rows_of(500_000, 10, mixed),
        &usual,
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "a measurement"]
fn time_of_a_file() {
    let path = std::env::var("CSV_FILE").unwrap();
    time_import(&path, &std::fs::read(&path).unwrap(), &Options::default());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "a measurement"]
fn speed_of_the_parts() {
    use super::column::{Column, Fields, specs};
    use super::split::Splitter;
    use crate::writer::TableWriter;
    use std::time::Instant;

    let file = orders(1_000_000);
    let text = core::str::from_utf8(&file).unwrap();
    let sample = super::sniff::Sample::new(&text[..HEAD_BYTES], false, &Options::default());
    for _ in 0..2 {
        let mut columns: Vec<Column> = (0..13)
            .map(|index| Column {
                name: sample.name(index, &|_| false),
                forced: None,
                kind: sample.kind(index),
                dictionary: true,
                seen: super::infer::Seen::default(),
                blanks: 0,
                misfits: 0,
                cut_texts: 0,
            })
            .collect();
        let mut writer = TableWriter::new(specs(&columns), 65_536).unwrap();
        let mut times = [0.0_f64; 13];
        let (mut splitting, mut writing) = (0.0, 0.0);
        let mut splitter = Splitter::new(b',', 13, true);
        let mut sink = Count(0);
        let mut filled = 0;
        while splitter.looked_at() < text.len() {
            let start = Instant::now();
            splitter.clear();
            let end = text.len().min(splitter.looked_at() + (256 << 10));
            let before = splitter.rows;
            splitter
                .run(text, end, end == text.len(), 65_536 - filled, 1 << 17)
                .unwrap();
            filled += splitter.rows - before;
            splitting += start.elapsed().as_secs_f64();
            let fields = Fields {
                input: text,
                joined: &splitter.joined,
                all: &splitter.fields,
                first: splitter.first,
                width: 13,
                markers: false,
            };
            for (index, column) in columns.iter_mut().enumerate() {
                let start = Instant::now();
                assert!(column.read(&fields, index, writer.column(index).unwrap()));
                times[index] += start.elapsed().as_secs_f64();
            }
            if filled == 65_536 {
                filled = 0;
                let start = Instant::now();
                writer.seal_group().unwrap();
                while writer.step(&mut sink).unwrap() {}
                writing += start.elapsed().as_secs_f64();
            }
        }
        std::println!(
            "cells {:.1} ns, {:.0} MB/s; writing {:.1} ns",
            splitting * 1e9 / 13e6,
            97.85 / splitting,
            writing * 1e9 / 13e6
        );
        for (column, time) in columns.iter().zip(times) {
            std::println!("  {:12} {:5.1} ns", column.name, time * 1e3);
        }
    }
}

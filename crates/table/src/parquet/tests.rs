//! Files of every kind written, imported and compared.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use std::format;

use reticula_core::rng::Rng;

use super::codec::tests::ALL as CODECS;
use super::encode::{self, Field, Layout, Means, Put, Raw};
use super::value::Physical;
use super::{
    Damage, Import, Limit, Opened, Options, ParquetError, ParquetFile, Report, SkipReason, Step,
    open,
};
use crate::calendar::{DAY_MICROS, FIRST_DAY, LAST_DAY};
use crate::source::{ByteRange, Source, SourceError};
use crate::tests::{Model, open as open_table};
use crate::types::{CellValue, ColumnType, MAX_ROWS, MAX_TEXT_BYTES};

/// How many rounds a test does. `RETICULA_ROUNDS` multiplies it.
pub(super) fn rounds(least: usize) -> usize {
    #[cfg(not(target_arch = "wasm32"))]
    let times = std::env::var("RETICULA_ROUNDS")
        .ok()
        .and_then(|times| times.parse().ok())
        .unwrap_or(1);
    #[cfg(target_arch = "wasm32")]
    let times = 1;
    least * times
}

/// A file of which only what was asked for last is here.
pub(super) struct Asked<'a> {
    pub(super) file: &'a [u8],
    held: Vec<ByteRange>,
    reads: usize,
}

impl<'a> Asked<'a> {
    pub(super) fn new(file: &'a [u8]) -> Self {
        Self {
            file,
            held: Vec::new(),
            reads: 0,
        }
    }
}

impl Source for Asked<'_> {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        let end = at + into.len() as u64;
        if !self
            .held
            .iter()
            .any(|range| range.at <= at && end <= range.end())
        {
            return Err(SourceError::Missing);
        }
        self.reads += 1;
        (&*self.file).read(at, into)
    }
}

pub(super) fn open_file(file: &[u8]) -> Result<ParquetFile, ParquetError> {
    let mut source = Asked::new(file);
    for _ in 0..4 {
        match open(&mut source, file.len() as u64)? {
            Opened::Ready(opened) => return Ok(opened),
            Opened::Need(range) => source.held = vec![range],
        }
    }
    panic!("opening never ends");
}

pub(super) struct Imported {
    pub(super) table: Vec<u8>,
    pub(super) report: Report,
    pub(super) restarts: u32,
    steps: usize,
    needs: usize,
}

/// Imports a file that is read piece by piece.
pub(super) fn import(file: &[u8], options: &Options) -> Result<Imported, ParquetError> {
    import_with(file, options, |_| {})
}

/// Imports with smaller groups than usual, or with streams that keep nothing
/// from one group to the next.
fn import_otherwise(file: &[u8], rng: &mut Rng) -> Result<Imported, ParquetError> {
    let (small, forgetful) = (rng.next_below(2) == 0, rng.next_below(2) == 0);
    import_with(file, &Options::default(), |import| {
        if small {
            import.group_rows = 256;
        }
        if forgetful {
            import.stream_budget = 0;
        }
    })
}

fn import_with(
    file: &[u8],
    options: &Options,
    prepare: impl FnOnce(&mut Import),
) -> Result<Imported, ParquetError> {
    let opened = open_file(file)?;
    let mut import = Import::new(opened, options)?;
    prepare(&mut import);
    let mut source = Asked::new(file);
    let mut table = Vec::new();
    let mut steps = 0;
    let mut needs = 0;
    let mut before = import.progress();
    loop {
        steps += 1;
        // Every step reads a page, or gives rows, or writes a column, or goes
        // on to the next thing.
        assert!(steps < 300_000, "importing never ends: {before:?}");
        match import.step(&mut source, &mut table)? {
            Step::Working => {}
            Step::Need(ranges) => {
                needs += 1;
                assert!(!ranges.is_empty() && ranges != source.held);
                for range in &ranges {
                    assert!(range.len > 0 && range.end() <= file.len() as u64);
                }
                source.held = ranges;
            }
            Step::Restart => table.clear(),
            Step::Done => break,
        }
        let progress = import.progress();
        assert!(progress.rows <= progress.of);
        if progress.restarts == before.restarts && progress.checking == before.checking {
            assert!(progress.checking || progress.rows >= before.rows);
        }
        before = progress;
    }
    assert!(import.step(&mut source, &mut table) == Ok(Step::Done));
    Ok(Imported {
        table,
        report: import.report(),
        restarts: before.restarts,
        steps,
        needs,
    })
}

/// The cells of a table, column by column.
pub(super) fn cells(table: &[u8]) -> Vec<(String, ColumnType, Vec<Model>)> {
    let mut reader = open_table(table);
    let mut source = table;
    let mut all = Vec::new();
    for index in 0..reader.columns().len() {
        let column = reader.columns()[index].clone();
        let mut cells = Vec::new();
        for row in 0..reader.rows() {
            let mut text = String::new();
            let value = reader
                .cell(&mut source, index, row, usize::MAX, &mut text)
                .unwrap();
            cells.push(match value {
                CellValue::Blank => Model::Blank,
                CellValue::Int(value) => Model::Int(value),
                CellValue::Float(value) => Model::Float(value),
                CellValue::Text => Model::Text(text),
            });
        }
        all.push((column.name, column.kind, cells));
    }
    all
}

/// A column of a file, and what is to come of it.
struct Case {
    field: Field,
    kind: ColumnType,
    cells: Vec<Model>,
    cut_nanos: u64,
    not_finite: u64,
    out_of_range: u64,
}

/// What a value is to become.
enum Becomes {
    Cell(Model),
    CutNanos(Model),
    NotFinite,
    OutOfRange,
}

fn case(
    rng: &mut Rng,
    rows: usize,
    name: &str,
    physical: Physical,
    means: Means,
    kind: ColumnType,
    value: &mut dyn FnMut(&mut Rng) -> (Raw, Becomes),
) -> Case {
    let mut field = Field::new(name, physical, means);
    let mut case = Case {
        kind,
        cells: Vec::new(),
        cut_nanos: 0,
        not_finite: 0,
        out_of_range: 0,
        field: field.clone(),
    };
    field.optional = rng.next_below(3) > 0;
    // Blanks come now and then, in runs, or everywhere.
    let blanks = rng.next_below(4);
    let mut run = (0, false);
    // Few different values make a dictionary, and runs of codes.
    let few = rng.next_below(2) == 0;
    let mut known: Vec<(Raw, u8, Model)> = Vec::new();
    for _ in 0..rows {
        if run.0 == 0 {
            run = (
                1 + rng.next_below(if blanks == 1 { 400 } else { 3 }),
                match blanks {
                    0 => rng.next_below(10) == 0,
                    3 => rng.next_below(10) > 0,
                    _ => rng.next_below(2) == 0,
                },
            );
        }
        run.0 -= 1;
        if field.optional && run.1 {
            field.values.push(None);
            case.cells.push(Model::Blank);
            continue;
        }
        let repeated = few && known.len() >= 12;
        let (raw, status, model) = if repeated {
            known[rng.next_below(known.len() as u64) as usize].clone()
        } else {
            let (raw, becomes) = value(rng);
            let (status, model) = match becomes {
                Becomes::Cell(model) => (0, model),
                Becomes::CutNanos(model) => (1, model),
                Becomes::NotFinite => (2, Model::Blank),
                Becomes::OutOfRange => (3, Model::Blank),
            };
            known.push((raw.clone(), status, model.clone()));
            (raw, status, model)
        };
        case.cut_nanos += u64::from(status == 1);
        case.not_finite += u64::from(status == 2);
        case.out_of_range += u64::from(status == 3);
        field.values.push(Some(raw));
        case.cells.push(match model {
            Model::Text(text) if text.is_empty() => Model::Blank,
            Model::Float(0.0) => Model::Float(0.0),
            other => other,
        });
    }
    let encodings = encode::encodings(physical);
    field.encoding = encodings[rng.next_below(encodings.len() as u64) as usize];
    field.codec = CODECS[rng.next_below(CODECS.len() as u64) as usize];
    case.field = field;
    case
}

fn int(value: i64) -> Becomes {
    Becomes::Cell(Model::Int(value))
}

fn text(value: &str) -> Becomes {
    Becomes::Cell(Model::Text(value.to_string()))
}

fn low32(value: i64) -> Raw {
    Raw::Number(u64::from((value as i32).cast_unsigned()))
}

fn wide(value: i64) -> Raw {
    Raw::Number(value.cast_unsigned())
}

fn some_text(rng: &mut Rng) -> String {
    let len = rng.next_below(14);
    (0..len)
        .map(|_| ['a', 'b', ' ', 'é', '漢', '😀', '0', '\u{feff}'][rng.next_below(8) as usize])
        .collect()
}

/// Decimal digits, divided by hand.
fn naive_decimal(value: i128, scale: usize) -> String {
    let digits = value.unsigned_abs().to_string();
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (whole, rest) = digits.split_at(digits.len() - scale);
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{rest}")
    }
}

fn naive_time(nanos: i64) -> String {
    let seconds = nanos / 1_000_000_000;
    let rest = nanos % 1_000_000_000;
    let mut out = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    );
    if rest > 0 {
        let mut digits = format!("{rest:09}");
        while digits.ends_with("000") {
            digits.truncate(digits.len() - 3);
        }
        out.push('.');
        out.push_str(&digits);
    }
    out
}

fn moment(micros: i128, whole: bool) -> Becomes {
    let first = i128::from(FIRST_DAY) * i128::from(DAY_MICROS);
    let end = (i128::from(LAST_DAY) + 1) * i128::from(DAY_MICROS);
    if micros < first || micros >= end {
        return Becomes::OutOfRange;
    }
    let model = Model::Int(micros as i64);
    if whole {
        Becomes::Cell(model)
    } else {
        Becomes::CutNanos(model)
    }
}

/// A number near the ones that matter, or any at all.
fn near(rng: &mut Rng, marks: &[i64]) -> i64 {
    match rng.next_below(3) {
        0 => rng.next_u64().cast_signed() >> rng.next_below(64),
        _ => {
            let mark = marks[rng.next_below(marks.len() as u64) as usize];
            mark.wrapping_add(rng.next_below(5) as i64 - 2)
        }
    }
}

/// The columns that can be taken, each with a kind of its own.
#[expect(clippy::too_many_lines, reason = "one column after another")]
fn cases(rng: &mut Rng, rows: usize) -> Vec<Case> {
    let mut all = Vec::new();
    let integer = ColumnType::Integer;
    let date_time = ColumnType::DateTime;
    let money = |scale| ColumnType::Decimal { scale, fixed: true };
    let mut add = |rng: &mut Rng,
                   name: &str,
                   physical,
                   means,
                   kind,
                   value: &mut dyn FnMut(&mut Rng) -> (Raw, Becomes)| {
        all.push(case(rng, rows, name, physical, means, kind, value));
    };
    add(
        rng,
        "truth",
        Physical::Boolean,
        Means::Nothing,
        ColumnType::Boolean,
        &mut |rng| {
            let truth = rng.next_below(2);
            (Raw::Number(truth), int(truth as i64))
        },
    );
    let marks_32 = [0, i64::from(i32::MIN) + 2, i64::from(i32::MAX) - 2];
    for (name, means) in [
        ("int32", Means::Nothing),
        ("int32 again", Means::Converted(17, 0, 0)),
        (
            "int8",
            Means::Integer {
                bits: 8,
                signed: true,
            },
        ),
    ] {
        add(rng, name, Physical::Int32, means, integer, &mut |rng| {
            let value = i64::from(near(rng, &marks_32) as i32);
            (low32(value), int(value))
        });
    }
    for (name, means) in [
        ("uint32", Means::Converted(13, 0, 0)),
        (
            "uint32 again",
            Means::Integer {
                bits: 32,
                signed: false,
            },
        ),
    ] {
        add(rng, name, Physical::Int32, means, integer, &mut |rng| {
            let value = near(rng, &marks_32) as u32;
            (Raw::Number(u64::from(value)), int(i64::from(value)))
        });
    }
    let marks_64 = [0, i64::MIN, i64::MAX, 1 << 40];
    for (name, means) in [
        ("int64", Means::Nothing),
        ("int64 again", Means::Converted(18, 0, 0)),
    ] {
        add(rng, name, Physical::Int64, means, integer, &mut |rng| {
            let value = near(rng, &marks_64);
            (wide(value), int(value))
        });
    }
    add(
        rng,
        "uint64",
        Physical::Int64,
        Means::Converted(14, 0, 0),
        integer,
        &mut |rng| {
            let value = near(rng, &marks_64) & i64::MAX;
            (wide(value), int(value))
        },
    );
    for (name, means) in [
        ("cents", Means::Converted(5, 2, 9)),
        (
            "cents again",
            Means::Decimal {
                scale: 2,
                precision: 9,
            },
        ),
    ] {
        add(rng, name, Physical::Int32, means, money(2), &mut |rng| {
            let value = rng.next_below(2_000_000_000) as i64 - 1_000_000_000;
            (low32(value), int(value))
        });
    }
    add(
        rng,
        "long cents",
        Physical::Int64,
        Means::Converted(5, 18, 18),
        money(18),
        &mut |rng| {
            let value = near(rng, &[0, 999_999_999_999_999_997, -999_999_999_999_999_997]);
            (wide(value), int(value))
        },
    );
    for (name, len, precision) in [("fixed cents", 5, 11), ("wide cents", 16, 18)] {
        add(
            rng,
            name,
            Physical::Fixed(len),
            Means::Converted(5, 4, precision),
            money(4),
            &mut |rng| {
                let value = near(rng, &[0, 255, -255, -32_768]) >> 26;
                let bytes = i128::from(value).to_be_bytes();
                (Raw::Bytes(bytes[16 - len as usize..].to_vec()), int(value))
            },
        );
    }
    add(
        rng,
        "cents of any length",
        Physical::Bytes,
        Means::Converted(5, 0, 18),
        money(0),
        &mut |rng| {
            let value = near(rng, &[0, 127, 128, -128, -129, 32_767]) / 10;
            let bytes = i128::from(value).to_be_bytes();
            let mut from = rng.next_below(9) as usize;
            while i128::from_be_bytes({
                let mut all = [if value < 0 { 0xff } else { 0 }; 16];
                all[from..].copy_from_slice(&bytes[from..]);
                all
            }) != i128::from(value)
                || (bytes[from] >= 0x80) != (value < 0)
            {
                from -= 1;
            }
            (Raw::Bytes(bytes[from..].to_vec()), int(value))
        },
    );
    for (name, physical, scale) in [
        ("digits", Physical::Fixed(16), 10),
        ("more digits", Physical::Bytes, 0),
    ] {
        add(
            rng,
            name,
            physical,
            Means::Converted(5, scale, 38),
            ColumnType::Text,
            &mut |rng| {
                let high = u128::from(rng.next_u64()) << 64;
                let value =
                    (high | u128::from(rng.next_u64())).cast_signed() >> rng.next_below(127);
                (
                    Raw::Bytes(value.to_be_bytes().to_vec()),
                    text(&naive_decimal(value, scale as usize)),
                )
            },
        );
    }
    for (name, means) in [
        ("day", Means::Converted(6, 0, 0)),
        ("day again", Means::Logical(6)),
    ] {
        add(
            rng,
            name,
            Physical::Int32,
            means,
            ColumnType::Date,
            &mut |rng| {
                let day = i64::from(near(rng, &[0, FIRST_DAY, LAST_DAY]) as i32);
                let becomes = if (FIRST_DAY..=LAST_DAY).contains(&day) {
                    int(day)
                } else {
                    Becomes::OutOfRange
                };
                (low32(day), becomes)
            },
        );
    }
    let first = FIRST_DAY * DAY_MICROS;
    let last = (LAST_DAY + 1) * DAY_MICROS;
    for (name, means) in [
        ("millis", Means::Converted(9, 0, 0)),
        ("millis again", Means::Time(8, 1)),
    ] {
        add(rng, name, Physical::Int64, means, date_time, &mut |rng| {
            let value = near(rng, &[0, first / 1_000, last / 1_000]);
            (wide(value), moment(i128::from(value) * 1_000, true))
        });
    }
    for (name, means) in [
        ("micros", Means::Converted(10, 0, 0)),
        ("micros again", Means::Time(8, 2)),
    ] {
        add(rng, name, Physical::Int64, means, date_time, &mut |rng| {
            let value = near(rng, &[0, first, last]);
            (wide(value), moment(i128::from(value), true))
        });
    }
    add(
        rng,
        "nanos",
        Physical::Int64,
        Means::Time(8, 3),
        date_time,
        &mut |rng| {
            let mut value = near(rng, &[0, i64::MIN, i64::MAX, -1_000, 1_000]);
            if rng.next_below(2) == 0 {
                value = value / 1_000 * 1_000;
            }
            let micros = i128::from(value).div_euclid(1_000);
            (wide(value), moment(micros, value % 1_000 == 0))
        },
    );
    add(
        rng,
        "int96",
        Physical::Int96,
        Means::Nothing,
        date_time,
        &mut |rng| {
            let nanos = match rng.next_below(3) {
                0 => rng.next_u64().cast_signed(),
                1 => rng.next_below(86_400_000_000) as i64 * 1_000,
                _ => rng.next_below(86_400_000_000_000) as i64,
            };
            let julian = match rng.next_below(3) {
                0 => rng.next_u64() as u32,
                _ => (2_440_588 + near(rng, &[0, FIRST_DAY, LAST_DAY])) as u32,
            };
            let mut bytes = nanos.to_le_bytes().to_vec();
            bytes.extend_from_slice(&julian.to_le_bytes());
            let days = i128::from(julian) - 2_440_588;
            let all = days * 86_400_000_000_000 + i128::from(nanos);
            let becomes = moment(all.div_euclid(1_000), all.rem_euclid(1_000) == 0);
            (Raw::Bytes(bytes), becomes)
        },
    );
    let finite = |number: f64| {
        if number.is_finite() {
            Becomes::Cell(Model::Float(number))
        } else {
            Becomes::NotFinite
        }
    };
    add(
        rng,
        "float",
        Physical::Float,
        Means::Nothing,
        ColumnType::Float,
        &mut |rng| {
            let bits = match rng.next_below(4) {
                0 => [0x7fc0_0000, 0x7f80_0000, 0xff80_0000, 0x8000_0000, 1]
                    [rng.next_below(5) as usize],
                1 => 0.1_f32.to_bits(),
                _ => rng.next_u64() as u32,
            };
            (
                Raw::Number(u64::from(bits)),
                finite(f64::from(f32::from_bits(bits))),
            )
        },
    );
    add(
        rng,
        "double",
        Physical::Double,
        Means::Nothing,
        ColumnType::Float,
        &mut |rng| {
            let bits = match rng.next_below(4) {
                0 => [
                    f64::NAN.to_bits(),
                    f64::INFINITY.to_bits(),
                    f64::NEG_INFINITY.to_bits(),
                    (-0.0_f64).to_bits(),
                    1,
                ][rng.next_below(5) as usize],
                _ => rng.next_u64(),
            };
            (Raw::Number(bits), finite(f64::from_bits(bits)))
        },
    );
    add(
        rng,
        "half",
        Physical::Fixed(2),
        Means::Logical(15),
        ColumnType::Float,
        &mut |rng| {
            let half = rng.next_u64() as u16;
            (
                Raw::Bytes(half.to_le_bytes().to_vec()),
                finite(naive_half(half)),
            )
        },
    );
    for (name, means) in [
        ("text", Means::Converted(0, 0, 0)),
        ("text again", Means::Logical(1)),
        ("enum", Means::Converted(4, 0, 0)),
        ("enum again", Means::Logical(4)),
        ("json", Means::Converted(19, 0, 0)),
        ("json again", Means::Logical(12)),
        ("bytes", Means::Nothing),
    ] {
        add(
            rng,
            name,
            Physical::Bytes,
            means,
            ColumnType::Text,
            &mut |rng| {
                let value = some_text(rng);
                (Raw::Bytes(value.clone().into_bytes()), text(&value))
            },
        );
    }
    add(
        rng,
        "uuid",
        Physical::Fixed(16),
        Means::Logical(14),
        ColumnType::Text,
        &mut |rng| {
            let bytes: Vec<u8> = (0..16).map(|_| rng.next_u64() as u8).collect();
            let mut shown = String::new();
            for (index, byte) in bytes.iter().enumerate() {
                shown.push_str(&format!("{byte:02x}"));
                if [3, 5, 7, 9].contains(&index) {
                    shown.push('-');
                }
            }
            (Raw::Bytes(bytes), text(&shown))
        },
    );
    for (name, physical, means, units) in [
        (
            "time",
            Physical::Int32,
            Means::Converted(7, 0, 0),
            1_000_i64,
        ),
        ("time again", Physical::Int32, Means::Time(7, 1), 1_000),
        (
            "time in micros",
            Physical::Int64,
            Means::Converted(8, 0, 0),
            1_000_000,
        ),
        (
            "time in nanos",
            Physical::Int64,
            Means::Time(7, 3),
            1_000_000_000,
        ),
    ] {
        add(rng, name, physical, means, ColumnType::Text, &mut |rng| {
            let day = 86_400 * units;
            let value = match rng.next_below(4) {
                0 => near(rng, &[0, day]) as i32 as i64,
                1 => rng.next_below(86_400) as i64 * units,
                _ => rng.next_below(day as u64) as i64,
            };
            let raw = if physical == Physical::Int32 {
                low32(value)
            } else {
                wide(value)
            };
            let becomes = if (0..=day).contains(&value) {
                text(&naive_time(value * (1_000_000_000 / units)))
            } else {
                Becomes::OutOfRange
            };
            (raw, becomes)
        });
    }
    add(
        rng,
        "nothing",
        Physical::Int32,
        Means::Logical(11),
        ColumnType::Text,
        &mut |_| (Raw::Number(7), Becomes::Cell(Model::Blank)),
    );
    all
}

fn naive_half(half: u16) -> f64 {
    let sign = if half & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = i32::from(half >> 10 & 31);
    let fraction = f64::from(half & 0x3ff);
    let mut power = 1.0;
    for _ in 0..(exponent - 15).abs() {
        power *= 2.0;
    }
    if exponent < 15 {
        power = 1.0 / power;
    }
    match exponent {
        0 => sign * fraction / 1_024.0 / 16_384.0,
        31 if fraction == 0.0 => sign * f64::INFINITY,
        31 => f64::NAN,
        _ => sign * (1.0 + fraction / 1_024.0) * power,
    }
}

fn some_layout(rng: &mut Rng, rows: usize) -> Layout {
    let mut groups = Vec::new();
    let mut left = rows;
    while rng.next_below(3) > 0 {
        let count = rng.next_below(left as u64 + 1) as usize;
        groups.push(count);
        left -= count;
    }
    Layout {
        groups,
        page_rows: [1, 7, 64, 300, 5_000][rng.next_below(5) as usize],
        version_2: rng.next_below(2) == 0,
        checksums: rng.next_below(2) == 0,
    }
}

fn compare(cases: &[Case], imported: &Imported) {
    let cells = cells(&imported.table);
    assert_eq!(cells.len(), cases.len());
    assert_eq!(imported.report.columns.len(), cases.len());
    for ((case, (name, kind, cells)), report) in
        cases.iter().zip(cells).zip(&imported.report.columns)
    {
        assert_eq!(name, case.field.name);
        assert_eq!(kind, case.kind, "{name}");
        assert_eq!(cells.len(), case.cells.len(), "{name}");
        for (row, (cell, expected)) in cells.iter().zip(&case.cells).enumerate() {
            assert_eq!(
                cell, expected,
                "{name}, row {row}, {:?}, encoding {}, {:?}",
                case.field.physical, case.field.encoding, case.field.values[row]
            );
        }
        let blanks = case
            .cells
            .iter()
            .filter(|cell| **cell == Model::Blank)
            .count();
        assert_eq!(report.name, name);
        assert_eq!(report.kind, kind);
        assert_eq!(report.blanks, blanks as u64, "{name}");
        assert_eq!(report.cut_nanos, case.cut_nanos, "{name}");
        assert_eq!(report.not_finite, case.not_finite, "{name}");
        assert_eq!(report.out_of_range, case.out_of_range, "{name}");
        assert_eq!(report.cut_texts, 0, "{name}");
    }
}

#[test]
fn every_type_in_every_encoding_is_imported() {
    let mut rng = Rng::from_seed(101);
    for round in 0..rounds(20) {
        let rows = [0, 1, 2, 7, 64, 255, 256, 257, 600, 1_000][round % 10];
        let cases = cases(&mut rng, rows);
        let fields: Vec<Field> = cases.iter().map(|case| case.field.clone()).collect();
        let layout = some_layout(&mut rng, rows);
        let file = encode::write(&fields, &layout, &mut rng);
        let imported = import_otherwise(&file, &mut rng).unwrap();
        assert_eq!(imported.restarts, 0);
        assert_eq!(imported.report.rows, rows as u64);
        assert!(imported.report.skipped.is_empty());
        compare(&cases, &imported);
    }
}

#[test]
fn every_encoding_with_every_codec() {
    let mut rng = Rng::from_seed(102);
    for codec in CODECS {
        for version_2 in [false, true] {
            let rows = rounds(130);
            let mut all = Vec::new();
            for mut case in cases(&mut rng, rows) {
                for encoding in encode::encodings(case.field.physical) {
                    case.field.codec = codec;
                    case.field.encoding = encoding;
                    all.push(Case {
                        field: case.field.clone(),
                        cells: case.cells.clone(),
                        ..case
                    });
                }
            }
            let fields: Vec<Field> = all.iter().map(|case| case.field.clone()).collect();
            let layout = Layout {
                version_2,
                ..some_layout(&mut rng, rows)
            };
            let file = encode::write(&fields, &layout, &mut rng);
            compare(&all, &import_otherwise(&file, &mut rng).unwrap());
        }
    }
}

#[test]
fn more_rows_than_a_group_has() {
    let mut rng = Rng::from_seed(103);
    let rows = 100_000;
    let wanted = ["int64", "text", "double", "truth", "nanos", "uint32"];
    let cases: Vec<Case> = cases(&mut rng, rows)
        .into_iter()
        .filter(|case| wanted.contains(&case.field.name.as_str()))
        .collect();
    let fields: Vec<Field> = cases.iter().map(|case| case.field.clone()).collect();
    let layout = Layout {
        groups: vec![30_000, 0, 20_000],
        page_rows: 9_000,
        version_2: false,
        checksums: true,
    };
    let file = encode::write(&fields, &layout, &mut rng);
    let imported = import(&file, &Options::default()).unwrap();
    compare(&cases, &imported);
    // Bytes are asked for in larger pieces than pages are.
    assert!(imported.needs < 200, "{} times", imported.needs);
    assert!(imported.steps < 5_000, "{} steps", imported.steps);
}

#[test]
fn only_the_columns_that_were_asked_for() {
    let mut rng = Rng::from_seed(104);
    let cases = cases(&mut rng, 50);
    let fields: Vec<Field> = cases.iter().map(|case| case.field.clone()).collect();
    let file = encode::write(&fields, &Layout::simple(), &mut rng);
    let opened = open_file(&file).unwrap();
    let names: Vec<&str> = opened.columns().map(|column| column.name).collect();
    assert_eq!(names.len(), cases.len());
    assert!(opened.columns().any(|column| column.bytes > 0));
    let options = Options {
        columns: Some(vec![3, 0]),
    };
    let imported = import(&file, &options).unwrap();
    let cells = cells(&imported.table);
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].0, names[0]);
    assert_eq!(cells[1].0, names[3]);
    for columns in [vec![], vec![999]] {
        let options = Options {
            columns: Some(columns),
        };
        assert_eq!(import(&file, &options).err(), Some(ParquetError::NoColumns));
    }
}

fn numbers(name: &str, means: Means, values: &[u64]) -> Field {
    let mut field = Field::new(name, Physical::Int64, means);
    field.values = values
        .iter()
        .map(|value| Some(Raw::Number(*value)))
        .collect();
    field
}

fn texts(name: &str, values: &[&[u8]]) -> Field {
    let mut field = Field::new(name, Physical::Bytes, Means::Nothing);
    field.values = values
        .iter()
        .map(|value| Some(Raw::Bytes(value.to_vec())))
        .collect();
    field
}

#[test]
fn columns_that_turn_out_otherwise() {
    let mut rng = Rng::from_seed(105);
    let rows = 70_000_usize;
    let last = rows - 1;
    let count: Vec<u64> = (0..rows as u64).collect();
    let mut large = count.clone();
    large[last] = u64::MAX;
    let mut early = count.clone();
    early[5] = 1 << 63;
    let mut words: Vec<&[u8]> = vec![b"word"; rows];
    let good = texts("words", &words);
    words[66_000] = b"\xff\xfe";
    let mut bad = texts("bytes", &words);
    bad.encoding = super::meta::RLE_DICTIONARY;
    let mut wide = Field::new(
        "decimal",
        Physical::Fixed(16),
        Means::Decimal {
            scale: 2,
            precision: 18,
        },
    );
    wide.values = count
        .iter()
        .map(|value| Some(Raw::Bytes(i128::from(*value).to_be_bytes().to_vec())))
        .collect();
    wide.values[69_000] = Some(Raw::Bytes((1_i128 << 80).to_be_bytes().to_vec()));
    let unsigned = Means::Converted(14, 0, 0);
    let fields = [
        numbers("small", unsigned, &count),
        numbers("large", unsigned, &large),
        good,
        numbers("early", unsigned, &early),
        bad,
        wide,
    ];
    let layout = Layout {
        groups: vec![30_000],
        page_rows: 20_000,
        ..Layout::simple()
    };
    let file = encode::write(&fields, &layout, &mut rng);
    let imported = import(&file, &Options::default()).unwrap();
    // All of it is found out the first time.
    assert_eq!(imported.restarts, 1);
    let kinds: Vec<(&str, ColumnType)> = imported
        .report
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("small", ColumnType::Integer),
            ("large", ColumnType::Text),
            ("words", ColumnType::Text),
            ("early", ColumnType::Text),
            ("decimal", ColumnType::Text),
        ]
    );
    assert_eq!(imported.report.skipped.len(), 1);
    assert_eq!(imported.report.skipped[0].name, "bytes");
    assert_eq!(imported.report.skipped[0].reason, SkipReason::NotText);
    let cells = cells(&imported.table);
    let shown = |text: &str| Model::Text(text.to_string());
    assert_eq!(cells[0].2[last], Model::Int(last as i64));
    assert_eq!(cells[1].2[last], shown("18446744073709551615"));
    assert_eq!(cells[1].2[7], shown("7"));
    assert_eq!(cells[3].2[5], shown("9223372036854775808"));
    assert_eq!(cells[4].2[69_000], shown("12089258196146291747061.76"));
    assert_eq!(cells[4].2[7], shown("0.07"));
    assert!(
        imported
            .report
            .columns
            .iter()
            .all(|column| column.blanks == 0)
    );
}

#[test]
fn nothing_is_left_when_no_column_is_text() {
    let mut rng = Rng::from_seed(106);
    let fields = [texts("bytes", &[b"a", b"\xff"])];
    let file = encode::write(&fields, &Layout::simple(), &mut rng);
    assert_eq!(
        import(&file, &Options::default()).err(),
        Some(ParquetError::NoColumns)
    );
}

#[test]
fn columns_that_cannot_be_taken_are_named() {
    let mut rng = Rng::from_seed(107);
    let one = |name: &str, physical: Physical, means: Means| {
        let mut field = Field::new(name, physical, means);
        let raw = if physical.is_number() {
            Raw::Number(1)
        } else {
            Raw::Bytes(vec![b'x'; physical.width().unwrap_or(3)])
        };
        field.values = vec![Some(raw); 3];
        field
    };
    let mut nested = one("list", Physical::Int32, Means::Nothing);
    nested.nested = true;
    let mut brotli = one("brotli", Physical::Int32, Means::Nothing);
    brotli.claimed_codec = Some(4);
    let mut odd = one("odd", Physical::Int32, Means::Nothing);
    odd.claimed_encoding = Some(10);
    let mut external = one("elsewhere", Physical::Int32, Means::Nothing);
    external.external = true;
    let decimal = |scale, precision| Means::Decimal { scale, precision };
    let skipped = [
        (external, SkipReason::External),
        (nested, SkipReason::Nested),
        (brotli, SkipReason::Codec(4)),
        (odd, SkipReason::Encoding(10)),
        (
            one("interval", Physical::Fixed(12), Means::Converted(21, 0, 0)),
            SkipReason::Interval,
        ),
        (
            one("bson", Physical::Bytes, Means::Converted(20, 0, 0)),
            SkipReason::Bson,
        ),
        (
            one("bson again", Physical::Bytes, Means::Logical(13)),
            SkipReason::Bson,
        ),
        (
            one("shape", Physical::Bytes, Means::Logical(17)),
            SkipReason::Geometry,
        ),
        (
            one("variant", Physical::Bytes, Means::Logical(16)),
            SkipReason::Variant,
        ),
        (
            one("new", Physical::Bytes, Means::Logical(40)),
            SkipReason::UnknownType,
        ),
        (
            one("old", Physical::Bytes, Means::Converted(22, 0, 0)),
            SkipReason::UnknownType,
        ),
        (
            one("binary", Physical::Fixed(7), Means::Nothing),
            SkipReason::Binary,
        ),
        (
            one("day", Physical::Int64, Means::Logical(6)),
            SkipReason::InvalidType,
        ),
        (
            one("uuid", Physical::Fixed(15), Means::Logical(14)),
            SkipReason::InvalidType,
        ),
        (
            one("scale", Physical::Int32, decimal(5, 4)),
            SkipReason::InvalidType,
        ),
        (
            one("negative", Physical::Int32, decimal(-1, 4)),
            SkipReason::InvalidType,
        ),
        (
            one("older", Physical::Int32, Means::Converted(5, -1, 4)),
            SkipReason::InvalidType,
        ),
        (
            one("precision", Physical::Bytes, decimal(2, 309)),
            SkipReason::InvalidType,
        ),
        (
            one("length", Physical::Fixed(129), decimal(2, 20)),
            SkipReason::InvalidType,
        ),
    ];
    let mut fields: Vec<Field> = skipped.iter().map(|(field, _)| field.clone()).collect();
    fields.insert(4, one("taken", Physical::Int32, Means::Nothing));
    fields.push(one("last", Physical::Bytes, Means::Nothing));
    let file = encode::write(&fields, &Layout::simple(), &mut rng);
    let opened = open_file(&file).unwrap();
    let names: Vec<&str> = opened.columns().map(|column| column.name).collect();
    assert_eq!(names, ["taken", "last"]);
    let mut reasons: Vec<(&str, SkipReason)> = opened
        .skipped()
        .iter()
        .map(|column| (column.name.as_str(), column.reason))
        .collect();
    let mut expected: Vec<(&str, SkipReason)> = skipped
        .iter()
        .map(|(field, reason)| (field.name.as_str(), *reason))
        .collect();
    reasons.sort_by_key(|(name, _)| *name);
    expected.sort_by_key(|(name, _)| *name);
    assert_eq!(reasons, expected);
    let imported = import(&file, &Options::default()).unwrap();
    assert_eq!(imported.report.skipped.len(), expected.len());
    let cells = cells(&imported.table);
    assert_eq!(cells[0].2, vec![Model::Int(1); 3]);
    assert_eq!(cells[1].2, vec![Model::Text("xxx".to_string()); 3]);
}

#[test]
fn long_texts_are_cut() {
    let mut rng = Rng::from_seed(108);
    // The last character before the limit has three bytes, and spans it.
    let mut long = "a".repeat(MAX_TEXT_BYTES - 1);
    long.push_str("漢字");
    let just = "b".repeat(MAX_TEXT_BYTES);
    let mut broken = long.clone().into_bytes();
    broken.push(0xff);
    for encoding in encode::encodings(Physical::Bytes) {
        let mut field = texts("text", &[long.as_bytes(), b"", just.as_bytes(), &broken]);
        field.encoding = encoding;
        let file = encode::write(&[field], &Layout::simple(), &mut rng);
        let imported = import(&file, &Options::default()).unwrap();
        let cells = &cells(&imported.table)[0].2;
        assert!(cells[0] == Model::Text("a".repeat(MAX_TEXT_BYTES - 1)));
        assert_eq!(cells[1], Model::Blank);
        assert!(cells[2] == Model::Text(just.clone()));
        // What is cut off is not even looked at.
        assert!(cells[3] == cells[0]);
        assert_eq!(imported.report.columns[0].cut_texts, 2);
        assert_eq!(imported.report.columns[0].blanks, 1);
    }
}

// Takes more memory than tests in a browser should.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn texts_that_are_too_long_for_a_group() {
    let mut rng = Rng::from_seed(109);
    // Short ones at first, so that the size of a group is guessed wrong.
    let long = "x".repeat(90_000);
    let mut values: Vec<String> = (0..4_000).map(|row| format!("{row}")).collect();
    values.extend((0..1_900).map(|row| format!("{row}{long}")));
    let all: Vec<&[u8]> = values.iter().map(String::as_bytes).collect();
    let layout = Layout {
        page_rows: 200,
        ..Layout::simple()
    };
    let file = encode::write(&[texts("text", &all)], &layout, &mut rng);
    let imported = import(&file, &Options::default()).unwrap();
    assert!(
        (1..=3).contains(&imported.restarts),
        "{}",
        imported.restarts
    );
    let cells = &cells(&imported.table)[0].2;
    assert_eq!(cells.len(), values.len());
    for (cell, value) in cells.iter().zip(&values) {
        assert!(*cell == Model::Text(value.clone()));
    }
}

#[test]
fn streams_that_hold_too_much_let_go() {
    // Every group is in the middle of a page, which is read again for it.
    let mut rng = Rng::from_seed(110);
    let rows = 3_000;
    let mut all = Vec::new();
    for mut case in cases(&mut rng, rows) {
        for encoding in encode::encodings(case.field.physical) {
            case.field.encoding = encoding;
            all.push(Case {
                field: case.field.clone(),
                cells: case.cells.clone(),
                ..case
            });
        }
    }
    // One of each way to store values is enough.
    all.retain(|case| {
        ["int64", "truth", "text", "uuid", "half", "uint32"].contains(&case.field.name.as_str())
    });
    let fields: Vec<Field> = all.iter().map(|case| case.field.clone()).collect();
    for version_2 in [false, true] {
        let layout = Layout {
            groups: vec![rows - 700],
            page_rows: 2_000,
            version_2,
            checksums: false,
        };
        let file = encode::write(&fields, &layout, &mut rng);
        let usual = import_with(&file, &Options::default(), |import| {
            import.group_rows = 256;
        })
        .unwrap();
        let forgetful = import_with(&file, &Options::default(), |import| {
            import.group_rows = 256;
            import.stream_budget = 0;
        })
        .unwrap();
        compare(&all, &usual);
        compare(&all, &forgetful);
        assert!(
            forgetful.needs > usual.needs * 4,
            "{} times, {} times",
            forgetful.needs,
            usual.needs
        );
        assert!(usual.table == forgetful.table);
    }
}

#[test]
fn entries_that_no_row_has_are_not_taken() {
    // Nor those that are the same as others, or blank.
    let mut rng = Rng::from_seed(115);
    let raw = |text: &str| Raw::Bytes(text.as_bytes().to_vec());
    let rows = 2_000_usize;
    let words: Vec<String> = (0..rows).map(|row| format!("word {}", row % 30)).collect();
    let all: Vec<&[u8]> = words.iter().map(String::as_bytes).collect();
    let mut field = texts("text", &all);
    field.encoding = super::meta::RLE_DICTIONARY;
    field.unused = vec![raw("never"), raw(""), raw("word 7"), raw("word 7")];
    let layout = Layout {
        groups: vec![700, 900],
        page_rows: 300,
        ..Layout::simple()
    };
    let file = encode::write(&[field], &layout, &mut rng);
    for budget in [0, 1 << 20] {
        let imported = import_with(&file, &Options::default(), |import| {
            import.group_rows = 256;
            import.stream_budget = budget;
        })
        .unwrap();
        let reader = open_table(&imported.table);
        let dictionary = reader.columns()[0].dictionary.unwrap();
        assert_eq!(dictionary.entries, 30);
        let cells = &cells(&imported.table)[0].2;
        for (cell, word) in cells.iter().zip(&words) {
            assert_eq!(*cell, Model::Text(word.clone()));
        }
    }
}

// Takes more memory than tests in a browser should.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_dictionary_that_the_table_gives_up_on() {
    use crate::layout::Encoding;

    // The texts of the first chunk come again in the last one, when the table
    // takes no more texts into its dictionary; and the first half of every
    // chunk comes again in its second, also where that happens.
    let mut rng = Rng::from_seed(116);
    let filler = "f".repeat(4_000);
    let word = |chunk: usize, row: usize| format!("{} {} {filler}", chunk % 5, row % 7_000);
    let (chunks, rows) = (6_usize, 14_000_usize);
    let values: Vec<String> = (0..chunks * rows)
        .map(|at| word(at / rows, at % rows))
        .collect();
    let all: Vec<&[u8]> = values.iter().map(String::as_bytes).collect();
    let mut field = texts("text", &all);
    field.encoding = super::meta::RLE_DICTIONARY;
    let layout = Layout {
        groups: vec![rows; chunks - 1],
        page_rows: 6_000,
        ..Layout::simple()
    };
    let file = encode::write(&[field], &layout, &mut rng);
    let imported = import(&file, &Options::default()).unwrap();
    let mut reader = open_table(&imported.table);
    let encodings: Vec<Encoding> = reader
        .directory()
        .blocks
        .iter()
        .map(|block| block.encoding)
        .collect();
    assert_eq!(encodings.first(), Some(&Encoding::Packed));
    assert_eq!(encodings.last(), Some(&Encoding::Text));
    let mut source = imported.table.as_slice();
    let mut text = String::new();
    for (row, value) in values.iter().enumerate().step_by(37) {
        text.clear();
        let cell = reader.cell(&mut source, 0, row as u32, usize::MAX, &mut text);
        assert_eq!(cell, Ok(CellValue::Text));
        assert!(text == *value, "row {row}");
    }
}

/// Imports with groups of at most so many rows and so much text.
fn import_in(file: &[u8], group_rows: u32, most_text: usize, budget: usize) -> Imported {
    import_with(file, &Options::default(), |import| {
        import.group_rows = group_rows;
        import.most_text = most_text;
        import.stream_budget = budget;
    })
    .unwrap()
}

fn group_rows(imported: &Imported) -> u32 {
    open_table(&imported.table).directory().group_rows
}

#[test]
fn dictionaries_of_the_file_are_not_text_of_a_group() {
    // Every chunk has a dictionary of the same hundred long texts, of which
    // the table keeps one. They are read again and again: several for each
    // group, and for every group when nothing is kept in between.
    let mut rng = Rng::from_seed(117);
    let filler = "g".repeat(2_000);
    let values: Vec<String> = (0..2_400)
        .map(|row| format!("{} {filler}", row % 100))
        .collect();
    let all: Vec<&[u8]> = values.iter().map(String::as_bytes).collect();
    let fields: Vec<Field> = ["a", "b", "c"]
        .into_iter()
        .map(|name| {
            let mut field = texts(name, &all);
            field.encoding = super::meta::RLE_DICTIONARY;
            field
        })
        .collect();
    let layout = Layout {
        groups: vec![200; 11],
        page_rows: 100,
        ..Layout::simple()
    };
    let file = encode::write(&fields, &layout, &mut rng);
    for budget in [0, 1 << 20] {
        let imported = import_in(&file, 512, 1 << 20, budget);
        assert_eq!(imported.restarts, 0);
        assert_eq!(group_rows(&imported), 512);
        for (_, _, cells) in cells(&imported.table) {
            for (cell, value) in cells.iter().zip(&values) {
                assert!(*cell == Model::Text(value.clone()));
            }
        }
    }
}

#[test]
fn texts_of_a_group_are_counted_wherever_they_come_from() {
    let mut rng = Rng::from_seed(118);
    let filler = "h".repeat(2_000);
    // Different texts at first, so that the table has no dictionary; then
    // five long ones, which the file has in a dictionary.
    let values: Vec<String> = (0..19_500)
        .map(|row| {
            if row < 17_000 {
                format!("{row}")
            } else {
                format!("{} {filler}", row % 5)
            }
        })
        .collect();
    let all: Vec<&[u8]> = values.iter().map(String::as_bytes).collect();
    let layout = Layout {
        groups: vec![17_000],
        page_rows: 4_000,
        ..Layout::simple()
    };
    for encoding in encode::encodings(Physical::Bytes) {
        let fields: Vec<Field> = ["a", "b"]
            .into_iter()
            .map(|name| {
                let mut field = texts(name, &all);
                field.encoding = encoding;
                field
            })
            .collect();
        let file = encode::write(&fields, &layout, &mut rng);
        // A group of 1,024 rows would have four megabytes.
        let imported = import_in(&file, 1_024, 2 << 20, 1 << 30);
        assert_eq!(imported.restarts, 1, "encoding {encoding}");
        assert_eq!(group_rows(&imported), 512, "encoding {encoding}");
        assert!(
            open_table(&imported.table).columns()[0]
                .dictionary
                .is_none()
        );
        for (_, _, cells) in cells(&imported.table) {
            for (cell, value) in cells.iter().zip(&values) {
                assert!(*cell == Model::Text(value.clone()));
            }
        }
        // As much as there is, and there is nothing to be made smaller.
        let imported = import_in(&file, 1_024, 5 << 20, 1 << 30);
        assert_eq!(imported.restarts, 0);
        assert_eq!(group_rows(&imported), 1_024);
        // Smaller than the smallest group cannot be.
        let imported = import_in(&file, 1_024, 1 << 19, 1 << 30);
        assert_eq!(group_rows(&imported), 256);
        let refused = import_with(&file, &Options::default(), |import| {
            import.group_rows = 256;
            import.most_text = 1 << 16;
        });
        assert_eq!(refused.err(), Some(ParquetError::Limit(Limit::Text)));
    }
}

#[test]
fn what_a_table_dictionary_gains_is_text_of_a_group() {
    // All different in every two rows: the table's dictionary holds them.
    let mut rng = Rng::from_seed(119);
    let filler = "k".repeat(1_000);
    let values: Vec<String> = (0..4_000)
        .map(|row| format!("{} {filler}", row / 4))
        .collect();
    let all: Vec<&[u8]> = values.iter().map(String::as_bytes).collect();
    let layout = Layout {
        groups: vec![1_500],
        page_rows: 400,
        ..Layout::simple()
    };
    for encoding in encode::encodings(Physical::Bytes) {
        let mut field = texts("a", &all);
        field.encoding = encoding;
        let file = encode::write(&[field], &layout, &mut rng);
        // A group of 1,024 rows gains 256 texts of a kilobyte.
        let imported = import_in(&file, 1_024, 300 << 10, 0);
        assert_eq!(imported.restarts, 0, "encoding {encoding}");
        let imported = import_in(&file, 1_024, 200 << 10, 0);
        assert!(imported.restarts > 0, "encoding {encoding}");
        assert!(group_rows(&imported) < 1_024);
        assert!(
            open_table(&imported.table).columns()[0]
                .dictionary
                .is_some()
        );
    }
}

#[test]
fn files_that_are_none() {
    let mut rng = Rng::from_seed(111);
    let file = encode::write(
        &[numbers("a", Means::Nothing, &[1, 2, 3])],
        &Layout::simple(),
        &mut rng,
    );
    assert!(open_file(&file).is_ok());
    for bytes in [&[][..], b"PAR1", b"PAR1PAR1", b"something else altogether"] {
        assert!(open_file(bytes).is_err(), "{bytes:?}");
    }
    let mut encrypted = file.clone();
    let end = encrypted.len() - 1;
    encrypted[end] = b'E';
    assert_eq!(open_file(&encrypted).err(), Some(ParquetError::Encrypted));
    // Metadata that begins before the file does, and that is too long.
    let mut long = file.clone();
    let at = long.len() - 8;
    long[at..at + 4].copy_from_slice(&10_000_u32.to_le_bytes());
    assert!(matches!(
        open_file(&long),
        Err(ParquetError::Damaged {
            damage: Damage::Place,
            ..
        })
    ));
    long[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        open_file(&long).err(),
        Some(ParquetError::Limit(Limit::Metadata))
    );
    // A file that cannot be read.
    struct Broken;
    impl Source for Broken {
        fn read(&mut self, _: u64, _: &mut [u8]) -> Result<(), SourceError> {
            Err(SourceError::Failed)
        }
    }
    assert_eq!(open(&mut Broken, 100).err(), Some(ParquetError::Source));
}

/// A file of one column without pages, with those numbers of rows.
fn claims(rows: i64, group_rows: i64, values: i64) -> Vec<u8> {
    let mut put = Put::default();
    put.begin_element();
    put.int32(1, 2);
    put.list(2, super::thrift::STRUCT, 2);
    put.begin_element();
    put.binary(4, b"schema");
    put.int32(5, 1);
    put.end();
    put.begin_element();
    put.int32(1, 1);
    put.int32(3, 0);
    put.binary(4, b"a");
    put.end();
    put.int64(3, rows);
    put.list(4, super::thrift::STRUCT, 1);
    put.begin_element();
    put.list(1, super::thrift::STRUCT, 1);
    put.begin_element();
    put.begin(3);
    put.int32(1, 1);
    put.int32(4, 0);
    put.int64(5, values);
    put.int64(7, 1_000);
    put.int64(9, 4);
    put.end();
    put.end();
    put.int64(3, group_rows);
    put.end();
    put.end();
    let mut file = b"PAR1".to_vec();
    encode::finish(&mut file, &put.out);
    file
}

#[test]
fn encrypted_files_are_refused() {
    // What says so in the metadata of a file, and in that of a column.
    for (in_file, field) in [(true, 8), (false, 8), (false, 9)] {
        let mut put = Put::default();
        put.begin_element();
        put.list(2, super::thrift::STRUCT, 2);
        put.begin_element();
        put.binary(4, b"schema");
        put.int32(5, 1);
        put.end();
        put.begin_element();
        put.int32(1, 1);
        put.binary(4, b"a");
        put.end();
        put.int64(3, 0);
        put.list(4, super::thrift::STRUCT, 1);
        put.begin_element();
        put.list(1, super::thrift::STRUCT, 1);
        put.begin_element();
        put.int64(2, 0);
        if !in_file {
            put.binary(field, b"secret");
        }
        put.end();
        put.int64(3, 0);
        put.end();
        if in_file {
            put.begin(field);
            put.end();
        }
        put.end();
        let mut file = b"PAR1".to_vec();
        encode::finish(&mut file, &put.out);
        assert_eq!(open_file(&file).err(), Some(ParquetError::Encrypted));
    }
}

#[test]
fn more_columns_than_a_table_has() {
    let mut rng = Rng::from_seed(114);
    let fields: Vec<Field> = (0..1_030)
        .map(|index| numbers(&format!("{index}"), Means::Nothing, &[index]))
        .collect();
    let file = encode::write(&fields, &Layout::simple(), &mut rng);
    let columns = |columns: Vec<usize>| Options {
        columns: Some(columns),
    };
    for options in [Options::default(), columns((0..1_025).collect())] {
        assert_eq!(
            import(&file, &options).err(),
            Some(ParquetError::Limit(Limit::Columns))
        );
    }
    let imported = import(&file, &columns((6..1_030).collect())).unwrap();
    let cells = cells(&imported.table);
    assert_eq!(cells.len(), 1_024);
    assert_eq!(cells[1_023].2, [Model::Int(1_029)]);
}

#[test]
fn numbers_of_rows_have_to_agree() {
    assert!(open_file(&claims(5, 5, 5)).is_ok());
    for (rows, group_rows, values) in [(5, 4, 4), (5, 5, 4), (-1, -1, -1), (5, -5, 5)] {
        assert!(matches!(
            open_file(&claims(rows, group_rows, values)),
            Err(ParquetError::Damaged { .. })
        ));
    }
    let most = MAX_ROWS as i64;
    assert!(open_file(&claims(most, most, most)).is_ok());
    assert_eq!(
        open_file(&claims(most + 1, most + 1, most + 1)).err(),
        Some(ParquetError::Limit(Limit::Rows))
    );
    assert!(open_file(&claims(i64::MAX, i64::MAX, i64::MAX)).is_err());
    // Pages that are not there.
    assert!(matches!(
        import(&claims(5, 5, 5), &Options::default()).err(),
        Some(ParquetError::Damaged {
            column: Some(0),
            ..
        })
    ));
}

/// A file of numbers of 64 bits, and of columns that are said to lie there:
/// from which byte on, how many bytes, how many rows.
fn said_to_lie(pages: &[u8], columns: &[(i64, i64)], rows: i64) -> Vec<u8> {
    let mut file = b"PAR1".to_vec();
    file.extend_from_slice(pages);
    let mut put = Put::default();
    put.begin_element();
    put.list(2, super::thrift::STRUCT, 1 + columns.len());
    put.begin_element();
    put.binary(4, b"schema");
    put.int32(5, columns.len() as i64);
    put.end();
    for _ in columns {
        put.begin_element();
        put.int32(1, 2);
        put.int32(3, 0);
        put.binary(4, b"a");
        put.end();
    }
    put.int64(3, rows);
    put.list(4, super::thrift::STRUCT, 1);
    put.begin_element();
    put.list(1, super::thrift::STRUCT, columns.len());
    for (at, len) in columns {
        put.begin_element();
        put.begin(3);
        put.int32(4, 0);
        put.int64(5, rows);
        put.int64(7, *len);
        put.int64(9, 4 + at);
        put.end();
        put.end();
    }
    put.int64(3, rows);
    put.end();
    put.end();
    encode::finish(&mut file, &put.out);
    file
}

/// A page of so many numbers of 64 bits.
fn number_page(values: i64) -> Vec<u8> {
    let mut page = Put::default();
    page.begin_element();
    data_header(values * 8, values * 8, values, 0)(&mut page);
    page.end();
    let mut page = page.out;
    for value in 0..values {
        page.extend_from_slice(&value.to_le_bytes());
    }
    page
}

#[test]
fn pages_of_other_columns_are_not_read() {
    let page = number_page(50);
    let len = page.len() as i64;
    let pages = [page.clone(), page.clone(), page.clone(), page].concat();
    let whole = said_to_lie(&pages, &[(0, 2 * len), (2 * len, 2 * len)], 100);
    assert!(import(&whole, &Options::default()).is_ok());
    // The first column says that it ends before its second page, which would
    // then be one of another.
    let place = |column| {
        Some(ParquetError::Damaged {
            column: Some(column),
            damage: Damage::Place,
        })
    };
    let short = said_to_lie(&pages, &[(0, len), (len, 3 * len)], 100);
    assert_eq!(import(&short, &Options::default()).err(), place(0));
    // However short the pages are.
    let tiny = number_page(2);
    let len_of_tiny = tiny.len() as i64;
    let tinies = [tiny.clone(), tiny.clone(), tiny].concat();
    let short = said_to_lie(
        &tinies,
        &[(0, len_of_tiny), (len_of_tiny, 2 * len_of_tiny)],
        4,
    );
    assert_eq!(import(&short, &Options::default()).err(), place(0));
    // Some writers forget the start of the dictionary when they count.
    let forgetful = said_to_lie(&pages, &[(0, 2 * len - 30), (2 * len, 2 * len)], 100);
    assert!(import(&forgetful, &Options::default()).is_ok());
    // Nobody forgets that much.
    let cut = said_to_lie(&pages, &[(0, 2 * len), (2 * len, len + 200)], 100);
    assert_eq!(import(&cut, &Options::default()).err(), place(1));
}

/// A file whose one column has that page.
fn one_page(
    physical: i64,
    optional: bool,
    rows: i64,
    header: impl FnOnce(&mut Put),
    body: &[u8],
) -> Vec<u8> {
    let mut page = Put::default();
    page.begin_element();
    header(&mut page);
    page.end();
    let mut file = b"PAR1".to_vec();
    file.extend(page.out);
    file.extend_from_slice(body);
    let len = file.len() as i64 - 4;
    let mut put = Put::default();
    put.begin_element();
    put.list(2, super::thrift::STRUCT, 2);
    put.begin_element();
    put.binary(4, b"schema");
    put.int32(5, 1);
    put.end();
    put.begin_element();
    put.int32(1, physical);
    put.int32(3, i64::from(optional));
    put.binary(4, b"a");
    put.end();
    put.int64(3, rows);
    put.list(4, super::thrift::STRUCT, 1);
    put.begin_element();
    put.list(1, super::thrift::STRUCT, 1);
    put.begin_element();
    put.begin(3);
    put.int32(4, 1);
    put.int64(5, rows);
    put.int64(7, len);
    put.int64(9, 4);
    put.end();
    put.end();
    put.int64(3, rows);
    put.end();
    put.end();
    encode::finish(&mut file, &put.out);
    file
}

fn data_header(len: i64, packed: i64, values: i64, encoding: i64) -> impl FnOnce(&mut Put) {
    move |put| {
        put.int32(1, 0);
        put.int32(2, len);
        put.int32(3, packed);
        put.begin(5);
        put.int32(1, values);
        put.int32(2, encoding);
        put.int32(3, 3);
        put.int32(4, 3);
        put.end();
    }
}

#[test]
fn pages_that_promise_too_much() {
    let failure = |file: &[u8]| import(file, &Options::default()).err();
    let damaged = |damage| {
        Some(ParquetError::Damaged {
            column: Some(0),
            damage,
        })
    };
    let snappy = super::snappy::tests::compress(&[0; 8]);
    let len = snappy.len() as i64;
    let good = one_page(1, false, 2, data_header(8, len, 2, 0), &snappy);
    assert_eq!(failure(&good), None);
    // A page of a gigabyte, in a file of some bytes.
    let large = one_page(1, false, 2, data_header(1 << 30, len, 2, 0), &snappy);
    assert_eq!(failure(&large), Some(ParquetError::Limit(Limit::Page)));
    let large = one_page(1, false, 2, data_header(8, 1 << 30, 2, 0), &snappy);
    assert_eq!(failure(&large), Some(ParquetError::Limit(Limit::Page)));
    // One that is longer than the file, one that unpacks to something else,
    // lengths that are negative.
    let long = one_page(1, false, 2, data_header(8, 5_000, 2, 0), &snappy);
    assert_eq!(failure(&long), damaged(Damage::Place));
    let other = one_page(1, false, 2, data_header(9, len, 2, 0), &snappy);
    assert_eq!(failure(&other), damaged(Damage::Packed));
    let negative = one_page(1, false, 2, data_header(-8, len, 2, 0), &snappy);
    assert_eq!(failure(&negative), damaged(Damage::Page));
    // More rows than there are, fewer, more than there are values.
    let more = one_page(1, false, 2, data_header(8, len, 3, 0), &snappy);
    assert_eq!(failure(&more), damaged(Damage::Count));
    let fewer = one_page(1, false, 2, data_header(8, len, 1, 0), &snappy);
    assert!(matches!(
        failure(&fewer),
        Some(ParquetError::Damaged { .. })
    ));
    let values = one_page(1, false, 3, data_header(8, len, 3, 0), &snappy);
    assert_eq!(failure(&values), damaged(Damage::Values));
    // A dictionary that is not there, an encoding that is none, one that is
    // not for numbers.
    for encoding in [8, 2, 77, -1, 6, 7, 3, 4] {
        let file = one_page(1, false, 2, data_header(8, len, 2, encoding), &snappy);
        assert_eq!(failure(&file), damaged(Damage::Encoding), "{encoding}");
    }
    // Levels that are longer than the page.
    let file = one_page(
        1,
        true,
        2,
        data_header(8, len, 2, 0),
        &super::snappy::tests::compress(&[0xff; 8]),
    );
    assert_eq!(failure(&file), damaged(Damage::Levels));
    // A dictionary with more entries than bytes.
    let file = one_page(
        1,
        false,
        2,
        |put| {
            put.int32(1, 2);
            put.int32(2, 8);
            put.int32(3, len);
            put.begin(7);
            put.int32(1, 3);
            put.int32(2, 0);
            put.end();
        },
        &snappy,
    );
    assert_eq!(failure(&file), damaged(Damage::Dictionary));
    // A page of nothing, which takes long to unpack.
    let zeros = super::codec::tests::pack(super::codec::Codec::Snappy, &vec![0; 1 << 20]);
    let nothing = data_header(1 << 20, zeros.len() as i64, 0, 0);
    let file = one_page(1, false, 2, nothing, &zeros);
    assert_eq!(failure(&file), damaged(Damage::Page));
    // A dictionary of a great many blank texts, and one that is too long.
    let dictionary = |len: i64, packed: i64, values: i64| {
        move |put: &mut Put| {
            put.int32(1, 2);
            put.int32(2, len);
            put.int32(3, packed);
            put.begin(7);
            put.int32(1, values);
            put.int32(2, 0);
            put.end();
        }
    };
    let many = 5_000_000_i64;
    let zeros = super::snappy::tests::compress(&vec![0; 4 * many as usize]);
    let file = one_page(
        6,
        false,
        2,
        dictionary(4 * many, zeros.len() as i64, many),
        &zeros,
    );
    assert_eq!(failure(&file), Some(ParquetError::Limit(Limit::Page)));
    let file = one_page(6, false, 2, dictionary(200 << 20, 100, 3), &zeros);
    assert_eq!(failure(&file), Some(ParquetError::Limit(Limit::Page)));
    // Levels of a newer page that are longer than it is.
    let file = one_page(
        1,
        true,
        2,
        |put| {
            put.int32(1, 3);
            put.int32(2, 8);
            put.int32(3, len);
            put.begin(8);
            put.int32(1, 2);
            put.int32(2, 0);
            put.int32(3, 2);
            put.int32(4, 0);
            put.int32(5, i64::from(i32::MAX));
            put.int32(6, i64::from(i32::MAX));
            put.end();
        },
        &snappy,
    );
    assert_eq!(failure(&file), damaged(Damage::Levels));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn texts_that_grow_from_their_predecessors_stay_bounded() {
    // Every text is the one before it: a short page stands for gigabytes.
    let long = vec![b'z'; 40 << 20];
    let rows = 20_000_usize;
    let mut shared = vec![long.len() as u64; rows];
    shared[0] = 0;
    let mut lens = vec![0; rows];
    lens[0] = long.len() as u64;
    let mut body = super::runs::tests::delta(&shared, 128, 4);
    body.extend(super::runs::tests::delta(&lens, 128, 4));
    body.extend_from_slice(&long);
    let packed = super::snappy::tests::compress(&body);
    let header = data_header(body.len() as i64, packed.len() as i64, rows as i64, 7);
    let file = one_page(6, false, rows as i64, header, &packed);
    // All texts are the same, but a page without a dictionary does not say
    // so, and the rows of the smallest group have to fit into memory.
    assert_eq!(
        import(&file, &Options::default()).err(),
        Some(ParquetError::Limit(Limit::Text))
    );
}

/// How many steps importing a file takes that is all here.
fn steps(file: &[u8]) -> usize {
    let mut import = Import::new(open_file(file).unwrap(), &Options::default()).unwrap();
    let mut source = file;
    let mut table = Vec::new();
    let mut steps = 1;
    while import.step(&mut source, &mut table).unwrap() != Step::Done {
        steps += 1;
    }
    steps
}

#[test]
fn large_dictionaries_are_read_in_pieces() {
    let mut rng = Rng::from_seed(120);
    let raw = |text: String| Raw::Bytes(text.into_bytes());
    for (physical, means) in [
        (Physical::Bytes, Means::Nothing),
        (Physical::Int64, Means::Nothing),
        (Physical::Fixed(16), Means::Logical(14)),
    ] {
        let entry = |number: usize| match physical {
            Physical::Bytes => raw(format!("{number}")),
            Physical::Int64 => Raw::Number(number as u64),
            _ => Raw::Bytes((number as u128).to_be_bytes().to_vec()),
        };
        let mut field = Field::new("a", physical, means);
        field.values = (0..40).map(|row| Some(entry(500_000 + row))).collect();
        field.encoding = super::meta::RLE_DICTIONARY;
        let file = encode::write(&[field.clone()], &Layout::simple(), &mut rng);
        let few = steps(&file);
        // Many more than there are rows, as when categories are written.
        field.unused = (0..300_000).map(entry).collect();
        let file = encode::write(&[field], &Layout::simple(), &mut rng);
        let imported = import(&file, &Options::default()).unwrap();
        let many = steps(&file);
        assert!(many > few + 3, "{many} steps, {few} steps");
        assert_eq!(imported.report.rows, 40);
        let cells = &cells(&imported.table)[0].2;
        assert_eq!(cells.len(), 40);
        if physical == Physical::Bytes {
            assert_eq!(cells[39], Model::Text("500039".to_string()));
        }
    }
}

#[test]
fn a_second_dictionary_is_refused() {
    let mut rng = Rng::from_seed(121);
    let mut field = texts("a", &[b"x", b"y"]);
    field.encoding = super::meta::RLE_DICTIONARY;
    field.twice = true;
    let file = encode::write(&[field], &Layout::simple(), &mut rng);
    assert_eq!(
        import(&file, &Options::default()).err(),
        Some(ParquetError::Damaged {
            column: Some(0),
            damage: Damage::Dictionary
        })
    );
}

#[test]
fn a_wrong_checksum_is_noticed() {
    let mut rng = Rng::from_seed(112);
    let fields = [numbers("a", Means::Nothing, &[1, 2, 3, 4])];
    let layout = Layout {
        checksums: true,
        ..Layout::simple()
    };
    let mut file = encode::write(&fields, &layout, &mut rng);
    // The values are the only run of these bytes.
    let at = file
        .windows(8)
        .position(|bytes| bytes == 3_u64.to_le_bytes())
        .unwrap();
    file[at] = 5;
    assert_eq!(
        import(&file, &Options::default()).err(),
        Some(ParquetError::Damaged {
            column: Some(0),
            damage: Damage::Checksum
        })
    );
}

#[test]
fn damaged_files_are_survived() {
    let mut rng = Rng::from_seed(113);
    let (mut good, mut bad) = (0, 0);
    for round in 0..rounds(80) {
        let rows = [1, 30, 200, 300][round % 4];
        let mut cases = cases(&mut rng, rows);
        while cases.len() > 6 {
            let index = rng.next_below(cases.len() as u64) as usize;
            cases.swap_remove(index);
        }
        let fields: Vec<Field> = cases.into_iter().map(|case| case.field).collect();
        let layout = Layout {
            checksums: rng.next_below(10) == 0,
            ..some_layout(&mut rng, rows)
        };
        let file = encode::write(&fields, &layout, &mut rng);
        let end = file.len() - 8;
        let len = u32::from_le_bytes(file[end..end + 4].try_into().unwrap());
        let metadata = end - len as usize;
        for mutation in 0..60 {
            let mut damaged = file.clone();
            // The metadata, the pages, or both.
            let (from, to) = match mutation % 3 {
                0 => (metadata, end),
                1 => (4, metadata),
                _ => (0, file.len()),
            };
            for _ in 0..=rng.next_below(3) {
                let at = from + rng.next_below((to - from) as u64) as usize;
                damaged[at] = match rng.next_below(3) {
                    0 => rng.next_u64() as u8,
                    1 => damaged[at] ^ (1 << rng.next_below(8)),
                    _ => [0, 0xff, 0x80, 0x7f, 1][rng.next_below(5) as usize],
                };
            }
            if mutation % 20 == 19 {
                let at = from + rng.next_below((to - from) as u64) as usize;
                let cut = rng.next_below(40) as usize;
                damaged.drain(at..(at + cut).min(to));
            }
            match import_otherwise(&damaged, &mut rng) {
                Ok(imported) => {
                    good += 1;
                    // What comes out is a table, whatever is in it.
                    assert!(crate::tests::exercise(&imported.table).is_ok());
                }
                Err(_) => bad += 1,
            }
        }
    }
    // Both happen, so that what lies behind the checks is reached.
    assert!(good > 500 && bad > 500, "{good} good, {bad} bad");
}

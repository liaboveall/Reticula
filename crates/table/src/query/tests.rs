//! Tables asked and answered, compared with a simple way to the same answers.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::{Ordering, Reverse};
use std::collections::BTreeSet;
use std::format;

use reticula_core::rng::Rng;
use reticula_core::viewport::PageFrame;

use super::context::Limits;
use super::space::Space;
use super::*;
use crate::calendar::DAY_MICROS;
use crate::layout::{Directory, Encoding, Trailer};
use crate::page;
use crate::source::{ByteRange, Source, SourceError};
use crate::tests::{Model, ModelColumn, open, reseal, write};
use crate::text;
use crate::types::{CellValue, ColumnType};

/// A table, and what is in it.
struct Table {
    file: Vec<u8>,
    kinds: Vec<ColumnType>,
    /// From which row on texts are not in the dictionary, of columns that have one.
    plain_from: Vec<Option<usize>>,
    columns: Vec<Vec<Model>>,
    rows: usize,
}

const GROUP: usize = 256;
/// The column whose texts are in a dictionary in the first groups only, and the
/// one after it, whose groups have few entries of its dictionary each.
const MIXED: usize = 12;
const CLUSTERED: usize = 13;

fn pick<'a, T>(rng: &mut Rng, from: &'a [T]) -> &'a T {
    &from[rng.next_below(from.len() as u64) as usize]
}

/// Texts that are hard to tell apart.
const TEXTS: &[&str] = &[
    "",
    "a",
    "A",
    "a\0",
    "A\0",
    "a\0\0",
    "\0",
    "\u{1}",
    "ab",
    "aB",
    "b",
    "North",
    "NORTH",
    "north ",
    "customer-0001",
    "Customer-0001",
    "customer-0002",
    "customer-0001-a",
    "customer-0001-A",
    "customer-0001-b",
    "customer",
    "CUSTOMER",
    "customer-",
    "customer-0001-and then a long tail",
    "customer-0001-and then a long tail that goes on",
    "customer-0001-and then a long tale",
    "12345678",
    "123456789",
    "1234567",
    "ΟΔΥΣΣΕΥΣ",
    "οδυσσευσ",
    "οδυσσευς",
    "Σ",
    "σ",
    "İstanbul",
    "i̇stanbul",
    "istanbul",
    "ﬁ",
    "漢字",
    "漢字カ",
    "é",
    "É",
    "e\u{301}",
    "Ⱥ",
    "\u{10ffff}",
];

fn some_text(rng: &mut Rng) -> String {
    match rng.next_below(8) {
        0 => format!("customer-{:04}", rng.next_below(40)),
        1 => format!("{:08x}{}", rng.next_below(3), pick(rng, TEXTS)),
        2 => format!("{:016x}", rng.next_u64()),
        _ => pick(rng, TEXTS).to_string(),
    }
}

/// A text column whose first groups are in a dictionary, and whose others are not.
fn mix(file: &[u8], coded: usize, plain: usize, from_group: u32) -> Vec<u8> {
    let reader = open(file);
    let old = reader.directory();
    let width = old.columns.len();
    let mut directory = Directory {
        blocks: Vec::new(),
        ..old.clone()
    };
    for group in 0..old.groups() {
        for column in 0..width {
            let from = if column == coded && group >= from_group {
                plain
            } else {
                column
            };
            if column != plain {
                directory.blocks.push(*old.block(from, group).unwrap());
            }
        }
    }
    directory.columns.remove(plain);
    let mut out = file[..old.data_len as usize].to_vec();
    let mut encoded = Vec::new();
    directory.encode(&mut encoded);
    let trailer = Trailer {
        directory_at: old.data_len,
        directory_len: encoded.len() as u64,
        directory_sha256: [0; 32],
    };
    trailer.encode(&mut encoded);
    out.extend_from_slice(&encoded);
    reseal(&mut out, old.data_len as usize);
    out
}

fn table(rng: &mut Rng, rows: usize) -> Table {
    let mut columns: Vec<ModelColumn> = Vec::new();
    let mut add = |name: &str, kind, dictionary, cells: Vec<Model>| {
        columns.push(ModelColumn {
            name: name.to_string(),
            kind,
            dictionary,
            cells,
        });
    };
    // How many different values there are, and how many are blank.
    let domain = *pick(rng, &[1_u64, 3, 20, 5_000]);
    let blank_in = *pick(rng, &[1_u64, 2, 10, u64::MAX]);
    let cells = |rng: &mut Rng, cell: &mut dyn FnMut(&mut Rng, usize) -> Model| {
        (0..rows)
            .map(|row| {
                if blank_in != 1 && rng.next_below(blank_in) == 0 {
                    Model::Blank
                } else {
                    cell(rng, row)
                }
            })
            .collect::<Vec<_>>()
    };
    let row_numbers = cells(rng, &mut |_, row| Model::Int(row as i64 * 3));
    add("row", Some(ColumnType::Integer), false, row_numbers);
    let small = cells(rng, &mut |rng, _| {
        Model::Int((rng.next_below(domain) as i64 - 2) * 100)
    });
    add("small", Some(ColumnType::Integer), false, small);
    let edges = cells(rng, &mut |rng, _| {
        Model::Int(match rng.next_below(5) {
            0 => i64::MIN,
            1 => i64::MAX,
            2 => rng.next_below(3) as i64 - 1,
            _ => rng.next_u64().cast_signed(),
        })
    });
    add("wide", Some(ColumnType::Integer), false, edges);
    let money = cells(rng, &mut |rng, _| {
        Model::Int(rng.next_below(domain.max(2)) as i64 * 25 - 100)
    });
    let decimal = ColumnType::Decimal {
        scale: 2,
        fixed: rng.next_below(2) == 0,
    };
    add("money", Some(decimal), false, money);
    let floats = cells(rng, &mut |rng, _| {
        Model::Float(match rng.next_below(6) {
            0 => -0.0,
            1 => 0.1,
            2 => 1e300 * (rng.next_f64() - 0.5),
            3 => -(rng.next_below(domain) as f64),
            4 => 5e-324,
            _ => rng.next_below(domain) as f64 / 8.0,
        })
    });
    add("float", Some(ColumnType::Float), false, floats);
    let days = cells(rng, &mut |rng, row| {
        Model::Int(19_000 + (row / 100) as i64 + rng.next_below(domain.min(30)) as i64)
    });
    add("day", Some(ColumnType::Date), false, days);
    let times = cells(rng, &mut |rng, row| {
        Model::Int((row as i64 * 40_000 + rng.next_below(3) as i64 - 50_000) * 1_000_000)
    });
    add("time", Some(ColumnType::DateTime), false, times);
    let truths = cells(rng, &mut |rng, _| Model::Int(rng.next_below(2) as i64));
    add("truth", Some(ColumnType::Boolean), false, truths);
    let coded = cells(rng, &mut |rng, _| Model::Text(some_text(rng)));
    add("coded", Some(ColumnType::Text), true, coded);
    let plain = cells(rng, &mut |rng, _| Model::Text(some_text(rng)));
    add("plain", Some(ColumnType::Text), false, plain);
    let late = (0..rows)
        .map(|row| {
            if row < rows / 2 {
                Model::Blank
            } else {
                Model::Text(some_text(rng))
            }
        })
        .collect();
    add("late", None, true, late);
    add("nothing", None, false, vec![Model::Blank; rows]);
    let mixed = cells(rng, &mut |rng, _| Model::Text(some_text(rng)));
    add("mixed", Some(ColumnType::Text), true, mixed.clone());
    add("mixed as well", Some(ColumnType::Text), false, mixed);
    // A group in which most entries occur, and then groups in which only a few
    // do, whose codes are far apart.
    let apart = *pick(rng, &[1, 10, 100]);
    let clustered = cells(rng, &mut |rng, row| {
        let number = if row < GROUP {
            row as u64 % 202
        } else {
            rng.next_below(3) * apart + (row / GROUP) as u64 % 2
        };
        Model::Text(format!("Cluster {}", number * 7 % 202))
    });
    add("clustered", Some(ColumnType::Text), true, clustered);

    let groups = rows.div_ceil(GROUP) as u64;
    let from_group = rng.next_below(groups + 1) as u32;
    let written = write(&columns, GROUP as u32);
    let file = mix(&written, MIXED, MIXED + 1, from_group);
    columns.remove(MIXED + 1);
    let mut plain_from = vec![None; columns.len()];
    plain_from[MIXED] = Some(from_group as usize * GROUP);
    Table {
        file,
        kinds: columns
            .iter()
            .map(|column| column.kind.unwrap_or(ColumnType::Text))
            .collect(),
        plain_from,
        columns: columns
            .into_iter()
            .map(|column| {
                let stored = column.cells.into_iter().map(|cell| match cell {
                    Model::Text(text) if text.is_empty() => Model::Blank,
                    // There is one zero.
                    Model::Float(value) => Model::Float(value + 0.0),
                    other => other,
                });
                stored.collect()
            })
            .collect(),
        rows,
    }
}

// The simple way.

/// A decimal number as its sign, the digits before and those after the point.
fn digits(text: &str) -> (bool, Vec<u8>, Vec<u8>) {
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let (mut whole, mut fraction) = (whole.as_bytes().to_vec(), fraction.as_bytes().to_vec());
    let exponent: i64 = exponent.parse().unwrap();
    for _ in 0..exponent {
        whole.push(if fraction.is_empty() {
            b'0'
        } else {
            fraction.remove(0)
        });
    }
    for _ in exponent..0 {
        fraction.insert(0, whole.pop().unwrap_or(b'0'));
    }
    while whole.first() == Some(&b'0') {
        whole.remove(0);
    }
    while fraction.last() == Some(&b'0') {
        fraction.pop();
    }
    let zero = whole.is_empty() && fraction.is_empty();
    (negative && !zero, whole, fraction)
}

fn compare_decimals(a: &str, b: &str) -> Ordering {
    let ((a_negative, a_whole, a_rest), (b_negative, b_whole, b_rest)) = (digits(a), digits(b));
    let size = a_whole
        .len()
        .cmp(&b_whole.len())
        .then(a_whole.cmp(&b_whole))
        .then(a_rest.cmp(&b_rest));
    match (a_negative, b_negative) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => size,
        (true, true) => size.reverse(),
    }
}

fn plain_decimal(mantissa: i64, scale: u8) -> String {
    let mut out = String::new();
    text::write_decimal(i128::from(mantissa), scale, true, &mut out);
    out
}

fn scale_of(kind: ColumnType) -> u8 {
    match kind {
        ColumnType::Decimal { scale, .. } => scale,
        _ => 0,
    }
}

/// How a number compares with a threshold.
fn compare_number(kind: ColumnType, cell: &Model, threshold: &str) -> Ordering {
    match cell {
        Model::Int(value) => compare_decimals(&plain_decimal(*value, scale_of(kind)), threshold),
        Model::Float(value) => {
            let near: f64 = threshold.parse().unwrap();
            value.partial_cmp(&near).unwrap()
        }
        _ => panic!("{cell:?}"),
    }
}

fn compare_cells(a: &Model, b: &Model) -> Ordering {
    match (a, b) {
        (Model::Int(a), Model::Int(b)) => a.cmp(b),
        (Model::Float(a), Model::Float(b)) => a.partial_cmp(b).unwrap(),
        (Model::Text(a), Model::Text(b)) => {
            a.to_lowercase().as_bytes().cmp(b.to_lowercase().as_bytes())
        }
        _ => panic!("{a:?} {b:?}"),
    }
}

impl Table {
    fn holds(&self, column: usize, condition: &Condition, row: usize) -> bool {
        let kind = self.kinds[column];
        let cell = &self.columns[column][row];
        let blank = *cell == Model::Blank;
        let folded = match cell {
            Model::Text(text) => text.to_lowercase(),
            _ => String::new(),
        };
        match condition {
            Condition::Blank => blank,
            Condition::NotBlank => !blank,
            Condition::OneOf { values } => values.iter().any(|value| match (value, cell) {
                (Value::Blank, Model::Blank) => true,
                (Value::Int(a), Model::Int(b)) => a == b,
                (Value::Float(a), Model::Float(b)) => f64::from_bits(*a) == *b,
                (Value::Text(text), Model::Blank) => text.is_empty(),
                (Value::Text(text), Model::Text(_)) => text.to_lowercase() == folded,
                _ => false,
            }),
            Condition::TextContains(part) => folded.contains(&part.to_lowercase()),
            Condition::TextStartsWith(part) => folded.starts_with(&part.to_lowercase()),
            Condition::TextEndsWith(part) => folded.ends_with(&part.to_lowercase()),
            _ if blank => false,
            Condition::Number { op, a, b } => {
                let order = compare_number(kind, cell, a);
                match op {
                    NumberOp::Equal => order.is_eq(),
                    NumberOp::NotEqual => order.is_ne(),
                    NumberOp::Greater => order.is_gt(),
                    NumberOp::AtLeast => order.is_ge(),
                    NumberOp::Less => order.is_lt(),
                    NumberOp::AtMost => order.is_le(),
                    NumberOp::Between => {
                        let other = compare_number(kind, cell, b.as_ref().unwrap());
                        (order.is_ge() && other.is_le()) || (order.is_le() && other.is_ge())
                    }
                }
            }
            Condition::Time { op, a, b } => {
                let Model::Int(value) = cell else {
                    panic!("{cell:?}");
                };
                match op {
                    TimeOp::Before => value < a,
                    TimeOp::After => value > a,
                    TimeOp::Between => {
                        let b = b.unwrap();
                        (*a.min(&b)..=*a.max(&b)).contains(value)
                    }
                    TimeOp::On if kind == ColumnType::DateTime => {
                        value.div_euclid(DAY_MICROS) == a.div_euclid(DAY_MICROS)
                    }
                    TimeOp::On => value == a,
                }
            }
            Condition::Top { n, bottom } => {
                let mut better = 0;
                for other in self.columns[column]
                    .iter()
                    .filter(|cell| **cell != Model::Blank)
                {
                    let order = compare_cells(other, cell);
                    if order
                        == (if *bottom {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        })
                    {
                        better += 1;
                    }
                }
                better < *n
            }
        }
    }

    fn rows_of(&self, filters: &[(usize, Condition)]) -> Vec<u32> {
        (0..self.rows)
            .filter(|row| {
                filters
                    .iter()
                    .all(|(column, condition)| self.holds(*column, condition, *row))
            })
            .map(|row| row as u32)
            .collect()
    }

    fn view(&self, spec: &Spec) -> Vec<u32> {
        let mut rows = self.rows_of(&spec.filters);
        rows.sort_by(|a, b| {
            for key in &spec.order {
                let cells = &self.columns[key.column];
                let (a, b) = (&cells[*a as usize], &cells[*b as usize]);
                let order = match (a == &Model::Blank, b == &Model::Blank) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) if key.descending => compare_cells(b, a),
                    (false, false) => compare_cells(a, b),
                };
                if order.is_ne() {
                    return order;
                }
            }
            Ordering::Equal
        });
        rows
    }

    fn shown(&self, column: usize, cell: &Model) -> String {
        let mut out = String::new();
        match cell {
            Model::Blank => {}
            Model::Text(text) => out.push_str(text),
            Model::Int(value) => {
                text::write_value(self.kinds[column], CellValue::Int(*value), &mut out);
            }
            Model::Float(value) => {
                text::write_value(self.kinds[column], CellValue::Float(*value), &mut out);
            }
        }
        out
    }

    fn is_coded(&self, column: usize) -> bool {
        let outside = |from: usize| self.columns[column].iter().skip(from);
        matches!(column, 8 | 10 | CLUSTERED)
            || (column == MIXED
                && outside(self.plain_from[MIXED].unwrap()).all(|cell| *cell == Model::Blank))
    }

    /// The cells of some rows that are not blank.
    fn cells<'a>(&'a self, column: usize, rows: &'a [u32]) -> impl Iterator<Item = &'a Model> {
        rows.iter()
            .map(move |row| &self.columns[column][*row as usize])
            .filter(|cell| **cell != Model::Blank)
    }

    fn summary(&self, column: usize, rows: &[u32]) -> ColumnSummary {
        let kind = self.kinds[column];
        let cells: Vec<&Model> = self.cells(column, rows).collect();
        let mut out = ColumnSummary {
            column,
            rows: rows.len() as u64,
            blanks: (rows.len() - cells.len()) as u64,
            distinct: None,
            sum: None,
            mean: None,
            min: None,
            max: None,
        };
        let figure = |text: String, exact| Some(Figure { text, exact });
        let min = cells.iter().min_by(|a, b| compare_cells(a, b));
        let max = cells.iter().max_by(|a, b| compare_cells(a, b));
        if kind == ColumnType::Text {
            let nothing = self.columns[column]
                .iter()
                .all(|cell| *cell == Model::Blank);
            if nothing || self.is_coded(column) {
                let texts: BTreeSet<String> = cells
                    .iter()
                    .map(|cell| self.shown(column, cell).to_lowercase())
                    .collect();
                out.distinct = Some(texts.len() as u64);
                // Spelled like the first one of the file.
                let spelled = |cell: &&Model| {
                    let folded = self.shown(column, cell).to_lowercase();
                    let all = self.columns[column]
                        .iter()
                        .map(|cell| self.shown(column, cell));
                    all.into_iter()
                        .find(|text| text.to_lowercase() == folded)
                        .unwrap()
                };
                out.min = min.and_then(|cell| figure(spelled(cell), true));
                out.max = max.and_then(|cell| figure(spelled(cell), true));
            }
            return out;
        }
        let exact = kind != ColumnType::Float;
        out.min = min.and_then(|cell| figure(self.shown(column, cell), exact));
        out.max = max.and_then(|cell| figure(self.shown(column, cell), exact));
        if cells.is_empty() || !kind.is_number() {
            return out;
        }
        if kind == ColumnType::Float {
            // Neumaier's sum.
            let (mut sum, mut lost) = (0.0_f64, 0.0_f64);
            for cell in &cells {
                let Model::Float(value) = cell else { panic!() };
                let next = sum + value;
                lost += if sum.abs() >= value.abs() {
                    (sum - next) + value
                } else {
                    (value - next) + sum
                };
                sum = next;
            }
            let total = sum + lost;
            out.sum = figure(self.shown(column, &Model::Float(total)), false);
            let mean = total / cells.len() as f64;
            out.mean = figure(self.shown(column, &Model::Float(mean)), false);
        } else {
            let sum: i128 = cells
                .iter()
                .map(|cell| match cell {
                    Model::Int(value) => i128::from(*value),
                    _ => panic!(),
                })
                .sum();
            let (scale, fixed) = match kind {
                ColumnType::Decimal { scale, fixed } => (scale, fixed),
                _ => (0, false),
            };
            let mut text = String::new();
            text::write_decimal(sum, scale, fixed, &mut text);
            out.sum = figure(text, true);
        }
        out
    }

    fn values(&self, spec: &Spec, column: usize, search: &str, limit: usize) -> ValueList {
        let others: Vec<(usize, Condition)> = spec
            .filters
            .iter()
            .filter(|(of, _)| *of != column)
            .cloned()
            .collect();
        let rows = self.rows_of(&others);
        let mut counted: BTreeMap<Vec<u8>, (Model, u64)> = BTreeMap::new();
        // The first row of the file where each spelling is, for those in a dictionary.
        let first_row = |text: &str| {
            let same = |cell: &Model| matches!(cell, Model::Text(other) if other == text);
            self.columns[column].iter().position(same).unwrap()
        };
        let plain_from = match column {
            8 | 10 | CLUSTERED => self.rows,
            MIXED => self.plain_from[MIXED].unwrap(),
            _ => 0,
        };
        let mut order: Vec<u32> = rows.clone();
        if self.kinds[column] == ColumnType::Text {
            order.sort_by_key(|row| {
                let row = *row as usize;
                match &self.columns[column][row] {
                    Model::Text(text) if row < plain_from => first_row(text),
                    _ => row,
                }
            });
        }
        for cell in self.cells(column, &order) {
            let key = match cell {
                Model::Int(value) => (value.cast_unsigned() ^ (1 << 63)).to_be_bytes().to_vec(),
                Model::Float(value) => {
                    let bits = value.to_bits().cast_signed();
                    let key = if bits < 0 { !bits } else { bits ^ i64::MIN };
                    key.to_be_bytes().to_vec()
                }
                Model::Text(text) => text.to_lowercase().into_bytes(),
                Model::Blank => panic!(),
            };
            counted.entry(key).or_insert_with(|| (cell.clone(), 0)).1 += 1;
        }
        if self.is_coded(column) {
            for (cell, _) in counted.values_mut() {
                let Model::Text(text) = cell else { panic!() };
                let folded = text.to_lowercase();
                let all = self.columns[column].iter();
                let first = all
                    .filter(|cell| **cell != Model::Blank)
                    .find(|cell| self.shown(column, cell).to_lowercase() == folded);
                *cell = first.unwrap().clone();
            }
        }
        let search = search.to_lowercase();
        let mut all: Vec<(Vec<u8>, Model, u64)> = counted
            .into_iter()
            .map(|(key, (cell, count))| (key, cell, count))
            .filter(|(_, cell, _)| self.shown(column, cell).to_lowercase().contains(&search))
            .collect();
        let complete = all.len() <= limit;
        all.sort_by_key(|(key, _, count)| (Reverse(*count), key.clone()));
        all.truncate(limit);
        all.sort_by(|a, b| a.0.cmp(&b.0));
        ValueList {
            column,
            values: all
                .into_iter()
                .map(|(_, cell, count)| ListedValue {
                    text: self.shown(column, &cell),
                    value: match cell {
                        Model::Int(value) => Value::Int(value),
                        Model::Float(value) => Value::Float(value.to_bits()),
                        Model::Text(text) => Value::Text(text),
                        Model::Blank => Value::Blank,
                    },
                    count,
                })
                .collect(),
            blanks: (rows.len() - self.cells(column, &rows).count()) as u64,
            complete,
        }
    }
}

// What is asked.

fn some_threshold(rng: &mut Rng, table: &Table, column: usize) -> String {
    let kind = table.kinds[column];
    let base = match table.columns[column].get(rng.next_below(table.rows.max(1) as u64) as usize) {
        Some(Model::Int(value)) => plain_decimal(*value, scale_of(kind)),
        Some(Model::Float(value)) => format!("{value}"),
        _ => format!("{}", rng.next_below(100)),
    };
    match rng.next_below(7) {
        0 if !base.contains('.') => format!("{base}.0"),
        1 if !base.contains('.') => format!("{base}.5"),
        2 if base.contains('.') => format!("{base}0001"),
        3 if !base.contains('.') => format!("{base}.00000000000000000000001"),
        4 if base.len() < 12 => format!("{base}e1"),
        5 => "-12.5e-1".to_string(),
        _ => base,
    }
}

fn some_value(rng: &mut Rng, table: &Table, column: usize) -> Value {
    let row = rng.next_below(table.rows.max(1) as u64) as usize;
    match table.columns[column].get(row) {
        Some(Model::Int(value)) => Value::Int(*value),
        Some(Model::Float(value)) => {
            let value = if *value == 0.0 { -0.0 } else { *value };
            Value::Float(value.to_bits())
        }
        Some(Model::Text(text)) => Value::Text(if rng.next_below(2) == 0 {
            text.to_uppercase()
        } else {
            text.clone()
        }),
        _ => Value::Blank,
    }
}

fn some_part(rng: &mut Rng) -> String {
    let text = some_text(rng);
    let chars: Vec<char> = text.chars().collect();
    let from = rng.next_below(chars.len() as u64 + 1) as usize;
    let to = from + rng.next_below((chars.len() - from) as u64 + 1) as usize;
    match rng.next_below(3) {
        0 => chars[..to].iter().collect(),
        1 => chars[from..].iter().collect(),
        _ => chars[from..to].iter().collect(),
    }
}

fn some_condition(rng: &mut Rng, table: &Table, column: usize) -> Condition {
    let kind = table.kinds[column];
    match rng.next_below(10) {
        0 => Condition::Blank,
        1 => Condition::NotBlank,
        2..=4 => {
            let count = *pick(rng, &[0, 1, 1, 2, 5, 40]);
            let values = (0..count).map(|_| some_value(rng, table, column));
            Condition::OneOf {
                values: values.collect(),
            }
        }
        _ if kind == ColumnType::Text => {
            let part = some_part(rng);
            match rng.next_below(3) {
                0 => Condition::TextContains(part),
                1 => Condition::TextStartsWith(part),
                _ => Condition::TextEndsWith(part),
            }
        }
        _ if kind == ColumnType::Boolean => Condition::OneOf {
            values: vec![Value::Int(1)],
        },
        5 => Condition::Top {
            n: *pick(rng, &[0, 1, 2, 10, 100, 1_000_000]),
            bottom: rng.next_below(2) == 0,
        },
        _ if kind.is_number() => {
            let op = *pick(
                rng,
                &[
                    NumberOp::Equal,
                    NumberOp::NotEqual,
                    NumberOp::Greater,
                    NumberOp::AtLeast,
                    NumberOp::Less,
                    NumberOp::AtMost,
                    NumberOp::Between,
                ],
            );
            Condition::Number {
                op,
                a: some_threshold(rng, table, column),
                b: (op == NumberOp::Between).then(|| some_threshold(rng, table, column)),
            }
        }
        _ => {
            let time = |rng: &mut Rng| match some_value(rng, table, column) {
                Value::Int(value) => value + rng.next_below(3) as i64 - 1,
                _ => *pick(rng, &[0, i64::MIN, i64::MAX]),
            };
            let op = *pick(
                rng,
                &[TimeOp::Before, TimeOp::After, TimeOp::Between, TimeOp::On],
            );
            Condition::Time {
                op,
                a: time(rng),
                b: (op == TimeOp::Between).then(|| time(rng)),
            }
        }
    }
}

fn some_filters(rng: &mut Rng, table: &Table) -> Vec<(usize, Condition)> {
    (0..*pick(rng, &[0, 1, 1, 2, 3]))
        .map(|_| {
            let column = rng.next_below(table.kinds.len() as u64) as usize;
            (column, some_condition(rng, table, column))
        })
        .collect()
}

fn some_order(rng: &mut Rng, table: &Table) -> Vec<SortKey> {
    (0..*pick(rng, &[0, 1, 1, 2, 3]))
        .map(|_| SortKey {
            column: rng.next_below(table.kinds.len() as u64) as usize,
            descending: rng.next_below(2) == 0,
        })
        .collect()
}

/// Limits at which other ways are taken.
fn some_limits(rng: &mut Rng) -> Limits {
    Limits {
        slice: *pick(rng, &[1, 7, 100, 262_144]),
        part: *pick(rng, &[1, 1_000, 4 << 20]),
        texts_at_once: *pick(rng, &[1, 50, 16_384]),
        entries: *pick(rng, &[64, 4_096]),
        run: *pick(rng, &[1, 4, 100, 32_768]),
        texts: *pick(rng, &[0, 50, 2_000, 64 << 20]),
        rounds: u32::MAX,
        unite: *pick(rng, &[0, 1, 4, u32::MAX]),
        sparse: *pick(rng, &[0, 16_384, 1 << 20]),
        few: *pick(rng, &[1, 4, 100]),
        direct: *pick(rng, &[0, 50, 1 << 22]),
        distinct: 1 << 20,
        listed: 64 << 20,
    }
}

/// A file of which some pieces are not here, and which remembers what was read.
struct Pieces<'a> {
    file: &'a [u8],
    piece: u64,
    here: Vec<bool>,
    reads: Vec<ByteRange>,
}

impl<'a> Pieces<'a> {
    fn new(file: &'a [u8], piece: u64, here: bool) -> Self {
        Self {
            file,
            piece,
            here: vec![here; file.len().div_ceil(piece as usize)],
            reads: Vec::new(),
        }
    }

    fn fetch(&mut self, ranges: &[ByteRange]) {
        assert!(!ranges.is_empty());
        let mut new = false;
        for range in ranges {
            for piece in range.at / self.piece..range.end().div_ceil(self.piece) {
                new |= !self.here[piece as usize];
                self.here[piece as usize] = true;
            }
        }
        assert!(new, "asked for what is here already");
    }

    fn has_read(&self, at: u64, len: u64) -> bool {
        self.reads
            .iter()
            .any(|read| read.at < at + len && at < read.end())
    }
}

impl Source for Pieces<'_> {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        let len = into.len() as u64;
        for piece in at / self.piece..(at + len).div_ceil(self.piece) {
            if !self.here[piece as usize] {
                return Err(SourceError::Missing);
            }
        }
        self.reads.push(ByteRange { at, len });
        (&*self.file).read(at, into)
    }
}

/// Does the next job, in steps of some sizes.
fn drive(
    session: &mut Session,
    source: &mut Pieces<'_>,
    budget: &mut dyn FnMut() -> u32,
) -> Result<(), QueryError> {
    let mut last = 0;
    loop {
        match session.step(source, budget())? {
            Progress::Done => return Ok(()),
            Progress::Working { done, total } => {
                assert!(last <= done && done <= total);
                last = done;
            }
            Progress::Need(ranges) => source.fetch(&ranges),
        }
    }
}

fn all_of(session: &Session) -> Vec<u32> {
    let rows: Vec<u32> = (0..session.view_len())
        .map(|position| session.view_row(position).unwrap())
        .collect();
    assert_eq!(session.view_row(session.view_len()), None);
    rows
}

const SIZES: &[usize] = &[
    0, 1, 2, 63, 64, 65, 255, 256, 257, 511, 512, 513, 700, 1_100,
];

#[test]
fn answers_are_those_of_the_simple_way() {
    let mut rng = Rng::from_seed(101);
    for round in 0..250 {
        let rows = *pick(&mut rng, SIZES);
        let table = table(&mut rng, rows);
        let mut session = Session::new(open(&table.file));
        let piece = *pick(&mut rng, &[4_096, 20_000, 1 << 20]);
        let here = rng.next_below(3) > 0;
        let mut source = Pieces::new(&table.file, piece, here);
        let mut order = Vec::new();
        for _ in 0..8 {
            session.limits = some_limits(&mut rng);
            // Often only the filter changes.
            if rng.next_below(3) == 0 {
                order = some_order(&mut rng, &table);
            }
            let spec = Spec {
                filters: some_filters(&mut rng, &table),
                order: order.clone(),
            };
            let before = all_of(&session);
            session.begin(spec.clone()).unwrap();
            let mut steps = rng.clone();
            let mut budget = move || *pick(&mut steps, &[1, 1, 300, 5_000, u32::MAX]);
            // The view there was stays until the new one is there.
            if let Ok(Progress::Working { .. }) = session.step(&mut source, 1) {
                assert_eq!(all_of(&session), before);
            }
            if session.running().is_some() {
                drive(&mut session, &mut source, &mut budget).unwrap();
            }
            assert_eq!(
                all_of(&session),
                table.view(&spec),
                "round {round}: {spec:?}"
            );

            let columns: Vec<usize> = (0..table.kinds.len()).collect();
            session.begin_summary(columns.clone()).unwrap();
            drive(&mut session, &mut source, &mut budget).unwrap();
            let found = session.take_summary().unwrap();
            assert_eq!(session.take_summary(), None);
            let rows = table.rows_of(&spec.filters);
            for (mut found, column) in found.into_iter().zip(columns) {
                let expected = table.summary(column, &rows);
                if table.kinds[column].is_number() && table.kinds[column] != ColumnType::Float {
                    check_mean(&table, column, &rows, found.mean.take());
                }
                assert_eq!(found, expected, "round {round}: {spec:?}");
            }

            let column = rng.next_below(table.kinds.len() as u64) as usize;
            let search = if rng.next_below(2) == 0 {
                some_part(&mut rng)
            } else {
                pick(&mut rng, &["", "", "1", "-", "2", "TRUE"]).to_string()
            };
            let limit = *pick(&mut rng, &[0, 1, 5, 1_000]);
            session.begin_values(column, search.clone(), limit).unwrap();
            drive(&mut session, &mut source, &mut budget).unwrap();
            assert_eq!(
                session.take_values().unwrap(),
                table.values(&spec, column, &search, limit),
                "round {round}: column {column}, {search:?}, {spec:?}"
            );
            assert_eq!(session.running(), None);
        }
    }
}

/// A mean is the sum divided by the number of values, to fifteen digits.
fn check_mean(table: &Table, column: usize, rows: &[u32], found: Option<Figure>) {
    let cells: Vec<&Model> = table.cells(column, rows).collect();
    let Some(found) = found else {
        assert!(cells.is_empty());
        return;
    };
    let sum: i128 = cells
        .iter()
        .map(|cell| match cell {
            Model::Int(value) => i128::from(*value),
            _ => panic!(),
        })
        .sum();
    let (negative, whole, rest) = digits(&found.text);
    let places = rest.len() as u32;
    let mantissa: i128 = whole
        .iter()
        .chain(&rest)
        .fold(0, |sum, digit| sum * 10 + i128::from(digit - b'0'));
    let mantissa = if negative { -mantissa } else { mantissa };
    // As far from the sum as rounding allows, all of it scaled.
    let count = cells.len() as i128;
    let scale = u32::from(scale_of(table.kinds[column]));
    let apart = mantissa * count * 10_i128.pow(scale) - sum * 10_i128.pow(places);
    assert!(2 * apart.abs() <= count * 10_i128.pow(scale), "{found:?}");
    assert_eq!(found.exact, apart == 0);
    let size = (sum * 10_i128.pow(places)).abs() as f64;
    assert!(apart.abs() as f64 <= 1e-14 * size, "{found:?}");
}

#[test]
fn many_more_small_tables() {
    let mut rng = Rng::from_seed(102);
    for round in 0..1_500 {
        let rows = rng.next_below(12) as usize;
        let table = table(&mut rng, rows);
        let mut session = Session::new(open(&table.file));
        session.limits = some_limits(&mut rng);
        let mut source = Pieces::new(&table.file, 1 << 20, true);
        for _ in 0..4 {
            let spec = Spec {
                filters: some_filters(&mut rng, &table),
                order: some_order(&mut rng, &table),
            };
            session.begin(spec.clone()).unwrap();
            drive(&mut session, &mut source, &mut || 1).unwrap();
            assert_eq!(
                all_of(&session),
                table.view(&spec),
                "round {round}: {spec:?}"
            );
        }
    }
}

#[test]
fn how_the_work_is_cut_changes_nothing() {
    let mut rng = Rng::from_seed(103);
    for _ in 0..30 {
        let table = table(&mut rng, 900);
        let spec = Spec {
            filters: some_filters(&mut rng, &table),
            order: some_order(&mut rng, &table),
        };
        let mut answers = Vec::new();
        for (budget, piece, here) in [
            (1, 4_096, false),
            (1, 1 << 20, true),
            (777, 8_192, false),
            (u32::MAX, 4_096, true),
            (u32::MAX, 30_000, false),
        ] {
            let mut session = Session::new(open(&table.file));
            session.limits = some_limits(&mut rng);
            let mut source = Pieces::new(&table.file, piece, here);
            session.begin(spec.clone()).unwrap();
            drive(&mut session, &mut source, &mut || budget).unwrap();
            let columns: Vec<usize> = (0..table.kinds.len()).collect();
            session.begin_summary(columns).unwrap();
            drive(&mut session, &mut source, &mut || budget).unwrap();
            answers.push((all_of(&session), session.take_summary().unwrap()));
        }
        for answer in &answers {
            assert_eq!(answer, &answers[0]);
        }
    }
}

#[test]
fn pages_and_cells_follow_the_view() {
    let mut rng = Rng::from_seed(104);
    let table = table(&mut rng, 600);
    let mut session = Session::new(open(&table.file));
    let mut source = Pieces::new(&table.file, 4_096, false);
    let spec = Spec {
        filters: vec![(9, Condition::NotBlank)],
        order: vec![SortKey {
            column: 8,
            descending: true,
        }],
    };
    session.begin(spec).unwrap();
    drive(&mut session, &mut source, &mut || 1_000).unwrap();
    let rows = all_of(&session);
    let mut scratch = String::new();
    for first_row in [0, 64, rows.len() as u32 / 64 * 64, rows.len() as u32 + 64] {
        let frame = PageFrame {
            sheet: 1,
            first_row,
            first_col: 0,
            epoch: 9,
        };
        let encoded = loop {
            match session.page(&mut source, frame, &mut scratch) {
                Ok(encoded) => break encoded,
                Err(ReadError::Missing) => {
                    let missing = session.take_missing();
                    source.fetch(&missing);
                }
                Err(error) => panic!("{error:?}"),
            }
        };
        let from = (first_row as usize).min(rows.len());
        let part = &rows[from..(from + 64).min(rows.len())];
        let mut whole = table.file.as_slice();
        let mut reader = open(&table.file);
        let expected = page::encode(&mut reader, &mut whole, frame, part, &mut scratch);
        assert_eq!(Ok(encoded), expected);
    }
    for (position, row) in rows.iter().enumerate().take(50) {
        for column in 0..table.kinds.len() {
            let mut whole = Pieces::new(&table.file, 1 << 20, true);
            let text = session
                .cell_text(&mut whole, position as u32, column)
                .unwrap();
            let cell = &table.columns[column][*row as usize];
            let mut whole = String::new();
            match cell {
                Model::Float(value) => {
                    let value = CellValue::Float(*value);
                    text::write_exact(table.kinds[column], value, &mut whole);
                }
                _ => whole = table.shown(column, cell),
            }
            assert_eq!(text, whole);
        }
    }
    let mut whole = Pieces::new(&table.file, 1 << 20, true);
    assert_eq!(session.cell_text(&mut whole, u32::MAX, 0).unwrap(), "");
    assert_eq!(session.cell_text(&mut whole, 0, 99).unwrap(), "");
}

#[test]
fn what_cannot_be_asked_is_refused() {
    let mut rng = Rng::from_seed(105);
    let table = table(&mut rng, 10);
    let mut session = Session::new(open(&table.file));
    let number = |a: &str| Condition::Number {
        op: NumberOp::Equal,
        a: a.to_string(),
        b: None,
    };
    let time = |op| Condition::Time { op, a: 0, b: None };
    let one = |value| Condition::OneOf {
        values: vec![Value::Blank, value],
    };
    let top = Condition::Top { n: 1, bottom: true };
    let (ints, floats, dates, times, truths, texts) = (0, 4, 5, 6, 7, 9);
    let refused = [
        (ints, time(TimeOp::On)),
        (ints, Condition::TextContains("1".to_string())),
        (ints, one(Value::Float(0))),
        (ints, one(Value::Text("1".to_string()))),
        (floats, one(Value::Int(0))),
        (floats, Condition::TextEndsWith(String::new())),
        (dates, number("1")),
        (times, number("1")),
        (times, one(Value::Text(String::new()))),
        (truths, number("1")),
        (truths, time(TimeOp::On)),
        (truths, top.clone()),
        (texts, number("1")),
        (texts, time(TimeOp::Before)),
        (texts, top),
        (texts, one(Value::Int(0))),
    ];
    for (column, condition) in refused {
        let spec = Spec {
            filters: vec![(column, condition.clone())],
            order: Vec::new(),
        };
        let expected = Err(SpecError::DoesNotApply { column });
        assert_eq!(session.begin(spec), expected, "{condition:?}");
    }
    let begin = |session: &mut Session, column, condition| {
        session.begin(Spec {
            filters: vec![(column, condition)],
            order: Vec::new(),
        })
    };
    for text in ["", "1,5", "x", " 1"] {
        let expected = Err(SpecError::NotANumber { column: ints });
        assert_eq!(begin(&mut session, ints, number(text)), expected);
    }
    let between = Condition::Number {
        op: NumberOp::Between,
        a: "1".to_string(),
        b: None,
    };
    let expected = Err(SpecError::OneThreshold { column: ints });
    assert_eq!(begin(&mut session, ints, between), expected);
    let expected = Err(SpecError::OneThreshold { column: dates });
    assert_eq!(begin(&mut session, dates, time(TimeOp::Between)), expected);
    let expected = Err(SpecError::NoColumn { column: 99 });
    assert_eq!(begin(&mut session, 99, Condition::Blank), expected);
    let order = vec![SortKey {
        column: 99,
        descending: false,
    }];
    let spec = Spec {
        filters: Vec::new(),
        order,
    };
    assert_eq!(session.begin(spec), expected);
    assert_eq!(session.begin_summary(vec![0, 99]), expected);
    assert_eq!(session.begin_values(99, String::new(), 10), expected);
    assert_eq!(session.running(), None);
}

#[test]
fn without_enough_memory_the_old_view_stays() {
    let mut rng = Rng::from_seed(106);
    let table = table(&mut rng, 1_100);
    let key = |column, descending| SortKey { column, descending };
    let old = Spec {
        filters: vec![(1, Condition::NotBlank)],
        order: vec![key(3, true)],
    };
    let specs = [
        Spec {
            filters: Vec::new(),
            order: vec![key(8, false), key(2, true)],
        },
        Spec {
            filters: vec![(12, Condition::TextContains("c".to_string()))],
            order: vec![key(12, true)],
        },
        Spec {
            filters: vec![(
                6,
                Condition::Top {
                    n: 50,
                    bottom: false,
                },
            )],
            order: vec![key(9, false)],
        },
    ];
    for spec in specs {
        let (mut failures, mut successes) = (0, 0);
        let mut limit = 0;
        while successes == 0 {
            let mut session = Session::new(open(&table.file));
            let mut source = Pieces::new(&table.file, 1 << 20, true);
            session.begin(old.clone()).unwrap();
            drive(&mut session, &mut source, &mut || 100).unwrap();
            let before = all_of(&session);
            session.set_memory_limit(limit);
            session.begin(spec.clone()).unwrap();
            let mut result = drive(&mut session, &mut source, &mut || 1);
            let view = result.is_ok();
            if view {
                assert_eq!(all_of(&session), table.view(&spec));
                session.begin_summary((0..13).collect()).unwrap();
                result = drive(&mut session, &mut source, &mut || 1);
            }
            if result.is_ok() {
                session.begin_values(8, String::new(), 10).unwrap();
                result = drive(&mut session, &mut source, &mut || 1);
            }
            match result {
                Ok(()) => successes += 1,
                Err(error) => {
                    assert_eq!(error, QueryError::OutOfMemory);
                    failures += 1;
                    if !view {
                        assert_eq!(all_of(&session), before);
                        assert_eq!(session.spec(), &old);
                    }
                }
            }
            assert_eq!(session.running(), None);
            // With more of it, everything goes on.
            session.set_memory_limit(usize::MAX);
            session.begin(spec.clone()).unwrap();
            drive(&mut session, &mut source, &mut || 1_000).unwrap();
            assert_eq!(all_of(&session), table.view(&spec));
            limit += 1_500;
        }
        assert!(failures > 10, "{failures}");
    }
}

#[test]
fn a_dictionary_has_to_fit() {
    let columns = [ModelColumn {
        name: "a".to_string(),
        kind: Some(ColumnType::Text),
        dictionary: true,
        cells: (0..3_000)
            .map(|row| Model::Text(format!("{row:0>100}")))
            .collect(),
    }];
    let file = write(&columns, 256);
    let mut source = Pieces::new(&file, 1 << 20, true);
    let spec = Spec {
        filters: vec![(0, Condition::TextContains("12".to_string()))],
        order: Vec::new(),
    };
    for (room, fits) in [(100_000, false), (300_000, false), (800_000, true)] {
        let mut session = Session::new(open(&file));
        session.set_memory_limit(session.memory() + room);
        session.begin(spec.clone()).unwrap();
        let result = drive(&mut session, &mut source, &mut || 1);
        assert_eq!(result.is_ok(), fits, "{room}");
        if !fits {
            assert_eq!(result, Err(QueryError::OutOfMemory));
            assert!(session.reader().dictionary(0).is_none());
        }
    }
}

#[test]
fn every_bit_of_a_key_counts() {
    let mut rng = Rng::from_seed(112);
    for width in 1..=64_u32 {
        // Numbers that differ in one bit, in some order, and some of them twice.
        let mut numbers: Vec<u64> = (0..width).map(|bit| 1 << bit).collect();
        numbers.extend((0..width).map(|bit| (1 << bit) - 1));
        numbers.extend([0, crate::bits::mask(width)]);
        for at in (1..numbers.len()).rev() {
            numbers.swap(at, rng.next_below(at as u64 + 1) as usize);
        }
        let columns = [
            ModelColumn {
                name: "a".to_string(),
                kind: Some(ColumnType::Integer),
                dictionary: false,
                cells: numbers
                    .iter()
                    .map(|number| Model::Int(number.wrapping_add(1 << 63).cast_signed()))
                    .collect(),
            },
            // The smallest positive numbers, whose bits are these integers.
            ModelColumn {
                name: "b".to_string(),
                kind: Some(ColumnType::Float),
                dictionary: false,
                cells: numbers
                    .iter()
                    .map(|number| Model::Float(f64::from_bits(number >> 2)))
                    .collect(),
            },
        ];
        let file = write(&columns, 256);
        let mut session = Session::new(open(&file));
        let mut source = Pieces::new(&file, 1 << 20, true);
        for column in [0, 1] {
            for descending in [false, true] {
                let key = SortKey { column, descending };
                session
                    .begin(Spec {
                        filters: Vec::new(),
                        order: vec![key],
                    })
                    .unwrap();
                drive(&mut session, &mut source, &mut || 1_000).unwrap();
                let mut expected: Vec<u32> = (0..numbers.len() as u32).collect();
                let of = |row: u32| numbers[row as usize] >> if column == 1 { 2 } else { 0 };
                expected.sort_by(|a, b| {
                    if descending {
                        of(*b).cmp(&of(*a))
                    } else {
                        of(*a).cmp(&of(*b))
                    }
                });
                assert_eq!(all_of(&session), expected, "{width} bits, {key:?}");
            }
        }
    }
}

#[test]
fn keys_leave_out_the_zeros_that_all_values_end_with() {
    let column = |kind, cells: &dyn Fn(i64) -> i64| ModelColumn {
        name: "a".to_string(),
        kind: Some(kind),
        dictionary: false,
        cells: (0..600).map(|row| Model::Int(cells(row))).collect(),
    };
    let columns = [
        column(ColumnType::DateTime, &|row| {
            (row * 7 + row % 3) * 1_000_000 + 250_000
        }),
        column(ColumnType::Integer, &|row| row * row * 1_000),
        column(ColumnType::Integer, &|row| {
            if row < 300 { row * 1_000 } else { row * 10 }
        }),
        column(ColumnType::Integer, &|row| {
            if row < 256 { 5 } else { row * 1_000 + 5 }
        }),
        column(ColumnType::Integer, &|_| -7),
        // Thousands in every group, but the groups are not thousands apart.
        column(ColumnType::Integer, &|row| {
            row * 1_000 + if row < 256 { 0 } else { 20 }
        }),
    ];
    let file = write(&columns, 256);
    let reader = open(&file);
    let space = |column| Space::of(reader.directory(), column, &|_| true).unwrap();
    let is = |low, factor, span| Space::Integral { low, factor, span };
    assert_eq!(space(0), is(250_000, 1_000_000, 599 * 7 + 2));
    assert_eq!(space(1), is(0, 1_000, 599 * 599));
    assert_eq!(space(2), is(0, 10, 29_900));
    assert_eq!(space(3), is(5, 1_000, 599));
    assert_eq!(space(4).span(), 0);
    assert_eq!(space(5), is(0, 10, 59_902));
    assert_eq!(
        Space::of(reader.directory(), 0, &|group| group == 1).unwrap(),
        is(1_793_250_000, 1_000_000, 511 * 7 + 1 - 1_793)
    );
}

#[test]
fn texts_that_begin_alike_are_sorted_or_refused() {
    let shared = "https://shop.example.com/orders/and/more/";
    let cells: Vec<Model> = (0..600)
        .map(|row| Model::Text(format!("{shared}{:03}", (row * 7) % 300)))
        .collect();
    let columns = [ModelColumn {
        name: "a".to_string(),
        kind: Some(ColumnType::Text),
        dictionary: false,
        cells: cells.clone(),
    }];
    let file = write(&columns, 256);
    let mut expected: Vec<u32> = (0..600).collect();
    expected.sort_by_key(|row| (row * 7) % 300);
    let spec = Spec {
        filters: Vec::new(),
        order: vec![SortKey {
            column: 0,
            descending: false,
        }],
    };
    for run in [1, 50, 400, 1_000] {
        for texts in [0, 1_000, 10_000, 1 << 20] {
            for rounds in [0, 1, 4, 5, 8] {
                let mut session = Session::new(open(&file));
                session.limits.run = run;
                session.limits.texts = texts;
                session.limits.rounds = rounds;
                let mut source = Pieces::new(&file, 1 << 20, true);
                session.begin(spec.clone()).unwrap();
                let result = drive(&mut session, &mut source, &mut || 100);
                // Five times eight bytes are the same for all.
                let possible = texts >= 1 << 20 || rounds >= 5;
                if possible {
                    result.unwrap();
                    assert_eq!(all_of(&session), expected);
                } else if result.is_err() {
                    assert_eq!(result, Err(QueryError::TooManyEqualPrefixes));
                    assert_eq!(all_of(&session).len(), 600);
                    assert_eq!(session.view_row(5), Some(5));
                } else {
                    // What was left fitted after some rounds.
                    assert!(texts >= 1_000);
                    assert_eq!(all_of(&session), expected);
                }
            }
        }
    }
}

fn file_of(texts: &[String]) -> Vec<u8> {
    let column = ModelColumn {
        name: "a".to_string(),
        kind: Some(ColumnType::Text),
        dictionary: false,
        cells: texts.iter().cloned().map(Model::Text).collect(),
    };
    write(&[column], 256)
}

/// Sorts a file of texts, and looks whether it came out right.
fn sort_texts(
    (file, texts): (&[u8], &[String]),
    limits: &dyn Fn(&mut Limits),
    memory: usize,
) -> Result<(), QueryError> {
    let mut session = Session::new(open(file));
    limits(&mut session.limits);
    session.set_memory_limit(session.memory().saturating_add(memory));
    let mut source = Pieces::new(file, 1 << 20, true);
    let key = SortKey {
        column: 0,
        descending: false,
    };
    session
        .begin(Spec {
            filters: Vec::new(),
            order: vec![key],
        })
        .unwrap();
    drive(&mut session, &mut source, &mut || 100)?;
    let mut expected: Vec<u32> = (0..texts.len() as u32).collect();
    expected.sort_by_cached_key(|row| texts[*row as usize].to_lowercase());
    assert_eq!(all_of(&session), expected);
    Ok(())
}

fn sorted_texts(texts: &[String], limits: &dyn Fn(&mut Limits)) -> Result<(), QueryError> {
    sort_texts((&file_of(texts), texts), limits, usize::MAX)
}

#[test]
fn texts_are_kept_once_few_are_left() {
    // Too many long ones to keep all that begin alike, until all of them but
    // two are told apart.
    let mut texts: Vec<String> = (0..1_200).map(|row| format!("{row:08}")).collect();
    let filler = "and so on ".repeat(40);
    texts.extend((0..800).map(|row| format!("Dear customer, {:05} {filler}", (row * 7) % 800)));
    let few = |limits: &mut Limits| limits.texts = 200_000;
    sorted_texts(&texts, &few).unwrap();
    let twice = texts[1_300].clone();
    texts.push(twice);
    sorted_texts(&texts, &few).unwrap();
    // As long as the longest there are.
    let long = "x".repeat(150_000);
    texts.extend([format!("{long}b"), format!("{long}a"), format!("{long}b")]);
    sorted_texts(&texts, &few).unwrap_err();
    sorted_texts(&texts, &|limits| limits.texts = 320_000).unwrap();
}

#[test]
fn equal_texts_are_no_reason_to_refuse() {
    let shared = "The same as everywhere, and longer than can be read in rounds. ".repeat(6);
    let mut rng = Rng::from_seed(114);
    for (different, rows) in [(1, 600), (2, 600), (10, 600), (40, 300), (200, 1_000)] {
        // In any case, which makes no difference.
        let mut texts: Vec<String> = (0..rows)
            .map(|_| {
                let text = format!("{shared}{:04}", rng.next_below(different));
                if rng.next_below(2) == 0 {
                    text.to_uppercase()
                } else {
                    text
                }
            })
            .collect();
        texts.extend((0..100).map(|row| format!("{row}")));
        texts.extend((0..100).map(|row| format!("The same {}", row / 2)));
        for run in [1, 100, 32_768] {
            let limits = |limits: &mut Limits| {
                limits.run = run;
                // The different ones fit, all of them do not.
                limits.texts = 450 * different as usize;
            };
            sorted_texts(&texts, &limits).unwrap();
        }
    }
    // Nothing at all may be kept.
    let texts = vec![format!("{shared}{shared}"); 500];
    sorted_texts(&texts, &|limits| limits.texts = 0).unwrap();
}

#[test]
fn too_little_memory_is_not_too_many_equal_prefixes() {
    let shared = "The same as everywhere. ".repeat(20);
    let texts: Vec<String> = (0..600)
        .map(|row| format!("{shared}{}", row % 300))
        .collect();
    let file = file_of(&texts);
    let (mut short, mut fits) = (0, 0);
    for memory in (0..60).map(|step| step * 15_000) {
        for rounds in [0, 3, 16] {
            match sort_texts((&file, &texts), &|limits| limits.rounds = rounds, memory) {
                Ok(()) => fits += 1,
                Err(error) => {
                    assert_eq!(error, QueryError::OutOfMemory, "{memory} {rounds}");
                    short += 1;
                }
            }
        }
    }
    assert!(short > 20 && fits > 20, "{short} {fits}");
}

#[test]
fn a_step_reads_little_of_a_long_block() {
    // A block of 24 MiB in one group, some cells of which are far longer.
    let rows = 24_000;
    let cells: Vec<Model> = (0..rows)
        .map(|row| {
            if row % 50 == 49 {
                Model::Blank
            } else {
                let len = if row % 4_000 < 10 { 100_000 } else { 750 };
                Model::Text(format!("{row:08}{}", "Text ".repeat(len / 5)))
            }
        })
        .collect();
    let columns = [
        ModelColumn {
            name: "a".to_string(),
            kind: Some(ColumnType::Text),
            dictionary: false,
            cells,
        },
        ModelColumn {
            name: "b".to_string(),
            kind: Some(ColumnType::Boolean),
            dictionary: false,
            cells: (0..rows).map(|row| Model::Int(row as i64 % 2)).collect(),
        },
    ];
    let file = write(&columns, 32_768);
    assert!(open(&file).directory().block(0, 0).unwrap().len > 20 << 20);
    let mut session = Session::new(open(&file));
    let mut source = Pieces::new(&file, 1 << 20, true);
    let mut most = 0;
    let mut run = |session: &mut Session| {
        while session.running().is_some() {
            source.reads.clear();
            session.step(&mut source, 1).unwrap();
            most = most.max(source.reads.iter().map(|read| read.len).sum::<u64>());
        }
        all_of(session)
    };
    let begin = |session: &mut Session, filters, order: &[usize]| {
        let key = |column: &usize| SortKey {
            column: *column,
            descending: false,
        };
        let order = order.iter().map(key).collect();
        session.begin(Spec { filters, order }).unwrap();
    };
    let contains = (0, Condition::TextContains("7TEXT text".to_string()));
    let odd = (
        1,
        Condition::OneOf {
            values: vec![Value::Int(1)],
        },
    );
    let sevens = |step: usize| (7..rows as u32).step_by(step).filter(|row| row % 50 != 49);
    begin(&mut session, vec![contains.clone()], &[]);
    assert!(run(&mut session).into_iter().eq(sevens(10)));
    begin(&mut session, vec![odd.clone(), contains], &[]);
    assert!(run(&mut session).into_iter().eq(sevens(10)));
    begin(&mut session, vec![(0, Condition::Blank)], &[]);
    assert!(
        run(&mut session)
            .into_iter()
            .eq((49..rows as u32).step_by(50))
    );
    begin(&mut session, vec![(0, Condition::NotBlank), odd], &[0]);
    assert_eq!(run(&mut session).len(), rows / 2 - rows / 50);
    begin(&mut session, Vec::new(), &[1, 0]);
    assert_eq!(run(&mut session).len(), rows);
    session.begin_summary(vec![0]).unwrap();
    session.begin_values(0, String::new(), 1_000).unwrap();
    run(&mut session);
    assert_eq!(
        session.take_summary().unwrap()[0].blanks as usize,
        rows / 50
    );
    assert_eq!(session.take_values().unwrap().blanks as usize, rows / 50);
    // What a step reads of the block, a cell more than is enough, and pages.
    assert!(most <= 6 << 20, "{most}");
}

#[test]
fn equal_texts_of_any_length_do_not_take_rounds() {
    let long = "x".repeat(100_000);
    let cells = |row: usize| match row % 4 {
        0 => Model::Text(long.clone()),
        1 => Model::Text(format!("{long}b")),
        2 => Model::Text(format!("{long}A")),
        _ => Model::Text(long.to_uppercase()),
    };
    for dictionary in [false, true] {
        let columns = [ModelColumn {
            name: "a".to_string(),
            kind: Some(ColumnType::Text),
            dictionary,
            cells: (0..20).map(cells).collect(),
        }];
        let file = write(&columns, 256);
        let mut session = Session::new(open(&file));
        let mut source = Pieces::new(&file, 1 << 20, true);
        let key = SortKey {
            column: 0,
            descending: false,
        };
        session
            .begin(Spec {
                filters: Vec::new(),
                order: vec![key],
            })
            .unwrap();
        drive(&mut session, &mut source, &mut || 1).unwrap();
        let mut expected: Vec<u32> = (0..20).collect();
        expected.sort_by_key(|row| [0, 2, 1, 0][(row % 4) as usize]);
        assert_eq!(all_of(&session), expected);
    }
}

fn blocks_of(file: &[u8], column: usize) -> Vec<(u64, u64)> {
    let reader = open(file);
    let directory = reader.directory();
    (0..directory.groups())
        .map(|group| directory.block(column, group).unwrap())
        .filter(|block| block.encoding != Encoding::Blank)
        // Pages are shared by neighbours; the middle of a block is its own.
        .map(|block| (block.at + u64::from(block.len) / 2, 1))
        .collect()
}

#[test]
fn only_what_is_needed_is_read() {
    // Blocks of several pages, so that each has some of its own.
    let rows = 8 * 4_096;
    let column = |name: &str, cell: &dyn Fn(usize) -> i64| ModelColumn {
        name: name.to_string(),
        kind: Some(ColumnType::Integer),
        dictionary: false,
        cells: (0..rows).map(|row| Model::Int(cell(row))).collect(),
    };
    let noise = |row: usize| (reticula_core::rng::mix64(row as u64) >> 2).cast_signed();
    let columns = [
        column("row", &|row| row as i64),
        column("noise", &noise),
        column("more", &|row| noise(row + rows)),
    ];
    let file = write(&columns, 4_096);
    let blocks = blocks_of(&file, 1);
    let more = blocks_of(&file, 2);
    assert_eq!(blocks.len(), 8);
    let mut session = Session::new(open(&file));
    let mut source = Pieces::new(&file, 1 << 20, true);
    let between = |a: i64, b: i64| Condition::Number {
        op: NumberOp::Between,
        a: a.to_string(),
        b: Some(b.to_string()),
    };
    // Rows of the third group only, as the directory tells.
    let filters = vec![
        (0, between(8_192, 12_287)),
        (1, between(0, i64::MAX)),
        (2, between(0, i64::MAX / 8)),
    ];
    let ask = |session: &mut Session, source: &mut Pieces<'_>, filters, order| {
        source.reads.clear();
        session.begin(Spec { filters, order }).unwrap();
        drive(session, source, &mut || 1 << 20).unwrap();
    };
    let by = |column| {
        vec![SortKey {
            column,
            descending: false,
        }]
    };
    ask(&mut session, &mut source, filters.clone(), Vec::new());
    for (group, (at, len)) in more.iter().enumerate() {
        assert_eq!(source.has_read(*at, *len), group == 2);
    }
    for (at, len) in &blocks {
        // All values are in the range, so nobody has to look.
        assert!(!source.has_read(*at, *len));
    }
    let few = session.view_len();
    assert!(few > 100 && few < 4_096);

    // Only the keys of these rows are read, and with the same filter again
    // nothing but the keys.
    ask(&mut session, &mut source, filters.clone(), by(1));
    for (group, (at, len)) in blocks.iter().enumerate() {
        assert_eq!(source.has_read(*at, *len), group == 2);
    }
    for (at, len) in &more {
        assert!(!source.has_read(*at, *len));
    }

    // An order of all rows is kept, and other filters go through it.
    ask(&mut session, &mut source, Vec::new(), by(1));
    assert!(blocks.iter().all(|(at, len)| source.has_read(*at, *len)));
    let sorted = all_of(&session);
    let memory = session.memory();
    ask(&mut session, &mut source, filters, by(1));
    assert!(blocks.iter().all(|(at, len)| !source.has_read(*at, *len)));
    assert_eq!(session.view_len(), few);
    ask(&mut session, &mut source, Vec::new(), by(1));
    assert!(source.reads.is_empty());
    assert_eq!(all_of(&session), sorted);
    assert_eq!(session.memory(), memory);

    // Values that are in order already need no order, and all rows no list.
    ask(&mut session, &mut source, Vec::new(), by(0));
    assert_eq!(all_of(&session), (0..rows as u32).collect::<Vec<_>>());
    ask(&mut session, &mut source, Vec::new(), Vec::new());
    assert!(source.reads.is_empty());
    assert!(
        session.memory() + rows * 4 <= memory,
        "the order was dropped"
    );

    // The sum of a few rows of a block is taken from those rows.
    session.limits.sparse = 1 << 20;
    ask(
        &mut session,
        &mut source,
        vec![(0, between(8_192, 8_193))],
        Vec::new(),
    );
    session.begin_summary(vec![1]).unwrap();
    source.reads.clear();
    drive(&mut session, &mut source, &mut || 1 << 20).unwrap();
    assert!(source.reads.iter().map(|read| read.len).sum::<u64>() <= 3 * 16_384);
    let summary = session.take_summary().unwrap();
    let sum = i128::from(noise(8_192)) + i128::from(noise(8_193));
    assert_eq!(summary[0].sum.as_ref().unwrap().text, sum.to_string());
}

#[test]
fn an_order_is_kept_for_fewer_rows() {
    let rows = 4 * 4_096;
    let noise = |row: usize| (reticula_core::rng::mix64(row as u64) >> 2).cast_signed();
    let column = |cell: &dyn Fn(usize) -> i64| ModelColumn {
        name: "a".to_string(),
        kind: Some(ColumnType::Integer),
        dictionary: false,
        cells: (0..rows).map(|row| Model::Int(cell(row))).collect(),
    };
    let file = write(&[column(&|row| row as i64), column(&noise)], 4_096);
    let blocks = blocks_of(&file, 1);
    let mut session = Session::new(open(&file));
    session.limits.sparse = usize::MAX;
    session.limits.few = 2;
    let mut source = Pieces::new(&file, 1 << 20, true);
    // Of some rows, of fewer, of others, and of fewer than those.
    for (from, to, reads) in [
        (0, 6_000, true),
        (100, 5_000, false),
        (100, 7_000, true),
        (101, 7_000, false),
        (101, 7_001, true),
    ] {
        let filter = Condition::Number {
            op: NumberOp::Between,
            a: from.to_string(),
            b: Some((to - 1).to_string()),
        };
        source.reads.clear();
        session
            .begin(Spec {
                filters: vec![(0, filter)],
                order: vec![SortKey {
                    column: 1,
                    descending: true,
                }],
            })
            .unwrap();
        drive(&mut session, &mut source, &mut || 1 << 20).unwrap();
        let mut expected: Vec<u32> = (from..to).collect();
        expected.sort_by_key(|row| Reverse(noise(*row as usize)));
        assert_eq!(all_of(&session), expected);
        let (at, len) = blocks[0];
        assert_eq!(source.has_read(at, len), reads, "{from}..{to}");
    }
}

#[test]
fn what_is_under_way_is_about_the_view_that_comes() {
    let mut rng = Rng::from_seed(113);
    let table = table(&mut rng, 1_100);
    let mut source = Pieces::new(&table.file, 1 << 20, true);
    let filtered = Spec {
        filters: vec![(
            3,
            Condition::Top {
                n: 40,
                bottom: false,
            },
        )],
        order: Vec::new(),
    };
    let rows = table.rows_of(&filtered.filters);
    let expected = |session: &mut Session| {
        let mut summary = session.take_summary().unwrap().remove(0);
        check_mean(&table, 1, &rows, summary.mean.take());
        assert_eq!(summary, table.summary(1, &rows));
    };
    // Each is stopped after every number of steps.
    for stop in 1..12 {
        let mut session = Session::new(open(&table.file));
        session.begin_summary(vec![1]).unwrap();
        let mut begun = 0;
        while begun < stop && session.running() == Some(Job::Summary) {
            session.step(&mut source, 1).unwrap();
            begun += 1;
        }
        let complete = session.running().is_none();
        session.begin(filtered.clone()).unwrap();
        // What was complete and not taken was about another view.
        assert_eq!(session.take_summary(), None);
        drive(&mut session, &mut source, &mut || 100).unwrap();
        if complete {
            assert_eq!(session.running(), None);
        } else {
            assert_eq!(session.running(), Some(Job::Summary));
            drive(&mut session, &mut source, &mut || 100).unwrap();
            expected(&mut session);
        }
    }
    let (mut under_way, mut complete) = (0, 0);
    for stop in 1..60 {
        let mut session = Session::new(open(&table.file));
        session.begin_values(8, String::new(), 1_000).unwrap();
        let mut begun = 0;
        while begun < stop && session.running() == Some(Job::Values) {
            session.step(&mut source, 1).unwrap();
            begun += 1;
        }
        let done = session.running().is_none();
        session.begin(filtered.clone()).unwrap();
        assert_eq!(session.take_values(), None);
        drive(&mut session, &mut source, &mut || 100).unwrap();
        if done {
            complete += 1;
            assert_eq!(session.running(), None);
        } else {
            under_way += 1;
            drive(&mut session, &mut source, &mut || 100).unwrap();
            let values = session.take_values().unwrap();
            assert_eq!(values, table.values(&filtered, 8, "", 1_000));
        }
    }
    assert!(under_way > 5 && complete > 5, "{under_way} {complete}");
}

#[test]
fn jobs_are_replaced_restarted_and_cancelled() {
    let mut rng = Rng::from_seed(108);
    let table = table(&mut rng, 1_100);
    let mut session = Session::new(open(&table.file));
    let mut source = Pieces::new(&table.file, 1 << 20, true);
    assert_eq!(session.step(&mut source, 1), Ok(Progress::Done));
    let spec = |column| Spec {
        filters: Vec::new(),
        order: vec![SortKey {
            column,
            descending: true,
        }],
    };
    let used = session.memory();
    session.begin(spec(2)).unwrap();
    for _ in 0..5 {
        session.step(&mut source, 1).unwrap();
    }
    session.begin(spec(12)).unwrap();
    for _ in 0..9 {
        session.step(&mut source, 1).unwrap();
    }
    session.cancel();
    assert_eq!(session.running(), None);
    assert_eq!(all_of(&session), (0..1_100).collect::<Vec<_>>());
    assert!(session.memory() < used + 600_000);

    // A summary and a list that are under way are about the view that comes.
    session.begin_summary(vec![3]).unwrap();
    session.begin_values(8, String::new(), 1_000).unwrap();
    for _ in 0..3 {
        session.step(&mut source, 1).unwrap();
    }
    assert_eq!(session.running(), Some(Job::Values));
    let filtered = Spec {
        filters: vec![(
            7,
            Condition::OneOf {
                values: vec![Value::Int(1)],
            },
        )],
        order: Vec::new(),
    };
    session.begin(filtered.clone()).unwrap();
    for job in [Job::View, Job::Values, Job::Summary] {
        assert_eq!(session.running(), Some(job));
        drive(&mut session, &mut source, &mut || 100).unwrap();
    }
    let rows = table.rows_of(&filtered.filters);
    let mut summary = session.take_summary().unwrap().remove(0);
    check_mean(&table, 3, &rows, summary.mean.take());
    assert_eq!(summary, table.summary(3, &rows));
    let values = session.take_values().unwrap();
    assert_eq!(values, table.values(&filtered, 8, "", 1_000));

    // One job is given up, the other goes on.
    session.begin_summary(vec![3]).unwrap();
    session.begin_values(8, String::new(), 1_000).unwrap();
    session.cancel_job(Job::Values);
    assert_eq!(session.running(), Some(Job::Summary));
}

#[test]
fn values_beyond_the_limit_are_left_out() {
    let column = |kind, cell: &dyn Fn(i64) -> Model| ModelColumn {
        name: "a".to_string(),
        kind: Some(kind),
        dictionary: false,
        cells: (0..500).map(cell).collect(),
    };
    let wide = |row: i64| Model::Int((row % 400) * 1_000_000_007);
    let texts = |row: i64| Model::Text(format!("text {}", row % 400));
    let columns = [
        column(ColumnType::Integer, &wide),
        column(ColumnType::Text, &texts),
    ];
    let file = write(&columns, 256);
    let mut session = Session::new(open(&file));
    session.limits.direct = 10;
    session.limits.distinct = 300;
    let mut source = Pieces::new(&file, 1 << 20, true);
    for column in [0, 1] {
        session.begin_values(column, String::new(), 1_000).unwrap();
        drive(&mut session, &mut source, &mut || 1).unwrap();
        let list = session.take_values().unwrap();
        assert!(!list.complete);
        assert_eq!(list.values.len(), 300);
        // Those that are listed are counted to the end.
        assert_eq!(
            list.values.iter().map(|value| value.count).sum::<u64>(),
            400
        );
    }
    session.limits.listed = 100;
    session.limits.distinct = 1_000;
    session.begin_values(1, String::new(), 1_000).unwrap();
    drive(&mut session, &mut source, &mut || 1).unwrap();
    let list = session.take_values().unwrap();
    assert!(!list.complete && !list.values.is_empty());
}

/// Asks everything of a file that may be damaged.
fn exercise(rng: &mut Rng, file: &[u8]) {
    let mut whole = file;
    let opened = crate::reader::Reader::open(&mut whole, file.len() as u64);
    let Ok(crate::reader::Opened::Ready(reader)) = opened else {
        return;
    };
    let mut session = Session::new(*reader);
    session.limits = some_limits(rng);
    // Nothing is sorted for ever.
    session.limits.rounds = 20;
    // A damaged directory may claim millions of rows.
    session.set_memory_limit(64 << 20);
    let columns = session.reader().columns().len();
    for column in 0..columns {
        let kind = session.reader().columns()[column].kind;
        let condition = match kind {
            ColumnType::Text => Condition::TextContains("a".to_string()),
            ColumnType::Boolean => Condition::OneOf {
                values: vec![Value::Int(1)],
            },
            _ => Condition::Top {
                n: 10,
                bottom: true,
            },
        };
        let spec = Spec {
            filters: if rng.next_below(2) == 0 {
                vec![(column, condition)]
            } else {
                Vec::new()
            },
            order: vec![SortKey {
                column,
                descending: rng.next_below(2) == 0,
            }],
        };
        session.begin(spec).unwrap();
        session.begin_summary(vec![column]).unwrap();
        session.begin_values(column, "1".to_string(), 10).unwrap();
        while session.running().is_some() {
            if let Ok(Progress::Need(ranges)) = session.step(&mut whole, u32::MAX) {
                panic!("{ranges:?}");
            }
        }
    }
}

#[test]
fn damage_is_noticed_and_the_view_stays() {
    let mut rng = Rng::from_seed(109);
    let table = table(&mut rng, 600);
    let directory_at = open(&table.file).directory().data_len as usize;
    let mut noticed = 0;
    for _ in 0..300 {
        let mut damaged = table.file.clone();
        damaged[rng.next_below(directory_at as u64) as usize] ^= 1 << rng.next_below(8);
        let mut session = Session::new(open(&damaged));
        let mut source = Pieces::new(&damaged, 1 << 20, true);
        for column in 0..table.kinds.len() {
            let spec = Spec {
                filters: vec![(column, Condition::NotBlank)],
                order: vec![SortKey {
                    column,
                    descending: false,
                }],
            };
            let before = all_of(&session);
            session.begin(spec.clone()).unwrap();
            match drive(&mut session, &mut source, &mut || 1_000) {
                Ok(()) => assert_eq!(all_of(&session), table.view(&spec)),
                Err(error) => {
                    assert!(matches!(error, QueryError::Read(ReadError::Corrupt(_))));
                    assert_eq!(all_of(&session), before);
                    noticed += 1;
                }
            }
        }
    }
    assert!(noticed > 100, "{noticed}");
}

#[test]
fn bytes_that_claim_to_be_a_table_are_survived() {
    let mut rng = Rng::from_seed(110);
    let table = table(&mut rng, 600);
    exercise(&mut rng, &table.file);
    let data_len = open(&table.file).directory().data_len as usize;
    for round in 0..600 {
        let mut damaged = table.file.clone();
        let directory_only = round % 2 == 0;
        for _ in 0..=rng.next_below(6) {
            let (from, to) = if directory_only {
                (data_len, damaged.len() - 64)
            } else {
                (16, data_len)
            };
            let at = from + rng.next_below((to - from) as u64) as usize;
            damaged[at] = rng.next_u64().to_le_bytes()[0];
        }
        reseal(&mut damaged, data_len);
        exercise(&mut rng, &damaged);
    }
}

#[test]
fn every_entry_of_a_dictionary_occurs() {
    // The number of different texts of a whole column relies on it.
    let mut rng = Rng::from_seed(111);
    let columns: Vec<ModelColumn> = [true, true]
        .map(|dictionary| ModelColumn {
            name: "a".to_string(),
            kind: Some(ColumnType::Text),
            dictionary,
            cells: (0..1_000)
                .map(|_| Model::Text(some_text(&mut rng)))
                .collect(),
        })
        .into();
    let file = write(&columns, 256);
    let reader = open(&file);
    for (column, of) in columns.iter().zip(reader.columns()) {
        let texts: BTreeSet<String> = column
            .cells
            .iter()
            .filter_map(|cell| match cell {
                Model::Text(text) if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(of.dictionary.unwrap().entries as usize, texts.len());
    }
}

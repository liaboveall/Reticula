//! Property-based tests of tables: written and read, imported from delimited
//! text, filtered, sorted, added up and listed, compared with the plain way to
//! do it. `RETICULA_TABLE_CASES=10` runs ten times as many cases.

use std::cmp::{Ordering, Reverse};
use std::collections::BTreeMap;

use proptest::prelude::*;
use reticula_core::rng::Rng;
use reticula_table::calendar::{DAY_MICROS, civil_from_days, days_from_civil};
use reticula_table::csv;
use reticula_table::layout::Encoding;
use reticula_table::query::{Condition, NumberOp, Session, Spec, TimeOp, Value};
use reticula_table::reader::BlockBuffer;
use reticula_table::text::{write_decimal, write_value};
use reticula_table::types::{CellValue, ColumnType};

use reticula_kernel_bench::tables::{
    Cell, Column, InMemory, Pieces, Table, import_csv, open, pick, read_all, rows_of, run,
    scale_of, some_filters, some_order, some_part, write,
};

fn cases(cases: u32) -> ProptestConfig {
    let times = std::env::var("RETICULA_TABLE_CASES")
        .ok()
        .and_then(|times| times.parse().ok())
        .unwrap_or(1);
    ProptestConfig::with_cases(cases * times)
}

/// Numbers of rows, also at the edges of groups.
fn rows() -> impl Strategy<Value = usize> {
    prop_oneof![
        2 => 0_usize..300,
        1 => prop::sample::select(vec![255_usize, 256, 257, 511, 512, 513, 4_096, 4_097]),
        1 => 300_usize..3_000,
        1 => 3_000_usize..20_000,
    ]
}

const TEXTS: [&str; 24] = [
    "",
    "a",
    "A",
    "a\0",
    "ab",
    "aB",
    "b",
    "North",
    "NORTH",
    "north ",
    "customer-0001",
    "Customer-0001",
    "customer-0001-a",
    "customer",
    "12345678",
    "123456789",
    "ΟΔΥΣΣΕΥΣ",
    "οδυσσευς",
    "Σ",
    "İstanbul",
    "漢字",
    "é",
    "É",
    "\u{10ffff}",
];

fn some_text(rng: &mut Rng, domain: u64) -> String {
    match rng.next_below(6) {
        0 => format!("customer-{:04}", rng.next_below(domain)),
        1 => format!("{:016x}", rng.next_u64()),
        _ => (*pick(rng, &TEXTS)).to_owned(),
    }
}

/// Columns of every type, made of a seed.
fn columns(rng: &mut Rng, rows: usize) -> Vec<Column> {
    let domain = *pick(rng, &[1_u64, 3, 20, 5_000]);
    let blank_in = *pick(rng, &[2_u64, 10, u64::MAX]);
    let mut all = Vec::new();
    let mut add = |kind: Option<ColumnType>,
                   dictionary: bool,
                   rng: &mut Rng,
                   cell: &dyn Fn(&mut Rng, usize) -> Cell| {
        let cells = (0..rows)
            .map(|row| {
                if rng.next_below(blank_in) == 0 {
                    Cell::Blank
                } else {
                    cell(rng, row)
                }
            })
            .collect();
        all.push(Column {
            name: format!("column {}", all.len()),
            kind,
            dictionary,
            cells,
        });
    };
    let decimal = ColumnType::Decimal {
        scale: rng.next_below(19) as u8,
        fixed: rng.next_below(2) == 0,
    };
    add(Some(ColumnType::Integer), false, rng, &|_, row| {
        Cell::Int(row as i64 * 7)
    });
    add(Some(ColumnType::Integer), false, rng, &|rng, _| {
        Cell::Int((rng.next_below(domain) as i64 - 2) * 1_000)
    });
    add(Some(ColumnType::Integer), false, rng, &|rng, _| {
        Cell::Int(match rng.next_below(4) {
            0 => i64::MIN,
            1 => i64::MAX,
            _ => rng.next_u64().cast_signed(),
        })
    });
    add(Some(decimal), false, rng, &|rng, _| {
        Cell::Int(rng.next_below(domain * 100) as i64 - 50)
    });
    add(Some(ColumnType::Float), false, rng, &|rng, _| {
        Cell::Float(match rng.next_below(5) {
            0 => -0.0,
            1 => 0.1,
            2 => (rng.next_f64() - 0.5) * 1e15,
            _ => rng.next_below(domain) as f64 / 8.0,
        })
    });
    add(Some(ColumnType::Date), false, rng, &|rng, _| {
        Cell::Int(19_000 + rng.next_below(domain) as i64)
    });
    add(Some(ColumnType::DateTime), false, rng, &|rng, row| {
        Cell::Int((row as i64 * 40 + rng.next_below(3) as i64 - 50_000) * 1_000_000)
    });
    // Where days begin, and around that.
    add(Some(ColumnType::DateTime), false, rng, &|rng, _| {
        let half_days = rng.next_below(domain.min(20)) as i64 - 3;
        Cell::Int(half_days * (DAY_MICROS / 2) + rng.next_below(3) as i64 - 1)
    });
    add(Some(ColumnType::Boolean), false, rng, &|rng, _| {
        Cell::Int(rng.next_below(2) as i64)
    });
    add(Some(ColumnType::Text), true, rng, &|rng, _| {
        Cell::Text(some_text(rng, domain))
    });
    add(Some(ColumnType::Text), false, rng, &|rng, _| {
        Cell::Text(some_text(rng, domain))
    });
    add(None, true, rng, &|rng, row| {
        if row * 2 < rows {
            Cell::Blank
        } else {
            Cell::Text(some_text(rng, domain))
        }
    });
    add(None, false, rng, &|_, _| Cell::Blank);
    all
}

/// What is stored of the cells: the empty text is a blank, and there is one zero.
fn stored(columns: &[Column]) -> (Vec<ColumnType>, Vec<Vec<Cell>>) {
    let kinds = columns
        .iter()
        .map(|column| column.kind.unwrap_or(ColumnType::Text))
        .collect();
    let cells = columns
        .iter()
        .map(|column| {
            let cells = column.cells.iter().map(|cell| match cell {
                Cell::Text(text) if text.is_empty() => Cell::Blank,
                Cell::Float(value) if *value == 0.0 => Cell::Float(0.0),
                other => other.clone(),
            });
            cells.collect()
        })
        .collect();
    (kinds, cells)
}

// The plain way.

struct Plain {
    kinds: Vec<ColumnType>,
    columns: Vec<Vec<Cell>>,
    rows: usize,
}

/// A decimal number without the zeros that say nothing: whether it is below
/// zero, and the digits before and after the point.
fn digits(text: &str) -> (bool, String, String) {
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let exponent: i64 = exponent.parse().expect("an exponent");
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut all = format!("{whole}{fraction}");
    let mut point = whole.len() as i64 + exponent.clamp(-500, 500);
    while point < 0 {
        all.insert(0, '0');
        point += 1;
    }
    while (all.len() as i64) < point {
        all.push('0');
    }
    let (whole, fraction) = all.split_at(point as usize);
    let (whole, fraction) = (
        whole.trim_start_matches('0'),
        fraction.trim_end_matches('0'),
    );
    let zero = whole.is_empty() && fraction.is_empty();
    (negative && !zero, whole.to_owned(), fraction.to_owned())
}

fn compare_decimals(a: &str, b: &str) -> Ordering {
    let ((a_negative, a_whole, a_rest), (b_negative, b_whole, b_rest)) = (digits(a), digits(b));
    let size = (a_whole.len(), a_whole, a_rest).cmp(&(b_whole.len(), b_whole, b_rest));
    match (a_negative, b_negative) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => size,
        (true, true) => size.reverse(),
    }
}

fn compare(a: &Cell, b: &Cell) -> Ordering {
    match (a, b) {
        (Cell::Int(a), Cell::Int(b)) => a.cmp(b),
        (Cell::Float(a), Cell::Float(b)) => a.partial_cmp(b).expect("numbers are finite"),
        (Cell::Text(a), Cell::Text(b)) => {
            a.to_lowercase().as_bytes().cmp(b.to_lowercase().as_bytes())
        }
        _ => panic!("{a:?} {b:?}"),
    }
}

impl Plain {
    fn compare_number(&self, column: usize, cell: &Cell, threshold: &str) -> Ordering {
        match cell {
            Cell::Int(value) => {
                let mut text = String::new();
                write_decimal(
                    i128::from(*value),
                    scale_of(self.kinds[column]),
                    true,
                    &mut text,
                );
                compare_decimals(&text, threshold)
            }
            Cell::Float(value) => value
                .partial_cmp(&threshold.parse().expect("a number"))
                .expect("numbers are finite"),
            _ => panic!("{cell:?}"),
        }
    }

    /// The value that the cells are compared with which are among the first ones.
    fn limit(&self, column: usize, n: u32, bottom: bool) -> Option<Cell> {
        let mut cells: Vec<&Cell> = self.columns[column]
            .iter()
            .filter(|cell| **cell != Cell::Blank)
            .collect();
        cells.sort_by(|a, b| if bottom { compare(a, b) } else { compare(b, a) });
        let last = (n as usize).min(cells.len()).checked_sub(1)?;
        Some(cells[last].clone())
    }

    fn holds(
        &self,
        column: usize,
        condition: &Condition,
        limit: Option<&Cell>,
        row: usize,
    ) -> bool {
        let cell = &self.columns[column][row];
        let blank = *cell == Cell::Blank;
        let folded = match cell {
            Cell::Text(text) => text.to_lowercase(),
            _ => String::new(),
        };
        match condition {
            Condition::Blank => blank,
            Condition::NotBlank => !blank,
            Condition::OneOf { values } => values.iter().any(|value| match (value, cell) {
                (Value::Blank, Cell::Blank) => true,
                (Value::Int(a), Cell::Int(b)) => a == b,
                (Value::Float(a), Cell::Float(b)) => f64::from_bits(*a) == *b,
                (Value::Text(text), Cell::Blank) => text.is_empty(),
                (Value::Text(text), Cell::Text(_)) => text.to_lowercase() == folded,
                _ => false,
            }),
            Condition::TextContains(part) => folded.contains(&part.to_lowercase()),
            Condition::TextStartsWith(part) => folded.starts_with(&part.to_lowercase()),
            Condition::TextEndsWith(part) => folded.ends_with(&part.to_lowercase()),
            _ if blank => false,
            Condition::Number { op, a, b } => {
                let order = self.compare_number(column, cell, a);
                match op {
                    NumberOp::Equal => order.is_eq(),
                    NumberOp::NotEqual => order.is_ne(),
                    NumberOp::Greater => order.is_gt(),
                    NumberOp::AtLeast => order.is_ge(),
                    NumberOp::Less => order.is_lt(),
                    NumberOp::AtMost => order.is_le(),
                    NumberOp::Between => {
                        let other = self.compare_number(column, cell, b.as_ref().expect("two"));
                        (order.is_ge() && other.is_le()) || (order.is_le() && other.is_ge())
                    }
                }
            }
            Condition::Time { op, a, b } => {
                let Cell::Int(value) = cell else {
                    panic!("{cell:?}");
                };
                let day = |value: i64| value.div_euclid(DAY_MICROS);
                match op {
                    TimeOp::Before => value < a,
                    TimeOp::After => value > a,
                    TimeOp::Between => {
                        let b = b.expect("two");
                        (*a.min(&b)..=*a.max(&b)).contains(value)
                    }
                    TimeOp::On if self.kinds[column] == ColumnType::DateTime => {
                        day(*value) == day(*a)
                    }
                    TimeOp::On => value == a,
                }
            }
            Condition::Top { bottom, .. } => limit.is_some_and(|limit| {
                let order = compare(cell, limit);
                if *bottom {
                    order.is_le()
                } else {
                    order.is_ge()
                }
            }),
        }
    }

    fn rows_of(&self, filters: &[(usize, Condition)]) -> Vec<u32> {
        let limits: Vec<Option<Cell>> = filters
            .iter()
            .map(|(column, condition)| match condition {
                Condition::Top { n, bottom } => self.limit(*column, *n, *bottom),
                _ => None,
            })
            .collect();
        (0..self.rows)
            .filter(|row| {
                filters
                    .iter()
                    .zip(&limits)
                    .all(|((column, condition), limit)| {
                        self.holds(*column, condition, limit.as_ref(), *row)
                    })
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
                let order = match (a == &Cell::Blank, b == &Cell::Blank) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) if key.descending => compare(b, a),
                    (false, false) => compare(a, b),
                };
                if order.is_ne() {
                    return order;
                }
            }
            Ordering::Equal
        });
        rows
    }

    fn shown(&self, column: usize, cell: &Cell) -> String {
        let mut out = String::new();
        let value = match cell {
            Cell::Blank => CellValue::Blank,
            Cell::Int(value) => CellValue::Int(*value),
            Cell::Float(value) => CellValue::Float(*value),
            Cell::Text(text) => return text.clone(),
        };
        write_value(self.kinds[column], value, &mut out);
        out
    }

    /// The values of some rows, with how often they occur, in their order.
    fn counted(&self, column: usize, rows: &[u32], search: &str) -> Vec<(Cell, u64)> {
        let mut counted: BTreeMap<Vec<u8>, (Cell, u64)> = BTreeMap::new();
        for row in rows {
            let cell = &self.columns[column][*row as usize];
            let key = match cell {
                Cell::Blank => continue,
                Cell::Int(value) => (value.cast_unsigned() ^ (1 << 63)).to_be_bytes().to_vec(),
                Cell::Float(value) => {
                    let bits = value.to_bits().cast_signed();
                    let key = if bits < 0 { !bits } else { bits ^ i64::MIN };
                    key.to_be_bytes().to_vec()
                }
                Cell::Text(text) => text.to_lowercase().into_bytes(),
            };
            counted.entry(key).or_insert_with(|| (cell.clone(), 0)).1 += 1;
        }
        let search = search.to_lowercase();
        counted
            .into_values()
            .filter(|(cell, _)| self.shown(column, cell).to_lowercase().contains(&search))
            .collect()
    }
}

proptest! {
    #![proptest_config(cases(120))]

    #[test]
    fn what_is_written_is_read(
        seed in any::<u64>(),
        rows in rows(),
        group_rows in prop::sample::select(vec![256_u32, 512, 1_024, 65_536]),
    ) {
        let mut rng = Rng::from_seed(seed);
        let columns = columns(&mut rng, rows);
        let file = write(&columns, group_rows);
        prop_assert_eq!(&file, &write(&columns, group_rows));
        let (kinds, cells) = stored(&columns);
        let (found_kinds, found) = read_all(&file).expect("what was written is a table");
        prop_assert_eq!(found_kinds, kinds);
        for (found, cells) in found.iter().zip(&cells) {
            prop_assert!(found == cells);
        }

        // Block by block, and what the directory says of them.
        let mut reader = open(&file).expect("what was written is a table");
        let mut source = file.as_slice();
        let mut buffer = BlockBuffer::default();
        for (column, cells) in cells.iter().enumerate() {
            while !reader.load_dictionary(&mut source, column).expect("it can be read") {}
            for (group, cells) in cells.chunks(group_rows as usize).enumerate() {
                reader
                    .block(&mut source, column, group as u32, &mut buffer)
                    .expect("it can be read");
                prop_assert_eq!(buffer.rows(), cells.len());
                let blanks = cells.iter().filter(|cell| **cell == Cell::Blank).count();
                prop_assert_eq!(buffer.block().blanks as usize, blanks);
                let (low, high) = (buffer.block().low, buffer.block().high);
                for (row, cell) in cells.iter().enumerate() {
                    prop_assert_eq!(buffer.is_blank(row), *cell == Cell::Blank);
                    match cell {
                        Cell::Blank => {}
                        Cell::Int(value) => {
                            prop_assert_eq!(buffer.int(row), *value);
                            prop_assert!(low.cast_signed() <= *value && *value <= high.cast_signed());
                        }
                        Cell::Float(value) => {
                            prop_assert_eq!(buffer.float(row), *value);
                            prop_assert!(f64::from_bits(low) <= *value && *value <= f64::from_bits(high));
                        }
                        Cell::Text(text) if buffer.block().encoding == Encoding::Text => {
                            prop_assert_eq!(buffer.text(row), text.as_bytes());
                        }
                        Cell::Text(text) => {
                            let dictionary = reader.dictionary(column).expect("it was loaded");
                            prop_assert_eq!(dictionary.text(buffer.code(row)), text);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn views_and_lists_are_those_of_the_plain_way(
        seed in any::<u64>(),
        rows in rows(),
        group_rows in prop::sample::select(vec![256_u32, 1_024, 65_536]),
        piece in prop::sample::select(vec![4_096_u64, 30_000, 1 << 20]),
    ) {
        let mut rng = Rng::from_seed(seed);
        let columns = columns(&mut rng, rows);
        let file = write(&columns, group_rows);
        let (kinds, columns) = stored(&columns);
        let plain = Plain { kinds, columns, rows };
        let mut session = Session::new(open(&file).expect("what was written is a table"));
        let mut source = Pieces::new(&file, piece);
        let mut from = InMemory { kinds: &plain.kinds, columns: &plain.columns };
        let mut order = Vec::new();
        for _ in 0..5 {
            if rng.next_below(3) > 0 {
                order = some_order(&mut rng, plain.kinds.len());
            }
            let filters = some_filters(&mut rng, &mut from);
            let spec = Spec { filters, order: order.clone() };
            session.begin(spec.clone()).expect("it can be asked");
            let mut steps = rng.clone();
            let mut budget = move || *pick(&mut steps, &[1, 700, 20_000, u32::MAX]);
            run(&mut session, &mut source, &mut budget, &mut |source, ranges| source.fetch(ranges))
                .expect("it can be answered");
            prop_assert_eq!(rows_of(&session), plain.view(&spec), "{:?}", spec);

            let column = rng.next_below(plain.kinds.len() as u64) as usize;
            let search = match rng.next_below(3) {
                0 => some_part(&mut rng, &mut from, column),
                1 => (*pick(&mut rng, &["1", "-", "2", "true"])).to_owned(),
                _ => String::new(),
            };
            let limit = *pick(&mut rng, &[0, 1, 7, 1_000]);
            session.begin_values(column, search.clone(), limit).expect("the column is there");
            run(&mut session, &mut source, &mut budget, &mut |source, ranges| source.fetch(ranges))
                .expect("it can be answered");
            let list = session.take_values().expect("the list is done");
            let others: Vec<(usize, Condition)> = spec
                .filters
                .iter()
                .filter(|(of, _)| *of != column)
                .cloned()
                .collect();
            let rows = plain.rows_of(&others);
            let cells = &plain.columns[column];
            let blanks = rows.iter().filter(|row| cells[**row as usize] == Cell::Blank).count();
            prop_assert_eq!(list.blanks, blanks as u64);
            let mut counted: Vec<(usize, (Cell, u64))> =
                plain.counted(column, &rows, &search).into_iter().enumerate().collect();
            prop_assert_eq!(list.complete, counted.len() <= limit);
            // The most frequent ones, of those that are as frequent the first.
            counted.sort_by_key(|(place, (_, count))| (Reverse(*count), *place));
            counted.truncate(limit);
            counted.sort_by_key(|(place, _)| *place);
            let found: Vec<(String, u64)> = list
                .values
                .iter()
                .map(|one| (one.text.to_lowercase(), one.count))
                .collect();
            let expected: Vec<(String, u64)> = counted
                .iter()
                .map(|(_, (cell, count))| (plain.shown(column, cell).to_lowercase(), *count))
                .collect();
            prop_assert_eq!(found, expected, "column {}, {:?}, {:?}", column, search, spec);
        }
    }

    #[test]
    fn sums_are_those_of_the_plain_way(seed in any::<u64>(), rows in rows()) {
        let mut rng = Rng::from_seed(seed);
        let columns = columns(&mut rng, rows);
        let file = write(&columns, 256);
        let (kinds, columns) = stored(&columns);
        let plain = Plain { kinds, columns, rows };
        let mut table = Table::open(&file).expect("what was written is a table");
        let mut from = InMemory { kinds: &plain.kinds, columns: &plain.columns };
        let filters = some_filters(&mut rng, &mut from);
        let shown = table
            .view(&Spec { filters, order: Vec::new() })
            .expect("it can be answered");
        let all = (0..plain.kinds.len()).collect();
        for summary in table.summary(all).expect("it can be answered") {
            let column = summary.column;
            let cells: Vec<&Cell> = shown
                .iter()
                .map(|row| &plain.columns[column][*row as usize])
                .filter(|cell| **cell != Cell::Blank)
                .collect();
            prop_assert_eq!(summary.rows, shown.len() as u64);
            prop_assert_eq!(summary.blanks, (shown.len() - cells.len()) as u64);
            let text = |figure: &Option<reticula_table::query::Figure>| {
                figure.as_ref().map(|figure| figure.text.clone())
            };
            if plain.kinds[column] != ColumnType::Text {
                let shown = |cell: Option<&&Cell>| cell.map(|cell| plain.shown(column, cell));
                let least = cells.iter().min_by(|a, b| compare(a, b));
                let most = cells.iter().max_by(|a, b| compare(a, b));
                prop_assert_eq!(text(&summary.min), shown(least));
                prop_assert_eq!(text(&summary.max), shown(most));
            }
            match plain.kinds[column] {
                ColumnType::Integer | ColumnType::Decimal { .. } if !cells.is_empty() => {
                    let sum: i128 = cells
                        .iter()
                        .map(|cell| match cell {
                            Cell::Int(value) => i128::from(*value),
                            other => panic!("{other:?}"),
                        })
                        .sum();
                    let (scale, fixed) = match plain.kinds[column] {
                        ColumnType::Decimal { scale, fixed } => (scale, fixed),
                        _ => (0, false),
                    };
                    let mut expected = String::new();
                    write_decimal(sum, scale, fixed, &mut expected);
                    prop_assert_eq!(text(&summary.sum), Some(expected));
                    let mean: f64 = text(&summary.mean).expect("a mean").parse().expect("a number");
                    let unit = 10_u64.pow(scale.into()) as f64;
                    let near = sum as f64 / unit / cells.len() as f64;
                    prop_assert!((mean - near).abs() <= near.abs() * 1e-12, "{} {}", mean, near);
                }
                ColumnType::Float if !cells.is_empty() => {
                    // The numbers are so large and so small that this is exact.
                    const UNIT: f64 = (1_u64 << 57) as f64;
                    let mut sum = 0_i128;
                    let mut size = 0.0;
                    for cell in &cells {
                        let Cell::Float(value) = cell else {
                            panic!("{cell:?}");
                        };
                        sum += (value * UNIT) as i128;
                        size += value.abs();
                    }
                    let sum = sum as f64 / UNIT;
                    let found: f64 = text(&summary.sum).expect("a sum").parse().expect("a number");
                    // Fifteen digits are shown, and what is lost is made up for.
                    let count = cells.len() as f64;
                    let lost = count * count * size * (f64::EPSILON * f64::EPSILON / 2.0);
                    let off = (found - sum).abs();
                    prop_assert!(off <= sum.abs() * 5.1e-15 + lost, "{} {}", found, sum);
                }
                _ => {
                    prop_assert!(summary.sum.is_none() && summary.mean.is_none());
                }
            }
        }
    }
}

// Delimited text.

/// How the values of a column are written.
#[derive(Clone, Copy, Debug)]
enum Written {
    Integers,
    Money,
    Decimals,
    Exponents,
    Digits,
    Sixteen,
    Dates,
    Times,
    Truths,
    Texts,
    Anything,
    Nothing,
}

fn field(rng: &mut Rng, written: Written) -> String {
    match written {
        _ if rng.next_below(9) == 0 => String::new(),
        Written::Integers => match rng.next_below(5) {
            0 => i64::MIN.to_string(),
            1 => i64::MAX.to_string(),
            _ => format!("{}", rng.next_below(2_000) as i64 - 1_000),
        },
        Written::Money => format!("{}.{:02}", rng.next_below(1_000_000), rng.next_below(100)),
        Written::Decimals => match rng.next_below(3) {
            0 => format!("{}", rng.next_below(1_000)),
            1 => format!("-{}.{}", 1 + rng.next_below(1_000), rng.next_below(10)),
            _ => format!(
                "{}.{:07}",
                rng.next_below(1_000_000),
                rng.next_below(10_000_000)
            ),
        },
        Written::Exponents => format!("{:e}", (rng.next_f64() - 0.5) * 1e9),
        // Some have more digits than anything but a text keeps.
        Written::Digits => {
            let count = 1 + rng.next_below(24) as usize;
            let mut text: String = (0..count)
                .map(|_| char::from(b'0' + rng.next_below(10) as u8))
                .collect();
            if rng.next_below(2) == 0 {
                text.insert(rng.next_below(count as u64) as usize, '.');
            }
            text
        }
        // Too many for a decimal, and just as many as a float keeps, or not.
        Written::Sixteen => match rng.next_below(12) {
            0 => format!("{}.5", 1 + rng.next_below(9_999)),
            1 => format!("0.{:016}", rng.next_below(10_000_000_000_000_000)),
            _ => format!("{:?}", rng.next_f64() + 0.001),
        },
        Written::Dates => {
            let (year, month, day) = (
                rng.next_below(9_999) + 1,
                rng.next_below(12) + 1,
                rng.next_below(28) + 1,
            );
            if rng.next_below(3) == 0 {
                format!("{year:04}/{month}/{day}")
            } else {
                format!("{year:04}-{month:02}-{day:02}")
            }
        }
        Written::Times => {
            let (hour, minute, second) =
                (rng.next_below(24), rng.next_below(60), rng.next_below(60));
            match rng.next_below(4) {
                0 => format!("2024-02-29T{hour:02}:{minute:02}"),
                1 => format!("1999-12-31 {hour:02}:{minute:02}:{second:02}Z"),
                2 => format!(
                    "0001-01-01 {hour:02}:{minute:02}:{second:02}.{:03}",
                    rng.next_below(1_000)
                ),
                _ => "2024/3/1".to_owned(),
            }
        }
        Written::Truths => (*pick(rng, &["true", "FALSE", "True", "false"])).to_owned(),
        Written::Texts => {
            const PARTS: [&str; 8] = ["North", ",", "\"", "\n", "é", "漢", " ", "x;y"];
            (0..rng.next_below(4)).map(|_| *pick(rng, &PARTS)).collect()
        }
        Written::Anything => (*pick(
            rng,
            &[
                "1",
                "-1",
                "0",
                "-0",
                "00",
                "+1",
                "1.",
                ".5",
                "1.5",
                "1e5",
                "1e400",
                "5e-324",
                "true",
                "TRUE",
                "t",
                "2024-02-30",
                "2024-02-29",
                "2024-2-9",
                "0000-01-01",
                "2024-02-29 24:00",
                "2024-02-29T12:00:00+01:00",
                "NaN",
                "inf",
                "NULL",
                "0.1",
                "0.10",
                "-0.0",
                "1 ",
                "9223372036854775808",
                "0x10",
                "1,5",
                "12:30",
            ],
        ))
        .to_owned(),
        Written::Nothing => String::new(),
    }
}

/// What a text is, read by the rules as they are told.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Read {
    Number {
        whole: usize,
        fraction: usize,
        exponent: bool,
        lossless: bool,
    },
    Date,
    Time,
    Truth,
    Text,
}

/// Digits without the ones that do not count.
fn significant(digits: &str) -> &str {
    digits.trim_start_matches('0').trim_end_matches('0')
}

/// Whether the digits are those that a floating-point number is written with,
/// as few as mean that number and as close to it as so many can be. Either of
/// two that are as close will do.
fn comes_back(digits: &str, value: f64) -> bool {
    let digits = significant(digits);
    let shortest = format!("{value:e}");
    let shortest = shortest
        .split('e')
        .next()
        .unwrap_or("")
        .replace(['-', '.'], "");
    if !value.is_finite() || value == 0.0 || digits.len() != significant(&shortest).len() {
        return false;
    }
    if digits == significant(&shortest) {
        return true;
    }
    // All digits of the number, which has a five after these.
    let exact = format!("{value:.1100}").replace(['-', '.'], "");
    let Some(before) = significant(&exact).strip_suffix('5') else {
        return false;
    };
    let number = |digits: &str| digits.parse::<u128>().unwrap_or(0);
    before.len() == digits.len() && [0, 1].contains(&number(digits).wrapping_sub(number(before)))
}

fn read_number(text: &str) -> Option<Read> {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let all_digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(whole)
        || !fraction.is_none_or(all_digits)
        || !exponent.is_none_or(|e| all_digits(e.strip_prefix(['+', '-']).unwrap_or(e)))
    {
        return None;
    }
    let digits = format!("{whole}{}", fraction.unwrap_or(""));
    let zero = digits.bytes().all(|b| b == b'0');
    // Written in another way than it is shown: it is kept as it is.
    if (whole.len() > 1 && whole.starts_with('0')) || (zero && text.starts_with('-')) {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    let lossless = zero || comes_back(&digits, value);
    if exponent.is_none() && fraction.is_none() && text.parse::<i64>().is_err() {
        return None;
    }
    Some(Read::Number {
        whole: whole.trim_start_matches('0').len(),
        fraction: fraction.map_or(0, str::len),
        exponent: exponent.is_some(),
        lossless,
    })
}

fn read_date(text: &str) -> Option<i64> {
    let (separator, strict) = match text.as_bytes().get(4)? {
        b'-' => ('-', true),
        b'/' => ('/', false),
        _ => return None,
    };
    let parts: Vec<&str> = text.split(separator).collect();
    let [year, month, day] = parts.as_slice() else {
        return None;
    };
    let number = |text: &str, least: usize| {
        let fits = (least..=2).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
        if fits { text.parse::<i64>().ok() } else { None }
    };
    let least = if strict { 2 } else { 1 };
    if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    days_from_civil(
        year.parse().ok()?,
        number(month, least)?,
        number(day, least)?,
    )
}

fn read_time(text: &str) -> Option<i64> {
    let at = text.find([' ', 'T'])?;
    let (date, time) = (&text[..at], &text[at + 1..]);
    let time = time.strip_suffix('Z').unwrap_or(time);
    let parts: Vec<&str> = time.split(':').collect();
    let two = |text: &str, below: i64| {
        let fits = text.len() == 2 && text.bytes().all(|b| b.is_ascii_digit());
        text.parse::<i64>()
            .ok()
            .filter(|number| fits && *number < below)
    };
    let (hour, minute, second) = match parts.as_slice() {
        [hour, minute] => (*hour, *minute, "00"),
        [hour, minute, second] => (*hour, *minute, *second),
        _ => return None,
    };
    let (second, micros) = match second.split_once('.') {
        Some((second, digits)) => {
            if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            (second, format!("{digits:0<6}").parse::<i64>().ok()?)
        }
        None => (second, 0),
    };
    let seconds = two(hour, 24)? * 3_600 + two(minute, 60)? * 60 + two(second, 60)?;
    Some(read_date(date)? * DAY_MICROS + seconds * 1_000_000 + micros)
}

fn read(text: &str) -> Read {
    if let Some(number) = read_number(text) {
        return number;
    }
    if read_date(text).is_some() {
        return Read::Date;
    }
    if read_time(text).is_some() {
        return Read::Time;
    }
    match text.to_ascii_lowercase().as_str() {
        "true" | "false" => Read::Truth,
        _ => Read::Text,
    }
}

/// The narrowest type of some texts, none of which is empty.
fn kind_of(texts: &[&str]) -> ColumnType {
    let all: Vec<Read> = texts.iter().map(|text| read(text)).collect();
    let is = |test: &dyn Fn(&Read) -> bool| !all.is_empty() && all.iter().all(test);
    if is(&|one| matches!(one, Read::Truth)) {
        return ColumnType::Boolean;
    }
    if is(&|one| matches!(one, Read::Date)) {
        return ColumnType::Date;
    }
    if is(&|one| matches!(one, Read::Date | Read::Time)) {
        return ColumnType::DateTime;
    }
    let (mut most_whole, mut least, mut most) = (0, usize::MAX, 0);
    let (mut exponents, mut kept) = (false, true);
    for one in &all {
        let Read::Number {
            whole,
            fraction,
            exponent,
            lossless,
        } = one
        else {
            return ColumnType::Text;
        };
        exponents |= exponent;
        kept &= lossless;
        if !exponent {
            most_whole = most_whole.max(*whole);
            least = least.min(*fraction);
            most = most.max(*fraction);
        }
    }
    if all.is_empty() {
        return ColumnType::Text;
    }
    if !exponents && most == 0 {
        return ColumnType::Integer;
    }
    if !exponents && most_whole + most <= 18 {
        return ColumnType::Decimal {
            scale: most as u8,
            fixed: least == most,
        };
    }
    if kept {
        ColumnType::Float
    } else {
        ColumnType::Text
    }
}

/// The cell that a text becomes in a column of that type.
fn cell_of(kind: ColumnType, text: &str) -> Cell {
    if text.is_empty() {
        return Cell::Blank;
    }
    match kind {
        ColumnType::Integer => Cell::Int(text.parse().expect("an integer")),
        ColumnType::Decimal { scale, .. } => {
            let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
            let digits = format!("{whole}{fraction:0<width$}", width = usize::from(scale));
            Cell::Int(digits.parse().expect("an integer"))
        }
        ColumnType::Float => {
            let value: f64 = text.parse().expect("a number");
            Cell::Float(if value == 0.0 { 0.0 } else { value })
        }
        ColumnType::Date => Cell::Int(read_date(text).expect("a date")),
        ColumnType::DateTime => Cell::Int(
            read_time(text)
                .or_else(|| Some(read_date(text)? * DAY_MICROS))
                .expect("a date, with a time or without"),
        ),
        ColumnType::Boolean => Cell::Int(i64::from(text.eq_ignore_ascii_case("true"))),
        ColumnType::Text => Cell::Text(text.to_owned()),
    }
}

fn some_file(
    rng: &mut Rng,
    columns: &[Written],
    rows: usize,
    delimiter: char,
) -> (String, Vec<Vec<String>>) {
    let mut text = (0..columns.len())
        .map(|column| format!("c{column}"))
        .collect::<Vec<_>>()
        .join(&delimiter.to_string());
    let end = *pick(rng, &["\n", "\r\n", "\r"]);
    let mut cells = vec![Vec::new(); columns.len()];
    for _ in 0..rows {
        text.push_str(end);
        let width = if rng.next_below(20) == 0 {
            1 + rng.next_below(columns.len() as u64) as usize
        } else {
            columns.len()
        };
        for (column, written) in columns.iter().enumerate() {
            let mut value = if column < width {
                field(rng, *written)
            } else {
                String::new()
            };
            if column == 0 && value.is_empty() && width == 1 {
                // A line without anything would be no row.
                value = "0".to_owned();
            }
            if column < width {
                if column > 0 {
                    text.push(delimiter);
                }
                let must = value.contains([delimiter, '"', '\n', '\r']);
                if must || rng.next_below(25) == 0 {
                    text.push_str(&format!("\"{}\"", value.replace('"', "\"\"")));
                } else {
                    text.push_str(&value);
                }
            }
            cells[column].push(value);
        }
    }
    if rng.next_below(2) == 0 {
        text.push_str(end);
    }
    (text, cells)
}

fn written() -> impl Strategy<Value = Written> {
    prop::sample::select(vec![
        Written::Integers,
        Written::Money,
        Written::Decimals,
        Written::Exponents,
        Written::Digits,
        Written::Sixteen,
        Written::Dates,
        Written::Times,
        Written::Truths,
        Written::Texts,
        Written::Anything,
        Written::Nothing,
    ])
}

fn check_csv(
    text: &str,
    cells: &[Vec<String>],
    delimiter: char,
    seed: u64,
) -> Result<(), TestCaseError> {
    let mut rng = Rng::from_seed(seed);
    let options = csv::Options {
        delimiter: Some(delimiter as u8),
        header: Some(true),
        ..csv::Options::default()
    };
    let longest = *pick(&mut rng, &[3_u64, 100, 10_000, 400_000]);
    let mut piece = || (rng.next_below(longest) as usize + 1, rng.next_below(3) == 0);
    let (file, report) =
        import_csv(&options, text.as_bytes(), &mut piece).expect("it can be imported");
    let (kinds, found) = read_all(&file).expect("what was written is a table");
    for (column, texts) in cells.iter().enumerate() {
        let filled: Vec<&str> = texts
            .iter()
            .map(String::as_str)
            .filter(|t| !t.is_empty())
            .collect();
        let some = &filled[..filled.len().min(20)];
        prop_assert_eq!(
            kinds[column],
            kind_of(&filled),
            "column {}: {:?}",
            column,
            some
        );
        prop_assert_eq!(report.columns[column].kind, kinds[column]);
        for (row, text) in texts.iter().enumerate() {
            prop_assert_eq!(
                &found[column][row],
                &cell_of(kinds[column], text),
                "row {}",
                row
            );
        }
        prop_assert_eq!(found[column].len(), texts.len());
    }
    prop_assert!(report.passes <= 2);
    Ok(())
}

proptest! {
    #![proptest_config(cases(200))]

    #[test]
    fn delimited_text_is_read_like_the_rules_say(
        seed in any::<u64>(),
        columns in prop::collection::vec(written(), 2..6),
        rows in prop_oneof![3 => 1_usize..12, 2 => 12_usize..600],
        delimiter in prop::sample::select(vec![',', ';', '\t', '|']),
    ) {
        let (text, cells) = some_file(&mut Rng::from_seed(seed), &columns, rows, delimiter);
        check_csv(&text, &cells, delimiter, seed)?;
    }
}

proptest! {
    #![proptest_config(cases(6))]

    // What the first mebibyte says of the types does not hold for what follows.
    #[test]
    fn types_are_those_of_all_rows(
        seed in any::<u64>(),
        first in prop::collection::vec(written(), 4),
        second in prop::collection::vec(written(), 4),
    ) {
        let mut rng = Rng::from_seed(seed);
        let (mut text, mut cells) = some_file(&mut rng, &first, 30_000, ',');
        while text.len() < csv::HEAD_BYTES {
            let (_, more) = some_file(&mut rng, &first, 30_000, ',');
            append(&mut text, &mut cells, &more);
        }
        let (_, more) = some_file(&mut rng, &second, 500, ',');
        append(&mut text, &mut cells, &more);
        check_csv(&text, &cells, ',', seed)?;
    }
}

fn append(text: &mut String, cells: &mut [Vec<String>], more: &[Vec<String>]) {
    if !text.ends_with(['\n', '\r']) {
        text.push('\n');
    }
    for row in 0..more[0].len() {
        for (column, values) in more.iter().enumerate() {
            if column > 0 {
                text.push(',');
            }
            text.push_str(&format!("\"{}\"", values[row].replace('"', "\"\"")));
        }
        text.push('\n');
    }
    for (cells, more) in cells.iter_mut().zip(more) {
        cells.extend(more.iter().cloned());
    }
}

#[test]
fn the_plain_way_is_right_about_itself() {
    assert_eq!(compare_decimals("1e2", "100.000"), Ordering::Equal);
    assert_eq!(compare_decimals("-0.0", "0"), Ordering::Equal);
    assert_eq!(compare_decimals("-12.5e-1", "-1.24"), Ordering::Less);
    assert_eq!(
        compare_decimals("1e400", "99999999999999999999"),
        Ordering::Greater
    );
    assert_eq!(
        read_date("2024-02-29").map(civil_from_days),
        Some((2024, 2, 29))
    );
    assert_eq!(read_date("2023-02-29"), None);
    assert_eq!(read_time("1970-01-01T00:00:01.5Z"), Some(1_500_000));
    assert_eq!(
        kind_of(&["1", "2.50"]),
        ColumnType::Decimal {
            scale: 2,
            fixed: false
        }
    );
    assert_eq!(kind_of(&["1", "00"]), ColumnType::Text);
    assert_eq!(kind_of(&["1", "1e5"]), ColumnType::Float);
    assert_eq!(
        kind_of(&["0.30000000000000004", "1234567890123.5"]),
        ColumnType::Float
    );
    assert_eq!(kind_of(&[]), ColumnType::Text);
    // Written as 677715396045378.8, which is as far from 677715396045378.75.
    assert!(comes_back("6777153960453787", 677_715_396_045_378.7));
    assert!(comes_back("6777153960453788", 677_715_396_045_378.7));
    assert!(!comes_back("10000000000000001", 1e16));
    assert!(!comes_back("30000000000000003", 0.1 + 0.2));
}

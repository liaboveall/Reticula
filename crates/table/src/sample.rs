//! A made-up table: the orders of a chain of shops.
//!
//! The same number of rows and the same seed give the same file, byte for
//! byte, on every target.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use reticula_core::rng::Rng;

use crate::calendar::DAY_MICROS;
use crate::source::Sink;
use crate::types::{ColumnType, MAX_ROWS};
use crate::writer::{ColumnSpec, TableWriter, WriteError, Written, group_rows_for};

/// Rows that are made in one step.
const STEP_ROWS: u32 = 2_048;
/// 2021-01-01, where the orders begin.
const FIRST_DAY: i64 = 18_628;
/// Days until the last order.
const DAYS: u64 = 1_826;

const REGIONS: [&str; 8] = [
    "North", "South", "East", "West", "Central", "Harbour", "Airport", "Online",
];
const THINGS: [&str; 20] = [
    "Lamp",
    "Chair",
    "Desk",
    "Kettle",
    "Mug",
    "Notebook",
    "Backpack",
    "Umbrella",
    "Blanket",
    "Headphones",
    "Keyboard",
    "Monitor",
    "Bottle",
    "Tray",
    "Clock",
    "Mirror",
    "Shelf",
    "Basket",
    "Towel",
    "Vase",
];
const FINISHES: [&str; 16] = [
    "Oak", "Walnut", "Steel", "Slate", "Ivory", "Amber", "Coral", "Jade", "Indigo", "Copper",
    "Sand", "Moss", "Plum", "Ash", "Pearl", "Cobalt",
];
const CHANNELS: [&str; 4] = ["shop", "web", "phone", "partner"];
const NOTES: [&str; 12] = [
    "gift wrap",
    "deliver after six",
    "repeat order",
    "call before delivery",
    "fragile",
    "no receipt",
    "exchange",
    "priority",
    "pick up in shop",
    "two parcels",
    "invoice by post",
    "leave at the door",
];

/// Makes the table, a step at a time.
#[derive(Debug)]
pub struct Sample {
    writer: TableWriter,
    rng: Rng,
    rows: u64,
    made: u64,
    products: Vec<(String, u32)>,
    text: String,
    finishing: bool,
}

impl Sample {
    /// Prepares a table of so many rows.
    ///
    /// # Errors
    ///
    /// [`WriteError`] when there are too many.
    pub fn new(rows: u64, seed: u64) -> Result<Self, WriteError> {
        if rows > MAX_ROWS {
            return Err(WriteError::Limit("too many rows"));
        }
        let money = ColumnType::Decimal {
            scale: 2,
            fixed: true,
        };
        let columns = [
            ("order", ColumnType::Integer, false),
            ("ordered at", ColumnType::DateTime, false),
            ("ship date", ColumnType::Date, false),
            ("region", ColumnType::Text, true),
            ("store", ColumnType::Integer, false),
            ("channel", ColumnType::Text, true),
            ("product", ColumnType::Text, true),
            ("customer", ColumnType::Text, true),
            ("quantity", ColumnType::Integer, false),
            ("unit price", money, false),
            ("discount", money, false),
            ("amount", money, false),
            ("paid", ColumnType::Boolean, false),
            (
                "rating",
                ColumnType::Decimal {
                    scale: 1,
                    fixed: true,
                },
                false,
            ),
            ("note", ColumnType::Text, true),
        ];
        let specs: Vec<ColumnSpec> = columns
            .into_iter()
            .map(|(name, kind, dictionary)| ColumnSpec {
                name: String::from(name),
                kind: Some(kind),
                dictionary,
            })
            .collect();
        let mut rng = Rng::from_seed(seed);
        let mut products = Vec::new();
        for number in 0..1_600_u32 {
            let mut name = String::new();
            let thing = pick(&THINGS, u64::from(number));
            let finish = pick(
                &FINISHES,
                u64::from(number)
                    .checked_div(THINGS.len() as u64)
                    .unwrap_or(0),
            );
            let _ = write!(name, "{finish} {thing} {:03}", number.wrapping_rem(997));
            let price = 199_u64.saturating_add(rng.next_below(39_800));
            products.push((name, u32::try_from(price).unwrap_or(0)));
        }
        Ok(Self {
            writer: TableWriter::new(specs, group_rows_for(columns.len(), 120))?,
            rng,
            rows,
            made: 0,
            products,
            text: String::new(),
            finishing: false,
        })
    }

    /// Rows made so far.
    #[must_use]
    pub const fn made(&self) -> u64 {
        self.made
    }

    /// Makes and writes some more. Gives whether something is left to do.
    ///
    /// # Errors
    ///
    /// [`WriteError`] when it cannot be written.
    pub fn step<S: Sink>(&mut self, sink: &mut S) -> Result<bool, WriteError> {
        if self.writer.step(sink)? {
            return Ok(true);
        }
        if self.made < self.rows {
            for _ in 0..STEP_ROWS {
                if self.made >= self.rows || self.writer.check_group()? {
                    break;
                }
                self.row();
                self.made = self.made.saturating_add(1);
            }
            if self.made >= self.rows || self.writer.check_group()? {
                self.writer.seal_group()?;
            }
            return Ok(true);
        }
        if !self.finishing {
            self.finishing = true;
            self.writer.finish();
            return Ok(true);
        }
        Ok(false)
    }

    /// What was written, once everything is.
    #[must_use]
    pub fn written(&self) -> Option<Written> {
        self.writer.written()
    }

    fn row(&mut self) {
        let rng = &mut self.rng;
        let bits = rng.next_u64();
        // Taking small numbers out of one draw saves most of the draws.
        let mut rest = bits;
        let mut take = |below: u64| {
            let value = rest.checked_rem(below).unwrap_or(0);
            rest = rest.checked_div(below).unwrap_or(0);
            value
        };
        let region = take(8);
        let channel = take(4);
        let quantity = take(12).saturating_add(1);
        let paid = take(25) != 0;
        let discounted = take(5) == 0;
        let has_note = take(12) == 0;
        let rated = take(3) != 0;
        let delay = take(9);
        let other = rng.next_u64();
        let store = other
            .wrapping_rem(640)
            .wrapping_mul(8)
            .wrapping_add(region)
            .wrapping_add(1_000);
        let popular = other.wrapping_shr(16) & 3 != 0;
        let range = if popular { 200 } else { 1_600 };
        let product = other.wrapping_shr(20).checked_rem(range).unwrap_or(0);
        let customer = other.wrapping_shr(32).wrapping_rem(1_200_000);

        // Orders come in order of time, spread over the days.
        let micros = u64::try_from(DAY_MICROS).unwrap_or(0).saturating_mul(DAYS);
        let step = micros.checked_div(self.rows.max(1)).unwrap_or(0);
        let at = self
            .made
            .saturating_mul(step)
            .saturating_add(rng.next_below(step.max(1)));
        let seconds = at.checked_div(1_000_000).unwrap_or(0);
        let at = i64::try_from(seconds)
            .unwrap_or(0)
            .saturating_mul(1_000_000)
            .saturating_add(FIRST_DAY.saturating_mul(DAY_MICROS));

        let (name, price) = self
            .products
            .get(usize::try_from(product).unwrap_or(0))
            .map_or(("", 0), |(name, price)| (name.as_str(), *price));
        let price = i64::from(price);
        let percent = if discounted {
            i64::try_from(rng.next_below(40)).unwrap_or(0)
        } else {
            0
        };
        let quantity = i64::try_from(quantity).unwrap_or(0);
        let amount = price
            .saturating_mul(quantity)
            .saturating_mul(100_i64.saturating_sub(percent))
            .saturating_add(50)
            .div_euclid(100);

        self.text.clear();
        self.text.push('C');
        let mut digit = 1_000_000_u64;
        while digit > 0 {
            let value = customer
                .checked_div(digit)
                .unwrap_or(0)
                .checked_rem(10)
                .unwrap_or(0);
            let value = u32::try_from(value).unwrap_or(0);
            self.text.push(char::from_digit(value, 10).unwrap_or('0'));
            digit = digit.checked_div(10).unwrap_or(0);
        }
        let note = (has_note).then(|| pick(&NOTES, rng.next_below(NOTES.len() as u64)));
        let rating = rated.then(|| {
            i64::try_from(rng.next_below(41))
                .unwrap_or(0)
                .saturating_add(10)
        });

        let row = i64::try_from(self.made).unwrap_or(0).saturating_add(1);
        for (index, column) in self.writer.columns().iter_mut().enumerate() {
            match index {
                0 => column.push_int(row),
                1 => column.push_int(at),
                2 => {
                    let days = at.div_euclid(DAY_MICROS);
                    let delay = i64::try_from(delay).unwrap_or(0);
                    column.push_int(days.saturating_add(delay));
                }
                3 => {
                    column.push_text(pick(&REGIONS, region));
                }
                4 => column.push_int(i64::try_from(store).unwrap_or(0)),
                5 => {
                    column.push_text(pick(&CHANNELS, channel));
                }
                6 => {
                    column.push_text(name);
                }
                7 => {
                    column.push_text(&self.text);
                }
                8 => column.push_int(quantity),
                9 => column.push_int(price),
                10 if percent == 0 => column.push_blank(),
                10 => column.push_int(percent),
                11 => column.push_int(amount),
                12 => column.push_int(i64::from(paid)),
                13 => match rating {
                    Some(rating) => column.push_int(rating),
                    None => column.push_blank(),
                },
                _ => {
                    column.push_text(note.unwrap_or(""));
                }
            }
        }
    }
}

fn pick<'a>(from: &[&'a str], index: u64) -> &'a str {
    usize::try_from(index)
        .ok()
        .and_then(|index| index.checked_rem(from.len()))
        .and_then(|index| from.get(index))
        .copied()
        .unwrap_or("")
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used, reason = "tests")]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use crate::hash;
    use crate::reader::{Opened, Reader};

    fn make(rows: u64) -> Vec<u8> {
        let mut sample = Sample::new(rows, 2026).unwrap();
        let mut file = Vec::new();
        while sample.step(&mut file).unwrap() {}
        assert_eq!(u64::from(sample.written().unwrap().rows), rows);
        file
    }

    #[test]
    fn the_same_bytes_everywhere() {
        let file = make(70_000);
        assert_eq!(file, make(70_000));
        // A change of this number is a change of the file format or of the sample.
        assert_eq!(hash::of_text(&file), EXPECTED);
        let mut source = file.as_slice();
        let Opened::Ready(reader) = Reader::open(&mut source, file.len() as u64).unwrap() else {
            panic!("all of it is here");
        };
        assert_eq!(reader.columns().len(), 15);
        assert!(reader.columns().iter().filter(|c| c.blanks > 0).count() == 3);
    }

    #[test]
    fn small_ones() {
        for rows in [0, 1, 2_047, 2_048, 2_049] {
            assert!(make(rows).len() > 4_096);
        }
        assert!(Sample::new(MAX_ROWS.saturating_add(1), 1).is_err());
    }

    const EXPECTED: u64 = 11_810_626_366_734_945_859;

    /// How long ten million rows take: `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    #[cfg(not(target_arch = "wasm32"))]
    fn ten_million_rows() {
        let start = std::time::Instant::now();
        let mut slowest = core::time::Duration::ZERO;
        let mut sample = Sample::new(10_000_000, 2026).unwrap();
        let mut file = Vec::new();
        loop {
            let step = std::time::Instant::now();
            let more = sample.step(&mut file).unwrap();
            slowest = slowest.max(step.elapsed());
            if !more {
                break;
            }
        }
        std::println!(
            "{:?}, {} bytes, longest step {slowest:?}",
            start.elapsed(),
            file.len()
        );
    }
}

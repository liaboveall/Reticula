//! The seeded sample: a sheet of synthetic sales records.
//!
//! The sample is computed, not downloaded: a visitor's first page load carries
//! no data, and the same seed gives the same sheet in every browser and on
//! every native target.
//!
//! This is the first cut of the generator. Its output is checked by the
//! determinism digest, so a change here shows up as a changed digest.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::format::NumberFormat;
use crate::memory::try_reserve_heap;
use crate::num;
use crate::rng::Rng;
use crate::sheet::Sheet;
use crate::strings::StringTable;
use crate::value::Value;

/// Number of columns in the sample.
pub const COLUMNS: usize = 20;

const HEADERS: [&str; COLUMNS] = [
    "订单号",
    "日期",
    "地区",
    "城市",
    "渠道",
    "品类",
    "SKU",
    "客户",
    "销售员",
    "数量",
    "单价",
    "折扣",
    "金额",
    "成本",
    "利润",
    "运费",
    "税率",
    "税额",
    "评分",
    "备注",
];

const FORMATS: [NumberFormat; COLUMNS] = [
    NumberFormat::General, // order number
    NumberFormat::Date,    // date
    NumberFormat::General, // region
    NumberFormat::General, // city
    NumberFormat::General, // channel
    NumberFormat::General, // category
    NumberFormat::General, // SKU
    NumberFormat::General, // customer
    NumberFormat::General, // sales representative
    NumberFormat::General, // quantity
    NumberFormat::Fixed2,  // unit price
    NumberFormat::Percent, // discount
    NumberFormat::Fixed2,  // amount
    NumberFormat::Fixed2,  // cost
    NumberFormat::Fixed2,  // profit
    NumberFormat::Fixed2,  // shipping
    NumberFormat::Percent, // tax rate
    NumberFormat::Fixed2,  // tax
    NumberFormat::General, // rating
    NumberFormat::General, // note
];

const REGIONS: [&str; 7] = ["华东", "华南", "华北", "华中", "西南", "西北", "东北"];

/// Cities, three for each region, in the order of [`REGIONS`].
const CITIES: [&str; 21] = [
    "上海",
    "杭州",
    "南京",
    "广州",
    "深圳",
    "厦门",
    "北京",
    "天津",
    "石家庄",
    "武汉",
    "长沙",
    "郑州",
    "成都",
    "重庆",
    "昆明",
    "西安",
    "兰州",
    "乌鲁木齐",
    "沈阳",
    "大连",
    "哈尔滨",
];
const CITIES_PER_REGION: u64 = 3;

const CHANNELS: [&str; 4] = ["线上商城", "直营门店", "经销商", "企业采购"];
/// Which channel each of ten equally likely draws selects.
const CHANNEL_WEIGHTS: [u32; 10] = [0, 0, 0, 0, 0, 1, 1, 1, 2, 3];
const CHANNEL_ONLINE: u32 = 0;
const CHANNEL_STORE: u32 = 1;

const CATEGORIES: [&str; 10] = [
    "办公用品",
    "电子配件",
    "家居日用",
    "食品饮料",
    "服装鞋帽",
    "运动户外",
    "图书文具",
    "美妆个护",
    "母婴用品",
    "数码家电",
];
/// Value-added tax rate of each category, in the order of [`CATEGORIES`].
const TAX_RATES: [f64; 10] = [0.13, 0.13, 0.13, 0.09, 0.13, 0.13, 0.09, 0.13, 0.13, 0.13];

const REPRESENTATIVES: [&str; 16] = [
    "张伟", "王芳", "李娜", "刘洋", "陈静", "杨帆", "赵磊", "黄敏", "周杰", "吴倩", "徐鹏", "孙丽",
    "马超", "朱琳", "胡军", "郭颖",
];

const NOTES: [&str; 6] = ["加急", "赠品", "需开发票", "大客户", "退换货", "预售"];
/// One row in this many carries a note, each note equally often.
const NOTE_ODDS: u64 = 64;

/// The discount each of ten equally likely draws selects.
const DISCOUNTS: [f64; 10] = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.05, 0.05, 0.1, 0.2];

/// The rating each of ten equally likely draws selects; zero is no rating.
const RATINGS: [u32; 10] = [0, 0, 0, 5, 5, 4, 4, 3, 2, 1];

const PRODUCTS: u32 = 2000;
const CUSTOMERS: u32 = 20_000;

const FIRST_ORDER_NUMBER: u32 = 10_000_000;
/// Serial number of 2023-01-01, the date of the first order.
const FIRST_DAY: u64 = 44_927;
/// The orders span three years: 2023 through 2025.
const DAYS: u64 = 1096;

/// A run of consecutive entries in the string table.
#[derive(Clone, Copy)]
struct Dictionary {
    first: u32,
}

impl Dictionary {
    fn of(strings: &mut StringTable, entries: &[&str]) -> Self {
        let first = next_string(strings);
        for entry in entries {
            strings.intern(entry);
        }
        Self { first }
    }

    /// Entries `prefix` followed by a number padded to `width` digits.
    fn numbered(strings: &mut StringTable, prefix: &str, width: usize, count: u32) -> Self {
        let first = next_string(strings);
        let mut text = String::new();
        for number in 0..count {
            text.clear();
            // Writing to a `String` cannot fail.
            let _ = write!(text, "{prefix}{number:0width$}");
            strings.intern(&text);
        }
        Self { first }
    }

    const fn entry(self, index: u32) -> Value {
        Value::Text(self.first.wrapping_add(index))
    }
}

fn next_string(strings: &StringTable) -> u32 {
    u32::try_from(strings.len()).unwrap_or(u32::MAX)
}

struct Product {
    category: u32,
    price: f64,
    unit_cost: f64,
}

/// Generates the sample: a header row followed by `rows - 1` orders.
///
/// The same `seed` and `rows` always give the same sheet.
#[must_use]
pub fn generate(seed: u64, rows: u32) -> Sheet {
    if rows == 0 {
        return Sheet::new(&FORMATS);
    }
    // Pre-warm linear memory in one step (~20 columns * 8 bytes * rows + slack)
    // to avoid thousands of small `memory.grow` calls on WebAssembly.
    let estimated_heap = (rows as usize)
        .saturating_mul(COLUMNS)
        .saturating_mul(size_of::<u64>())
        .saturating_add(4 * 1024 * 1024);
    let _ = try_reserve_heap(estimated_heap);

    let mut rng = Rng::from_seed(seed);
    let mut strings = StringTable::with_capacity(22_100, 220_000);

    let headers = Dictionary::of(&mut strings, &HEADERS);
    let regions = Dictionary::of(&mut strings, &REGIONS);
    let cities = Dictionary::of(&mut strings, &CITIES);
    let channels = Dictionary::of(&mut strings, &CHANNELS);
    let categories = Dictionary::of(&mut strings, &CATEGORIES);
    let representatives = Dictionary::of(&mut strings, &REPRESENTATIVES);
    let notes = Dictionary::of(&mut strings, &NOTES);
    let skus = Dictionary::numbered(&mut strings, "SKU-", 5, PRODUCTS);
    let customers = Dictionary::numbered(&mut strings, "C-", 6, CUSTOMERS);

    let mut sheet = Sheet::with_base_formats(rows, &FORMATS, strings);

    let products: Vec<Product> = (0..PRODUCTS)
        .map(|_| {
            // Prices from 5 to about 2,000, evenly spread on a logarithmic scale.
            let price = round_to_cents(5.0 * num::exp(rng.next_f64() * 6.0));
            Product {
                category: below(&mut rng, CATEGORIES.len()),
                price,
                unit_cost: round_to_cents(price * (0.45 + 0.35 * rng.next_f64())),
            }
        })
        .collect();

    for col in 0..COLUMNS {
        let idx = u32::try_from(col).unwrap_or(0);
        sheet.set_base_slot(0, col, headers.entry(idx));
    }

    let orders = u64::from(rows.saturating_sub(1));
    for order in 0..rows.saturating_sub(1) {
        let city = below(&mut rng, CITIES.len());
        let channel = pick(&CHANNEL_WEIGHTS, &mut rng).unwrap_or(CHANNEL_ONLINE);
        let sku = below(&mut rng, PRODUCTS as usize);
        let customer = below(&mut rng, CUSTOMERS as usize);
        let representative = below(&mut rng, REPRESENTATIVES.len());
        let quantity = f64::from(below(&mut rng, 12).saturating_add(1));
        let (category, list_price, unit_cost) =
            products.get(sku as usize).map_or((0, 0.0, 0.0), |product| {
                (product.category, product.price, product.unit_cost)
            });
        let price = round_to_cents(list_price * (0.9 + 0.2 * rng.next_f64()));
        let discount = pick(&DISCOUNTS, &mut rng).unwrap_or(0.0);
        let amount = round_to_cents(quantity * price * (1.0 - discount));
        let cost = round_to_cents(quantity * unit_cost);
        let shipping = if channel == CHANNEL_STORE {
            0.0
        } else if channel == CHANNEL_ONLINE && amount < 99.0 {
            8.0
        } else {
            round_to_cents(5.0 + 25.0 * rng.next_f64())
        };
        let tax_rate = TAX_RATES.get(category as usize).copied().unwrap_or(0.13);
        let rating = pick(&RATINGS, &mut rng).unwrap_or(0);
        let note = rng.next_below(NOTE_ODDS);

        // Orders arrive in date order, evenly over the period.
        let day = u64::from(order)
            .saturating_mul(DAYS)
            .checked_div(orders)
            .unwrap_or(0);

        let row = [
            Value::Number(f64::from(FIRST_ORDER_NUMBER.saturating_add(order))),
            Value::Number(whole(FIRST_DAY.saturating_add(day))),
            regions.entry(city.checked_div(narrow(CITIES_PER_REGION)).unwrap_or(0)),
            cities.entry(city),
            channels.entry(channel),
            categories.entry(category),
            skus.entry(sku),
            customers.entry(customer),
            representatives.entry(representative),
            Value::Number(quantity),
            Value::Number(price),
            Value::Number(discount),
            Value::Number(amount),
            Value::Number(cost),
            Value::Number(round_to_cents(amount - cost)),
            Value::Number(shipping),
            Value::Number(tax_rate),
            Value::Number(round_to_cents(amount * tax_rate / (1.0 + tax_rate))),
            if rating == 0 {
                Value::Empty
            } else {
                Value::Number(f64::from(rating))
            },
            if note < NOTES.len() as u64 {
                notes.entry(narrow(note))
            } else {
                Value::Empty
            },
        ];
        let row_slot = order.saturating_add(1);
        for (col, val) in row.into_iter().enumerate() {
            if val != Value::Empty {
                sheet.set_base_slot(row_slot, col, val);
            }
        }
    }
    sheet
}

fn round_to_cents(value: f64) -> f64 {
    num::round(value * 100.0) / 100.0
}

/// A uniformly distributed index into a table of `len` entries.
fn below(rng: &mut Rng, len: usize) -> u32 {
    narrow(rng.next_below(len as u64))
}

/// One entry of a table, each equally likely.
fn pick<T: Copy>(table: &[T], rng: &mut Rng) -> Option<T> {
    table.get(below(rng, table.len()) as usize).copied()
}

/// A small count as `u32`. Every caller passes a value far below the limit.
fn narrow(small: u64) -> u32 {
    u32::try_from(small).unwrap_or(u32::MAX)
}

/// A day count as a float. Day counts are far below 2^53, so this is exact.
const fn whole(days: u64) -> f64 {
    days as f64
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;

    use super::{COLUMNS, generate};
    use crate::sheet::Sheet;
    use crate::value::Value;

    fn text(sheet: &Sheet, row: u32, col: u32) -> String {
        let mut out = String::new();
        sheet.write_text(row, col, &mut out);
        out
    }

    fn number(sheet: &Sheet, row: u32, col: u32) -> f64 {
        match sheet.value(row, col) {
            Value::Number(number) => number,
            other => panic!("row {row}, column {col} holds {other:?}, not a number"),
        }
    }

    #[test]
    fn the_shape_is_as_requested() {
        let sheet = generate(1, 500);
        assert_eq!(sheet.rows(), 500);
        assert_eq!(sheet.cols() as usize, COLUMNS);
        assert_eq!(generate(1, 0).rows(), 0);
        assert_eq!(generate(1, 1).rows(), 1);
    }

    #[test]
    fn the_first_row_names_the_columns() {
        let sheet = generate(7, 3);
        assert_eq!(text(&sheet, 0, 0), "订单号");
        assert_eq!(text(&sheet, 0, 6), "SKU");
        assert_eq!(text(&sheet, 0, 19), "备注");
    }

    #[test]
    fn the_same_seed_gives_the_same_sheet() {
        let (a, b, other) = (generate(42, 300), generate(42, 300), generate(43, 300));
        let mut differs = false;
        for col in 0..a.cols() {
            for row in 0..a.rows() {
                assert_eq!(a.value(row, col).pack(), b.value(row, col).pack());
                differs |= a.value(row, col).pack() != other.value(row, col).pack();
            }
        }
        assert!(differs, "a different seed must give different data");
    }

    #[test]
    fn records_are_consistent() {
        let sheet = generate(2026, 2000);
        for row in 1..sheet.rows() {
            let quantity = number(&sheet, row, 9);
            let price = number(&sheet, row, 10);
            let discount = number(&sheet, row, 11);
            let amount = number(&sheet, row, 12);
            let cost = number(&sheet, row, 13);
            let profit = number(&sheet, row, 14);
            assert!((1.0..=12.0).contains(&quantity) && quantity == quantity.trunc());
            assert!(price > 0.0 && (0.0..=0.2).contains(&discount));
            assert!((amount - quantity * price * (1.0 - discount)).abs() < 0.006);
            assert!((profit - (amount - cost)).abs() < 0.006);
            // Text cells refer to strings that exist.
            for col in [2, 3, 4, 5, 6, 7, 8] {
                assert!(!text(&sheet, row, col).is_empty());
            }
            assert!(text(&sheet, row, 6).starts_with("SKU-"));
            assert!(text(&sheet, row, 7).starts_with("C-"));
        }
    }

    #[test]
    fn orders_are_dated_in_order_across_the_period() {
        let sheet = generate(5, 1000);
        assert_eq!(text(&sheet, 1, 1), "2023-01-01");
        assert_eq!(text(&sheet, 999, 1), "2025-12-30");
        let mut previous = 0.0;
        for row in 1..sheet.rows() {
            let day = number(&sheet, row, 1);
            assert!(day >= previous);
            previous = day;
        }
        assert_eq!(text(&sheet, 1, 0), "10000000");
    }

    #[test]
    fn some_cells_are_empty() {
        let sheet = generate(9, 2000);
        let empty = |col| {
            (1..sheet.rows())
                .filter(|row| sheet.value(*row, col) == Value::Empty)
                .count()
        };
        // About three ratings in ten and all but one note in 64 are missing.
        assert!((400..800).contains(&empty(18)));
        assert!(empty(19) > 1800 && empty(19) < 1999);
    }
}

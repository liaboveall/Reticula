//! The native half of the comparison of tables with DuckDB.
//!
//! [`generate`] makes up delimited files, [`answer`] imports them, or Parquet
//! files that DuckDB wrote from them, asks questions made up at random, and
//! writes the questions with the answers. `bench/table-diff/run.mjs` asks
//! DuckDB the same and compares.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use reticula_core::rng::Rng;
use reticula_table::calendar::{FIRST_DAY, LAST_DAY, civil_from_days};
use reticula_table::csv;
use reticula_table::layout::Encoding;
use reticula_table::query::{
    Condition, Figure, NumberOp, QueryError, SortKey, Spec, TimeOp, Value,
};
use reticula_table::reader::Reader;
use reticula_table::types::ColumnType;
use serde_json::{Value as Json, json};

use crate::tables::{
    Cell, Sampler, Table, import_csv_whole, import_parquet, open, pick, read_cell, some_filters,
    some_order, some_part,
};

type Result<T = ()> = std::result::Result<T, String>;

/// What the values of a column look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Row,
    Small,
    Wide,
    Steps,
    Money,
    Decimal,
    DecimalWide,
    DecimalLong,
    Exponent,
    Float,
    Sixteen,
    Lossy,
    LongInteger,
    Padded,
    Plus,
    MinusZero,
    Date,
    SlashDate,
    MixedDate,
    BadDate,
    Time,
    TimeVariants,
    TimeOrDate,
    TimeOffset,
    TimeOfDay,
    Boolean,
    Category,
    Customer,
    Hard,
    LongText,
    Identifier,
    Blank,
    Late,
    Mixed,
    Markers,
    Spaced,
    /// The first one, and from some row on the second one.
    Turns(&'static Class, &'static Class),
    /// Different texts of some bytes.
    Distinct(u16),
    /// Texts of some bytes that begin alike.
    Alike(u16),
}

const CLASSES: [Class; 36] = [
    Class::Row,
    Class::Small,
    Class::Wide,
    Class::Steps,
    Class::Money,
    Class::Decimal,
    Class::DecimalWide,
    Class::DecimalLong,
    Class::Exponent,
    Class::Float,
    Class::Sixteen,
    Class::Lossy,
    Class::LongInteger,
    Class::Padded,
    Class::Plus,
    Class::MinusZero,
    Class::Date,
    Class::SlashDate,
    Class::MixedDate,
    Class::BadDate,
    Class::Time,
    Class::TimeVariants,
    Class::TimeOrDate,
    Class::TimeOffset,
    Class::TimeOfDay,
    Class::Boolean,
    Class::Category,
    Class::Customer,
    Class::Hard,
    Class::LongText,
    Class::Identifier,
    Class::Blank,
    Class::Late,
    Class::Mixed,
    Class::Markers,
    Class::Spaced,
];

/// Turns that the start of a file does not tell of.
const TURNS: [Class; 14] = [
    Class::Turns(&Class::Small, &Class::Money),
    Class::Turns(&Class::Small, &Class::Exponent),
    Class::Turns(&Class::Small, &Class::Category),
    Class::Turns(&Class::Small, &Class::Lossy),
    Class::Turns(&Class::Small, &Class::Date),
    Class::Turns(&Class::Money, &Class::Decimal),
    Class::Turns(&Class::Money, &Class::DecimalWide),
    Class::Turns(&Class::Decimal, &Class::Float),
    Class::Turns(&Class::Date, &Class::Time),
    Class::Turns(&Class::Date, &Class::BadDate),
    Class::Turns(&Class::Boolean, &Class::Customer),
    Class::Turns(&Class::Blank, &Class::Date),
    Class::Turns(&Class::Blank, &Class::Hard),
    Class::Turns(&Class::Steps, &Class::Wide),
];

const CATEGORIES: [&str; 14] = [
    "North",
    "NORTH",
    "north",
    "South",
    "South ",
    "East",
    "West",
    "Ost",
    "Straße",
    "STRASSE",
    "Über",
    "ÜBER",
    "harbour-1",
    "Harbour-1",
];

/// Texts that are hard to tell apart, to fold or to write.
const HARD: [&str; 34] = [
    "a",
    "A",
    "aa",
    "ab",
    "aB",
    "b",
    "a,b",
    "a;b",
    "a|b",
    "a\tb",
    "say \"hello\"",
    "\"",
    "two\nlines",
    "\r\n",
    " lead",
    "trail ",
    "Σ",
    "σ",
    "ΟΔΥΣΣΕΥΣ",
    "οδυσσευς",
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
    "😀",
    "customer-1",
    "#N/A",
];

fn date(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn some_day(rng: &mut Rng) -> i64 {
    match rng.next_below(8) {
        0 => FIRST_DAY + rng.next_below((LAST_DAY - FIRST_DAY) as u64 + 1) as i64,
        1 => *pick(rng, &[FIRST_DAY, LAST_DAY, 0, -1, 11_016]),
        _ => 19_000 + rng.next_below(900) as i64,
    }
}

fn slashed(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}/{month}/{day}")
}

/// What a file is made of at random, and where it turns.
struct Shape {
    rows: usize,
    turn: usize,
    /// Number of different values of the columns that have few.
    domain: u64,
    /// One cell in so many is blank.
    blank_in: u64,
}

impl Class {
    fn value(self, rng: &mut Rng, row: usize, shape: &Shape) -> String {
        let domain = shape.domain;
        if !matches!(self, Self::Row | Self::Distinct(_) | Self::Alike(_))
            && rng.next_below(shape.blank_in) == 0
        {
            return String::new();
        }
        match self {
            Self::Row => format!("{}", row * 3),
            Self::Small => format!("{}", rng.next_below(domain) as i64 - 2),
            Self::Wide => match rng.next_below(6) {
                0 => i64::MIN.to_string(),
                1 => i64::MAX.to_string(),
                2 => format!("{}", rng.next_below(3) as i64 - 1),
                3 => format!("{}", rng.next_u64() >> 20),
                _ => rng.next_u64().cast_signed().to_string(),
            },
            Self::Steps => format!("{}", rng.next_below(domain) * 1_000),
            Self::Money => {
                let cents = rng.next_below(domain * 100) as i64 - 100;
                let sign = if cents < 0 && cents > -100 { "-" } else { "" };
                format!("{sign}{}.{:02}", cents / 100, cents.abs() % 100)
            }
            Self::Decimal => {
                let whole = rng.next_below(domain);
                match rng.next_below(4) {
                    0 => format!("{whole}"),
                    1 => format!("{whole}.{}", rng.next_below(10)),
                    2 => format!("-{}.5", whole + 1),
                    _ => format!("{whole}.{:06}", rng.next_below(1_000_000)),
                }
            }
            Self::DecimalWide => {
                let whole = rng.next_below(1_000_000_000_000);
                format!("{whole}.{:06}", rng.next_below(1_000_000))
            }
            // Too many digits for a decimal, and few enough for a float.
            Self::DecimalLong => match rng.next_below(2) {
                0 => format!("{}.5", rng.next_below(100_000_000_000_000)),
                _ => format!("0.{:010}1", rng.next_below(10_000_000_000)),
            },
            Self::Exponent => match rng.next_below(7) {
                0 => "5e-324".to_owned(),
                1 => "1.5E10".to_owned(),
                2 => format!("{}e-3", rng.next_below(domain)),
                3 => format!("-{}e+2", rng.next_below(domain) + 1),
                4 => format!("{:e}", rng.next_f64() * 1e200),
                _ => format!("{:e}", (rng.next_f64() - 0.5) * 1e6),
            },
            Self::Float => {
                let value = rng.next_f64() - 0.5;
                let factor = [1.0, 1.0, 1e3, 1e9, 1e-5][rng.next_below(5) as usize];
                format!("{:?}", value * factor)
            }
            // As many digits as a float keeps, or one too many, rarely.
            Self::Sixteen => match rng.next_below(3 * shape.rows as u64 / 2 + 2) {
                0 => format!("0.{:016}", rng.next_below(10_000_000_000_000_000)),
                1 => format!("{}.5", rng.next_below(9_999)),
                _ => format!("{:?}", rng.next_f64() + 0.001),
            },
            Self::Lossy => match rng.next_below(3) {
                0 => format!(
                    "0.{:017}{:04}1",
                    rng.next_below(u64::MAX >> 8),
                    rng.next_below(9_999)
                ),
                1 => format!("123456789012345.{:010}7", rng.next_below(10_000_000_000)),
                _ => format!("{}", rng.next_below(100)),
            },
            Self::LongInteger => match rng.next_below(4) {
                0 => "9223372036854775808".to_owned(),
                1 => format!(
                    "{}{:018}",
                    1 + rng.next_below(9_999),
                    rng.next_below(1 << 59)
                ),
                _ => format!("{}", rng.next_below(1 << 40)),
            },
            Self::Padded => format!("{:05}", rng.next_below(20_000)),
            Self::Plus => format!("{}{}", *pick(rng, &["+", "", "-"]), rng.next_below(90) + 1),
            Self::MinusZero => pick(rng, &["-0", "0", "1", "-1", "-0.0"]).to_string(),
            Self::Date => date(some_day(rng)),
            Self::SlashDate => slashed(some_day(rng)),
            Self::MixedDate => {
                let day = some_day(rng);
                if rng.next_below(2) == 0 {
                    date(day)
                } else {
                    slashed(day)
                }
            }
            Self::BadDate => match rng.next_below(4) {
                0 => "2023-02-29".to_owned(),
                1 => "2024-13-01".to_owned(),
                _ => date(some_day(rng)),
            },
            Self::Time => format!(
                "{} {:02}:{:02}:{:02}",
                date(some_day(rng)),
                rng.next_below(24),
                rng.next_below(60),
                rng.next_below(60)
            ),
            Self::TimeVariants => {
                let day = some_day(rng);
                let (hour, minute, second) =
                    (rng.next_below(24), rng.next_below(60), rng.next_below(60));
                match rng.next_below(5) {
                    0 => format!("{}T{hour:02}:{minute:02}", date(day)),
                    1 => format!("{} {hour:02}:{minute:02}:{second:02}Z", date(day)),
                    2 => format!("{} {hour:02}:{minute:02}:{second:02}.5", slashed(day)),
                    3 => format!(
                        "{}T{hour:02}:{minute:02}:{second:02}.{:06}",
                        date(day),
                        rng.next_below(1_000_000)
                    ),
                    _ => format!("{} {hour:02}:{minute:02}:{second:02}.123", date(day)),
                }
            }
            Self::TimeOrDate => {
                if rng.next_below(2) == 0 {
                    date(some_day(rng))
                } else {
                    Self::Time.value(
                        rng,
                        row,
                        &Shape {
                            blank_in: u64::MAX,
                            ..*shape
                        },
                    )
                }
            }
            Self::TimeOffset => {
                format!("{} 12:00:00+02:00", date(19_000 + rng.next_below(9) as i64))
            }
            Self::TimeOfDay => format!("{:02}:{:02}:00", rng.next_below(24), rng.next_below(60)),
            Self::Boolean => pick(rng, &["true", "false", "True", "FALSE"]).to_string(),
            Self::Category => pick(rng, &CATEGORIES).to_string(),
            Self::Customer => {
                let name = *pick(rng, &["customer", "Customer", "CUSTOMER"]);
                format!("{name}-{:06}", rng.next_below(domain * 20))
            }
            Self::Hard => match rng.next_below(3) {
                0 => format!("{}{}", pick(rng, &HARD), pick(rng, &HARD)),
                _ => pick(rng, &HARD).to_string(),
            },
            Self::LongText => {
                let mut text = format!("{:016x}", rng.next_u64());
                for _ in 0..rng.next_below(60) {
                    text.push(' ');
                    text.push_str(pick(rng, &CATEGORIES));
                }
                text
            }
            Self::Identifier => {
                let (a, b) = (rng.next_u64(), rng.next_u64());
                format!(
                    "{:08x}-{:04x}-{:04X}-{:016x}",
                    a >> 32,
                    a >> 16 & 0xffff,
                    a & 0xffff,
                    b
                )
            }
            Self::Blank => String::new(),
            Self::Late if row * 2 < shape.rows => String::new(),
            Self::Late => format!("{}", rng.next_below(domain)),
            Self::Mixed => match rng.next_below(3) {
                0 => format!("{}", rng.next_below(domain)),
                1 => "2024-02-03".to_owned(),
                _ => pick(rng, &CATEGORIES).to_string(),
            },
            Self::Markers => pick(rng, &["NA", "N/A", "NULL", "NaN", "nan", "0", "1"]).to_string(),
            Self::Spaced => pick(rng, &[" 12", "12 ", "7", " "]).to_string(),
            Self::Turns(first, second) => {
                let of = if row < shape.turn { first } else { second };
                of.value(
                    rng,
                    row,
                    &Shape {
                        blank_in: u64::MAX,
                        ..*shape
                    },
                )
            }
            Self::Alike(_) if row < shape.turn => Self::Distinct(8).value(rng, row, shape),
            Self::Alike(bytes) => {
                const BEGIN: &str = "It begins like all of the others, ";
                let mixed = reticula_core::rng::mix64(row as u64);
                let mut text: String = BEGIN
                    .chars()
                    .cycle()
                    .take(usize::from(bytes).saturating_sub(7))
                    .collect();
                if mixed >> 60 == 0 {
                    text = text.to_uppercase();
                }
                // Some come twice.
                text.push_str(&format!("{:07}", mixed % (shape.rows as u64 * 2)));
                text
            }
            // Longer ones begin after the start of the file.
            Self::Distinct(bytes) => {
                let mixed = reticula_core::rng::mix64(row as u64);
                if bytes <= 16 || row < shape.turn {
                    format!("{:07x}", row as u64 | mixed & 0xf000_0000)
                } else {
                    let mut text = format!("{mixed:016x}");
                    while text.len() < usize::from(bytes) {
                        text.push_str(CATEGORIES[(mixed >> (text.len() % 60)) as usize % 14]);
                    }
                    text
                }
            }
        }
    }
}

/// A file to be made.
struct Plan {
    name: String,
    /// What is special about it.
    about: &'static str,
    rows: usize,
    turn: usize,
    classes: Vec<Class>,
    delimiter: char,
    /// Whether some rows are to have too few fields.
    short_rows: bool,
    /// Whether NA and the like are blank.
    markers: bool,
    /// Whether the texts begin so much alike that they need not be sorted.
    may_refuse: bool,
    /// Share of the questions.
    weight: u32,
    parquet: Option<Json>,
}

fn plans(rng: &mut Rng) -> Vec<Plan> {
    let plan = |name: &str, about, rows, classes: &[Class]| Plan {
        name: name.to_owned(),
        about,
        rows,
        turn: usize::MAX,
        classes: classes.to_vec(),
        delimiter: ',',
        short_rows: false,
        markers: false,
        may_refuse: false,
        weight: 2,
        parquet: None,
    };
    let all_kinds = [
        Class::Row,
        Class::Small,
        Class::Wide,
        Class::Money,
        Class::Decimal,
        Class::Float,
        Class::Date,
        Class::Time,
        Class::Boolean,
        Class::Category,
        Class::Customer,
        Class::Identifier,
        Class::Hard,
    ];
    let copy = |compression: &str, row_group: u32, version: &str| {
        Some(json!({ "compression": compression, "rowGroup": row_group, "version": version }))
    };
    let mut plans = vec![
        Plan {
            weight: 8,
            parquet: copy("zstd", 50_000, "V2"),
            ..plan("groups", "three groups of rows", 150_000, &all_kinds)
        },
        Plan {
            weight: 1,
            ..plan(
                "entries",
                "a dictionary that has too many entries",
                4_300_000,
                &[Class::Distinct(8), Class::Small],
            )
        },
        Plan {
            weight: 1,
            turn: 65_000,
            ..plan(
                "bytes",
                "dictionaries that take too much memory",
                145_000,
                &[Class::Distinct(1_200), Class::Category, Class::Small],
            )
        },
        Plan {
            turn: 0,
            ..plan(
                "long",
                "many rows to be sorted, and texts that begin alike outside of a dictionary",
                700_000,
                &[
                    Class::Small,
                    Class::Customer,
                    Class::Alike(24),
                    Class::Money,
                    Class::Float,
                ],
            )
        },
        Plan {
            turn: 0,
            ..plan(
                "alike",
                "longer texts that begin alike outside of a dictionary",
                70_000,
                &[Class::Alike(40), Class::Alike(90), Class::Category],
            )
        },
        Plan {
            turn: 65_000,
            weight: 1,
            may_refuse: true,
            ..plan(
                "alike mixed",
                "texts that begin alike, some of them in a dictionary",
                145_000,
                &[Class::Alike(1_150), Class::Alike(60), Class::Small],
            )
        },
        Plan {
            turn: 30_000,
            weight: 3,
            ..plan(
                "turns",
                "types that the first mebibyte does not tell",
                36_000,
                &TURNS,
            )
        },
        Plan {
            turn: 90_000,
            weight: 2,
            delimiter: ';',
            parquet: copy("snappy", 20_000, "V1"),
            ..plan(
                "turns late",
                "one column that the first mebibyte does not tell",
                100_000,
                &[
                    Class::Row,
                    Class::Turns(&Class::Small, &Class::Decimal),
                    Class::Customer,
                ],
            )
        },
        Plan {
            short_rows: true,
            ..plan("short rows", "rows with too few fields", 3_000, &all_kinds)
        },
    ];
    let wide: Vec<Class> = (0..120)
        .map(|index| CLASSES[index % CLASSES.len()])
        .collect();
    plans.push(Plan {
        weight: 3,
        ..plan("wide", "many columns", 9_000, &wide)
    });
    for rows in [65_535, 65_536, 65_537] {
        let mut classes = all_kinds.to_vec();
        classes.rotate_left(rows % 5);
        classes.truncate(5);
        plans.push(plan(
            &format!("edge {rows}"),
            "at the edge of a group",
            rows,
            &classes,
        ));
    }
    for (index, chunk) in CLASSES.chunks(6).enumerate() {
        let rows = [400, 2_500, 9_000, 1_100, 20_000][index % 5];
        let compression = ["gzip", "uncompressed", "lz4_raw", "zstd", "snappy"][index % 5];
        let version = ["V1", "V2"][index % 2];
        plans.push(Plan {
            weight: 4,
            parquet: copy(compression, 2_048, version),
            ..plan(
                &format!("kinds {index}"),
                "every kind of column",
                rows,
                chunk,
            )
        });
    }
    for number in 0..28 {
        let rows = *pick(
            rng,
            &[
                0, 1, 2, 63, 64, 65, 255, 256, 257, 700, 1_500, 5_000, 12_000,
            ],
        );
        let classes: Vec<Class> = (0..2 + rng.next_below(7))
            .map(|_| *pick(rng, &CLASSES))
            .collect();
        plans.push(Plan {
            delimiter: *pick(rng, &[',', ';', '\t', '|']),
            short_rows: rng.next_below(5) == 0,
            markers: rng.next_below(4) == 0,
            parquet: if rng.next_below(2) == 0 {
                copy(
                    ["uncompressed", "snappy", "gzip", "zstd", "lz4_raw"]
                        [rng.next_below(5) as usize],
                    *pick(rng, &[2_048, 4_096, 122_880]),
                    ["V1", "V2"][rng.next_below(2) as usize],
                )
            } else {
                None
            },
            ..plan(
                &format!("random {number}"),
                "columns at random",
                rows,
                &classes,
            )
        });
    }
    plans
}

fn write_field(out: &mut impl Write, rng: &mut Rng, text: &str, delimiter: char) -> Result {
    let must = text.contains([delimiter, '"', '\n', '\r']);
    let quoted = must || rng.next_below(30) == 0;
    let written = if quoted {
        write!(out, "\"{}\"", text.replace('"', "\"\""))
    } else {
        write!(out, "{text}")
    };
    written.map_err(|error| error.to_string())
}

/// Makes the files, and writes what they are.
///
/// # Errors
///
/// What cannot be written.
pub fn generate(dir: &Path, seed: u64) -> Result {
    let shown = |error: std::io::Error| error.to_string();
    fs::create_dir_all(dir).map_err(shown)?;
    let mut rng = Rng::from_seed(seed);
    let mut tables = Vec::new();
    for plan in plans(&mut rng) {
        let file = format!("{}.csv", plan.name.replace(' ', "-"));
        let mut out = BufWriter::new(File::create(dir.join(&file)).map_err(shown)?);
        let shape = Shape {
            rows: plan.rows,
            turn: plan.turn,
            domain: *pick(&mut rng, &[1, 3, 20, 500, 5_000]),
            blank_in: *pick(&mut rng, &[2, 10, 40, u64::MAX]),
        };
        let names: Vec<String> = plan
            .classes
            .iter()
            .enumerate()
            .map(|(index, class)| format!("{index} {class:?}"))
            .collect();
        let end = if rng.next_below(3) == 0 { "\r\n" } else { "\n" };
        for (index, name) in names.iter().enumerate() {
            if index > 0 {
                write!(out, "{}", plan.delimiter).map_err(shown)?;
            }
            write_field(&mut out, &mut rng, name, plan.delimiter)?;
        }
        out.write_all(end.as_bytes()).map_err(shown)?;
        for row in 0..plan.rows {
            let mut width = plan.classes.len();
            if plan.short_rows && rng.next_below(6) == 0 {
                // One field at least: a line without any is no row.
                width = 1 + rng.next_below(width as u64) as usize;
            }
            for (index, class) in plan.classes.iter().take(width).enumerate() {
                if index > 0 {
                    write!(out, "{}", plan.delimiter).map_err(shown)?;
                }
                let mut value = class.value(&mut rng, row, &shape);
                if index == 0 && width == 1 && value.is_empty() {
                    "0".clone_into(&mut value);
                }
                write_field(&mut out, &mut rng, &value, plan.delimiter)?;
            }
            if row + 1 < plan.rows || rng.next_below(2) == 0 {
                out.write_all(end.as_bytes()).map_err(shown)?;
            }
        }
        out.flush().map_err(shown)?;
        tables.push(json!({
            "name": plan.name,
            "about": plan.about,
            "csv": file,
            "delimiter": plan.delimiter.to_string(),
            "rows": plan.rows,
            "weight": plan.weight,
            "columns": names,
            "shortRows": plan.short_rows,
            "markers": plan.markers,
            "mayRefuse": plan.may_refuse,
            "parquet": plan.parquet,
        }));
    }
    let text = json!({ "seed": seed, "tables": tables });
    fs::write(dir.join("tables.json"), format!("{text:#}\n")).map_err(shown)
}

/// Values of questions.
struct FromFile<'a> {
    file: &'a [u8],
    reader: Reader,
    kinds: Vec<ColumnType>,
}

impl Sampler for FromFile<'_> {
    fn kinds(&self) -> &[ColumnType] {
        &self.kinds
    }

    fn sample(&mut self, rng: &mut Rng, column: usize) -> Cell {
        let row = rng.next_below(u64::from(self.reader.rows().max(1))) as u32;
        read_cell(&mut self.reader, self.file, column, row).expect("the table was just written")
    }
}

fn kind_json(kind: ColumnType) -> Json {
    match kind {
        ColumnType::Integer => json!({ "type": "integer" }),
        ColumnType::Decimal { scale, fixed } => {
            json!({ "type": "decimal", "scale": scale, "fixed": fixed })
        }
        ColumnType::Float => json!({ "type": "float" }),
        ColumnType::Date => json!({ "type": "date" }),
        ColumnType::DateTime => json!({ "type": "datetime" }),
        ColumnType::Boolean => json!({ "type": "boolean" }),
        ColumnType::Text => json!({ "type": "text" }),
    }
}

fn value_json(value: &Value) -> Json {
    match value {
        Value::Blank => Json::Null,
        Value::Int(value) => json!({ "int": value.to_string() }),
        Value::Float(bits) => json!({ "float": format!("{bits:016x}") }),
        Value::Text(text) => json!({ "text": text }),
    }
}

fn condition_json(condition: &Condition) -> Json {
    match condition {
        Condition::OneOf { values } => {
            let values: Vec<Json> = values.iter().map(value_json).collect();
            json!({ "kind": "oneOf", "values": values })
        }
        Condition::TextContains(text) => json!({ "kind": "contains", "text": text }),
        Condition::TextStartsWith(text) => json!({ "kind": "starts", "text": text }),
        Condition::TextEndsWith(text) => json!({ "kind": "ends", "text": text }),
        Condition::Number { op, a, b } => {
            let op = match op {
                NumberOp::Equal => "=",
                NumberOp::NotEqual => "<>",
                NumberOp::Greater => ">",
                NumberOp::AtLeast => ">=",
                NumberOp::Less => "<",
                NumberOp::AtMost => "<=",
                NumberOp::Between => "between",
            };
            json!({ "kind": "number", "op": op, "a": a, "b": b })
        }
        Condition::Time { op, a, b } => {
            let op = match op {
                TimeOp::Before => "<",
                TimeOp::After => ">",
                TimeOp::Between => "between",
                TimeOp::On => "on",
            };
            let b = b.map(|b| b.to_string());
            json!({ "kind": "time", "op": op, "a": a.to_string(), "b": b })
        }
        Condition::Top { n, bottom } => json!({ "kind": "top", "n": n, "bottom": bottom }),
        Condition::Blank => json!({ "kind": "blank" }),
        Condition::NotBlank => json!({ "kind": "notBlank" }),
    }
}

fn filters_json(filters: &[(usize, Condition)]) -> Json {
    filters
        .iter()
        .map(|(column, condition)| json!([column, condition_json(condition)]))
        .collect()
}

fn order_json(order: &[SortKey]) -> Json {
    order
        .iter()
        .map(|key| json!([key.column, key.descending]))
        .collect()
}

fn figure_json(figure: Option<&Figure>) -> Json {
    figure.map_or(
        Json::Null,
        |figure| json!({ "text": figure.text, "exact": figure.exact }),
    )
}

fn summary_json(table: &mut Table<'_>, columns: Vec<usize>) -> Result<Vec<Json>> {
    let summary = table
        .summary(columns)
        .map_err(|error| format!("{error:?}"))?;
    Ok(summary
        .iter()
        .map(|one| {
            json!({
                "column": one.column,
                "rows": one.rows,
                "blanks": one.blanks,
                "distinct": one.distinct,
                "sum": figure_json(one.sum.as_ref()),
                "mean": figure_json(one.mean.as_ref()),
                "min": figure_json(one.min.as_ref()),
                "max": figure_json(one.max.as_ref()),
            })
        })
        .collect())
}

/// A file that numbers are appended to.
struct Numbers {
    out: BufWriter<File>,
    at: u64,
}

impl Numbers {
    fn write(&mut self, bytes: &[u8]) -> Result {
        self.at += bytes.len() as u64;
        self.out.write_all(bytes).map_err(|error| error.to_string())
    }
}

/// Writes the cells of a column: a byte for each that says whether it is
/// blank, then the values. Of texts, where each ends, then the texts.
fn write_cells(
    numbers: &mut Numbers,
    reader: &mut Reader,
    file: &[u8],
    column: usize,
) -> Result<Json> {
    let start = numbers.at;
    let rows = reader.rows();
    let mut blanks = Vec::with_capacity(rows as usize);
    let mut values = Vec::with_capacity(rows as usize * 8);
    let mut texts = String::new();
    for row in 0..rows {
        let cell = read_cell(reader, file, column, row).map_err(|error| format!("{error:?}"))?;
        blanks.push(u8::from(cell == Cell::Blank));
        let word = match cell {
            Cell::Blank => 0,
            Cell::Int(value) => value.cast_unsigned(),
            Cell::Float(value) => value.to_bits(),
            Cell::Text(text) => {
                texts.push_str(&text);
                0
            }
        };
        let word = if reader.columns()[column].kind == ColumnType::Text {
            texts.len() as u64
        } else {
            word
        };
        values.extend_from_slice(&word.to_le_bytes());
    }
    blanks.resize(blanks.len().next_multiple_of(8), 0);
    numbers.write(&blanks)?;
    numbers.write(&values)?;
    let text_len = texts.len();
    let mut texts = texts.into_bytes();
    texts.resize(text_len.next_multiple_of(8), 0);
    numbers.write(&texts)?;
    Ok(json!({ "at": start, "texts": text_len }))
}

/// Imports the files, asks questions and writes the answers.
///
/// # Errors
///
/// What cannot be read, imported, answered or written.
pub fn answer(dir: &Path, from: &str, count: usize, seed: u64) -> Result {
    let shown = |error: std::io::Error| error.to_string();
    let text = fs::read_to_string(dir.join("tables.json")).map_err(shown)?;
    let list: Json = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let mut tables: Vec<(String, String, u64)> = Vec::new();
    for table in list["tables"].as_array().ok_or("no tables")? {
        let name = table["name"].as_str().ok_or("no name")?.to_owned();
        let weight = table["weight"].as_u64().unwrap_or(1);
        if from == "csv" {
            tables.push((
                name,
                table["csv"].as_str().ok_or("no file")?.to_owned(),
                weight,
            ));
        } else if table["parquet"].is_object() {
            let file = format!("{}.parquet", name.replace(' ', "-"));
            tables.push((name, file, weight));
        }
    }
    if from != "csv" {
        // What only Parquet files have is in files of its own.
        for table in list["parquetOnly"].as_array().into_iter().flatten() {
            let name = table["name"].as_str().ok_or("no name")?.to_owned();
            let file = format!("{}.parquet", name.replace(' ', "-"));
            tables.push((name, file, 4));
        }
    }
    let total: u64 = tables.iter().map(|(_, _, weight)| weight).sum();
    for (index, (name, file, weight)) in tables.iter().enumerate() {
        let share = (count as u64 * weight).div_ceil(total.max(1)).max(3);
        let mut rng = Rng::from_seed(seed ^ reticula_core::rng::mix64(index as u64 + 1));
        let table = list["tables"]
            .as_array()
            .and_then(|all| all.iter().find(|table| table["name"] == **name));
        let options = csv::Options {
            delimiter: table
                .and_then(|table| table["delimiter"].as_str())
                .and_then(|text| text.bytes().next()),
            header: Some(true),
            blank_markers: table.is_some_and(|table| table["markers"] == true),
            ..csv::Options::default()
        };
        answer_one(dir, name, file, from, &options, share, &mut rng)
            .map_err(|error| format!("{name} ({from}): {error}"))?;
    }
    Ok(())
}

fn answer_one(
    dir: &Path,
    name: &str,
    file: &str,
    from: &str,
    options: &csv::Options,
    count: u64,
    rng: &mut Rng,
) -> Result {
    let shown = |error: std::io::Error| error.to_string();
    let bytes = fs::read(dir.join(file)).map_err(shown)?;
    let (table, report) = if from == "csv" {
        let (table, report) =
            import_csv_whole(options, &bytes).map_err(|error| format!("{error:?}"))?;
        let short = report.short_rows;
        (
            table,
            json!({ "passes": report.passes, "shortRows": short }),
        )
    } else {
        let mut source = bytes.as_slice();
        let imported = import_parquet(&mut source, bytes.len() as u64, &mut |_, ranges| {
            panic!("all of the file is here, but not {ranges:?}");
        });
        let (table, report) = imported.map_err(|error| format!("{error:?}"))?;
        let counts: Vec<Json> = report
            .columns
            .iter()
            .map(|column| {
                json!({
                    "cutNanos": column.cut_nanos,
                    "notFinite": column.not_finite,
                    "outOfRange": column.out_of_range,
                })
            })
            .collect();
        let skipped: Vec<&str> = report.skipped.iter().map(|one| one.name.as_str()).collect();
        (table, json!({ "counts": counts, "skipped": skipped }))
    };
    drop(bytes);

    let stem = format!("{}.{from}", name.replace(' ', "-"));
    let mut numbers = Numbers {
        out: BufWriter::new(File::create(dir.join(format!("{stem}.bin"))).map_err(shown)?),
        at: 0,
    };
    let mut lines = BufWriter::new(File::create(dir.join(format!("{stem}.jsonl"))).map_err(shown)?);
    let mut line = |value: Json| writeln!(lines, "{value}").map_err(shown);

    let mut reader = open(&table).map_err(|error| format!("{error:?}"))?;
    let kinds: Vec<ColumnType> = reader.columns().iter().map(|column| column.kind).collect();
    let mut columns = Vec::new();
    for column in 0..kinds.len() {
        let cells = write_cells(&mut numbers, &mut reader, &table, column)?;
        let directory = reader.directory();
        let of = &directory.columns[column];
        let plain = (0..directory.groups())
            .filter_map(|group| directory.block(column, group))
            .filter(|block| block.encoding == Encoding::Text)
            .count();
        columns.push(json!({
            "name": of.name,
            "kind": kind_json(of.kind),
            "blanks": of.blanks,
            // All of its texts are in a dictionary, or some, or none.
            "dictionary": of.dictionary.is_some(),
            "plainBlocks": plain,
            "cells": cells,
        }));
    }
    line(json!({
        "what": "table",
        "name": name,
        "from": from,
        "rows": reader.rows(),
        "groups": reader.directory().groups(),
        "groupRows": reader.directory().group_rows,
        "bytes": table.len(),
        "columns": columns,
        "report": report,
    }))?;

    let width = kinds.len();
    let mut sampler = FromFile {
        file: &table,
        reader,
        kinds,
    };
    let mut asked = Table::open(&table).map_err(|error| format!("{error:?}"))?;
    // What is said of whole columns may come from dictionaries and directories.
    for first in (0..width).step_by(16) {
        let columns = (first..width.min(first + 16)).collect();
        let found = summary_json(&mut asked, columns)?;
        line(json!({ "what": "summary", "filters": [], "columns": found }))?;
    }
    let mut order = Vec::new();
    let mut filters = Vec::new();
    for number in 0..count {
        match number % 5 {
            // Often only the filter changes.
            0..=2 => {
                if rng.next_below(3) > 0 {
                    order = some_order(rng, width);
                }
                filters = some_filters(rng, &mut sampler);
                let spec = Spec {
                    filters: filters.clone(),
                    order: order.clone(),
                };
                let mut answer = json!({
                    "what": "view",
                    "filters": filters_json(&filters),
                    "order": order_json(&order),
                });
                match asked.view(&spec) {
                    Ok(rows) => {
                        answer["len"] = rows.len().into();
                        answer["at"] = numbers.at.into();
                        let mut words = Vec::with_capacity(rows.len() * 4 + 4);
                        for row in rows {
                            words.extend_from_slice(&row.to_le_bytes());
                        }
                        words.resize(words.len().next_multiple_of(8), 0);
                        numbers.write(&words)?;
                    }
                    // A refusal is an answer too, and what was there stays.
                    Err(error @ (QueryError::OutOfMemory | QueryError::TooManyEqualPrefixes)) => {
                        answer["refused"] = format!("{error:?}").into();
                        line(answer)?;
                        filters.clone_from(&asked.session.spec().filters);
                        continue;
                    }
                    Err(error) => return Err(format!("{error:?}: {spec:?}")),
                }
                line(answer)?;
            }
            3 => {
                let first = rng.next_below(width as u64) as usize;
                let columns: Vec<usize> =
                    (0..width).cycle().skip(first).take(width.min(16)).collect();
                let found = summary_json(&mut asked, columns)?;
                line(json!({
                    "what": "summary",
                    "filters": filters_json(&filters),
                    "columns": found,
                }))?;
            }
            _ => {
                let column = rng.next_below(width as u64) as usize;
                // What a floating-point number shows is not worked out again.
                let search = match rng.next_below(4) {
                    _ if sampler.kinds[column] == ColumnType::Float => String::new(),
                    0 => some_part(rng, &mut sampler, column),
                    1 => pick(rng, &["1", "-", "2", "TRUE", "0."]).to_string(),
                    _ => String::new(),
                };
                let limit = *pick(rng, &[0, 1, 5, 40, 1_000, 1_000, 1_000]);
                let list = asked
                    .values(column, &search, limit)
                    .map_err(|error| format!("{error:?}"))?;
                let values: Vec<Json> = list
                    .values
                    .iter()
                    .map(|one| json!([value_json(&one.value), one.text, one.count]))
                    .collect();
                line(json!({
                    "what": "values",
                    "filters": filters_json(&filters),
                    "column": column,
                    "search": search,
                    "limit": limit,
                    "values": values,
                    "blanks": list.blanks,
                    "complete": list.complete,
                }))?;
            }
        }
    }
    lines.flush().map_err(shown)?;
    numbers.out.flush().map_err(shown)
}

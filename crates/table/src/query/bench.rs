//! Measurements of ten million rows in memory.
//!
//! ```text
//! cargo test --release -p reticula-table bench -- --ignored --nocapture --test-threads 1
//! ```

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use std::fmt::Write as _;
use std::println;
use std::time::{Duration, Instant};

use reticula_core::rng::Rng;

use super::*;
use crate::tests::open;
use crate::types::ColumnType;
use crate::writer::{ColumnSpec, TableWriter};

const ROWS: usize = 10_000_000;

fn spec(name: &str, kind: ColumnType, dictionary: bool) -> ColumnSpec {
    ColumnSpec {
        name: name.to_string(),
        kind: Some(kind),
        dictionary,
    }
}

fn written(
    specs: Vec<ColumnSpec>,
    rows: usize,
    mut row: impl FnMut(usize, &mut TableWriter),
) -> Vec<u8> {
    let begun = Instant::now();
    let mut writer = TableWriter::new(specs, 65_536).unwrap();
    let mut file = Vec::new();
    for number in 0..rows {
        row(number, &mut writer);
        if (number + 1) % 65_536 == 0 {
            writer.seal_group().unwrap();
            while writer.step(&mut file).unwrap() {}
        }
    }
    writer.seal_group().unwrap();
    while writer.step(&mut file).unwrap() {}
    writer.finish();
    while writer.step(&mut file).unwrap() {}
    println!(
        "wrote {} bytes in {:.1} s",
        file.len(),
        begun.elapsed().as_secs_f64()
    );
    file
}

/// Orders like those of the CSV file that other engines are measured with.
fn orders() -> Vec<u8> {
    let decimal = ColumnType::Decimal {
        scale: 2,
        fixed: true,
    };
    let specs = vec![
        spec("id", ColumnType::Integer, false),
        spec("ordered_at", ColumnType::DateTime, false),
        spec("day", ColumnType::Date, false),
        spec("store", ColumnType::Integer, false),
        spec("region", ColumnType::Text, true),
        spec("product", ColumnType::Text, true),
        spec("customer", ColumnType::Text, true),
        spec("quantity", ColumnType::Integer, false),
        spec("unit_price", decimal, false),
        spec("discount", decimal, false),
        spec("amount", decimal, false),
        spec("paid", ColumnType::Boolean, false),
        spec("note", ColumnType::Text, true),
    ];
    let words = [
        "late", "gift", "return", "exchange", "fragile", "priority", "repeat", "bulk", "online",
        "counter",
    ];
    let regions = [
        "North", "South", "East", "West", "Central", "Harbour", "Airport", "Online",
    ];
    let mut rng = Rng::from_seed(1);
    let mut at = 1_672_531_200_000_i64;
    let mut text = String::new();
    written(specs, ROWS, move |row, writer| {
        at += rng.next_below(16_000) as i64;
        let seconds = at / 1_000;
        let columns = writer.columns();
        columns[0].push_int(row as i64 + 1);
        columns[1].push_int(seconds * 1_000_000);
        columns[2].push_int(seconds / 86_400);
        columns[3].push_int(rng.next_below(5_000) as i64);
        columns[4].push_text(regions[rng.next_below(8) as usize]);
        let product = rng.next_below(2_000);
        text.clear();
        let _ = write!(text, "P-{product:04} {}", words[product as usize % 10]);
        columns[5].push_text(&text);
        text.clear();
        let _ = write!(text, "C{:07}", rng.next_below(1_200_000));
        columns[6].push_text(&text);
        let quantity = 1 + rng.next_below(12) as i64;
        let cents = 99 + rng.next_below(50_000) as i64;
        let discount = if rng.next_below(5) == 0 {
            rng.next_below(40) as i64
        } else {
            0
        };
        columns[7].push_int(quantity);
        columns[8].push_int(cents);
        if discount == 0 {
            columns[9].push_blank();
        } else {
            columns[9].push_int(discount);
        }
        columns[10].push_int((quantity * cents * (100 - discount) + 50) / 100);
        columns[11].push_int(i64::from(rng.next_below(20) != 0));
        if rng.next_below(10) == 0 {
            text.clear();
            let word = words[rng.next_below(10) as usize];
            if rng.next_below(3) == 0 {
                let _ = write!(text, "{word}, {}", words[rng.next_below(10) as usize]);
            } else {
                let _ = write!(text, "{word} {}", rng.next_below(1_000));
            }
            columns[12].push_text(&text);
        } else {
            columns[12].push_blank();
        }
    })
}

/// Columns that are hard for the engine.
fn hard() -> Vec<u8> {
    let specs = vec![
        spec("uuid", ColumnType::Text, false),
        spec("float", ColumnType::Float, false),
        spec("any", ColumnType::Integer, false),
        spec("sequence", ColumnType::Text, false),
        spec("url", ColumnType::Text, false),
    ];
    let mut rng = Rng::from_seed(2);
    let mut text = String::new();
    written(specs, ROWS, move |row, writer| {
        let columns = writer.columns();
        let (a, b) = (rng.next_u64(), rng.next_u64());
        text.clear();
        let _ = write!(
            text,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xffff,
            a & 0xffff,
            b >> 48,
            b & 0xffff_ffff_ffff
        );
        columns[0].push_text(&text);
        columns[1].push_float((rng.next_f64() - 0.5) * 1e6);
        columns[2].push_int(rng.next_u64().cast_signed());
        text.clear();
        let _ = write!(text, "ORD-2024-{:09}", (row * 7_919) % ROWS);
        columns[3].push_text(&text);
        text.clear();
        let _ = write!(
            text,
            "https://shop.example.com/Orders/{:08}",
            rng.next_below(100_000_000)
        );
        columns[4].push_text(&text);
    })
}

/// Does the next job a piece at a time, and tells how it went.
fn timed(name: &str, session: &mut Session, file: &[u8], cells: usize) -> Duration {
    let mut source = file;
    let mut longest = Duration::ZERO;
    let mut most = 0;
    let mut steps = 0_u32;
    let begun = Instant::now();
    loop {
        let step = Instant::now();
        let progress = session.step(&mut source, 1);
        longest = longest.max(step.elapsed());
        steps += 1;
        most = most.max(session.memory());
        if begun.elapsed() > Duration::from_secs(60) {
            println!("{name:<44} given up after {steps} steps: {progress:?}");
            session.cancel();
            break;
        }
        match progress {
            Ok(Progress::Done) => break,
            Ok(Progress::Working { .. }) => {}
            other => {
                println!("{name:<44} {other:?}");
                break;
            }
        }
    }
    let total = begun.elapsed();
    println!(
        "{name:<44} {:>7.1} ms {:>6.2} ns/cell, {steps:>6} steps, longest {:>5.1} ms, {:>4} MB at most",
        total.as_secs_f64() * 1e3,
        total.as_secs_f64() * 1e9 / cells as f64,
        longest.as_secs_f64() * 1e3,
        most >> 20,
    );
    total
}

fn by(columns: &[(usize, bool)]) -> Vec<SortKey> {
    columns
        .iter()
        .map(|(column, descending)| SortKey {
            column: *column,
            descending: *descending,
        })
        .collect()
}

fn view(
    name: &str,
    session: &mut Session,
    file: &[u8],
    filters: Vec<(usize, Condition)>,
    order: Vec<SortKey>,
) {
    session.begin(Spec { filters, order }).unwrap();
    timed(name, session, file, ROWS);
    assert!(session.running().is_none());
}

fn text(value: &str) -> Condition {
    Condition::OneOf {
        values: vec![Value::Text(value.to_string())],
    }
}

fn number(op: NumberOp, a: &str) -> Condition {
    Condition::Number {
        op,
        a: a.to_string(),
        b: None,
    }
}

#[test]
#[ignore = "takes a while"]
fn ten_million_orders() {
    let file = orders();
    let fresh = || Session::new(open(&file));
    let mut session = fresh();
    // The first time, pages are checked against their checksums.
    let all: Vec<usize> = (0..13).collect();
    session.begin_summary(all.clone()).unwrap();
    timed(
        "summary of 13 columns, first time",
        &mut session,
        &file,
        ROWS * 13,
    );
    session.begin_summary(all.clone()).unwrap();
    timed("summary of 13 columns", &mut session, &file, ROWS * 13);
    for column in 0..13 {
        session.begin_values(column, String::new(), 1_000).unwrap();
        timed(
            &std::format!("values of column {column}, first time"),
            &mut session,
            &file,
            ROWS,
        );
    }
    let mut warm = session;
    let sort = |warm: &mut Session, name: &str, order: &[(usize, bool)]| {
        view(name, warm, &file, Vec::new(), by(order));
        view("  (none)", warm, &file, Vec::new(), Vec::new());
    };
    sort(&mut warm, "sort amount", &[(10, false)]);
    sort(&mut warm, "sort amount, descending", &[(10, true)]);
    sort(&mut warm, "sort store", &[(3, false)]);
    sort(&mut warm, "sort id (in order already)", &[(0, false)]);
    sort(&mut warm, "sort time, descending", &[(1, true)]);
    sort(
        &mut warm,
        "sort customer (1.2 million), ranks kept",
        &[(6, false)],
    );
    sort(
        &mut warm,
        "sort region and amount",
        &[(4, false), (10, true)],
    );
    sort(&mut warm, "sort discount (mostly blank)", &[(9, false)]);
    sort(&mut warm, "sort note (mostly blank)", &[(12, false)]);

    let mut cold = fresh();
    view(
        "sort customer, from nothing",
        &mut cold,
        &file,
        Vec::new(),
        by(&[(6, false)]),
    );
    view(
        "  then region = North",
        &mut cold,
        &file,
        vec![(4, text("north"))],
        by(&[(6, false)]),
    );
    let mut cold = fresh();
    view(
        "region = North, from nothing",
        &mut cold,
        &file,
        vec![(4, text("north"))],
        Vec::new(),
    );
    let mut cold = fresh();
    view(
        "customer contains 1234, from nothing",
        &mut cold,
        &file,
        vec![(6, Condition::TextContains("1234".to_string()))],
        Vec::new(),
    );
    let filter = |warm: &mut Session, name: &str, filters: Vec<(usize, Condition)>| {
        view(name, warm, &file, filters, Vec::new());
        println!("  {} rows", warm.view_len());
    };
    filter(&mut warm, "region = North", vec![(4, text("north"))]);
    filter(
        &mut warm,
        "store = 1234",
        vec![(3, number(NumberOp::Equal, "1234"))],
    );
    filter(
        &mut warm,
        "amount > 5000.005",
        vec![(10, number(NumberOp::Greater, "5000.005"))],
    );
    filter(
        &mut warm,
        "region = North and amount > 5000",
        vec![(4, text("north")), (10, number(NumberOp::Greater, "5000"))],
    );
    filter(
        &mut warm,
        "customer contains 1234",
        vec![(6, Condition::TextContains("1234".to_string()))],
    );
    filter(
        &mut warm,
        "customer = C0001234",
        vec![(6, text("c0001234"))],
    );
    filter(
        &mut warm,
        "top 100 amounts",
        vec![(
            10,
            Condition::Top {
                n: 100,
                bottom: false,
            },
        )],
    );
    let few = vec![(4, text("north")), (10, number(NumberOp::Greater, "5000"))];
    view(
        "0.2% of the rows, sorted by customer",
        &mut warm,
        &file,
        few.clone(),
        by(&[(6, false)]),
    );
    warm.begin_summary(all).unwrap();
    timed(
        "  summary of 13 columns of those",
        &mut warm,
        &file,
        ROWS * 13,
    );
    for column in [0, 5, 6, 10] {
        warm.begin_values(column, String::new(), 1_000).unwrap();
        timed(
            &std::format!("  values of column {column} of those"),
            &mut warm,
            &file,
            ROWS,
        );
    }
    warm.begin_values(6, "123".to_string(), 1_000).unwrap();
    timed("  customers that contain 123", &mut warm, &file, ROWS);
}

#[test]
#[ignore = "takes a while"]
fn ten_million_hard_rows() {
    let file = hard();
    let mut session = Session::new(open(&file));
    session.begin_summary((0..5).collect()).unwrap();
    timed(
        "summary of 5 columns, first time",
        &mut session,
        &file,
        ROWS * 5,
    );
    for (column, name) in [
        (1, "sort full floats"),
        (2, "sort full integers"),
        (0, "sort 36-byte texts"),
        (3, "sort 18-byte texts that begin alike"),
        (4, "sort 40-byte texts that begin alike"),
    ] {
        view(
            name,
            &mut session,
            &file,
            Vec::new(),
            by(&[(column, false)]),
        );
        view("  (none)", &mut session, &file, Vec::new(), Vec::new());
    }
    for column in [0, 2] {
        session.begin_values(column, String::new(), 1_000).unwrap();
        timed("values", &mut session, &file, ROWS);
    }
    view(
        "uuid contains abcd",
        &mut session,
        &file,
        vec![(0, Condition::TextContains("ABCD".to_string()))],
        Vec::new(),
    );
    view(
        "float > 0.1",
        &mut session,
        &file,
        vec![(1, number(NumberOp::Greater, "0.1"))],
        Vec::new(),
    );
}

#[test]
#[ignore = "takes a while"]
fn long_texts() {
    // Blocks of 62 MiB and of 125 MiB.
    for (rows, len) in [(3 * 65_536, 1_000), (2 * 65_536, 2_000)] {
        println!("{rows} texts of {len} bytes");
        let mut rng = Rng::from_seed(3);
        let mut text = String::new();
        let file = written(
            vec![
                spec("text", ColumnType::Text, false),
                spec("even", ColumnType::Boolean, false),
            ],
            rows,
            move |row, writer| {
                text.clear();
                let _ = write!(text, "Dear customer {:08}, ", rng.next_below(100_000_000));
                while text.len() < len {
                    let _ = write!(text, "{} and ", rng.next_u64());
                }
                writer.columns()[0].push_text(&text);
                writer.columns()[1].push_int(row as i64 % 2);
            },
        );
        let mut session = Session::new(open(&file));
        let contains = (0, Condition::TextContains("99 AND 1".to_string()));
        let even = Condition::OneOf {
            values: vec![Value::Int(0)],
        };
        for round in ["  first", "  again"] {
            session
                .begin(Spec {
                    filters: vec![contains.clone()],
                    order: Vec::new(),
                })
                .unwrap();
            timed(
                &std::format!("{round}: contains"),
                &mut session,
                &file,
                rows,
            );
        }
        let order = by(&[(0, false)]);
        let filters = vec![(1, even)];
        session.begin(Spec { filters, order }).unwrap();
        timed("  half of them, sorted", &mut session, &file, rows);
        session
            .begin(Spec {
                filters: Vec::new(),
                order: by(&[(0, true)]),
            })
            .unwrap();
        timed("  sorted", &mut session, &file, rows);
        session.begin_values(0, String::new(), 1_000).unwrap();
        timed("  values", &mut session, &file, rows);
        session.begin_summary(vec![0]).unwrap();
        timed("  summary", &mut session, &file, rows);
    }
    let pairs = written(
        vec![spec("text", ColumnType::Text, false)],
        1_000_000,
        |row, writer| {
            let number = (row * 7_919) % 500_000;
            let text = std::format!("{:0>192}{number:08}", "Dear customer, ");
            writer.columns()[0].push_text(&text);
        },
    );
    let mut session = Session::new(open(&pairs));
    let order = by(&[(0, false)]);
    let spec = Spec {
        filters: Vec::new(),
        order,
    };
    session.begin(spec.clone()).unwrap();
    timed(
        "a million texts, each twice, sorted",
        &mut session,
        &pairs,
        1_000_000,
    );
    let letters = written(
        vec![self::spec("text", ColumnType::Text, false)],
        200_001,
        |row, writer| {
            let text = match row {
                0..120_000 => std::format!("{row:08}"),
                _ => {
                    let number = row.min(199_999);
                    std::format!("Dear customer, {number:06} {:0>1000}", "x")
                }
            };
            writer.columns()[0].push_text(&text);
        },
    );
    let mut session = Session::new(open(&letters));
    session.begin(spec).unwrap();
    timed(
        "letters, one of them twice, sorted",
        &mut session,
        &letters,
        200_001,
    );
}

//! How fast importing is. Run with:
//!
//! `cargo test --release -p reticula-table parquet::speed -- --ignored --nocapture`
//!
//! `RETICULA_PARQUET` may name a file to be measured besides.

use alloc::format;
use alloc::vec::Vec;
use std::time::{Duration, Instant};
use std::{env, fs, println};

use reticula_core::rng::Rng;

use super::encode::{self, Field, Layout, Means, Raw};
use super::meta::{
    BYTE_STREAM_SPLIT, DELTA_BINARY_PACKED, DELTA_BYTE_ARRAY, DELTA_LENGTH_BYTE_ARRAY, PLAIN, RLE,
    RLE_DICTIONARY,
};
use super::stream::{Advance, Input, Out, Scratch, Stream};
use super::value::Physical;
use super::{Import, Opened, Options, Step, open};
use crate::source::{Sink, SinkFailed, Source};

/// Counts the bytes that are written.
struct Count(u64);

impl Sink for Count {
    fn append(&mut self, bytes: &[u8]) -> Result<(), SinkFailed> {
        self.0 += bytes.len() as u64;
        Ok(())
    }
}

fn opened(file: &[u8]) -> super::ParquetFile {
    let mut source = file;
    match open(&mut source, file.len() as u64).unwrap() {
        Opened::Ready(opened) => opened,
        Opened::Need(_) => panic!("the file is here"),
    }
}

/// Time of all steps and of the longest one, and bytes of the table.
fn whole(file: &[u8]) -> (Duration, Duration, u64) {
    let mut import = Import::new(opened(file), &Options::default()).unwrap();
    let mut source = file;
    let mut table = Count(0);
    let mut longest = Duration::ZERO;
    let start = Instant::now();
    loop {
        let before = Instant::now();
        let step = import.step(&mut source, &mut table).unwrap();
        longest = longest.max(before.elapsed());
        if step == Step::Done {
            return (start.elapsed(), longest, table.0);
        }
    }
}

/// Time to read every value of a column without keeping it.
fn decoded<S: Source>(file: &super::ParquetFile, column: usize, source: &mut S) -> Duration {
    let mut scratch = Scratch::new();
    let mut stream = Stream::new();
    let mut input = Input {
        source,
        column: &file.columns[column],
        data_end: file.data_end,
        window: 1 << 20,
    };
    let mut rows = 0;
    let mut work = 0;
    let start = Instant::now();
    while rows < file.rows {
        let advanced = stream
            .advance(
                &mut input,
                &mut scratch,
                usize::MAX,
                &mut Out::Nowhere,
                &mut work,
            )
            .unwrap();
        if let Advance::Rows(count) = advanced {
            rows += count as u64;
        }
    }
    start.elapsed()
}

fn per_cell(time: Duration, cells: u64) -> f64 {
    time.as_secs_f64() * 1e9 / cells as f64
}

#[test]
#[ignore = "a measurement"]
fn encodings() {
    let mut rng = Rng::from_seed(1);
    let rows = 1_000_000_usize;
    let number = |rng: &mut Rng, few: bool| {
        Raw::Number(if few {
            rng.next_below(200)
        } else {
            rng.next_below(1 << 40)
        })
    };
    let text = |rng: &mut Rng, few: bool| {
        let value = rng.next_below(if few { 200 } else { 1 << 40 });
        Raw::Bytes(format!("text {value}").into_bytes())
    };
    let half_empty = |values: &mut Vec<Option<Raw>>, rng: &mut Rng| {
        for value in values {
            if rng.next_below(2) == 0 {
                *value = None;
            }
        }
    };
    for (name, physical, encoding, few, optional) in [
        ("int64 plain", Physical::Int64, PLAIN, false, false),
        (
            "int64 plain, half blank",
            Physical::Int64,
            PLAIN,
            false,
            true,
        ),
        (
            "int64 delta",
            Physical::Int64,
            DELTA_BINARY_PACKED,
            false,
            false,
        ),
        (
            "int64 split",
            Physical::Int64,
            BYTE_STREAM_SPLIT,
            false,
            false,
        ),
        (
            "int64 dictionary",
            Physical::Int64,
            RLE_DICTIONARY,
            true,
            false,
        ),
        ("double plain", Physical::Double, PLAIN, false, false),
        ("truth plain", Physical::Boolean, PLAIN, false, false),
        ("truth runs", Physical::Boolean, RLE, false, false),
        (
            "text dictionary",
            Physical::Bytes,
            RLE_DICTIONARY,
            true,
            false,
        ),
        (
            "text dictionary, half blank",
            Physical::Bytes,
            RLE_DICTIONARY,
            true,
            true,
        ),
        ("text plain, few", Physical::Bytes, PLAIN, true, false),
        ("text plain, many", Physical::Bytes, PLAIN, false, false),
        (
            "text lengths",
            Physical::Bytes,
            DELTA_LENGTH_BYTE_ARRAY,
            false,
            false,
        ),
        (
            "text parts",
            Physical::Bytes,
            DELTA_BYTE_ARRAY,
            false,
            false,
        ),
    ] {
        let mut field = Field::new("a", physical, Means::Nothing);
        field.encoding = encoding;
        field.optional = optional;
        field.values = (0..rows)
            .map(|_| {
                Some(match physical {
                    Physical::Bytes => text(&mut rng, few),
                    Physical::Boolean => Raw::Number(rng.next_below(2)),
                    _ => number(&mut rng, few),
                })
            })
            .collect();
        if optional {
            half_empty(&mut field.values, &mut rng);
        }
        let layout = Layout {
            page_rows: 200_000,
            ..Layout::simple()
        };
        let file = encode::write(&[field], &layout, &mut rng);
        let meta = opened(&file);
        let mut source = file.as_slice();
        let read = (0..3)
            .map(|_| decoded(&meta, 0, &mut source))
            .min()
            .unwrap();
        let (time, _, _) = (0..3).map(|_| whole(&file)).min().unwrap();
        println!(
            "{name:30} read {:6.2} ns, imported {:6.2} ns per cell",
            per_cell(read, rows as u64),
            per_cell(time, rows as u64),
        );
    }
}

#[test]
#[ignore = "a measurement"]
fn codecs() {
    let mut rng = Rng::from_seed(2);
    // Text of some words, which the codecs find something in.
    let words: Vec<_> = (0..2_000)
        .map(|_| format!("{:x} ", rng.next_below(1 << 24)))
        .collect();
    let mut bytes = Vec::new();
    while bytes.len() < 4 << 20 {
        let word = &words[rng.next_below(words.len() as u64) as usize];
        bytes.extend_from_slice(word.as_bytes());
    }
    let mut unpacker = super::codec::Unpacker::default();
    let mut out = alloc::vec![0; bytes.len()];
    for codec in super::codec::tests::ALL {
        // Snappy and gzip are not really packed by the tests.
        let packed = super::codec::tests::pack(codec, &bytes);
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let start = Instant::now();
            unpacker.unpack(codec, &packed, &mut out).unwrap();
            best = best.min(start.elapsed());
        }
        println!(
            "{codec:?}: {:.0} MB/s, packed to {:.0} %",
            bytes.len() as f64 / 1e6 / best.as_secs_f64(),
            packed.len() as f64 * 100.0 / bytes.len() as f64,
        );
    }
    let start = Instant::now();
    let sum = unpacker.checksum(&bytes);
    println!(
        "CRC-32: {:.0} MB/s ({sum:x})",
        bytes.len() as f64 / 1e6 / start.elapsed().as_secs_f64()
    );
}

#[test]
#[ignore = "a measurement"]
fn file() {
    let Ok(path) = env::var("RETICULA_PARQUET") else {
        return;
    };
    let file = fs::read(path).unwrap();
    let meta = opened(&file);
    let columns = meta.columns.len() as u64;
    let mut source = file.as_slice();
    for (index, column) in meta.columns.iter().enumerate() {
        let time = decoded(&meta, index, &mut source);
        println!(
            "{:20} {:24} read {:6.2} ns per cell",
            column.name,
            format!("{:?}", column.kind),
            per_cell(time, meta.rows)
        );
    }
    let (time, longest, bytes) = whole(&file);
    println!(
        "{} rows, {columns} columns: {:.2} s, {:.2} ns per cell, longest step {:.1} ms, {} bytes to {bytes}",
        meta.rows,
        time.as_secs_f64(),
        per_cell(time, meta.rows * columns),
        longest.as_secs_f64() * 1e3,
        file.len(),
    );
}

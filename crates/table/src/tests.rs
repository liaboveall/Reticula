//! Tables written, read back and compared with what went in.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use std::format;

use reticula_core::rng::Rng;
use reticula_core::viewport::PageFrame;

use crate::layout::{Encoding, PAGE_BYTES};
use crate::page;
use crate::reader::{BlockBuffer, Opened, ReadError, Reader};
use crate::source::{Source, SourceError};
use crate::text;
use crate::types::{CellValue, ColumnType};
use crate::writer::{ColumnSpec, TableWriter, WriteError, group_rows_for};

/// A cell as the tests have it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Model {
    Blank,
    Int(i64),
    Float(f64),
    Text(String),
}

pub(crate) struct ModelColumn {
    pub(crate) name: String,
    pub(crate) kind: Option<ColumnType>,
    pub(crate) dictionary: bool,
    pub(crate) cells: Vec<Model>,
}

/// Writes the columns the way the importers do.
pub(crate) fn write(columns: &[ModelColumn], group_rows: u32) -> Vec<u8> {
    let specs = columns
        .iter()
        .map(|column| ColumnSpec {
            name: column.name.clone(),
            kind: column.kind,
            dictionary: column.dictionary,
        })
        .collect();
    let mut writer = TableWriter::new(specs, group_rows).unwrap();
    let mut file = Vec::new();
    let rows = columns.first().map_or(0, |column| column.cells.len());
    for row in 0..rows {
        for (builder, column) in writer.columns().iter_mut().zip(columns) {
            match &column.cells[row] {
                Model::Blank => builder.push_blank(),
                Model::Int(value) => builder.push_int(*value),
                Model::Float(value) => builder.push_float(*value),
                Model::Text(value) => {
                    if builder.kind().is_none() {
                        builder.set_kind(ColumnType::Text);
                    }
                    assert!(!builder.push_text(value));
                }
            }
        }
        if writer.check_group().unwrap() {
            writer.seal_group().unwrap();
            while writer.step(&mut file).unwrap() {}
        }
    }
    writer.seal_group().unwrap();
    while writer.step(&mut file).unwrap() {}
    writer.finish();
    while writer.step(&mut file).unwrap() {}
    let written = writer.written().unwrap();
    assert_eq!(written.rows as usize, rows);
    assert_eq!(written.bytes, file.len() as u64);
    file
}

pub(crate) fn open(file: &[u8]) -> Reader {
    let mut source = file;
    match Reader::open(&mut source, file.len() as u64).unwrap() {
        Opened::Ready(reader) => *reader,
        Opened::Need(range) => panic!("needs {range:?}"),
    }
}

/// Columns of every kind, with shapes that lead to different encodings.
pub(crate) fn columns(rng: &mut Rng, rows: usize) -> Vec<ModelColumn> {
    let mut make = |name: &str,
                    kind: Option<ColumnType>,
                    dictionary: bool,
                    cell: &mut dyn FnMut(&mut Rng, usize) -> Model| {
        let cells = (0..rows).map(|row| cell(rng, row)).collect();
        ModelColumn {
            name: name.to_string(),
            kind,
            dictionary,
            cells,
        }
    };
    let int = |value: i64| Model::Int(value);
    vec![
        make("row", Some(ColumnType::Integer), false, &mut |_, row| {
            int(row as i64)
        }),
        make("any", Some(ColumnType::Integer), false, &mut |rng, _| {
            int(rng.next_u64().cast_signed())
        }),
        make("same", Some(ColumnType::Integer), false, &mut |_, _| {
            int(i64::MIN)
        }),
        make("holes", Some(ColumnType::Integer), false, &mut |rng, _| {
            if rng.next_below(3) == 0 {
                int(rng.next_below(1_000) as i64 - 500)
            } else {
                Model::Blank
            }
        }),
        make(
            "seconds",
            Some(ColumnType::DateTime),
            false,
            &mut |rng, row| int((row as i64 * 7 + rng.next_below(5) as i64) * 1_000_000 + 250_000),
        ),
        make("day", Some(ColumnType::Date), false, &mut |_, row| {
            int(19_000 + row as i64 / 100)
        }),
        make("truth", Some(ColumnType::Boolean), false, &mut |rng, _| {
            int(rng.next_below(2) as i64)
        }),
        make(
            "money",
            Some(ColumnType::Decimal {
                scale: 2,
                fixed: true,
            }),
            false,
            &mut |rng, _| int(rng.next_below(1_000_000) as i64 * 5),
        ),
        make(
            "float",
            Some(ColumnType::Float),
            false,
            &mut |rng, _| match rng.next_below(5) {
                0 => Model::Blank,
                1 => Model::Float(-0.0),
                _ => Model::Float((rng.next_f64() - 0.5) * 1e18),
            },
        ),
        make("few", Some(ColumnType::Text), true, &mut |rng, _| {
            Model::Text(format!("值 {}", rng.next_below(20)))
        }),
        make("many", Some(ColumnType::Text), true, &mut |rng, _| {
            if rng.next_below(10) == 0 {
                Model::Blank
            } else {
                Model::Text(format!("{:x}", rng.next_below(5_000)))
            }
        }),
        make("plain", Some(ColumnType::Text), false, &mut |rng, _| {
            let len = rng.next_below(40) as usize;
            if len == 0 {
                Model::Text(String::new())
            } else {
                Model::Text("é漢a".chars().cycle().take(len).collect())
            }
        }),
        make("late", None, true, &mut |rng, row| {
            if row < rows / 2 {
                Model::Blank
            } else {
                Model::Text(rng.next_below(3).to_string())
            }
        }),
        make("nothing", None, false, &mut |_, _| Model::Blank),
    ]
}

/// What comes back for a cell: the empty text is a blank, and zero has no sign.
fn expected(cell: &Model) -> Model {
    match cell {
        Model::Text(text) if text.is_empty() => Model::Blank,
        Model::Float(value) if *value == 0.0 => Model::Float(0.0),
        other => other.clone(),
    }
}

fn read<S: Source>(reader: &mut Reader, source: &mut S, column: usize, row: u32) -> Model {
    let mut text = String::from("x");
    match reader
        .cell(source, column, row, usize::MAX, &mut text)
        .unwrap()
    {
        CellValue::Blank => Model::Blank,
        CellValue::Int(value) => Model::Int(value),
        CellValue::Float(value) => Model::Float(value),
        CellValue::Text => Model::Text(text.split_off(1)),
    }
}

#[test]
fn what_was_written_is_read() {
    let mut rng = Rng::from_seed(11);
    for rows in [0_usize, 1, 2, 63, 64, 65, 255, 256, 257, 512, 1_000, 3_000] {
        let columns = columns(&mut rng, rows);
        let file = write(&columns, 256);
        let mut reader = open(&file);
        let mut source = file.as_slice();
        assert_eq!(reader.rows() as usize, rows);
        for (index, column) in columns.iter().enumerate() {
            assert_eq!(reader.columns()[index].name, column.name);
            let blanks = column
                .cells
                .iter()
                .filter(|cell| expected(cell) == Model::Blank)
                .count();
            assert_eq!(reader.columns()[index].blanks, blanks as u64);
            // Single cells, in no particular order.
            for _ in 0..rows.min(300) {
                let row = rng.next_below(rows as u64) as usize;
                assert_eq!(
                    read(&mut reader, &mut source, index, row as u32),
                    expected(&column.cells[row]),
                    "column {} of {rows} rows, row {row}",
                    column.name
                );
            }
            // Whole blocks.
            while !reader.load_dictionary(&mut source, index).unwrap() {}
            let mut buffer = BlockBuffer::default();
            for group in 0..reader.directory().groups() {
                reader
                    .block(&mut source, index, group, &mut buffer)
                    .unwrap();
                for within in 0..buffer.rows() {
                    let row = group as usize * 256 + within;
                    let value = if buffer.is_blank(within) {
                        Model::Blank
                    } else {
                        match buffer.block().encoding {
                            Encoding::Blank => Model::Blank,
                            Encoding::Packed => match reader.dictionary(index) {
                                Some(texts) => {
                                    Model::Text(texts.text(buffer.code(within)).to_string())
                                }
                                None => Model::Int(buffer.int(within)),
                            },
                            Encoding::Raw => Model::Float(buffer.float(within)),
                            Encoding::Text => Model::Text(
                                core::str::from_utf8(buffer.text(within))
                                    .unwrap()
                                    .to_string(),
                            ),
                        }
                    };
                    assert_eq!(value, expected(&column.cells[row]));
                    // The bounds in the directory hold.
                    if let Model::Int(value) = value {
                        let block = buffer.block();
                        assert!(block.low.cast_signed() <= value);
                        assert!(value <= block.high.cast_signed());
                    }
                }
            }
        }
        // Past the end there is nothing.
        assert_eq!(read(&mut reader, &mut source, 0, rows as u32), Model::Blank);
        assert_eq!(read(&mut reader, &mut source, 99, 0), Model::Blank);
    }
}

#[test]
fn a_file_can_be_written_again_to_the_same_bytes() {
    let columns = columns(&mut Rng::from_seed(5), 1_000);
    assert_eq!(write(&columns, 256), write(&columns, 256));
}

#[test]
fn narrow_columns_take_few_bytes() {
    let rows = 65_536;
    let one = |kind, cell: &dyn Fn(usize) -> Model| {
        let file = write(
            &[ModelColumn {
                name: String::from("a"),
                kind: Some(kind),
                dictionary: true,
                cells: (0..rows).map(cell).collect(),
            }],
            65_536,
        );
        file.len()
    };
    // One bit, sixteen bits, and whole seconds a minute apart at most.
    assert!(one(ColumnType::Boolean, &|row| Model::Int((row % 2) as i64)) < 13_000);
    assert!(one(ColumnType::Integer, &|row| Model::Int(row as i64)) < 136_000);
    assert!(
        one(ColumnType::DateTime, &|row| Model::Int(
            1_700_000_000_000_000 + row as i64 * 60_000_000
        )) < 190_000
    );
    assert!(
        one(ColumnType::Text, &|row| Model::Text(
            ["north", "south", "east", "west"][row % 4].to_string()
        )) < 21_000
    );
}

#[test]
fn a_dictionary_that_becomes_too_large_is_left_for_the_rest() {
    // Texts that are all different, in a dictionary whose limit is reached
    // between two groups: rows before and after it both come back.
    let rows = 70_000;
    let columns = [ModelColumn {
        name: String::from("a"),
        kind: Some(ColumnType::Text),
        dictionary: true,
        cells: (0..rows)
            .map(|row| Model::Text(format!("{row:0>2100}")))
            .collect(),
    }];
    let file = write(&columns, 8_192);
    let mut reader = open(&file);
    let mut source = file.as_slice();
    let encodings: Vec<Encoding> = reader
        .directory()
        .blocks
        .iter()
        .map(|block| block.encoding)
        .collect();
    assert_eq!(encodings.first(), Some(&Encoding::Packed));
    assert_eq!(encodings.last(), Some(&Encoding::Text));
    for row in [0, 1, 8_191, 8_192, 60_000, 69_999] {
        assert_eq!(
            read(&mut reader, &mut source, 0, row),
            columns[0].cells[row as usize]
        );
    }
}

#[test]
fn texts_can_be_given_by_their_codes() {
    let spec = |dictionary| ColumnSpec {
        name: String::from("a"),
        kind: Some(ColumnType::Text),
        dictionary,
    };
    let mut writer = TableWriter::new(vec![spec(true)], 256).unwrap();
    let mut file = Vec::new();
    let column = &mut writer.columns()[0];
    let (one, two) = (
        column.code_of("one").unwrap(),
        column.code_of("two").unwrap(),
    );
    assert_eq!(column.code_of("one"), Some(one));
    assert_eq!(column.code_of(""), None);
    column.push_code(two);
    column.push_blank();
    column.push_text("one");
    column.push_code(one);
    column.push_code(99);
    writer.seal_group().unwrap();
    while writer.step(&mut file).unwrap() {}
    writer.finish();
    while writer.step(&mut file).unwrap() {}
    let mut reader = open(&file);
    let mut source = file.as_slice();
    let cells: Vec<Model> = (0..5)
        .map(|row| read(&mut reader, &mut source, 0, row))
        .collect();
    let text = |text: &str| Model::Text(text.to_string());
    assert_eq!(
        cells,
        [
            text("two"),
            Model::Blank,
            text("one"),
            text("one"),
            Model::Blank
        ]
    );

    let mut plain = TableWriter::new(vec![spec(false)], 256).unwrap();
    assert_eq!(plain.columns()[0].code_of("one"), None);
    plain.columns()[0].push_code(0);
    assert_eq!(plain.columns()[0].len(), 1);
}

#[test]
fn long_texts_make_a_group_too_large_with_and_without_a_dictionary() {
    for dictionary in [false, true] {
        let mut writer = TableWriter::new(
            vec![ColumnSpec {
                name: String::from("a"),
                kind: Some(ColumnType::Text),
                dictionary,
            }],
            65_536,
        )
        .unwrap();
        let mut refused = false;
        for row in 0..200 {
            let text = format!("{row} {}", "x".repeat(1_000_000));
            writer.columns()[0].push_text(&text);
            if writer.check_group().is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused);
        assert_eq!(writer.seal_group(), Err(WriteError::GroupTooLarge));
    }
}

#[test]
fn codes_are_stored_as_they_are() {
    // The second group has only the first and the eleventh text: a multiple
    // of ten apart, like numbers that are stored in tens.
    let text = |number: usize| Model::Text(format!("t{number:02}"));
    let cells: Vec<Model> = (0..512)
        .map(|row| {
            if row < 256 {
                text(row % 11)
            } else {
                text(row % 2 * 10)
            }
        })
        .collect();
    let columns = [ModelColumn {
        name: String::from("a"),
        kind: Some(ColumnType::Text),
        dictionary: true,
        cells: cells.clone(),
    }];
    let file = write(&columns, 256);
    let mut reader = open(&file);
    let mut source = file.as_slice();
    assert_eq!(reader.directory().block(0, 1).unwrap().unit, 0);
    for (row, cell) in cells.iter().enumerate() {
        assert_eq!(read(&mut reader, &mut source, 0, row as u32), *cell);
    }
    let mut session = crate::query::Session::new(reader);
    for number in 0..11 {
        session
            .begin(crate::query::Spec {
                filters: vec![(
                    0,
                    crate::query::Condition::OneOf {
                        values: vec![crate::query::Value::Text(format!("t{number:02}"))],
                    },
                )],
                order: Vec::new(),
            })
            .unwrap();
        while session.step(&mut source, 1_000).unwrap() != crate::query::Progress::Done {}
        let expected: Vec<u32> = (0..512)
            .filter(|row| cells[*row as usize] == text(number))
            .collect();
        let found: Vec<u32> = (0..session.view_len())
            .map(|position| session.view_row(position).unwrap())
            .collect();
        assert_eq!(found, expected, "t{number:02}");
    }
}

#[test]
fn columns_of_different_lengths_are_refused() {
    let spec = |name: &str| ColumnSpec {
        name: name.to_string(),
        kind: Some(ColumnType::Integer),
        dictionary: false,
    };
    let mut writer = TableWriter::new(vec![spec("a"), spec("b")], 256).unwrap();
    writer.columns()[0].push_int(1);
    assert_eq!(writer.seal_group(), Err(WriteError::Uneven));
    assert!(TableWriter::new(Vec::new(), 256).is_err());
}

#[test]
fn a_group_that_is_not_the_last_has_to_be_full() {
    let spec = ColumnSpec {
        name: String::from("a"),
        kind: Some(ColumnType::Integer),
        dictionary: false,
    };
    let mut writer = TableWriter::new(vec![spec], 256).unwrap();
    let mut file = Vec::new();
    for _ in 0..2 {
        writer.columns()[0].push_int(1);
    }
    writer.seal_group().unwrap();
    while writer.step(&mut file).unwrap() {}
    writer.columns()[0].push_int(1);
    assert_eq!(writer.seal_group(), Err(WriteError::Uneven));
}

#[test]
fn groups_get_smaller_for_wide_tables() {
    assert_eq!(group_rows_for(13, 100), 65_536);
    assert_eq!(group_rows_for(1_024, 8_192), 8_192);
    assert_eq!(group_rows_for(1, 1 << 30), 256);
    assert_eq!(group_rows_for(0, 0), 65_536);
}

/// Opens a damaged file and does everything that can be done with it.
pub(crate) fn exercise(file: &[u8]) -> Result<(), ReadError> {
    let mut source = file;
    let mut reader = match Reader::open(&mut source, file.len() as u64)? {
        Opened::Ready(reader) => *reader,
        Opened::Need(_) => return Ok(()),
    };
    let mut text = String::new();
    let mut buffer = BlockBuffer::default();
    let mut failed = None;
    let last = reader.rows().saturating_sub(1);
    for column in 0..reader.columns().len() {
        for row in [0, 1, 255, 256, last] {
            text.clear();
            let kind = reader.columns()[column].kind;
            match reader.cell(&mut source, column, row, 2_000, &mut text) {
                Ok(value) => text::write_value(kind, value, &mut text),
                Err(error) => failed = Some(error),
            }
        }
        for group in 0..reader.directory().groups().min(16) {
            match reader.block(&mut source, column, group, &mut buffer) {
                Ok(()) => {
                    for row in 0..buffer.rows() {
                        let _ = (
                            buffer.is_blank(row),
                            buffer.int(row),
                            buffer.float(row),
                            buffer.text(row).len(),
                            buffer.code(row),
                        );
                    }
                }
                Err(error) => failed = Some(error),
            }
        }
        loop {
            match reader.load_dictionary(&mut source, column) {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
    }
    failed.map_or(Ok(()), Err)
}

#[test]
fn every_damage_to_the_data_is_noticed() {
    let mut rng = Rng::from_seed(21);
    let file = write(&columns(&mut rng, 600), 256);
    exercise(&file).unwrap();
    let directory_at = open(&file).directory().data_len as usize;
    for _ in 0..3_000 {
        let mut damaged = file.clone();
        let at = rng.next_below(directory_at as u64) as usize;
        damaged[at] ^= 1 << rng.next_below(8);
        // The header is compared, everything else has a checksum. The bytes
        // between the last block and the end of its page may never be read.
        let noticed = exercise(&damaged).is_err();
        let padding = at + PAGE_BYTES as usize >= directory_at;
        assert!(noticed || padding, "a flipped bit of byte {at}");
    }
    for len in [0, 1, 63, 64, 79, 80, file.len() / 2, file.len() - 1] {
        assert!(exercise(&file[..len]).is_err());
    }
}

#[test]
fn bytes_that_claim_to_be_a_table_are_survived() {
    // With the checksums made right again, damage reaches the code behind them.
    let mut rng = Rng::from_seed(22);
    let file = write(&columns(&mut rng, 600), 256);
    let data_len = open(&file).directory().data_len as usize;
    for round in 0..4_000 {
        let mut damaged = file.clone();
        let directory_only = round % 2 == 0;
        for _ in 0..=rng.next_below(6) {
            let (from, to) = if directory_only {
                (data_len, file.len() - 64)
            } else {
                (16, data_len)
            };
            let at = from + rng.next_below((to - from) as u64) as usize;
            damaged[at] = rng.next_u64().to_le_bytes()[0];
        }
        reseal(&mut damaged, data_len);
        let _ = exercise(&damaged);
    }
}

/// Makes the checksums of a file right.
pub(crate) fn reseal(file: &mut [u8], data_len: usize) {
    use sha2::{Digest, Sha256};
    let end = file.len() - 64;
    let pages = data_len / PAGE_BYTES as usize;
    for page in 0..pages {
        let sum = crate::hash::checksum(&file[page * 4_096..(page + 1) * 4_096]);
        let at = end - (pages - page) * 4;
        file[at..at + 4].copy_from_slice(&sum.to_le_bytes());
    }
    let sha256: [u8; 32] = Sha256::digest(&file[data_len..end]).into();
    file[end + 16..end + 48].copy_from_slice(&sha256);
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn a_full_cache_gives_the_page_that_was_asked_for() {
    // As many pages as there is room for, all but one of them asked for
    // twice, then a page whose neighbours come with it.
    let rows = 9_000 * 512;
    let column = ModelColumn {
        name: String::from("a"),
        kind: Some(ColumnType::Float),
        dictionary: false,
        cells: (0..rows).map(|row| Model::Float(f64::from(row))).collect(),
    };
    let file = write(&[column], 65_536);
    for untouched in [0_u32, 1, 2, 3, 4_095, 8_190, 8_191] {
        let mut reader = open(&file);
        let mut source = file.as_slice();
        // The first page begins with the header.
        let row = |page: u32| page * 512 + 2;
        for page in 0..8_192 {
            let times = if page == untouched { 1 } else { 2 };
            for _ in 0..times {
                read(&mut reader, &mut source, 0, row(page));
            }
        }
        for page in (8_192..8_800).chain(0..600) {
            assert_eq!(
                read(&mut reader, &mut source, 0, row(page)),
                Model::Float(f64::from(row(page))),
                "page {page}"
            );
        }
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn one_large_dictionary_leaves_the_others_open() {
    let spec = |name: &str| ColumnSpec {
        name: name.to_string(),
        kind: Some(ColumnType::Text),
        dictionary: true,
    };
    let mut writer = TableWriter::new(vec![spec("long"), spec("few")], 4_096).unwrap();
    let mut file = Vec::new();
    let rows = 100_000_u32;
    for row in 0..rows {
        let long = format!("{row} {}", "x".repeat(2_000));
        writer.columns()[0].push_text(&long);
        writer.columns()[1].push_text(["a", "b", "c"][row as usize % 3]);
        if writer.check_group().unwrap() {
            writer.seal_group().unwrap();
            while writer.step(&mut file).unwrap() {}
        }
    }
    writer.seal_group().unwrap();
    while writer.step(&mut file).unwrap() {}
    writer.finish();
    while writer.step(&mut file).unwrap() {}
    let mut reader = open(&file);
    let mut source = file.as_slice();
    let groups = reader.directory().groups();
    let encodings = |column: usize| {
        let directory = reader.directory();
        (0..groups)
            .map(|group| directory.block(column, group).unwrap().encoding)
            .collect::<Vec<Encoding>>()
    };
    assert!(encodings(0).contains(&Encoding::Text));
    assert!(encodings(0).contains(&Encoding::Packed));
    assert_eq!(encodings(1), vec![Encoding::Packed; groups as usize]);
    let place = reader.columns()[0].dictionary.unwrap();
    assert!(place.bytes_len <= 128 << 20, "{}", place.bytes_len);
    for row in [0, 4_095, 4_096, 60_000, 60_001, rows - 1] {
        assert_eq!(
            read(&mut reader, &mut source, 0, row),
            Model::Text(format!("{row} {}", "x".repeat(2_000)))
        );
        assert_eq!(
            read(&mut reader, &mut source, 1, row),
            Model::Text(["a", "b", "c"][row as usize % 3].to_string())
        );
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn texts_that_are_hard_to_tell_apart_do_not_stop_the_writer() {
    let texts = crate::hash::alike(3_000);
    let cells: Vec<Model> = (0..20_000_usize)
        .map(|row| match row % 3 {
            0 => Model::Text(String::from("often")),
            1 => Model::Text(texts[row * 7 % texts.len()].clone()),
            _ => Model::Text(texts[row % 20].clone()),
        })
        .collect();
    let columns = [ModelColumn {
        name: String::from("a"),
        kind: Some(ColumnType::Text),
        dictionary: true,
        cells,
    }];
    let file = write(&columns, 4_096);
    let mut reader = open(&file);
    let mut source = file.as_slice();
    // The dictionary of the first group is the last.
    assert_eq!(
        reader.directory().block(0, 0).unwrap().encoding,
        Encoding::Packed
    );
    assert_eq!(
        reader.directory().block(0, 1).unwrap().encoding,
        Encoding::Text
    );
    for (row, cell) in columns[0].cells.iter().enumerate() {
        assert_eq!(read(&mut reader, &mut source, 0, row as u32), *cell);
    }
}

#[test]
fn a_dictionary_that_is_damaged_is_so_every_time() {
    let columns = [ModelColumn {
        name: String::from("a"),
        kind: Some(ColumnType::Text),
        dictionary: true,
        cells: ["alpha", "beta", "gamma", "delta"]
            .iter()
            .map(|text| Model::Text((*text).to_string()))
            .collect(),
    }];
    let mut file = write(&columns, 256);
    let data_len = open(&file).directory().data_len as usize;
    let place = open(&file).columns()[0].dictionary.unwrap();
    // Ends that do not rise.
    let mut ends = Vec::new();
    crate::bits::pack(
        [5_u64, 3, 14, place.bytes_len],
        u32::from(place.ends_width),
        &mut ends,
    );
    let at = place.ends_at as usize;
    file[at..at + ends.len()].copy_from_slice(&ends);
    reseal(&mut file, data_len);
    let mut reader = open(&file);
    let mut source = file.as_slice();
    for _ in 0..3 {
        let mut text = String::new();
        assert!(
            reader.cell(&mut source, 0, 0, 100, &mut text).is_err(),
            "{text}"
        );
        assert!(matches!(
            (0..9)
                .map(|_| reader.load_dictionary(&mut source, 0))
                .find(|result| *result != Ok(false)),
            Some(Err(ReadError::Corrupt(_)))
        ));
        assert!(reader.dictionary(0).is_none());
    }
}

#[test]
fn a_dictionary_has_no_more_entries_than_there_can_be() {
    let columns = [ModelColumn {
        name: String::from("a"),
        kind: Some(ColumnType::Text),
        dictionary: true,
        cells: vec![
            Model::Text(String::from("x")),
            Model::Text(String::from("y")),
        ],
    }];
    let file = write(&columns, 256);
    let directory = open(&file).directory().clone();
    for (entries, bytes_len) in [(3, 3), (4_194_304, 0), (2, 1)] {
        let mut claims = directory.clone();
        let place = claims.columns[0].dictionary.as_mut().unwrap();
        place.entries = entries;
        place.bytes_len = bytes_len;
        place.ends_width = 2;
        place.ends_at = 16;
        place.bytes_at = 16;
        let mut bytes = Vec::new();
        claims.encode(&mut bytes);
        assert!(crate::layout::Directory::decode(&bytes, claims.data_len).is_err());
    }
}

#[test]
fn a_text_that_is_cut_in_the_middle_of_a_character_counts_as_cut() {
    let text = format!("{}😀tail", "漢".repeat(512));
    let file = write(
        &[ModelColumn {
            name: String::from("a"),
            kind: Some(ColumnType::Text),
            dictionary: false,
            cells: vec![Model::Text(text)],
        }],
        256,
    );
    let mut reader = open(&file);
    let mut source = file.as_slice();
    let mut scratch = String::new();
    let frame = PageFrame {
        sheet: 0,
        first_row: 0,
        first_col: 0,
        epoch: 0,
    };
    let encoded = page::encode(&mut reader, &mut source, frame, &[0], &mut scratch).unwrap();
    // One cell: the header, two identifiers, then its record.
    let flags = u16::from_le_bytes([encoded[32 + 16 + 8], encoded[32 + 16 + 9]]);
    assert_eq!(flags & 16, 16);
}

#[test]
fn rows_that_were_not_sealed_are_not_lost_silently() {
    let spec = || {
        vec![ColumnSpec {
            name: String::from("a"),
            kind: Some(ColumnType::Integer),
            dictionary: false,
        }]
    };
    let mut file = Vec::new();
    let mut writer = TableWriter::new(spec(), 256).unwrap();
    writer.columns()[0].push_int(1);
    writer.finish();
    assert_eq!(writer.step(&mut file), Err(WriteError::Uneven));
    let mut writer = TableWriter::new(spec(), 256).unwrap();
    writer.columns()[0].push_int(1);
    writer.seal_group().unwrap();
    writer.columns()[0].push_int(2);
    assert_eq!(writer.step(&mut file), Err(WriteError::Uneven));
}

/// A file of which only some pieces are here.
struct Partial<'a> {
    file: &'a [u8],
    piece: u64,
    here: Vec<bool>,
    reads: usize,
}

impl Source for Partial<'_> {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        self.reads += 1;
        let end = at + into.len() as u64;
        for piece in at / self.piece..end.div_ceil(self.piece) {
            if !self.here[piece as usize] {
                return Err(SourceError::Missing);
            }
        }
        (&*self.file).read(at, into)
    }
}

#[test]
fn a_file_that_is_still_coming_says_what_it_needs() {
    let mut rng = Rng::from_seed(31);
    let columns = columns(&mut rng, 2_000);
    let file = write(&columns, 256);
    let piece = 8_192;
    let mut source = Partial {
        file: &file,
        piece,
        here: vec![false; file.len().div_ceil(piece as usize)],
        reads: 0,
    };
    let mut reader = loop {
        match Reader::open(&mut source, file.len() as u64).unwrap() {
            Opened::Ready(reader) => break *reader,
            Opened::Need(range) => {
                for piece in range.at / piece..range.end().div_ceil(piece) {
                    source.here[piece as usize] = true;
                }
            }
        }
    };
    let rows: Vec<u32> = (0..64).map(|row| row * 31).collect();
    let mut scratch = String::new();
    let frame = PageFrame {
        sheet: 0,
        first_row: 0,
        first_col: 0,
        epoch: 1,
    };
    let mut rounds = 0;
    let encoded = loop {
        match page::encode(&mut reader, &mut source, frame, &rows, &mut scratch) {
            Ok(encoded) => break encoded,
            Err(ReadError::Missing) => {
                rounds += 1;
                let missing = reader.take_missing();
                assert!(!missing.is_empty());
                for pair in missing.windows(2) {
                    assert!(pair[0].end() < pair[1].at);
                }
                for range in missing {
                    for piece in range.at / piece..range.end().div_ceil(piece) {
                        source.here[piece as usize] = true;
                    }
                }
            }
            Err(error) => panic!("{error:?}"),
        }
    };
    // Where a text lies is known only after its code, so more than one round
    // is needed, but not one for every cell.
    assert!(rounds <= 5, "{rounds} rounds");
    let mut whole = file.as_slice();
    let mut all = open(&file);
    assert_eq!(
        page::encode(&mut all, &mut whole, frame, &rows, &mut scratch),
        Ok(encoded)
    );
}

#[test]
fn a_page_shows_the_cells() {
    let mut rng = Rng::from_seed(41);
    let columns = columns(&mut rng, 500);
    let file = write(&columns, 256);
    let mut reader = open(&file);
    let mut source = file.as_slice();
    let mut scratch = String::new();
    let rows = [499, 0, 256, 255];
    let frame = PageFrame {
        sheet: 7,
        first_row: 128,
        first_col: 0,
        epoch: 3,
    };
    let encoded = page::encode(&mut reader, &mut source, frame, &rows, &mut scratch).unwrap();
    let mut expected_text = String::new();
    for row in rows {
        for column in &columns {
            let kind = column.kind.unwrap_or(ColumnType::Text);
            match expected(&column.cells[row as usize]) {
                Model::Blank => {}
                Model::Int(value) => {
                    text::write_value(kind, CellValue::Int(value), &mut expected_text);
                }
                Model::Float(value) => {
                    text::write_value(kind, CellValue::Float(value), &mut expected_text);
                }
                Model::Text(value) => expected_text.push_str(&value),
            }
        }
    }
    assert!(encoded.ends_with(expected_text.as_bytes()));
    // Beyond the last column, and without rows, there is an empty page.
    let beyond = PageFrame {
        first_col: 16,
        ..frame
    };
    let empty = page::encode(&mut reader, &mut source, beyond, &rows, &mut scratch).unwrap();
    assert_eq!(empty.len(), 32);
    let none = page::encode(&mut reader, &mut source, frame, &[], &mut scratch).unwrap();
    assert_eq!(none.len(), 32);
}

#[test]
fn a_long_text_is_cut_in_a_page_and_whole_in_a_cell() {
    let long: String = "漢".repeat(3_000);
    let columns = [ModelColumn {
        name: String::from("a"),
        kind: Some(ColumnType::Text),
        dictionary: false,
        cells: vec![Model::Text(long.clone())],
    }];
    let file = write(&columns, 256);
    let mut reader = open(&file);
    let mut source = file.as_slice();
    assert_eq!(read(&mut reader, &mut source, 0, 0), Model::Text(long));
    let mut scratch = String::new();
    let frame = PageFrame {
        sheet: 0,
        first_row: 0,
        first_col: 0,
        epoch: 0,
    };
    page::encode(&mut reader, &mut source, frame, &[0], &mut scratch).unwrap();
    assert_eq!(scratch.chars().count(), 512);
}

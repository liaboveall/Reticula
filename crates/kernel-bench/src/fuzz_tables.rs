//! Fuzz harnesses of the table engine: files that claim to be tables, Parquet
//! files, and delimited text.
//!
//! Bytes that begin like a file are tried as they are, and with checksums that
//! are made right. Others say how to damage a file that was right, or what
//! directory to make up: what lies behind the checks is not reached otherwise.

use std::path::Path;
use std::sync::OnceLock;

use arbitrary::Unstructured;
use reticula_core::rng::Rng;
use reticula_core::viewport::PageFrame;
use reticula_table::bits::width_of;
use reticula_table::csv::{self, HEAD_BYTES};
use reticula_table::layout::{
    Block, Column as FileColumn, DictionaryPlace, Directory, Encoding, MAGIC, PAGE_BYTES, Trailer,
    header,
};
use reticula_table::parquet;
use reticula_table::query::{Condition, QueryError, Session, Spec};
use reticula_table::reader::{BlockBuffer, Reader};
use reticula_table::source::Source;
use reticula_table::text::write_value;
use reticula_table::types::ColumnType;

use crate::tables::{
    Cell, Column, Pieces, Sampler, import_csv, import_csv_whole, import_parquet, open, read_cell,
    reseal, rows_of, run, some_condition, some_order, some_part, write,
};

/// What a session may use: a directory of a few bytes can claim many rows.
const MEMORY: usize = 16 << 20;

/// Arbitrary bytes opened as a table, and everything asked of it.
pub fn fuzz_one_table(data: &[u8]) {
    ask_everything(data, seed_of(data));
    if data.starts_with(&MAGIC) {
        if let Some(file) = resealed(data) {
            ask_everything(&file, seed_of(data));
        }
        return;
    }
    let mut u = Unstructured::new(data);
    let file = if u.ratio(1, 3).unwrap_or(false) {
        made_up(&mut u)
    } else {
        damaged(&mut u)
    };
    if let Ok(file) = file {
        ask_everything(&file, seed_of(data));
    }
}

/// A number for what is asked of a file, which the file says nothing about.
fn seed_of(data: &[u8]) -> u64 {
    reticula_table::hash::of_text(data)
}

/// The same bytes with an end that fits them: the lengths, the checksums of
/// the pages and that of the directory.
fn resealed(data: &[u8]) -> Option<Vec<u8>> {
    let end = data.len().checked_sub(64)?;
    let said = data[end..end + 8].try_into().map_or(0, u64::from_le_bytes);
    let data_len = usize::try_from(said).ok().filter(|said| {
        said.is_multiple_of(PAGE_BYTES as usize)
            && *said > 0
            && said.saturating_add(said / PAGE_BYTES as usize * 4) <= end
    })?;
    let mut file = data[..end].to_vec();
    let trailer = Trailer {
        directory_at: data_len as u64,
        directory_len: (end - data_len) as u64,
        directory_sha256: [0; 32],
    };
    trailer.encode(&mut file);
    reseal(&mut file, data_len);
    Some(file)
}

/// Tables that are right, with blocks of all kinds.
pub fn table_templates() -> &'static [Vec<u8>] {
    static FILES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    FILES.get_or_init(|| {
        let mut rng = Rng::from_seed(0xF022_7AB1);
        let mut files: Vec<Vec<u8>> = [1, 300, 700]
            .iter()
            .map(|rows| write(&columns(&mut rng, *rows), 256))
            .collect();
        // Texts of a dictionary in the first group, and others after it.
        let mixed = some_texts(&mut rng, 600);
        let pair = [true, false].map(|dictionary| Column {
            name: "mixed".to_owned(),
            kind: Some(ColumnType::Text),
            dictionary,
            cells: mixed.clone(),
        });
        files.push(mix(&write(&pair, 256)));
        files
    })
}

fn some_texts(rng: &mut Rng, rows: usize) -> Vec<Cell> {
    const TEXTS: [&str; 8] = [
        "",
        "North",
        "NORTH",
        "customer-1",
        "漢字",
        "ΟΔΟΣ",
        "a\0",
        "é",
    ];
    (0..rows)
        .map(|_| match rng.next_below(4) {
            0 => Cell::Text(format!("{:x}", rng.next_below(1_000))),
            1 => Cell::Blank,
            _ => Cell::Text(TEXTS[rng.next_below(8) as usize].to_owned()),
        })
        .collect()
}

fn columns(rng: &mut Rng, rows: usize) -> Vec<Column> {
    let ints = |rng: &mut Rng, below: u64, factor: i64| -> Vec<Cell> {
        (0..rows)
            .map(|_| match rng.next_below(9) {
                0 => Cell::Blank,
                _ => Cell::Int((rng.next_below(below) as i64 - 5).wrapping_mul(factor)),
            })
            .collect()
    };
    let decimal = ColumnType::Decimal {
        scale: 2,
        fixed: true,
    };
    let mut all = vec![
        (Some(ColumnType::Integer), ints(rng, 1 << 40, 1)),
        (Some(ColumnType::Integer), ints(rng, u64::MAX, 1)),
        (Some(decimal), ints(rng, 1_000, 100)),
        (Some(ColumnType::Date), ints(rng, 20_000, 1)),
        (Some(ColumnType::DateTime), ints(rng, 1 << 30, 1_000_000)),
        (
            Some(ColumnType::Boolean),
            (0..rows)
                .map(|_| Cell::Int(rng.next_below(2) as i64))
                .collect(),
        ),
        (
            Some(ColumnType::Float),
            (0..rows)
                .map(|_| Cell::Float(rng.next_f64() - 0.5))
                .collect(),
        ),
        (None, vec![Cell::Blank; rows]),
    ];
    all.push((Some(ColumnType::Text), some_texts(rng, rows)));
    all.push((Some(ColumnType::Text), some_texts(rng, rows)));
    all.into_iter()
        .enumerate()
        .map(|(index, (kind, cells))| Column {
            name: format!("column {index}"),
            kind,
            dictionary: index != 9,
            cells,
        })
        .collect()
}

/// Makes one column of two with the same texts: the first group of the one in
/// the dictionary, the other groups of the one without.
fn mix(file: &[u8]) -> Vec<u8> {
    let reader = open(file).expect("a template is a table");
    let old = reader.directory();
    let mut directory = Directory {
        blocks: (0..old.groups())
            .map(|group| *old.block(usize::from(group > 0), group).expect("a block"))
            .collect(),
        ..old.clone()
    };
    directory.columns.truncate(1);
    seal(&file[..old.data_len as usize], &directory)
}

/// A file of some data and a directory, whose checksums are right.
fn seal(data: &[u8], directory: &Directory) -> Vec<u8> {
    let mut encoded = Vec::new();
    directory.encode(&mut encoded);
    let trailer = Trailer {
        directory_at: directory.data_len,
        directory_len: encoded.len() as u64,
        directory_sha256: [0; 32],
    };
    trailer.encode(&mut encoded);
    let mut out = data.to_vec();
    out.extend_from_slice(&encoded);
    reseal(&mut out, directory.data_len as usize);
    out
}

/// A file that was right, some bytes of which are others.
fn damaged(u: &mut Unstructured<'_>) -> arbitrary::Result<Vec<u8>> {
    let templates = table_templates();
    let mut file = u.choose(templates)?.clone();
    let end = file.len() - 64;
    let data_len = open(&file).map_or(0, |reader| reader.directory().data_len as usize);
    let mut directory_len = end - data_len;
    for _ in 0..*u.choose(&[1_u8, 1, 1, 2, 4])? {
        let (from, len) = if u.arbitrary()? {
            (data_len, directory_len)
        } else {
            (16, data_len - 16)
        };
        let at = from + u.int_in_range(0..=len - 1)?;
        match u.int_in_range(0_u8..=3)? {
            0 => file[at] ^= 1 << u.int_in_range(0..=7)?,
            1 => file[at] = *u.choose(&[0, 1, 2, 3, 63, 64, 65, 127, 128, 255])?,
            2 => {
                let bytes = u.arbitrary::<[u8; 8]>()?;
                for (slot, byte) in file[at..from + len].iter_mut().zip(bytes) {
                    *slot = byte;
                }
            }
            _ => file[at] = u.arbitrary()?,
        }
    }
    if u.ratio(1, 16)? {
        // A directory that is too short or too long for what it says.
        let cut = u.int_in_range(0..=directory_len)?;
        let trailer = file.split_off(end);
        if u.arbitrary()? {
            file.truncate(data_len + cut);
        } else {
            file.extend_from_slice(u.bytes(u.len().min(cut))?);
        }
        directory_len = file.len() - data_len;
        let of = Trailer {
            directory_at: data_len as u64,
            directory_len: directory_len as u64,
            directory_sha256: [0; 32],
        };
        let mut encoded = Vec::new();
        of.encode(&mut encoded);
        debug_assert_eq!(encoded.len(), trailer.len());
        file.extend_from_slice(&encoded);
        if directory_len < data_len / PAGE_BYTES as usize * 4 {
            return Ok(file);
        }
    }
    reseal(&mut file, data_len);
    Ok(file)
}

/// Some data, and a directory that says what it might be.
fn made_up(u: &mut Unstructured<'_>) -> arbitrary::Result<Vec<u8>> {
    let pages = *u.choose(&[2_usize, 2, 3, 9])?;
    let data_len = pages * PAGE_BYTES as usize;
    let mut data = header().to_vec();
    let given = u.bytes(u.len() / 2)?;
    data.extend(given.iter().copied().cycle().take(if given.is_empty() {
        0
    } else {
        data_len - 16
    }));
    data.resize(data_len, 0);
    let group_rows = *u.choose(&[256_u32, 256, 512, 65_536])?;
    let rows = match u.int_in_range(0_u8..=5)? {
        0 => u.int_in_range(0..=3)?,
        1 => group_rows + u.int_in_range(0..=1)?,
        // Too many to have values of eight bytes.
        2 => u.int_in_range(0..=70_000)?,
        _ => u.int_in_range(1..=600)?,
    };
    let mut directory = Directory {
        group_rows,
        rows,
        columns: Vec::new(),
        blocks: Vec::new(),
        sums: vec![0; pages],
        data_len: data_len as u64,
    };
    if directory.groups() > 64 {
        directory.rows = 600;
    }
    let place = |u: &mut Unstructured<'_>, len: u64| {
        let last = (data_len as u64).saturating_sub(len).max(16);
        u.int_in_range(16..=last)
    };
    for number in 0..u.int_in_range(1_u8..=3)? {
        let kind = *u.choose(&[
            ColumnType::Integer,
            ColumnType::Decimal {
                scale: 18,
                fixed: false,
            },
            ColumnType::Float,
            ColumnType::Date,
            ColumnType::DateTime,
            ColumnType::Boolean,
            ColumnType::Text,
            ColumnType::Text,
        ])?;
        let dictionary = if kind == ColumnType::Text && u.arbitrary()? {
            let entries = u.int_in_range(1_u32..=300)?;
            let bytes_len = u.int_in_range(0_u64..=600)?;
            let least = width_of(bytes_len) as u8;
            let ends_width = u.int_in_range(least..=12)?;
            let mut dictionary = DictionaryPlace {
                entries,
                ends_width,
                ends_at: 16,
                bytes_at: place(u, bytes_len)?,
                bytes_len,
            };
            dictionary.ends_at = place(u, dictionary.ends_len())?;
            Some(dictionary)
        } else {
            None
        };
        directory.columns.push(FileColumn {
            name: format!("{number}"),
            kind,
            blanks: 0,
            dictionary,
        });
    }
    for group in 0..directory.groups() {
        let rows = directory.rows_in(group);
        // As wide as there is room for.
        let widest = ((data_len as u64 - 16) / 8 * 64 / u64::from(rows.max(1))).min(64) as u8;
        for index in 0..directory.columns.len() {
            let column = &directory.columns[index];
            let nulls_at = place(u, u64::from(rows).div_ceil(64) * 8)?;
            let mut block = Block {
                low: u.arbitrary()?,
                high: u.arbitrary()?,
                unit: *u.choose(&[0, 0, 1, 9, 18])?,
                width: u.int_in_range(0..=widest)?,
                blanks: 0,
                nulls_at: 0,
                ..Block::BLANK
            };
            if block.low.cast_signed() > block.high.cast_signed() {
                std::mem::swap(&mut block.low, &mut block.high);
            }
            if u.arbitrary()? && rows > 1 {
                block.blanks = u.int_in_range(1..=rows - 1)?;
                block.nulls_at = nulls_at;
            }
            let packed = |width: u8| (u64::from(rows) * u64::from(width)).div_ceil(64) * 8;
            let len = match column.kind {
                _ if u.ratio(1, 6)? => {
                    block = Block {
                        blanks: rows,
                        ..Block::BLANK
                    };
                    0
                }
                ColumnType::Float if widest == 64 => {
                    block.encoding = Encoding::Raw;
                    (block.width, block.unit) = (0, 0);
                    (block.low, block.high) = (0, f64::MAX.to_bits());
                    u64::from(rows) * 8
                }
                ColumnType::Text if column.dictionary.is_none() || u.arbitrary()? => {
                    block.encoding = Encoding::Text;
                    block.width = u.int_in_range(0..=widest.min(16))?;
                    block.unit = 0;
                    let room = (data_len as u64 - 16).saturating_sub(packed(block.width));
                    packed(block.width) + u.int_in_range(0..=room.min(600))?
                }
                ColumnType::Float => {
                    block = Block {
                        blanks: rows,
                        ..Block::BLANK
                    };
                    0
                }
                _ => {
                    block.encoding = Encoding::Packed;
                    if column.dictionary.is_some() {
                        (block.low, block.high) = (0, 300);
                        (block.unit, block.width) = (0, block.width.min(9));
                    }
                    packed(block.width)
                }
            };
            block.len = len as u32;
            if block.encoding != Encoding::Blank {
                block.at = place(u, len)?;
            }
            directory.blocks.push(block);
        }
    }
    let width = directory.columns.len();
    for index in 0..width {
        let blanks = directory.blocks.iter().skip(index).step_by(width);
        directory.columns[index].blanks = blanks.map(|block| u64::from(block.blanks)).sum();
    }
    Ok(seal(&data, &directory))
}

/// Values of questions, as far as the cells can be read.
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
        read_cell(&mut self.reader, self.file, column, row).unwrap_or(Cell::Blank)
    }
}

/// Opens a file that may be damaged, and does everything that can be done with it.
fn ask_everything(file: &[u8], seed: u64) {
    let Ok(mut reader) = open(file) else {
        return;
    };
    let mut source = file;
    let width = reader.columns().len();
    let kinds: Vec<ColumnType> = reader.columns().iter().map(|column| column.kind).collect();
    let last = reader.rows().saturating_sub(1);
    let mut text = String::new();
    let mut buffer = BlockBuffer::default();
    for (column, kind) in kinds.iter().enumerate() {
        for row in [0, 1, 255, 256, last, last.saturating_add(1)] {
            text.clear();
            if let Ok(value) = reader.cell(&mut source, column, row, 2_000, &mut text) {
                write_value(*kind, value, &mut text);
            }
        }
        for group in (0..reader.directory().groups()).take(8) {
            if reader
                .block(&mut source, column, group, &mut buffer)
                .is_ok()
            {
                for row in 0..buffer.rows() {
                    let _ = (buffer.is_blank(row), buffer.int(row), buffer.float(row));
                    let _ = (buffer.text(row).len(), buffer.code(row));
                }
            }
        }
        while let Ok(false) = reader.load_dictionary(&mut source, column) {}
    }
    let mut rng = Rng::from_seed(seed);
    let mut from = FromFile {
        file,
        reader,
        kinds,
    };
    if rng.next_below(4) == 0 {
        // A file that is still being fetched.
        let Some(session) = open(file).ok().map(Session::new) else {
            return;
        };
        let mut source = Pieces::new(file, 4_096);
        ask(
            session,
            &mut source,
            &mut from,
            width,
            &mut rng,
            &mut |source, ranges| {
                source.fetch(ranges);
            },
        );
    } else if let Ok(reader) = open(file) {
        let session = Session::new(reader);
        ask(
            session,
            &mut source,
            &mut from,
            width,
            &mut rng,
            &mut |_, ranges| {
                panic!("all of the file is here, but not {ranges:?}");
            },
        );
    }
}

fn ask<S: Source>(
    mut session: Session,
    source: &mut S,
    from: &mut FromFile<'_>,
    width: usize,
    rng: &mut Rng,
    need: &mut dyn FnMut(&mut S, &[reticula_table::source::ByteRange]),
) {
    session.set_memory_limit(MEMORY);
    let rows = session.reader().rows();
    for _ in 0..3 {
        let column = rng.next_below(width as u64) as usize;
        let filters = (0..rng.next_below(3))
            .map(|_| {
                let column = rng.next_below(width as u64) as usize;
                let condition = match some_condition(rng, from, column) {
                    // Nothing is sorted for ever.
                    Condition::OneOf { mut values } => {
                        values.truncate(8);
                        Condition::OneOf { values }
                    }
                    other => other,
                };
                (column, condition)
            })
            .collect();
        let spec = Spec {
            filters,
            order: some_order(rng, width),
        };
        let before = session.view_len();
        session
            .begin(spec)
            .expect("the condition is one of the column");
        session
            .begin_summary(vec![column])
            .expect("the column is there");
        let search = some_part(rng, from, column);
        session
            .begin_values(column, search, 10)
            .expect("the column is there");
        let mut steps = rng.clone();
        let mut budget = move || [1, 500, u32::MAX][steps.next_below(3) as usize];
        // A job that fails is given up, the others are worked on.
        for _ in 0..3 {
            match run(&mut session, source, &mut budget, need) {
                Ok(()) => break,
                Err(QueryError::OutOfMemory | QueryError::TooManyEqualPrefixes) => {
                    assert!(session.view_len() <= rows.max(before));
                }
                Err(QueryError::Read(_)) => {}
            }
        }
        assert!(
            session.running().is_none(),
            "every job ends, one way or another"
        );
        let _ = (session.take_summary(), session.take_values());
        if session.view_len() <= 100_000 {
            let mut shown = rows_of(&session);
            shown.sort_unstable();
            assert!(shown.is_sorted_by(|a, b| a < b), "a row is shown twice");
            assert!(
                shown.last().is_none_or(|last| *last < rows),
                "a row that is not there"
            );
        }
        let mut scratch = String::new();
        let frame = PageFrame {
            sheet: 0,
            first_row: session.view_len().saturating_sub(64) & !63,
            first_col: 0,
            epoch: 1,
        };
        for _ in 0..8 {
            match session.page(source, frame, &mut scratch) {
                Err(reticula_table::reader::ReadError::Missing) => {
                    let missing = session.take_missing();
                    need(source, &missing);
                }
                _ => break,
            }
        }
    }
}

/// The Parquet files that others wrote, the smaller ones.
pub fn parquet_templates() -> &'static [Vec<u8>] {
    static FILES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    FILES.get_or_init(|| {
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance/parquet");
        let mut paths: Vec<_> = std::fs::read_dir(&directory)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        paths.sort();
        paths
            .into_iter()
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "parquet")
            })
            .filter_map(|path| std::fs::read(path).ok())
            .filter(|file| file.len() <= 32 << 10)
            .collect()
    })
}

/// Arbitrary bytes, and Parquet files that were right, imported.
pub fn fuzz_one_parquet(data: &[u8]) {
    import_arbitrary(data, [0, 0, 1_000, 65_536][data.len() % 4]);
    if data.starts_with(b"PAR1") {
        return;
    }
    let mut u = Unstructured::new(data);
    if let Ok((file, piece)) = damaged_parquet(&mut u) {
        import_arbitrary(&file, piece);
    }
}

fn damaged_parquet(u: &mut Unstructured<'_>) -> arbitrary::Result<(Vec<u8>, u64)> {
    let mut file = u.choose(parquet_templates())?.clone();
    let end = file.len() - 8;
    let len = file[end..end + 4].try_into().map_or(0, u32::from_le_bytes) as usize;
    let metadata = end.saturating_sub(len);
    for _ in 0..u.int_in_range(1_u8..=5)? {
        let at = match u.int_in_range(0_u8..=3)? {
            0 => u.int_in_range(4..=metadata.max(5) - 1)?,
            // Where a page begins, about.
            1 => u.int_in_range(4..=40.min(end - 1))?,
            _ => u.int_in_range(metadata..=end - 1)?,
        };
        match u.int_in_range(0_u8..=5)? {
            0 => file[at] ^= 1 << u.int_in_range(0..=7)?,
            1 => file[at] = *u.choose(&[0, 1, 2, 3, 5, 6, 8, 12, 0x15, 0x7f, 0x80, 0xff])?,
            2 => {
                // A number of many bytes.
                let bytes = [
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    *u.choose(&[0x0f, 0x7f, 0xff, 0x01])?,
                ];
                for (slot, byte) in file[at..end].iter_mut().zip(bytes) {
                    *slot = byte;
                }
            }
            3 => file[at] = file[at].wrapping_add(2),
            4 => file[at] = file[at].wrapping_sub(2),
            _ => file[at] = u.arbitrary()?,
        }
    }
    if u.ratio(1, 20)? {
        let from = u.int_in_range(4..=end - 1)?;
        let to = u.int_in_range(from..=end - 1)?;
        file.drain(from..to);
    }
    let piece = *u.choose(&[0, 0, 1_000, 65_536])?;
    Ok((file, piece))
}

/// More rows than are imported here: a few bytes can stand for many.
const MANY_ROWS: u64 = 1 << 20;

fn import_arbitrary(file: &[u8], piece: u64) {
    let length = file.len() as u64;
    let mut whole = file;
    match parquet::open(&mut whole, length) {
        Ok(parquet::Opened::Ready(opened)) => {
            let _ = opened.columns().map(|column| column.bytes).max();
            if opened.rows() > MANY_ROWS {
                return;
            }
        }
        Ok(parquet::Opened::Need(range)) => panic!("all of the file is here, but not {range:?}"),
        Err(_) => return,
    }
    let imported = if piece == 0 {
        import_parquet(&mut whole, length, &mut |_, ranges| {
            panic!("all of the file is here, but not {ranges:?}");
        })
    } else {
        let mut source = Pieces::new(file, piece);
        import_parquet(&mut source, length, &mut |source, ranges| {
            source.fetch(ranges);
        })
    };
    if let Ok((table, report)) = imported {
        let mut reader = open(&table).expect("what was written is a table");
        assert_eq!(u64::from(reader.rows()), report.rows);
        assert_eq!(reader.columns().len(), report.columns.len());
        let last = reader.rows().saturating_sub(1);
        for (column, reported) in report.columns.iter().enumerate() {
            assert_eq!(reader.columns()[column].blanks, reported.blanks);
            for row in [0, last / 2, last] {
                let cell = read_cell(&mut reader, &table, column, row);
                cell.expect("what was written can be read");
            }
        }
    }
}

/// Bytes imported as delimited text: how they are cut into pieces changes nothing.
pub fn fuzz_one_csv(data: &[u8]) {
    let Some(([flags, pieces, kinds], rest)) = data.split_first_chunk::<3>() else {
        return;
    };
    let kind = |bits: u8| match bits & 7 {
        0 => Some(ColumnType::Integer),
        1 => Some(ColumnType::Decimal {
            scale: 2,
            fixed: true,
        }),
        2 => Some(ColumnType::Float),
        3 => Some(ColumnType::Date),
        4 => Some(ColumnType::DateTime),
        5 => Some(ColumnType::Boolean),
        _ => None,
    };
    let options = csv::Options {
        delimiter: [None, Some(b','), Some(b';'), Some(b'x')][usize::from(flags & 3)],
        header: [None, Some(true), Some(false), None][usize::from(flags >> 2 & 3)],
        blank_markers: flags & 16 != 0,
        kinds: kind(*kinds)
            .into_iter()
            .chain(kind(kinds >> 3))
            .enumerate()
            .collect(),
    };
    let long;
    let text = if flags >> 5 == 7 && pieces & 3 == 0 && !rest.is_empty() {
        // More than is looked at first, and then something else.
        let (first, second) = rest.split_at(rest.len() / 2);
        let first = if first.is_empty() { rest } else { first };
        let times = (HEAD_BYTES / first.len()).min(400_000) + 1;
        long = [&first.repeat(times), second].concat();
        long.as_slice()
    } else {
        rest
    };
    let head = &text[..text.len().min(HEAD_BYTES)];
    let sniffed = csv::sniff(head, head.len() < HEAD_BYTES, &options);
    let whole = import_csv_whole(&options, text);
    let mut rng = Rng::from_seed(u64::from(*pieces));
    let mut piece = || {
        let len = match rng.next_below(4) {
            0 => 1,
            1 => rng.next_below(8),
            2 => rng.next_below(100),
            _ => rng.next_below(300_000),
        };
        (len as usize + 1, rng.next_below(3) == 0)
    };
    if text.len() > 4_096 {
        let mut many = || (piece().0 * 64, piece().1);
        assert_same(&whole, &import_csv(&options, text, &mut many));
    } else {
        assert_same(&whole, &import_csv(&options, text, &mut piece));
    }
    let Ok((file, report)) = whole else {
        return;
    };
    assert!(sniffed.valid_utf8);
    assert_eq!(sniffed.columns.len(), report.columns.len());
    let mut reader = open(&file).expect("what was written is a table");
    assert_eq!(u64::from(reader.rows()), report.rows);
    let last = reader.rows().saturating_sub(1);
    for (column, reported) in report.columns.iter().enumerate() {
        assert_eq!(reader.columns()[column].blanks, reported.blanks);
        assert!(reported.misfits <= reported.blanks);
        for row in [0, 1, last / 2, last] {
            let cell = read_cell(&mut reader, &file, column, row);
            cell.expect("what was written can be read");
        }
    }
}

type Imported = Result<(Vec<u8>, csv::Report), csv::ImportError>;

fn assert_same(one: &Imported, other: &Imported) {
    match (one, other) {
        (Ok(one), Ok(other)) => {
            assert_eq!(one.1, other.1);
            assert!(one.0 == other.0, "other bytes");
        }
        (Err(one), Err(other)) => assert_eq!(one, other),
        _ => panic!(
            "{:?}, {:?}",
            one.as_ref().map(|one| &one.1),
            other.as_ref().map(|other| &other.1)
        ),
    }
}

/// Inputs to begin with.
#[must_use]
pub fn csv_seeds() -> Vec<Vec<u8>> {
    const TEXTS: [&str; 10] = [
        "a,b\n1,2\n3,4\n",
        "name;value\r\n\"x;y\";1.50\r\n\"two\nlines\";-0.25\r\n",
        "\u{feff}when,day,yes\n2024-02-29T12:30:00.123456Z,2024/2/9,true\n2024-02-29 12:30,0001-01-01,FALSE\n",
        "i,j,k\n9223372036854775807,00123,+5\n-9223372036854775808,-0,1e400\n9223372036854775808,1e-5,5e-324\n",
        "x\n\n1\n\n2\n",
        "a,a,a\nNA,N/A,NULL\nNaN,,\"\"\n1\n1,2,3,4\n",
        "t\tu\nİ\tΟΔΟΣ\n漢字\te\u{301}\n",
        "q\n\"never closed\n1\n",
        "d,e\n0.1,0.30000000000000004\n123456789012345678,12345678901234567890.5\n",
        "a|b\r1|2\r3|4",
    ];
    TEXTS
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let mut seed = vec![(index % 4 * 4) as u8, 7, 0xff];
            seed.extend_from_slice(text.as_bytes());
            seed
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick_template(rng: &mut Rng) -> Vec<u8> {
        let templates = table_templates();
        templates[rng.next_below(templates.len() as u64) as usize].clone()
    }

    /// Files that are refused at once would test little.
    #[test]
    fn many_of_the_files_reach_what_lies_behind_the_checks() {
        let mut rng = Rng::from_seed(5);
        let (mut made, mut kept, mut again, mut imported) = (0, 0, 0, 0);
        for _ in 0..1_000 {
            let bytes: Vec<u8> = (0..2_000)
                .map(|_| rng.next_u64().to_le_bytes()[0])
                .collect();
            let opens =
                |file: arbitrary::Result<Vec<u8>>| file.is_ok_and(|file| open(&file).is_ok());
            made += u32::from(opens(made_up(&mut Unstructured::new(&bytes))));
            kept += u32::from(opens(damaged(&mut Unstructured::new(&bytes))));
            let mut file = pick_template(&mut rng);
            let at = rng.next_below(file.len() as u64) as usize;
            file[at] = file[at].wrapping_add(1);
            again += u32::from(resealed(&file).is_some_and(|file| open(&file).is_ok()));
            let (file, _) =
                damaged_parquet(&mut Unstructured::new(&bytes)).expect("there are some");
            let mut source = file.as_slice();
            imported += u32::from(parquet::open(&mut source, file.len() as u64).is_ok());
        }
        assert!(made > 700, "{made}");
        assert!(kept > 300, "{kept}");
        assert!(again > 700, "{again}");
        assert!(imported > 250, "{imported}");
    }
}

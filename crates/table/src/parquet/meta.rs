//! What the metadata at the end of a file says, and at the start of a page.

use alloc::string::String;
use alloc::vec::Vec;

use super::codec::Codec;
use super::thrift::{self, Reader, ThriftError};
use super::value::{self, Logical, Physical, Plan, SkipReason};
use super::{Column, Damage, Limit, ParquetError, ParquetFile, SkippedColumn};
use crate::types::MAX_ROWS;

/// Most pieces of columns, over all groups of rows.
const MAX_CHUNKS: usize = 1 << 21;
/// Most elements of a schema.
const MAX_SCHEMA: usize = 1 << 17;

pub(super) const PLAIN: i64 = 0;
pub(super) const PLAIN_DICTIONARY: i64 = 2;
pub(super) const RLE: i64 = 3;
pub(super) const BIT_PACKED: i64 = 4;
pub(super) const DELTA_BINARY_PACKED: i64 = 5;
pub(super) const DELTA_LENGTH_BYTE_ARRAY: i64 = 6;
pub(super) const DELTA_BYTE_ARRAY: i64 = 7;
pub(super) const RLE_DICTIONARY: i64 = 8;
pub(super) const BYTE_STREAM_SPLIT: i64 = 9;

/// The values of a column in one group of rows.
#[derive(Clone, Copy, Debug)]
pub(super) struct Chunk {
    /// Offset of the first page.
    pub(super) start: u64,
    /// Bytes of the pages, and how many they are unpacked.
    pub(super) len: u64,
    pub(super) unpacked: u64,
    /// Number of values, blank ones too.
    pub(super) values: u64,
    pub(super) codec: Codec,
}

/// Where nothing was said.
const UNSAID: i64 = -64;

/// An element of the schema.
#[derive(Clone, Debug)]
struct Element<'a> {
    /// What it says in numbers, by the number of the field.
    numbers: [i64; 10],
    name: &'a [u8],
    logical: Option<Logical>,
}

impl Element<'_> {
    /// How the values are stored; nothing for an element that only has others
    /// below it.
    const fn physical(&self) -> i64 {
        let [_, physical, ..] = self.numbers;
        physical
    }

    /// How many elements there are right below it.
    fn children(&self) -> i64 {
        let [_, _, _, _, _, children, ..] = self.numbers;
        if self.physical() == UNSAID {
            children.max(0)
        } else {
            0
        }
    }

    fn name(&self) -> String {
        String::from_utf8_lossy(self.name).into_owned()
    }
}

/// Why a file cannot be taken: a [`ParquetError`] that fits into two bytes, and
/// so costs little to hand on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    Encrypted,
    Memory,
    Source,
    Limit(Limit),
    Damaged(Damage),
}

impl From<Fault> for ParquetError {
    fn from(fault: Fault) -> Self {
        fault.at(None)
    }
}

impl Fault {
    /// The error, where the column is known.
    pub(super) const fn at(self, column: Option<u32>) -> ParquetError {
        match self {
            Self::Encrypted => ParquetError::Encrypted,
            Self::Memory => ParquetError::Memory,
            Self::Source => ParquetError::Source,
            Self::Limit(limit) => ParquetError::Limit(limit),
            Self::Damaged(damage) => ParquetError::Damaged { column, damage },
        }
    }
}

/// The metadata contradicts itself.
const BAD: Fault = Fault::Damaged(Damage::Metadata);

/// Reads the metadata of a file. The pages end before `data_end`.
pub(super) fn read_file(bytes: &[u8], data_end: u64) -> Result<ParquetFile, Fault> {
    let mut elements = Vec::new();
    let mut leaves = Vec::new();
    let mut file = ParquetFile {
        rows: 0,
        columns: Vec::new(),
        skipped: Vec::new(),
        data_end,
    };
    match fill_file(bytes, &mut elements, &mut leaves, &mut file) {
        Ok(()) => Ok(file),
        Err(fault) => Err(fault),
    }
}

/// The part of it that can fail, apart from what there is to be dropped.
fn fill_file<'a>(
    bytes: &'a [u8],
    elements: &mut Vec<Element<'a>>,
    leaves: &mut Vec<Option<usize>>,
    file: &mut ParquetFile,
) -> Result<(), Fault> {
    let mut reader = Reader::new(bytes);
    let mut groups = None;
    let mut numbers = [UNSAID; 4];
    let mut last = 0;
    while let Some((field, kind)) = reader.field(&mut last) {
        match (field, kind) {
            (2, thrift::LIST) => read_schema(&mut reader, elements)?,
            (4, thrift::LIST) => {
                groups = Some(reader);
                reader.skip(kind);
            }
            (8, _) => return Err(Fault::Encrypted),
            _ => reader.keep(field, kind, &mut numbers),
        }
    }
    reader.check().map_err(|_| BAD)?;
    let [_, _, _, rows] = numbers;
    file.rows = u64::try_from(rows).map_err(|_| BAD)?;
    if file.rows > MAX_ROWS {
        return Err(Fault::Limit(Limit::Rows));
    }
    read_columns(elements, leaves, file)?;
    let mut groups = groups.ok_or(BAD)?;
    let (count, kind) = groups.list();
    let mut seen = 0_u64;
    let mut chunks = 0_usize;
    for _ in 0..count {
        if kind != thrift::STRUCT {
            return Err(BAD);
        }
        let rows = read_group(&mut groups, leaves, file, &mut chunks)?;
        seen = seen.saturating_add(rows);
    }
    if seen != file.rows {
        return Err(Fault::Damaged(Damage::Count));
    }
    // The ones that can be read go to the front.
    let mut kept = 0;
    for index in 0..file.columns.len() {
        let Some(column) = file.columns.get_mut(index) else {
            break;
        };
        if let Some(reason) = column.unreadable {
            let name = core::mem::take(&mut column.name);
            file.skipped.push(SkippedColumn { name, reason });
        } else {
            if kept < index {
                file.columns.swap(kept, index);
            }
            kept = kept.saturating_add(1);
        }
    }
    file.columns.truncate(kept);
    Ok(())
}

fn read_schema<'a>(reader: &mut Reader<'a>, elements: &mut Vec<Element<'a>>) -> Result<(), Fault> {
    let (count, kind) = reader.list();
    if kind != thrift::STRUCT || count > MAX_SCHEMA {
        return Err(BAD);
    }
    for _ in 0..count {
        let mut element = Element {
            numbers: [UNSAID; 10],
            name: &[],
            logical: None,
        };
        let mut last = 0;
        while let Some((field, kind)) = reader.field(&mut last) {
            match (field, kind) {
                (4, thrift::BINARY) => element.name = reader.binary(),
                (10, thrift::STRUCT) => element.logical = read_logical(reader),
                _ => reader.keep(field, kind, &mut element.numbers),
            }
        }
        reader.check().map_err(|_| BAD)?;
        elements.push(element);
    }
    Ok(())
}

/// What a column says its values mean, which is a structure with one field.
fn read_logical(reader: &mut Reader<'_>) -> Option<Logical> {
    let mut logical = None;
    let mut last = 0;
    while let Some((field, kind)) = reader.field(&mut last) {
        if kind != thrift::STRUCT {
            reader.skip(kind);
            continue;
        }
        let mut numbers = [0; 3];
        let mut unit = None;
        let mut signed = false;
        let mut inner = 0;
        while let Some((field, kind)) = reader.field(&mut inner) {
            match (field, kind) {
                (2, thrift::TRUE) => signed = true,
                // The unit of a time is a structure with one empty field.
                (2, thrift::STRUCT) => {
                    let mut last = 0;
                    while let Some((field, kind)) = reader.field(&mut last) {
                        reader.skip(kind);
                        unit = match field {
                            1 => Some(1_000),
                            2 => Some(1_000_000),
                            3 => Some(1_000_000_000),
                            _ => None,
                        };
                    }
                }
                _ => reader.keep(field, kind, &mut numbers),
            }
        }
        let [_, scale, precision] = numbers;
        logical = Some(match (field, unit) {
            (1 | 4 | 12, _) => Logical::Text,
            (5, _) => Logical::Decimal { scale, precision },
            (6, _) => Logical::Date,
            (7, Some(units)) => Logical::Time(units),
            (8, Some(units)) => Logical::Timestamp(units),
            (10, _) => Logical::Integer { signed },
            (11, _) => Logical::Null,
            (13, _) => Logical::Bson,
            (14, _) => Logical::Uuid,
            (15, _) => Logical::Half,
            (16, _) => Logical::Variant,
            (17 | 18, _) => Logical::Geometry,
            _ => Logical::Other,
        });
    }
    logical
}

/// What the older way of saying it says.
fn converted(element: &Element<'_>) -> Logical {
    let [_, _, _, _, _, _, converted, scale, precision, _] = element.numbers;
    match converted {
        UNSAID => Logical::None,
        0 | 4 | 19 => Logical::Text,
        // Without a scale, there are no decimals.
        5 => Logical::Decimal {
            scale: if scale == UNSAID { 0 } else { scale },
            precision,
        },
        6 => Logical::Date,
        7 => Logical::Time(1_000),
        8 => Logical::Time(1_000_000),
        9 => Logical::Timestamp(1_000),
        10 => Logical::Timestamp(1_000_000),
        11..=14 => Logical::Integer { signed: false },
        15..=18 => Logical::Integer { signed: true },
        20 => Logical::Bson,
        21 => Logical::Interval,
        _ => Logical::Other,
    }
}

/// Finds the columns of the file, and which of them each part is that has
/// values of its own; nothing for one that is left out.
fn read_columns(
    elements: &[Element<'_>],
    leaves: &mut Vec<Option<usize>>,
    file: &mut ParquetFile,
) -> Result<(), Fault> {
    let mut elements = elements.iter();
    let root = elements.next().ok_or(BAD)?;
    for _ in 0..root.children() {
        let element = elements.next().ok_or(BAD)?;
        let mut leaf = None;
        match read_column(element) {
            Ok(column) => {
                leaf = Some(file.columns.len());
                file.columns.push(column);
            }
            Err(reason) => file.skipped.push(SkippedColumn {
                name: element.name(),
                reason,
            }),
        }
        if element.physical() != UNSAID {
            leaves.push(leaf);
        }
        // Everything below it, of which there may be whole trees.
        let mut below = element.children();
        while below > 0 {
            let element = elements.next().ok_or(BAD)?;
            below = below.saturating_sub(1).saturating_add(element.children());
            if element.physical() != UNSAID {
                leaves.push(None);
            }
        }
    }
    match elements.next() {
        Some(_) => Err(BAD),
        None => Ok(()),
    }
}

fn read_column(element: &Element<'_>) -> Result<Column, SkipReason> {
    let [_, physical, len, repetition, ..] = element.numbers;
    let physical = match physical {
        UNSAID if element.logical == Some(Logical::Variant) => return Err(SkipReason::Variant),
        UNSAID => return Err(SkipReason::Nested),
        _ if repetition == 2 => return Err(SkipReason::Nested),
        0 => Physical::Boolean,
        1 => Physical::Int32,
        2 => Physical::Int64,
        3 => Physical::Int96,
        4 => Physical::Float,
        5 => Physical::Double,
        6 => Physical::Bytes,
        7 => match u32::try_from(len) {
            Ok(len @ 1..) => Physical::Fixed(len),
            _ => return Err(SkipReason::InvalidType),
        },
        _ => return Err(SkipReason::UnknownType),
    };
    // What both say is the same, unless the newer way says nothing known.
    let logical = match (element.logical, converted(element)) {
        (None, converted) => converted,
        (Some(Logical::Other), converted) if converted != Logical::None => converted,
        (Some(logical), _) => logical,
    };
    let (kind, plan) = value::plan(physical, logical)?;
    Ok(Column {
        name: element.name(),
        kind,
        plan,
        physical,
        optional: repetition == 1,
        chunks: Vec::new(),
        unreadable: None,
    })
}

/// Reads what is said of a group of rows. Gives the number of rows.
fn read_group(
    reader: &mut Reader<'_>,
    leaves: &[Option<usize>],
    file: &mut ParquetFile,
    chunks: &mut usize,
) -> Result<u64, Fault> {
    let mut columns = None;
    let mut numbers = [UNSAID; 4];
    let mut last = 0;
    while let Some((field, kind)) = reader.field(&mut last) {
        if (field, kind) == (1, thrift::LIST) {
            columns = Some(*reader);
        }
        reader.keep(field, kind, &mut numbers);
    }
    reader.check().map_err(|_| BAD)?;
    let [_, _, _, rows] = numbers;
    let rows = u64::try_from(rows).map_err(|_| BAD)?;
    let mut columns = columns.ok_or(BAD)?;
    let (count, kind) = columns.list();
    if kind != thrift::STRUCT || count != leaves.len() {
        return Err(BAD);
    }
    for leaf in leaves {
        let Some(column) = leaf.and_then(|index| file.columns.get_mut(index)) else {
            columns.skip_element(kind);
            continue;
        };
        let (chunk, unreadable) = read_chunk(&mut columns, file.data_end)?;
        // Nothing but blanks is not even read.
        if column.plan != Plan::Blank {
            column.unreadable = column.unreadable.or(unreadable);
            if chunk.values != rows {
                return Err(Fault::Damaged(Damage::Count));
            }
            *chunks = chunks.saturating_add(1);
            if *chunks > MAX_CHUNKS {
                return Err(Fault::Limit(Limit::Metadata));
            }
            column.chunks.try_reserve(1).map_err(|_| Fault::Memory)?;
            column.chunks.push(chunk);
        }
    }
    Ok(rows)
}

fn read_chunk(
    reader: &mut Reader<'_>,
    data_end: u64,
) -> Result<(Chunk, Option<SkipReason>), Fault> {
    let mut unreadable = None;
    let mut numbers = [UNSAID; 12];
    let mut last = 0;
    while let Some((field, kind)) = reader.field(&mut last) {
        match (field, kind) {
            (1, thrift::BINARY) => {
                if !reader.binary().is_empty() {
                    unreadable = Some(SkipReason::External);
                }
            }
            (8 | 9, _) => return Err(Fault::Encrypted),
            (3, thrift::STRUCT) => {
                let mut last = 0;
                while let Some((field, kind)) = reader.field(&mut last) {
                    if (field, kind) != (2, thrift::LIST) {
                        reader.keep(field, kind, &mut numbers);
                        continue;
                    }
                    let (count, _) = reader.list();
                    for _ in 0..count {
                        let encoding = reader.int();
                        if !matches!(encoding, PLAIN | PLAIN_DICTIONARY..=BYTE_STREAM_SPLIT) {
                            let number = i32::try_from(encoding).unwrap_or(-1);
                            unreadable = Some(SkipReason::Encoding(number));
                        }
                    }
                }
            }
            _ => reader.skip(kind),
        }
    }
    reader.check().map_err(|_| BAD)?;
    let [
        _,
        _,
        _,
        _,
        codec,
        values,
        unpacked,
        len,
        _,
        data_at,
        _,
        dictionary_at,
    ] = numbers;
    let known_codec = Codec::from_number(codec);
    if known_codec.is_none() {
        let number = i32::try_from(codec).unwrap_or(-1);
        unreadable = Some(SkipReason::Codec(number));
    }
    // Some writers say 0 where there is no dictionary.
    let start = if dictionary_at > 0 {
        dictionary_at.min(data_at)
    } else {
        data_at
    };
    let known = |number: i64| u64::try_from(number).map_err(|_| BAD);
    let chunk = Chunk {
        start: known(start)?,
        len: known(len)?,
        unpacked: u64::try_from(unpacked).unwrap_or(0),
        values: known(values)?,
        codec: known_codec.unwrap_or(Codec::Plain),
    };
    if chunk.start > data_end {
        return Err(Fault::Damaged(Damage::Place));
    }
    Ok((chunk, unreadable))
}

/// What kind of page it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageKind {
    Dictionary,
    /// Values, after the levels, which are packed with them.
    Data,
    /// Values, after the levels, which are not packed.
    DataV2,
    /// Nothing that is read here.
    Other,
}

/// What the start of a page says.
#[derive(Clone, Copy, Debug)]
pub(super) struct PageHeader {
    pub(super) kind: PageKind,
    /// Bytes of the page, as it lies in the file.
    pub(super) packed_len: usize,
    /// Bytes of the page, unpacked.
    pub(super) len: usize,
    pub(super) checksum: Option<u32>,
    /// Number of values, blank ones too.
    pub(super) values: usize,
    pub(super) encoding: i64,
    /// How the levels are encoded.
    pub(super) levels: i64,
    /// Of a page whose levels are not packed: how long they are, those of
    /// repetition and those of definition, how many values are blank, and
    /// whether the rest is packed.
    pub(super) repetition: usize,
    pub(super) definition: usize,
    pub(super) blanks: usize,
    pub(super) packed: bool,
}

/// Reads the start of a page. Gives how many bytes it takes besides.
pub(super) fn read_page_header(bytes: &[u8]) -> Result<(PageHeader, usize), ThriftError> {
    let mut reader = Reader::new(bytes);
    let mut numbers = [UNSAID; 4];
    let mut checksum = None;
    // The numbers a kind of page has: a page of data with its levels, a
    // dictionary, a page of data whose levels are apart.
    let mut data = [UNSAID; 5];
    let mut dictionary = [UNSAID; 3];
    let mut data_v2 = [UNSAID; 7];
    let mut packed = true;
    let mut last = 0;
    while let Some((field, kind)) = reader.field(&mut last) {
        let inside = match (field, kind) {
            (5, thrift::STRUCT) => data.as_mut_slice(),
            (7, thrift::STRUCT) => &mut dictionary,
            (8, thrift::STRUCT) => &mut data_v2,
            (4, thrift::I32) => {
                checksum = u32::try_from(reader.int() & 0xffff_ffff).ok();
                continue;
            }
            _ => {
                reader.keep(field, kind, &mut numbers);
                continue;
            }
        };
        let mut last = 0;
        while let Some((field, kind)) = reader.field(&mut last) {
            if (field, kind) == (7, thrift::FALSE) {
                packed = false;
            }
            reader.keep(field, kind, inside);
        }
    }
    reader.check()?;
    let [_, kind, len, packed_len] = numbers;
    let (kind, [values, encoding, levels]) = match kind {
        0 => {
            let [_, values, encoding, levels, _] = data;
            (PageKind::Data, [values, encoding, levels])
        }
        1 => (PageKind::Other, [0, PLAIN, RLE]),
        2 => {
            let [_, values, encoding] = dictionary;
            (PageKind::Dictionary, [values, encoding, RLE])
        }
        3 => {
            let [_, values, _, _, encoding, _, _] = data_v2;
            (PageKind::DataV2, [values, encoding, RLE])
        }
        _ => return Err(ThriftError::Bad),
    };
    let [_, _, blanks, _, _, definition, repetition] = data_v2;
    let size = |value: i64| usize::try_from(value).map_err(|_| ThriftError::Bad);
    let levels_apart = kind == PageKind::DataV2;
    let apart = |value: i64| if levels_apart { size(value) } else { Ok(0) };
    let header = PageHeader {
        kind,
        packed_len: size(packed_len)?,
        len: size(len)?,
        checksum,
        values: size(values)?,
        encoding,
        levels,
        repetition: apart(repetition)?,
        definition: apart(definition)?,
        blanks: apart(blanks)?,
        packed: packed || !levels_apart,
    };
    Ok((header, bytes.len().saturating_sub(reader.left())))
}

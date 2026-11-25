//! The file format, version 1.
//!
//! A table file is written once, from its start to its end, and never changed.
//!
//! | bytes | content |
//! |---|---|
//! | [`HEADER_LEN`] | `"RTBL"`, the version, eight zero bytes |
//! | to `data_len` | blocks and dictionaries, then zeros up to a whole page |
//! | directory | what the columns are and where everything lies, see [`Directory`] |
//! | [`TRAILER_LEN`] | where the directory lies, its SHA-256, the length of the file, `"RTBLEND1"` |
//!
//! All integers are little-endian. Rows come in groups of the same number of
//! rows, the last one excepted, and every column has one block for each group.
//! Every block can be asked for a single row without reading the rest of it:
//!
//! - [`Encoding::Blank`]: every cell is blank. There are no bytes.
//! - [`Encoding::Packed`]: integers as `(value - low) / 10^unit`, packed with
//!   `width` bits each ([`crate::bits`]). In a text column they are codes of
//!   the column's dictionary, and `unit` is 0.
//! - [`Encoding::Raw`]: floating-point numbers, eight bytes each.
//! - [`Encoding::Text`]: where each text ends, packed with `width` bits, then
//!   the texts.
//!
//! A block with some blank cells has a bitmap of them besides, in 64-bit
//! words; what the block itself holds for a blank cell means nothing.
//!
//! The bytes before the directory are divided into pages of [`PAGE`] bytes,
//! and the directory has a checksum of each. Whatever the bytes are, reading
//! them does not panic and allocates no more than the limits here allow.

use alloc::string::String;
use alloc::vec::Vec;

use crate::bits;
use crate::types::{ColumnType, MAX_COLUMNS, MAX_NAME_BYTES, MAX_ROWS, MAX_SCALE};

/// The first bytes of a file.
pub const MAGIC: [u8; 4] = *b"RTBL";
/// The last bytes of a file.
pub const END_MAGIC: [u8; 8] = *b"RTBLEND1";
/// The version this module describes.
pub const VERSION: u32 = 1;
/// Bytes of the header.
pub const HEADER_LEN: u64 = 16;
/// Bytes of the trailer.
pub const TRAILER_LEN: u64 = 64;
/// Bytes of a page, the unit of checksums.
pub const PAGE: usize = 4_096;
/// [`PAGE`] as an offset.
pub const PAGE_BYTES: u64 = 4_096;
/// Longest directory, in bytes.
pub const MAX_DIRECTORY: u64 = 64 << 20;
/// Longest file, in bytes.
pub const MAX_FILE: u64 = 16 << 30;
/// Fewest rows of a group.
pub const MIN_GROUP_ROWS: u32 = 256;
/// Most rows of a group.
pub const MAX_GROUP_ROWS: u32 = 65_536;
/// Most entries of a dictionary.
pub const MAX_DICTIONARY_ENTRIES: u32 = 4_194_304;
/// Most bytes of the texts of a dictionary.
pub const MAX_DICTIONARY_BYTES: u64 = 1 << 30;
/// Longest block, in bytes.
pub const MAX_BLOCK: u32 = 256 << 20;

/// Bytes of a block's record in the directory.
const BLOCK_RECORD: u64 = 44;

/// Why a directory is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

/// How a block holds its values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// Nothing but blank cells.
    Blank,
    /// Packed integers.
    Packed,
    /// Floating-point numbers.
    Raw,
    /// Texts.
    Text,
}

/// The cells of one column in one group of rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// How the values are held.
    pub encoding: Encoding,
    /// Bits of a packed number.
    pub width: u8,
    /// Power of ten the packed numbers are to be multiplied with.
    pub unit: u8,
    /// Number of blank cells.
    pub blanks: u32,
    /// Offset of the block in the file.
    pub at: u64,
    /// Bytes of the block.
    pub len: u32,
    /// Offset of the bitmap of blank cells; 0 when there is none.
    pub nulls_at: u64,
    /// The smallest value: the integer's bits, or the number's. 0 for texts
    /// that are not in a dictionary.
    pub low: u64,
    /// The largest value, likewise.
    pub high: u64,
}

impl Block {
    /// The block of a group without values.
    pub const BLANK: Self = Self {
        encoding: Encoding::Blank,
        width: 0,
        unit: 0,
        blanks: 0,
        at: 0,
        len: 0,
        nulls_at: 0,
        low: 0,
        high: 0,
    };

    /// What the packed numbers are to be multiplied with.
    #[must_use]
    pub fn factor(&self) -> u64 {
        10_u64.saturating_pow(u32::from(self.unit))
    }

    /// The value a packed number stands for.
    #[must_use]
    pub fn value(&self, packed: u64, factor: u64) -> i64 {
        self.low
            .wrapping_add(packed.wrapping_mul(factor))
            .cast_signed()
    }
}

/// Where the dictionary of a text column lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DictionaryPlace {
    /// Number of texts.
    pub entries: u32,
    /// Bits of the packed offsets at which the texts end.
    pub ends_width: u8,
    /// Offset of those.
    pub ends_at: u64,
    /// Offset of the texts.
    pub bytes_at: u64,
    /// Bytes of the texts.
    pub bytes_len: u64,
}

impl DictionaryPlace {
    /// Bytes of the packed offsets.
    #[must_use]
    pub fn ends_len(&self) -> u64 {
        bits::packed_len(self.entries as usize, u32::from(self.ends_width))
    }
}

/// One column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// Its name.
    pub name: String,
    /// What it holds.
    pub kind: ColumnType,
    /// Number of blank cells.
    pub blanks: u64,
    /// The dictionary, of a text column that has one.
    pub dictionary: Option<DictionaryPlace>,
}

/// What a file holds, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directory {
    /// Rows of every group but the last. A power of two.
    pub group_rows: u32,
    /// Rows of the table.
    pub rows: u32,
    /// The columns.
    pub columns: Vec<Column>,
    /// The blocks, group by group and in a group column by column.
    pub blocks: Vec<Block>,
    /// The checksums of the pages.
    pub sums: Vec<u32>,
    /// Bytes before the directory.
    pub data_len: u64,
}

impl Directory {
    /// Number of groups.
    #[must_use]
    pub const fn groups(&self) -> u32 {
        self.rows.div_ceil(if self.group_rows == 0 {
            1
        } else {
            self.group_rows
        })
    }

    /// Rows of a group.
    #[must_use]
    pub const fn rows_in(&self, group: u32) -> u32 {
        let rest = self
            .rows
            .saturating_sub(group.saturating_mul(self.group_rows));
        if rest < self.group_rows {
            rest
        } else {
            self.group_rows
        }
    }

    /// The group of a row, and the row's place in it.
    #[must_use]
    pub const fn group_of(&self, row: u32) -> (u32, u32) {
        let size = if self.group_rows == 0 {
            1
        } else {
            self.group_rows
        };
        match (row.checked_div(size), row.checked_rem(size)) {
            (Some(group), Some(within)) => (group, within),
            _ => (0, 0),
        }
    }

    /// The block of a column in a group.
    #[must_use]
    pub fn block(&self, column: usize, group: u32) -> Option<&Block> {
        if column >= self.columns.len() {
            return None;
        }
        self.blocks.get(
            (group as usize)
                .checked_mul(self.columns.len())?
                .checked_add(column)?,
        )
    }

    /// Bytes [`Directory::encode`] gives.
    #[must_use]
    pub fn encoded_len(&self) -> u64 {
        let columns: u64 = self
            .columns
            .iter()
            .map(|column| {
                let dictionary = if column.dictionary.is_some() { 29 } else { 0 };
                u64::try_from(column.name.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(14)
                    .saturating_add(dictionary)
            })
            .fold(0, u64::saturating_add);
        let blocks = u64::try_from(self.blocks.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(BLOCK_RECORD);
        let sums = u64::try_from(self.sums.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(4);
        columns
            .saturating_add(blocks)
            .saturating_add(sums)
            .saturating_add(32)
    }

    /// Writes the directory.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&0_u32.to_le_bytes()); // flags
        out.extend_from_slice(&self.group_rows.to_le_bytes());
        out.extend_from_slice(&u64::from(self.rows).to_le_bytes());
        out.extend_from_slice(&count(self.columns.len()).to_le_bytes());
        out.extend_from_slice(&count(self.sums.len()).to_le_bytes());
        out.extend_from_slice(&self.data_len.to_le_bytes());
        for column in &self.columns {
            let name = column.name.as_bytes();
            out.extend_from_slice(&u16::try_from(name.len()).unwrap_or(0).to_le_bytes());
            out.extend_from_slice(name);
            let (kind, scale, fixed) = kind_bytes(column.kind);
            out.push(kind);
            out.push(scale);
            out.push(fixed);
            out.push(u8::from(column.dictionary.is_some()));
            out.extend_from_slice(&column.blanks.to_le_bytes());
            if let Some(dictionary) = &column.dictionary {
                out.extend_from_slice(&dictionary.entries.to_le_bytes());
                out.push(dictionary.ends_width);
                out.extend_from_slice(&dictionary.ends_at.to_le_bytes());
                out.extend_from_slice(&dictionary.bytes_at.to_le_bytes());
                out.extend_from_slice(&dictionary.bytes_len.to_le_bytes());
            }
        }
        for block in &self.blocks {
            out.push(match block.encoding {
                Encoding::Blank => 0,
                Encoding::Packed => 1,
                Encoding::Raw => 2,
                Encoding::Text => 3,
            });
            out.push(block.width);
            out.push(block.unit);
            out.push(0);
            out.extend_from_slice(&block.blanks.to_le_bytes());
            out.extend_from_slice(&block.at.to_le_bytes());
            out.extend_from_slice(&block.len.to_le_bytes());
            out.extend_from_slice(&block.nulls_at.to_le_bytes());
            out.extend_from_slice(&block.low.to_le_bytes());
            out.extend_from_slice(&block.high.to_le_bytes());
        }
        for sum in &self.sums {
            out.extend_from_slice(&sum.to_le_bytes());
        }
    }

    /// Reads a directory, checking that everything in it lies where it can.
    ///
    /// # Errors
    ///
    /// [`Malformed`] with what is wrong.
    pub fn decode(bytes: &[u8], data_len: u64) -> Result<Self, Malformed> {
        let mut input = Input { rest: bytes };
        if input.u32()? != 0 {
            return Err(Malformed("unknown flags"));
        }
        let group_rows = input.u32()?;
        if !group_rows.is_power_of_two() || !(MIN_GROUP_ROWS..=MAX_GROUP_ROWS).contains(&group_rows)
        {
            return Err(Malformed("rows of a group"));
        }
        let rows = input.u64()?;
        if rows > MAX_ROWS {
            return Err(Malformed("number of rows"));
        }
        let rows = u32::try_from(rows).map_err(|_| Malformed("number of rows"))?;
        let column_count = input.u32()? as usize;
        if column_count == 0 || column_count > MAX_COLUMNS {
            return Err(Malformed("number of columns"));
        }
        let sum_count = input.u32()? as usize;
        if input.u64()? != data_len
            || !(HEADER_LEN..=MAX_FILE).contains(&data_len)
            || data_len.checked_rem(PAGE_BYTES) != Some(0)
            || u64::try_from(sum_count).ok() != data_len.checked_div(PAGE_BYTES)
        {
            return Err(Malformed("length of the data"));
        }

        let place = Place { data_len };
        let mut columns = Vec::new();
        for _ in 0..column_count {
            columns.push(input.column(rows, place)?);
        }

        let mut directory = Self {
            group_rows,
            rows,
            columns,
            blocks: Vec::new(),
            sums: Vec::new(),
            data_len,
        };
        let groups = directory.groups();
        let block_count = (groups as usize)
            .checked_mul(column_count)
            .ok_or(Malformed("number of blocks"))?;
        // What is left has to be exactly the blocks and the sums, so neither
        // can be longer than the directory is.
        let expected = u64::try_from(block_count)
            .unwrap_or(u64::MAX)
            .saturating_mul(BLOCK_RECORD)
            .saturating_add(
                u64::try_from(sum_count)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(4),
            );
        if u64::try_from(input.rest.len()).ok() != Some(expected) {
            return Err(Malformed("length of the directory"));
        }
        directory
            .blocks
            .try_reserve_exact(block_count)
            .map_err(|_| Malformed("too many blocks"))?;
        for group in 0..groups {
            let group_len = directory.rows_in(group);
            for column in &directory.columns {
                let block = input.block(column, group_len, place)?;
                directory.blocks.push(block);
            }
        }
        for (index, column) in directory.columns.iter().enumerate() {
            let blanks = directory
                .blocks
                .iter()
                .skip(index)
                .step_by(column_count)
                .map(|block| u64::from(block.blanks))
                .fold(0, u64::saturating_add);
            if blanks != column.blanks {
                return Err(Malformed("number of blank cells"));
            }
        }
        directory
            .sums
            .try_reserve_exact(sum_count)
            .map_err(|_| Malformed("too many pages"))?;
        for _ in 0..sum_count {
            directory.sums.push(input.u32()?);
        }
        Ok(directory)
    }
}

fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

const fn kind_bytes(kind: ColumnType) -> (u8, u8, u8) {
    match kind {
        ColumnType::Integer => (0, 0, 0),
        ColumnType::Decimal { scale, fixed } => (1, scale, if fixed { 1 } else { 0 }),
        ColumnType::Float => (2, 0, 0),
        ColumnType::Date => (3, 0, 0),
        ColumnType::DateTime => (4, 0, 0),
        ColumnType::Boolean => (5, 0, 0),
        ColumnType::Text => (6, 0, 0),
    }
}

const fn kind_from(kind: u8, scale: u8, fixed: u8) -> Option<ColumnType> {
    let kind = match (kind, scale, fixed) {
        (0, 0, 0) => ColumnType::Integer,
        (1, scale, 0 | 1) if scale <= MAX_SCALE => ColumnType::Decimal {
            scale,
            fixed: fixed == 1,
        },
        (2, 0, 0) => ColumnType::Float,
        (3, 0, 0) => ColumnType::Date,
        (4, 0, 0) => ColumnType::DateTime,
        (5, 0, 0) => ColumnType::Boolean,
        (6, 0, 0) => ColumnType::Text,
        _ => return None,
    };
    Some(kind)
}

/// Where things in the file can lie.
#[derive(Clone, Copy)]
struct Place {
    data_len: u64,
}

impl Place {
    fn check(self, at: u64, len: u64) -> Result<(), Malformed> {
        let end = at.checked_add(len).ok_or(Malformed("offset"))?;
        if at < HEADER_LEN || end > self.data_len {
            return Err(Malformed("offset"));
        }
        Ok(())
    }
}

/// Bytes being read.
struct Input<'a> {
    rest: &'a [u8],
}

impl<'a> Input<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], Malformed> {
        let (taken, rest) = self
            .rest
            .split_at_checked(len)
            .ok_or(Malformed("ends early"))?;
        self.rest = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        self.take(N)?
            .try_into()
            .map_err(|_| Malformed("ends early"))
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        self.array::<1>().map(u8::from_le_bytes)
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        self.array().map(u64::from_le_bytes)
    }

    fn column(&mut self, rows: u32, place: Place) -> Result<Column, Malformed> {
        let name_len = usize::from(self.u16()?);
        if name_len > MAX_NAME_BYTES {
            return Err(Malformed("name of a column"));
        }
        let name = core::str::from_utf8(self.take(name_len)?)
            .map_err(|_| Malformed("name of a column"))?;
        let (kind, scale, fixed) = (self.u8()?, self.u8()?, self.u8()?);
        let kind = kind_from(kind, scale, fixed).ok_or(Malformed("type of a column"))?;
        let has_dictionary = match self.u8()? {
            0 => false,
            1 if kind == ColumnType::Text => true,
            _ => return Err(Malformed("dictionary")),
        };
        let blanks = self.u64()?;
        if blanks > u64::from(rows) {
            return Err(Malformed("number of blank cells"));
        }
        let dictionary = if has_dictionary {
            let dictionary = DictionaryPlace {
                entries: self.u32()?,
                ends_width: self.u8()?,
                ends_at: self.u64()?,
                bytes_at: self.u64()?,
                bytes_len: self.u64()?,
            };
            // Every entry occurs, and none is empty.
            if dictionary.entries == 0
                || dictionary.entries > MAX_DICTIONARY_ENTRIES
                || u64::from(dictionary.entries) > u64::from(rows).saturating_sub(blanks)
                || u64::from(dictionary.entries) > dictionary.bytes_len
                || dictionary.ends_width > 32
                || dictionary.bytes_len > MAX_DICTIONARY_BYTES
                || bits::width_of(dictionary.bytes_len) > u32::from(dictionary.ends_width)
            {
                return Err(Malformed("dictionary"));
            }
            place.check(dictionary.ends_at, dictionary.ends_len())?;
            place.check(dictionary.bytes_at, dictionary.bytes_len)?;
            Some(dictionary)
        } else {
            None
        };
        Ok(Column {
            name: String::from(name),
            kind,
            blanks,
            dictionary,
        })
    }

    fn block(&mut self, column: &Column, rows: u32, place: Place) -> Result<Block, Malformed> {
        let encoding = match self.u8()? {
            0 => Encoding::Blank,
            1 => Encoding::Packed,
            2 => Encoding::Raw,
            3 => Encoding::Text,
            _ => return Err(Malformed("encoding")),
        };
        let (width, unit, reserved) = (self.u8()?, self.u8()?, self.u8()?);
        let block = Block {
            encoding,
            width,
            unit,
            blanks: self.u32()?,
            at: self.u64()?,
            len: self.u32()?,
            nulls_at: self.u64()?,
            low: self.u64()?,
            high: self.u64()?,
        };
        if reserved != 0 || block.blanks > rows {
            return Err(Malformed("block"));
        }
        let fits = match encoding {
            Encoding::Blank => {
                let empty = Block {
                    blanks: rows,
                    ..Block::BLANK
                };
                return if block == empty {
                    Ok(block)
                } else {
                    Err(Malformed("blank block"))
                };
            }
            Encoding::Packed => {
                let holds = column.kind.is_integral() || column.dictionary.is_some();
                holds
                    && width <= 64
                    && unit <= if column.dictionary.is_some() { 0 } else { 18 }
                    && block.low.cast_signed() <= block.high.cast_signed()
                    && u64::from(block.len) == bits::packed_len(rows as usize, u32::from(width))
            }
            Encoding::Raw => {
                column.kind == ColumnType::Float
                    && width == 0
                    && unit == 0
                    && u64::from(block.len) == u64::from(rows).saturating_mul(8)
                    && f64::from_bits(block.low) <= f64::from_bits(block.high)
            }
            Encoding::Text => {
                column.kind == ColumnType::Text
                    && width <= 32
                    && unit == 0
                    && u64::from(block.len) >= bits::packed_len(rows as usize, u32::from(width))
            }
        };
        if !fits || block.len > MAX_BLOCK || block.blanks >= rows {
            return Err(Malformed("block"));
        }
        place.check(block.at, u64::from(block.len))?;
        if (block.nulls_at == 0) != (block.blanks == 0) {
            return Err(Malformed("blank cells"));
        }
        if block.nulls_at != 0 {
            place.check(block.nulls_at, nulls_len(rows))?;
        }
        Ok(block)
    }
}

/// Bytes of the bitmap of blank cells of so many rows.
#[must_use]
pub const fn nulls_len(rows: u32) -> u64 {
    (rows as u64).div_ceil(64).saturating_mul(8)
}

/// The header of a file.
#[must_use]
pub fn header() -> [u8; 16] {
    let mut out = [0; 16];
    let version = VERSION.to_le_bytes();
    for (slot, byte) in out.iter_mut().zip(MAGIC.iter().chain(&version)) {
        *slot = *byte;
    }
    out
}

/// The end of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trailer {
    /// Offset of the directory, which is the length of the data.
    pub directory_at: u64,
    /// Bytes of the directory.
    pub directory_len: u64,
    /// SHA-256 of the directory.
    pub directory_sha256: [u8; 32],
}

impl Trailer {
    /// Length of the file.
    #[must_use]
    pub const fn file_len(&self) -> u64 {
        self.directory_at
            .saturating_add(self.directory_len)
            .saturating_add(TRAILER_LEN)
    }

    /// Writes the trailer.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.directory_at.to_le_bytes());
        out.extend_from_slice(&self.directory_len.to_le_bytes());
        out.extend_from_slice(&self.directory_sha256);
        out.extend_from_slice(&self.file_len().to_le_bytes());
        out.extend_from_slice(&END_MAGIC);
    }

    /// Reads the trailer of a file of that length.
    ///
    /// # Errors
    ///
    /// [`Malformed`] when it is not the end of a table file, or not of one so long.
    pub fn decode(bytes: &[u8], file_len: u64) -> Result<Self, Malformed> {
        let mut input = Input { rest: bytes };
        let trailer = Self {
            directory_at: input.u64()?,
            directory_len: input.u64()?,
            directory_sha256: input.array()?,
        };
        let length = input.u64()?;
        if input.array::<8>()? != END_MAGIC || !input.rest.is_empty() {
            return Err(Malformed("not a table file"));
        }
        if length != file_len
            || trailer.file_len() != file_len
            || trailer.directory_len > MAX_DIRECTORY
            || trailer.directory_at < HEADER_LEN
        {
            return Err(Malformed("length of the file"));
        }
        Ok(trailer)
    }
}

#[cfg(test)]
#[allow(clippy::get_unwrap, clippy::unwrap_used, reason = "tests")]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use alloc::vec;
    use reticula_core::rng::Rng;

    fn sample() -> Directory {
        Directory {
            group_rows: 256,
            rows: 300,
            columns: vec![
                Column {
                    name: String::from("number"),
                    kind: ColumnType::Decimal {
                        scale: 2,
                        fixed: true,
                    },
                    blanks: 0,
                    dictionary: None,
                },
                Column {
                    name: String::from("名称"),
                    kind: ColumnType::Text,
                    blanks: 44,
                    dictionary: Some(DictionaryPlace {
                        entries: 3,
                        ends_width: 4,
                        ends_at: 2_000,
                        bytes_at: 2_008,
                        bytes_len: 9,
                    }),
                },
            ],
            blocks: vec![
                Block {
                    encoding: Encoding::Packed,
                    width: 10,
                    at: 16,
                    len: 320,
                    high: 1_000,
                    ..Block::BLANK
                },
                Block {
                    encoding: Encoding::Packed,
                    width: 2,
                    at: 400,
                    len: 64,
                    high: 2,
                    ..Block::BLANK
                },
                Block {
                    encoding: Encoding::Packed,
                    width: 0,
                    at: 500,
                    ..Block::BLANK
                },
                Block {
                    blanks: 44,
                    ..Block::BLANK
                },
            ],
            sums: vec![7],
            data_len: 4_096,
        }
    }

    #[test]
    fn a_directory_comes_back() {
        let directory = sample();
        let mut bytes = Vec::new();
        directory.encode(&mut bytes);
        assert_eq!(bytes.len() as u64, directory.encoded_len());
        assert_eq!(Directory::decode(&bytes, 4_096).unwrap(), directory);
        assert!(Directory::decode(&bytes, 8_192).is_err());
    }

    #[test]
    fn a_directory_cut_short_or_damaged_is_refused_or_still_one() {
        let directory = sample();
        let mut bytes = Vec::new();
        directory.encode(&mut bytes);
        for len in 0..bytes.len() {
            assert!(Directory::decode(bytes.get(..len).unwrap(), 4_096).is_err());
        }
        let mut rng = Rng::from_seed(3);
        for _ in 0..20_000 {
            let mut damaged = bytes.clone();
            for _ in 0..=rng.next_below(3) {
                let at = usize::try_from(rng.next_below(bytes.len() as u64)).unwrap();
                *damaged.get_mut(at).unwrap() =
                    rng.next_u64().to_le_bytes().first().copied().unwrap();
            }
            if let Ok(decoded) = Directory::decode(&damaged, 4_096) {
                let mut again = Vec::new();
                decoded.encode(&mut again);
                assert_eq!(again, damaged);
            }
        }
    }

    #[test]
    fn a_trailer_comes_back() {
        let trailer = Trailer {
            directory_at: 8_192,
            directory_len: 100,
            directory_sha256: [9; 32],
        };
        let mut bytes = Vec::new();
        trailer.encode(&mut bytes);
        assert_eq!(bytes.len() as u64, TRAILER_LEN);
        assert_eq!(Trailer::decode(&bytes, 8_356), Ok(trailer));
        assert!(Trailer::decode(&bytes, 8_357).is_err());
        assert!(Trailer::decode(bytes.get(1..).unwrap(), 8_356).is_err());
    }
}

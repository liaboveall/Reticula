//! The cells of a column in one group of rows: all of them, or the selected
//! ones when those are few.

use alloc::string::String;
use alloc::vec::Vec;

use super::QueryError;
use super::memory::Charge;
use super::selection::tail;
use crate::layout::{Block, Directory, Encoding};
use crate::reader::{BlockBuffer, ReadError, Reader};
use crate::source::AnySource;
use crate::types::CellValue;

/// Which of the selected cells of a group are wanted.
#[derive(Clone, Copy, Debug)]
pub(super) struct Part {
    /// The first row, in the group.
    pub(super) from: usize,
    /// Most cells. There are fewer when their texts are long.
    pub(super) most: usize,
    /// Whether the texts are wanted, of a block of texts, and not only which
    /// cells are blank.
    pub(super) texts: bool,
}

impl Part {
    /// All of them.
    pub(super) const ALL: Self = Self {
        from: 0,
        most: usize::MAX,
        texts: false,
    };

    /// All from a row on, with their texts.
    pub(super) const fn texts(from: usize) -> Self {
        Self {
            from,
            texts: true,
            ..Self::ALL
        }
    }
}

/// The cells that were loaded last.
///
/// Cells are numbered from 0: as in the group when all of it was loaded, and
/// one after another when only the selected cells were.
#[derive(Debug)]
pub(super) struct Cells {
    whole: BlockBuffer,
    /// The column and group that are in it.
    held: Option<(usize, u32)>,
    /// Whether only the selected cells are here.
    compact: bool,
    record: Block,
    rows: usize,
    /// The first of them that is wanted, and the first after it that is not.
    window: (usize, usize),
    words: Vec<u64>,
    nulls: Vec<u64>,
    ends: Vec<usize>,
    bytes: Vec<u8>,
    text: String,
    charge: Charge,
}

impl Cells {
    pub(super) fn new(charge: Charge) -> Self {
        Self {
            whole: BlockBuffer::default(),
            held: None,
            compact: false,
            record: Block::BLANK,
            rows: 0,
            window: (0, 0),
            words: Vec::new(),
            nulls: Vec::new(),
            ends: Vec::new(),
            bytes: Vec::new(),
            text: String::new(),
            charge,
        }
    }

    /// Bytes the cells of a block take.
    pub(super) fn size(record: &Block, rows: usize) -> usize {
        (record.len as usize)
            .saturating_add(rows.saturating_mul(8))
            .saturating_add(rows >> 3)
            .saturating_add(8_192)
    }

    /// Bytes that are counted for this.
    pub(super) const fn charged(&self) -> usize {
        self.charge.bytes()
    }

    /// Counts that many bytes for this.
    pub(super) fn count_bytes(&mut self, bytes: usize) -> Result<(), QueryError> {
        self.charge.resize(bytes)
    }

    /// Loads all cells of a group, of which a part is wanted. Gives the first
    /// row after it.
    pub(super) fn load(
        &mut self,
        reader: &mut Reader,
        source: &mut AnySource<'_>,
        at: (usize, u32),
        record: &Block,
        part: Part,
    ) -> Result<usize, QueryError> {
        self.record = *record;
        self.compact = false;
        self.rows = reader.directory().rows_in(at.1) as usize;
        let to = part.from.saturating_add(part.most).min(self.rows);
        self.window = (part.from, to);
        if self.held != Some(at) {
            self.held = None;
            reader.block(source, at.0, at.1, &mut self.whole)?;
            self.held = Some(at);
        }
        Ok(to)
    }

    /// Loads a part of the selected cells of a group one by one, until there
    /// are so many bytes of texts. Gives the first row after it. Not those of a
    /// block of codes, because reading one of them gives its text.
    pub(super) fn gather(
        &mut self,
        reader: &mut Reader,
        source: &mut AnySource<'_>,
        (column, group): (usize, u32),
        record: &Block,
        (selected, part): (Option<&[u64]>, Part),
        bytes: usize,
    ) -> Result<usize, QueryError> {
        self.record = *record;
        self.compact = true;
        self.rows = 0;
        self.words.clear();
        self.nulls.clear();
        self.ends.clear();
        self.bytes.clear();
        let first = group.saturating_mul(reader.directory().group_rows);
        let rows = reader.directory().rows_in(group) as usize;
        let factor = self.record.factor();
        let mut missing = false;
        let limit = if part.texts { usize::MAX } else { 0 };
        let mut next = rows;
        'rows: for at in part.from >> 6..rows.div_ceil(64) {
            let picked = selected.map_or(u64::MAX, |bits| bits.get(at).copied().unwrap_or(0));
            let mut left = picked & tail(rows, at) & !tail(part.from, at);
            while left != 0 {
                let row = at
                    .saturating_mul(64)
                    .saturating_add(left.trailing_zeros() as usize);
                if self.rows >= part.most || self.bytes.len() >= bytes {
                    next = row;
                    break 'rows;
                }
                left &= left.wrapping_sub(1);
                self.text.clear();
                let row = first.saturating_add(u32::try_from(row).unwrap_or(u32::MAX));
                let value = match reader.cell(source, column, row, limit, &mut self.text) {
                    Ok(value) => value,
                    Err(ReadError::Missing) => {
                        // Whatever else is missing is noted as well.
                        missing = true;
                        CellValue::Blank
                    }
                    Err(error) => return Err(error.into()),
                };
                let word = match value {
                    CellValue::Blank | CellValue::Text => 0,
                    CellValue::Int(value) => value
                        .cast_unsigned()
                        .wrapping_sub(self.record.low)
                        .checked_div(factor)
                        .unwrap_or(0),
                    CellValue::Float(value) => value.to_bits(),
                };
                if self.rows & 63 == 0 {
                    self.nulls.push(0);
                }
                if value == CellValue::Blank
                    && let Some(last) = self.nulls.last_mut()
                {
                    *last |= 1_u64.wrapping_shl(u32::try_from(self.rows & 63).unwrap_or(0));
                }
                self.words.push(word);
                self.bytes.extend_from_slice(self.text.as_bytes());
                self.ends.push(self.bytes.len());
                self.rows = self.rows.saturating_add(1);
            }
        }
        self.window = (0, self.rows);
        if missing {
            return Err(ReadError::Missing.into());
        }
        Ok(next)
    }

    /// Whether only the selected cells are here.
    pub(super) const fn is_compact(&self) -> bool {
        self.compact
    }

    /// Number of cells.
    pub(super) const fn rows(&self) -> usize {
        self.rows
    }

    pub(super) const fn record(&self) -> &Block {
        &self.record
    }

    /// The words of the cells, see [`BlockBuffer::words`].
    pub(super) fn words(&self) -> &[u64] {
        if self.compact {
            &self.words
        } else {
            self.whole.words()
        }
    }

    /// The blank cells.
    pub(super) fn nulls(&self) -> &[u64] {
        if self.compact {
            &self.nulls
        } else {
            self.whole.nulls()
        }
    }

    /// The bytes of a cell of a block of texts.
    pub(super) fn text(&self, cell: usize) -> &[u8] {
        if !self.compact {
            return self.whole.text(cell);
        }
        let end = |cell: usize| self.ends.get(cell).copied();
        let start = cell.checked_sub(1).and_then(end).unwrap_or(0);
        end(cell)
            .and_then(|end| self.bytes.get(start..end))
            .unwrap_or(&[])
    }

    /// The cells that are selected, as the bits of 64 cells that are here.
    pub(super) fn mask(&self, selected: Option<&[u64]>, word: usize) -> u64 {
        let here = tail(self.window.1, word) & !tail(self.window.0, word);
        match selected {
            Some(selected) if !self.compact => selected.get(word).copied().unwrap_or(0) & here,
            _ => here,
        }
    }

    /// The cells that are selected and not blank, likewise.
    pub(super) fn values(&self, selected: Option<&[u64]>, word: usize) -> u64 {
        let nulls = self.nulls().get(word).copied().unwrap_or(u64::MAX);
        self.mask(selected, word) & !nulls
    }

    /// How many of the selected cells are blank.
    pub(super) fn blanks(&self, selected: Option<&[u64]>) -> u64 {
        if self.record.blanks == 0 {
            return 0;
        }
        (0..self.rows.div_ceil(64))
            .map(|word| self.mask(selected, word) & !self.values(selected, word))
            .fold(0, |sum, blank| {
                sum.saturating_add(u64::from(blank.count_ones()))
            })
    }

    /// Visits the selected cells in order: the number of each, and whether it
    /// is blank.
    #[inline(never)]
    pub(super) fn each(
        &self,
        selected: Option<&[u64]>,
        visit: &mut dyn FnMut(usize, bool) -> Result<(), QueryError>,
    ) -> Result<(), QueryError> {
        let nulls = self.nulls();
        for word in 0..self.rows.div_ceil(64) {
            let mut left = self.mask(selected, word);
            let blank = nulls.get(word).copied().unwrap_or(0);
            while left != 0 {
                let bit = left.trailing_zeros();
                left &= left.wrapping_sub(1);
                let cell = word.saturating_mul(64).saturating_add(bit as usize);
                visit(cell, blank.wrapping_shr(bit) & 1 == 1)?;
            }
        }
        Ok(())
    }
}

/// Visits the words that are picked by the bits of a mask.
pub(super) fn each_word(mask: u64, words: &[u64], mut visit: impl FnMut(u64)) {
    if mask == u64::MAX {
        for word in words {
            visit(*word);
        }
        return;
    }
    let mut left = mask;
    while left != 0 {
        let bit = left.trailing_zeros() as usize;
        left &= left.wrapping_sub(1);
        if let Some(word) = words.get(bit) {
            visit(*word);
        }
    }
}

/// Whether a block holds codes of a dictionary.
pub(super) fn is_coded(directory: &Directory, column: usize, record: &Block) -> bool {
    record.encoding == Encoding::Packed
        && directory
            .columns
            .get(column)
            .is_some_and(|column| column.dictionary.is_some())
}

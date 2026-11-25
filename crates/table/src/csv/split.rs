//! Cutting delimited text into fields.

use alloc::string::String;
use alloc::vec::Vec;

use super::value::nonzero;
use crate::types::{MAX_COLUMNS, MAX_TEXT_BYTES};

const HIGH: u64 = 0x8080_8080_8080_8080;
/// Set in the place of a field that is not in the input as it is.
const ELSEWHERE: u32 = 1 << 31;
/// No offset of the input.
const NOWHERE: usize = usize::MAX;

/// Where a field is: in the input, or among those that had to be put together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Field {
    place: u32,
    len: u32,
}

impl Field {
    pub(super) const BLANK: Self = Self { place: 0, len: 0 };

    fn of(start: usize, len: usize) -> Self {
        Self {
            place: u32::try_from(start).unwrap_or(0),
            len: u32::try_from(len).unwrap_or(0),
        }
    }

    /// Its text.
    #[inline]
    pub(super) fn text<'a>(self, input: &'a str, joined: &'a str) -> &'a str {
        let from = if self.place & ELSEWHERE == 0 {
            input
        } else {
            joined
        };
        let start = (self.place & !ELSEWHERE) as usize;
        from.get(start..start.wrapping_add(self.len as usize))
            .unwrap_or("")
    }

    /// The bytes of its text.
    #[inline]
    pub(super) fn bytes<'a>(self, input: &'a str, joined: &'a str) -> &'a [u8] {
        let from = if self.place & ELSEWHERE == 0 {
            input
        } else {
            joined
        };
        let start = (self.place & !ELSEWHERE) as usize;
        from.as_bytes()
            .get(start..start.wrapping_add(self.len as usize))
            .unwrap_or(&[])
    }
}

/// Why no more fields can be cut. It happened in the current row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SplitError {
    /// A quote is not closed.
    Unclosed,
    /// A field in quotes is too long.
    TooLong,
}

/// How far the input was cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reached {
    /// As many rows or fields as were asked for.
    Limit,
    /// Everything.
    End,
}

/// Where in a field the cutting stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Before it.
    Start,
    /// In a field without quotes around it.
    Plain,
    /// Between its quotes. So many doubled ones came.
    Quoted { doubled: usize },
    /// After the quote that ends it, which is at `close`.
    Closed { doubled: usize, close: usize },
    /// In a field that is longer than a text can be. What can be kept of it
    /// was put aside.
    Long,
}

/// Cuts text into fields, with any number of breaks.
#[derive(Debug)]
pub(super) struct Splitter {
    delimiter: u8,
    /// Fields of every row. 0 to take the rows as they are.
    width: usize,
    /// How many fields of the current row are kept.
    keep: usize,
    /// Whether the current row is the one with the names, which is dropped.
    header: bool,
    /// Whether a row has come.
    started: bool,
    /// Where the current field begins.
    start: usize,
    /// From where on it has not been looked at.
    from: usize,
    state: State,
    /// How many fields of the current row are complete.
    column: usize,
    /// The offset after the last carriage return.
    after_return: usize,
    /// The start of a field that is too long.
    long: String,
    /// The fields since the last call of [`Splitter::clear`], row by row.
    pub(super) fields: Vec<Field>,
    /// The column of the first one.
    pub(super) first: usize,
    /// The texts of fields that had to be put together.
    pub(super) joined: String,
    /// Where each row ends among the fields, when rows are taken as they are.
    pub(super) ends: Vec<usize>,
    /// Where the current row begins in the input, likewise.
    pub(super) row_start: usize,
    /// The number of the current row, from 1 on, counting everything.
    pub(super) number: u64,
    /// Rows that were complete.
    pub(super) rows: u64,
    /// Those that had too few fields.
    pub(super) short_rows: u64,
    /// Fields that were too many.
    pub(super) extra_fields: u64,
    /// Lines that were skipped because there is nothing on them.
    pub(super) empty_lines: u64,
}

impl Splitter {
    /// Cuts into rows of `width` fields, after a row of names if there is one.
    #[inline(never)]
    pub(super) fn new(delimiter: u8, width: usize, header: bool) -> Self {
        Self {
            delimiter,
            width,
            keep: if header { 0 } else { width },
            header,
            started: false,
            start: 0,
            from: 0,
            state: State::Start,
            column: 0,
            after_return: NOWHERE,
            long: String::new(),
            fields: Vec::new(),
            first: 0,
            joined: String::new(),
            ends: Vec::new(),
            row_start: 0,
            number: 1,
            rows: 0,
            short_rows: 0,
            extra_fields: 0,
            empty_lines: 0,
        }
    }

    /// Cuts into rows as they are. Of one that is longer than any table, one
    /// field too many is kept.
    pub(super) fn of_any_width(delimiter: u8) -> Self {
        let mut splitter = Self::new(delimiter, 0, false);
        splitter.keep = MAX_COLUMNS.saturating_add(1);
        splitter
    }

    /// The fields of a row, when rows are taken as they are.
    pub(super) fn row(&self, row: usize) -> &[Field] {
        let start = row.checked_sub(1).and_then(|before| self.ends.get(before));
        let start = start.copied().unwrap_or(0);
        let end = self.ends.get(row).copied().unwrap_or(0);
        self.fields.get(start..end).unwrap_or(&[])
    }

    /// From where on the input has not been looked at.
    pub(super) const fn looked_at(&self) -> usize {
        self.from
    }

    /// Starts at an offset, before anything was cut.
    pub(super) const fn skip(&mut self, bytes: usize) {
        self.start = bytes;
        self.from = bytes;
        self.row_start = bytes;
    }

    /// Forgets the fields, which were used.
    pub(super) fn clear(&mut self) {
        self.fields.clear();
        self.joined.clear();
        self.first = if self.column < self.keep {
            self.column
        } else {
            0
        };
    }

    /// Says how many bytes of the input are not needed any more, and goes on
    /// as if they had never been there. The fields have to be forgotten before.
    pub(super) const fn forget(&mut self) -> usize {
        let gone = self.start;
        self.start = 0;
        self.from = self.from.saturating_sub(gone);
        self.after_return = match self.after_return.checked_sub(gone) {
            Some(after) if self.after_return != NOWHERE => after,
            _ => NOWHERE,
        };
        self.row_start = self.row_start.saturating_sub(gone);
        if let State::Closed { doubled, close } = self.state {
            self.state = State::Closed {
                doubled,
                close: close.saturating_sub(gone),
            };
        }
        gone
    }

    /// Cuts the input up to `end`, until there are so many more rows, or so
    /// many fields. What was found does not depend on where the earlier calls
    /// stopped. `last` says that nothing comes after `end`.
    ///
    /// # Errors
    ///
    /// [`SplitError`] when that is not possible.
    pub(super) fn run(
        &mut self,
        input: &str,
        end: usize,
        last: bool,
        rows: u64,
        fields: usize,
    ) -> Result<Reached, SplitError> {
        let bytes = input.as_bytes().get(..end).unwrap_or(&[]);
        let stop = self.rows.saturating_add(rows);
        let mut marks = Marks::new(self.delimiter, bytes, self.from);
        let mut start = self.start;
        let mut column = self.column;
        loop {
            if self.state == State::Start {
                match bytes.get(start) {
                    None if column == 0 || !last => {
                        self.from = start;
                        break;
                    }
                    Some(b'"') => {
                        self.state = State::Quoted { doubled: 0 };
                        self.from = start.wrapping_add(1);
                    }
                    _ => self.state = State::Plain,
                }
            }
            if let State::Quoted { doubled } = self.state {
                if !self.pass_quoted(input.as_bytes(), end, start, doubled, last)? {
                    if last {
                        return Err(SplitError::Unclosed);
                    }
                    break;
                }
                marks = Marks::new(self.delimiter, bytes, self.from);
            }
            let at = match marks.next(bytes) {
                Some(at) => at,
                None if last => bytes.len(),
                None => {
                    self.from = bytes.len();
                    break;
                }
            };
            // The end of the text ends a row like the end of a line.
            let byte = bytes.get(at).copied().unwrap_or(b'\n');
            let after = at.wrapping_add(1).min(bytes.len());
            let field = if self.state == State::Plain {
                if column == 0 && at == start && byte != self.delimiter {
                    // There is nothing on this line. It may be the line feed
                    // of a line that a carriage return has ended.
                    let feed = byte == b'\n' && self.after_return == at;
                    if feed || !(self.width == 1 && self.started) {
                        start = after;
                        if byte == b'\r' {
                            self.after_return = start;
                        }
                        if !feed {
                            self.number = self.number.wrapping_add(1);
                            self.empty_lines = self.empty_lines.wrapping_add(1);
                        }
                        self.row_start = start;
                        self.state = State::Start;
                        continue;
                    }
                }
                Field::of(start, at.wrapping_sub(start))
            } else {
                self.put_together(input, start, at)?
            };
            if column < self.keep {
                self.fields.push(field);
            }
            column = column.wrapping_add(1);
            start = after;
            self.state = State::Start;
            if byte != self.delimiter {
                if byte == b'\r' {
                    self.after_return = start;
                }
                self.end_row(column);
                self.row_start = start;
                column = 0;
                if self.rows >= stop || self.fields.len() >= fields {
                    self.start = start;
                    self.from = start;
                    self.column = 0;
                    return Ok(Reached::Limit);
                }
            }
        }
        self.start = start;
        self.column = column;
        self.put_aside(input, bytes.len())?;
        Ok(Reached::End)
    }

    /// Looks for the quote that ends a field, from where that was last done.
    /// Gives whether it was found. The byte after a quote at the end of what
    /// is to be cut is looked at as well.
    fn pass_quoted(
        &mut self,
        input: &[u8],
        end: usize,
        start: usize,
        mut doubled: usize,
        last: bool,
    ) -> Result<bool, SplitError> {
        let bytes = input.get(..end).unwrap_or(&[]);
        let mut from = self.from;
        let found = loop {
            let Some(at) = find_quote(bytes, from) else {
                from = from.max(bytes.len());
                break false;
            };
            match input.get(at.wrapping_add(1)) {
                Some(b'"') => {
                    doubled = doubled.saturating_add(1);
                    from = at.wrapping_add(2);
                }
                None if !last => {
                    // What comes after it decides.
                    from = at;
                    break false;
                }
                _ => {
                    from = at;
                    break true;
                }
            }
        };
        let len = from
            .saturating_sub(start)
            .saturating_sub(1)
            .saturating_sub(doubled);
        self.start = start;
        self.from = from;
        self.state = State::Quoted { doubled };
        if len > MAX_TEXT_BYTES {
            return Err(SplitError::TooLong);
        }
        if found {
            self.from = from.wrapping_add(1);
            self.state = State::Closed {
                doubled,
                close: from,
            };
        }
        Ok(found)
    }

    /// The field that begins at `start` and ends before `end`, one with
    /// quotes or a long one.
    #[cold]
    #[inline(never)]
    fn put_together(&mut self, input: &str, start: usize, end: usize) -> Result<Field, SplitError> {
        let from = self.joined.len();
        if let State::Closed { doubled, close } = self.state {
            if self.quoted_len(start, end) > MAX_TEXT_BYTES {
                return Err(SplitError::TooLong);
            }
            let inner = start.wrapping_add(1);
            let after = close.wrapping_add(1);
            if doubled == 0 && after >= end {
                return Ok(Field::of(inner, close.saturating_sub(inner)));
            }
            // The quotes in there come in pairs, the first of which stays.
            let quoted = input.as_bytes().get(..close).unwrap_or(&[]);
            let mut at = inner;
            while let Some(quote) = find_quote(quoted, at) {
                let next = quote.wrapping_add(1);
                self.join(input, at, next);
                at = next.wrapping_add(1);
            }
            self.join(input, at, close);
            self.join(input, after, end);
        } else {
            let long = core::mem::take(&mut self.long);
            self.join(&long, 0, long.len());
        }
        let mut field = Field::of(from, self.joined.len().saturating_sub(from));
        field.place |= ELSEWHERE;
        Ok(field)
    }

    #[inline(never)]
    fn join(&mut self, input: &str, start: usize, end: usize) {
        self.joined.push_str(input.get(start..end).unwrap_or(""));
    }

    /// The length of a field whose closing quote has come, up to `end`.
    fn quoted_len(&self, start: usize, end: usize) -> usize {
        match self.state {
            State::Closed { doubled, close } => close
                .saturating_sub(start)
                .saturating_sub(1)
                .saturating_sub(doubled)
                .saturating_add(end.saturating_sub(close).saturating_sub(1)),
            State::Start | State::Plain | State::Quoted { .. } | State::Long => 0,
        }
    }

    fn end_row(&mut self, column: usize) {
        self.number = self.number.saturating_add(1);
        self.started = true;
        if self.header {
            self.header = false;
            self.keep = self.width;
            return;
        }
        self.rows = self.rows.saturating_add(1);
        if self.width == 0 {
            self.ends.push(self.fields.len());
        } else if column != self.width {
            self.end_uneven_row(column);
        }
    }

    #[cold]
    #[inline(never)]
    fn end_uneven_row(&mut self, column: usize) {
        match self.width.checked_sub(column) {
            Some(missing) => {
                self.short_rows = self.short_rows.saturating_add(1);
                self.fields
                    .resize(self.fields.len().saturating_add(missing), Field::BLANK);
            }
            None => {
                let extra = column.saturating_sub(self.width);
                self.extra_fields = self
                    .extra_fields
                    .saturating_add(u64::try_from(extra).unwrap_or(u64::MAX));
            }
        }
    }

    /// Where a field has not ended, makes sure that it does not take up more
    /// and more room.
    fn put_aside(&mut self, input: &str, end: usize) -> Result<(), SplitError> {
        match self.state {
            State::Plain if end.saturating_sub(self.start) > MAX_TEXT_BYTES => {
                self.long = String::from(input.get(self.start..end).unwrap_or(""));
                self.state = State::Long;
                self.start = end;
            }
            State::Long => self.start = end,
            State::Closed { .. } if self.quoted_len(self.start, end) > MAX_TEXT_BYTES => {
                return Err(SplitError::TooLong);
            }
            _ => {}
        }
        Ok(())
    }
}

const fn repeated(byte: u8) -> u64 {
    u64::from_le_bytes([byte; 8])
}

/// Eight bytes as a word. Where the text ends early, the rest are zero.
fn word_at(bytes: &[u8], at: usize) -> u64 {
    let rest = bytes.get(at..).unwrap_or(&[]);
    if let Some(all) = rest.first_chunk::<8>() {
        return u64::from_le_bytes(*all);
    }
    let mut word = [0_u8; 8];
    for (slot, byte) in word.iter_mut().zip(rest) {
        *slot = *byte;
    }
    u64::from_le_bytes(word)
}

/// The bytes that end a field without quotes, one after another.
struct Marks {
    delimiter: u64,
    /// A multiple of eight.
    base: usize,
    /// One bit for each of the eight bytes there that ends a field and has
    /// not been given.
    found: u64,
}

impl Marks {
    /// The ones from an offset on.
    fn new(delimiter: u8, bytes: &[u8], from: usize) -> Self {
        let mut marks = Self {
            delimiter: repeated(delimiter),
            base: from & !7,
            found: 0,
        };
        let passed = u32::try_from(from & 7).unwrap_or(0) << 3;
        marks.found = marks.in_word(word_at(bytes, marks.base)) & u64::MAX.wrapping_shl(passed);
        marks
    }

    const fn in_word(&self, word: u64) -> u64 {
        let others = nonzero(word ^ self.delimiter)
            & nonzero(word ^ repeated(b'\n'))
            & nonzero(word ^ repeated(b'\r'));
        !others & HIGH
    }

    #[inline]
    fn next(&mut self, bytes: &[u8]) -> Option<usize> {
        while self.found == 0 {
            let next = self.base.wrapping_add(8);
            if next >= bytes.len() {
                return None;
            }
            self.base = next;
            self.found = self.in_word(word_at(bytes, next));
        }
        let place = (self.found.trailing_zeros() >> 3) as usize;
        self.found &= self.found.wrapping_sub(1);
        Some(self.base.wrapping_add(place))
    }
}

/// The next quote.
#[inline(never)]
fn find_quote(bytes: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    while let Some(chunk) = bytes.get(at..).and_then(<[u8]>::first_chunk::<8>) {
        let found = !nonzero(u64::from_le_bytes(*chunk) ^ repeated(b'"')) & HIGH;
        if found != 0 {
            return at.checked_add((found.trailing_zeros() >> 3) as usize);
        }
        at = at.wrapping_add(8);
    }
    let rest = bytes.get(at..)?;
    at.checked_add(rest.iter().position(|byte| *byte == b'"')?)
}

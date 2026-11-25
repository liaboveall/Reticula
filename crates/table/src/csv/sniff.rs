//! Finding out what delimited text holds from the start of it.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use super::infer::Seen;
use super::split::{Field, Reached, SplitError, Splitter};
use super::value::{self, Class, classify};
use super::{Options, SAMPLE_BYTES};
use crate::hash;
use crate::types::{ColumnType, MAX_COLUMNS, MAX_NAME_BYTES};

/// What can separate fields. Of two that do as well, the one before.
const DELIMITERS: [u8; 4] = *b",\t;|";
/// Rows of a preview.
const PREVIEW_ROWS: usize = 50;
/// Texts that are no longer than this on average can have a dictionary even
/// where most of them differ: with more rows than were looked at they may
/// repeat, and if they do not, little is lost.
const SHORT_TEXT: usize = 16;
/// The mark that some programs put at the start of a text.
const MARK: &[u8] = "\u{feff}".as_bytes();

/// What a column seems to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SniffedColumn {
    /// Its name.
    pub name: String,
    /// The type of the values there are so far. `None` when there are none.
    pub kind: Option<ColumnType>,
}

/// What a file seems to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sniffed {
    /// What separates the fields.
    pub delimiter: u8,
    /// Whether the first row holds the names of the columns.
    pub header: bool,
    /// The columns. Empty when there is not one whole row.
    pub columns: Vec<SniffedColumn>,
    /// The texts of the first rows, without the one with the names.
    pub preview: Vec<Vec<String>>,
    /// Whether the text is UTF-8. When it is not, the rest is a guess.
    pub valid_utf8: bool,
    /// Bytes a row takes, on average.
    pub row_bytes: usize,
}

/// Looks at the start of a file, of which a mebibyte is used. It may end
/// anywhere, unless it is the whole file.
#[must_use]
pub fn sniff(head: &[u8], whole_file: bool, options: &Options) -> Sniffed {
    let head = Head::new(head, whole_file);
    let text = if head.is_valid {
        Cow::Borrowed(head.text)
    } else {
        String::from_utf8_lossy(head.bytes)
    };
    let sample = Sample::new(&text, head.is_whole, options);
    let mut columns = Vec::new();
    for index in 0..sample.width.min(MAX_COLUMNS) {
        let is_taken = |name: &str| {
            columns
                .iter()
                .any(|column: &SniffedColumn| column.name == name)
        };
        let name = sample.name(index, &is_taken);
        let kind = sample.kind(index);
        columns.push(SniffedColumn { name, kind });
    }
    let mut preview = Vec::new();
    for row in (sample.first_row()..sample.len()).take(PREVIEW_ROWS) {
        let mut texts = Vec::new();
        for column in 0..columns.len() {
            texts.push(String::from(sample.text(row, column)));
        }
        preview.push(texts);
    }
    Sniffed {
        delimiter: sample.delimiter,
        header: sample.header,
        columns,
        preview,
        valid_utf8: head.is_valid,
        row_bytes: sample.row_bytes(),
    }
}

/// The start of a file.
pub(super) struct Head<'a> {
    /// What of it is used,
    bytes: &'a [u8],
    /// and what of that is text, up to what is not.
    pub(super) text: &'a str,
    /// Whether that is all.
    pub(super) is_valid: bool,
    /// Whether the file ends there.
    pub(super) is_whole: bool,
}

impl<'a> Head<'a> {
    pub(super) fn new(bytes: &'a [u8], whole_file: bool) -> Self {
        let used = bytes.get(..SAMPLE_BYTES).unwrap_or(bytes);
        let is_whole = whole_file && used.len() == bytes.len();
        let bytes = used.strip_prefix(MARK).unwrap_or(used);
        let (text, cut) = valid_start(bytes);
        Self {
            bytes,
            text,
            is_valid: text.len() == bytes.len() || (cut && !is_whole),
            is_whole,
        }
    }
}

/// What of some bytes is text, and whether the rest could become so with
/// more bytes after it.
#[inline(never)]
pub(super) fn valid_start(bytes: &[u8]) -> (&str, bool) {
    match core::str::from_utf8(bytes) {
        Ok(text) => (text, true),
        Err(error) => {
            let valid = bytes.get(..error.valid_up_to()).unwrap_or(&[]);
            let text = core::str::from_utf8(valid).unwrap_or("");
            (text, error.error_len().is_none())
        }
    }
}

/// The rows at the start of a file.
pub(super) struct Sample<'a> {
    input: &'a str,
    splitter: Splitter,
    /// Why there are no more rows, when they cannot be cut.
    pub(super) trouble: Option<SplitError>,
    markers: bool,
    pub(super) delimiter: u8,
    pub(super) header: bool,
    /// Number of columns, which can be one too many.
    pub(super) width: usize,
}

impl<'a> Sample<'a> {
    /// `whole` says that the text ends where the file does.
    pub(super) fn new(input: &'a str, whole: bool, options: &Options) -> Self {
        let delimiter = match options.delimiter {
            Some(byte) if byte.is_ascii() && !matches!(byte, 0 | b'"' | b'\r' | b'\n') => byte,
            _ => detect(input, whole),
        };
        let mut sample = Self {
            input,
            splitter: Splitter::of_any_width(delimiter),
            trouble: None,
            markers: options.blank_markers,
            delimiter,
            header: false,
            width: 0,
        };
        sample.trouble = cut(&mut sample.splitter, input, whole).err();
        sample.header = match options.header {
            Some(header) => header,
            None => sample.begins_with_names(),
        };
        sample.width = sample.row_len(0);
        if !sample.header {
            for row in 0..sample.len() {
                sample.width = sample.width.max(sample.row_len(row));
            }
        }
        sample
    }

    /// The row the text cannot be cut in, or ends in.
    pub(super) const fn last_row(&self) -> u64 {
        self.splitter.number
    }

    /// Number of rows.
    pub(super) const fn len(&self) -> usize {
        self.splitter.ends.len()
    }

    /// The first row that is not one of names.
    fn first_row(&self) -> usize {
        usize::from(self.header)
    }

    /// How many fields a row has.
    fn row_len(&self, row: usize) -> usize {
        self.splitter.row(row).len()
    }

    fn text(&self, row: usize, column: usize) -> &str {
        let field = self.splitter.row(row).get(column);
        let field = field.copied().unwrap_or(Field::BLANK);
        field.text(self.input, &self.splitter.joined)
    }

    /// Whether the first row is one of names, as far as that can be told.
    /// There is none where a field of it could be a value like those below.
    fn begins_with_names(&self) -> bool {
        let len = self.row_len(0);
        if len == 0 || len < most_common(&self.splitter, 1).fields {
            return false;
        }
        // Names of columns that could not hold them. Where nothing is or only
        // a sign for that, though there are values below. Names that were there.
        let (mut told, mut missing, mut again) = (0_usize, 0_usize, 0_usize);
        // Years that are no values: the last one, how many there are, and
        // whether they are near to each other.
        let (mut year, mut years, mut near) = (0_i64, 0_usize, true);
        let mut texts = false;
        for column in 0..len {
            let name = self.text(0, column);
            let seen = self.seen(column, 1);
            if is_no_name(name.as_bytes()) {
                let above_values = seen.kind().is_some() || self.len() == 1;
                missing = missing.wrapping_add(usize::from(above_values));
                continue;
            }
            let class = classify(name.as_bytes());
            let is_text = class == Class::Text;
            texts |= is_text;
            let was_there = (0..column).any(|before| self.text(0, before) == name);
            again = again.wrapping_add(usize::from(is_text && was_there));
            let mut with = seen;
            with.add(class);
            if seen.kind().is_some() && !seen.is_text() && with.is_text() {
                told = told.wrapping_add(1);
            } else if !is_text {
                let next = year_of(class);
                if next == 0 || !self.has_other_than_years(column) {
                    return false;
                }
                let gap = next.wrapping_sub(year).unsigned_abs();
                near &= years == 0 || (1..=10).contains(&gap);
                years = years.wrapping_add(1);
                year = next;
            }
        }
        if told > 0 {
            missing.max(again) <= len >> 1
        } else if years > 0 {
            years > 1 && near && missing <= 1 && (texts || missing == 1)
        } else {
            // The numbers of the rows often have no name.
            let numbers = self.text(0, 0).is_empty() && matches!(self.text(1, 0), "0" | "1");
            again == 0 && missing == usize::from(numbers)
        }
    }

    /// Whether some value of a column does not look like a year.
    fn has_other_than_years(&self, column: usize) -> bool {
        (1..self.len()).any(|row| {
            let text = self.text(row, column);
            let marker = self.markers && value::is_blank_marker(text.as_bytes());
            !text.is_empty() && !marker && year_of(classify(text.as_bytes())) == 0
        })
    }

    /// The name of a column, which is none of those that are taken.
    pub(super) fn name(&self, index: usize, is_taken: &dyn Fn(&str) -> bool) -> String {
        let given = if self.header { self.text(0, index) } else { "" };
        let mut name = String::from(cut_name(given));
        // The number to go on from: as many had that name before.
        let mut number = 0_usize;
        if name.is_empty() {
            let _ = write!(name, "Column {}", index.saturating_add(1));
        } else {
            for before in 0..index {
                number = number.saturating_add(usize::from(self.text(0, before) == given));
            }
        }
        number = number.max(1);
        let len = name.len();
        while is_taken(&name) {
            number = number.saturating_add(1);
            name.truncate(len);
            let _ = write!(name, " ({number})");
        }
        name
    }

    /// The type of a column's values.
    pub(super) fn kind(&self, column: usize) -> Option<ColumnType> {
        self.seen(column, self.first_row()).kind()
    }

    /// What the values of a column are from a row on.
    #[inline(never)]
    fn seen(&self, column: usize, first: usize) -> Seen {
        let mut seen = Seen::default();
        for row in first..self.len() {
            seen.look(self.text(row, column).as_bytes(), self.markers);
            if seen.is_text() {
                break;
            }
        }
        seen
    }

    /// Whether the texts of a column should have a dictionary.
    pub(super) fn repeats(&self, column: usize) -> bool {
        let rows = self.len().saturating_sub(self.first_row());
        // Hashes of the texts, each where it says or soon after.
        let places = rows.saturating_mul(2).next_power_of_two();
        let mut taken = alloc::vec![0_u64; places];
        let (mut different, mut texts, mut bytes) = (0_usize, 0_usize, 0_usize);
        for row in self.first_row()..self.len() {
            let text = self.text(row, column);
            if text.is_empty() {
                continue;
            }
            texts = texts.saturating_add(1);
            bytes = bytes.saturating_add(text.len());
            // Zero says that a place is free.
            let hash = hash::of_text(text.as_bytes()) | 1;
            let mut place = usize::try_from(hash >> 33).unwrap_or(0);
            loop {
                place = place.wrapping_add(1) & places.wrapping_sub(1);
                match taken.get_mut(place) {
                    Some(slot) if *slot == 0 => {
                        *slot = hash;
                        different = different.saturating_add(1);
                        break;
                    }
                    Some(slot) if *slot != hash => {}
                    _ => break,
                }
            }
        }
        different <= rows >> 1 || bytes <= texts.saturating_mul(SHORT_TEXT)
    }

    /// Bytes a row takes, on average.
    pub(super) fn row_bytes(&self) -> usize {
        let bytes = self.splitter.row_start;
        bytes.checked_div(self.len()).unwrap_or(self.input.len())
    }
}

/// Whether a text is nothing, or only a sign for that.
fn is_no_name(text: &[u8]) -> bool {
    text.iter().all(|byte| b" -.?*_".contains(byte)) || value::is_blank_marker(text)
}

/// The year that a value could be. 0 when it could be none.
const fn year_of(class: Class) -> i64 {
    match class {
        Class::Integer {
            value: year @ 1_800..=2_100,
            ..
        } => year,
        _ => 0,
    }
}

/// A name that is not too long, not even with a number after it.
fn cut_name(name: &str) -> &str {
    let mut end = (MAX_NAME_BYTES - 8).min(name.len());
    while !name.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    name.get(..end).unwrap_or("")
}

fn cut(splitter: &mut Splitter, input: &str, whole: bool) -> Result<Reached, SplitError> {
    splitter.run(input, input.len(), whole, u64::MAX, usize::MAX)
}

/// The number of fields that most rows have.
struct MostCommon {
    fields: usize,
    /// Rows that have it,
    rows: u32,
    /// and all of them.
    of: u32,
}

/// Counts the rows from one on.
#[inline(never)]
fn most_common(splitter: &Splitter, first: usize) -> MostCommon {
    let mut counts = [0_u32; MAX_COLUMNS + 2];
    let mut common = MostCommon {
        fields: 0,
        rows: 0,
        of: 0,
    };
    for row in first..splitter.ends.len() {
        common.of = common.of.saturating_add(1);
        if let Some(count) = counts.get_mut(splitter.row(row).len()) {
            *count = count.saturating_add(1);
        }
    }
    for (fields, count) in counts.into_iter().enumerate() {
        if count > 0 && count >= common.rows {
            common.fields = fields;
            common.rows = count;
        }
    }
    common
}

/// The delimiter with which most rows have the same number of fields.
fn detect(input: &str, whole: bool) -> u8 {
    let mut best = (0_u64, 0_usize, b',');
    for delimiter in DELIMITERS {
        let mut splitter = Splitter::of_any_width(delimiter);
        // Where a quote seems to open and never to close, the rows before it
        // say little.
        let _ = cut(&mut splitter, input, whole);
        let common = most_common(&splitter, 0);
        let bytes = u64::try_from(splitter.row_start).unwrap_or(0);
        let score = u64::from(common.rows)
            .saturating_mul(bytes)
            .checked_div(u64::from(common.of))
            .unwrap_or(0);
        if common.fields > 1 && (score, common.fields) > (best.0, best.1) {
            best = (score, common.fields, delimiter);
        }
    }
    best.2
}

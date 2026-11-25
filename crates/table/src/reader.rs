//! Reading a table: single cells through a cache of pages, whole blocks
//! straight from the file.
//!
//! Every page is checked against its checksum the first time it is read. A
//! read that needs bytes that are not here yet fails with
//! [`ReadError::Missing`] and notes them; [`Reader::take_missing`] tells
//! what to fetch, and the read can then be tried again.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use rustc_hash::FxHasher;
use sha2::{Digest, Sha256};

use crate::bits;
use crate::hash;
use crate::layout::{
    self, Block, Column, DictionaryPlace, Directory, Encoding, HEADER_LEN, Malformed, PAGE,
    PAGE_BYTES, TRAILER_LEN, Trailer,
};
use crate::source::{ByteRange, Source, SourceError};
use crate::types::CellValue;

/// Bytes of pages that are kept.
const CACHE_BYTES: usize = 32 << 20;
/// Pages that are read together.
const CLUSTER: u64 = 4;
/// A dictionary of at most so many bytes is kept as soon as a cell needs it.
const SMALL_DICTIONARY: u64 = 256 << 10;
/// Entries of such a dictionary.
const SMALL_ENTRIES: u32 = 65_536;
/// Bytes of all such dictionaries together.
const SMALL_DICTIONARIES: usize = 16 << 20;

/// Why something could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Not all of the bytes are here yet. They are noted.
    Missing,
    /// The file is damaged.
    Corrupt(&'static str),
    /// The file cannot be read.
    Failed,
    /// There is not enough memory.
    OutOfMemory,
}

impl From<Malformed> for ReadError {
    fn from(error: Malformed) -> Self {
        Self::Corrupt(error.0)
    }
}

/// The texts of a dictionary, in memory.
#[derive(Debug, Default)]
pub struct Dictionary {
    ends: Vec<u32>,
    bytes: Vec<u8>,
}

impl Dictionary {
    /// Number of texts.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The bytes of a text. What is not an entry is empty.
    #[must_use]
    pub fn bytes_of(&self, code: usize) -> &[u8] {
        let end = |code: usize| self.ends.get(code).map(|end| *end as usize);
        let start = code.checked_sub(1).and_then(end).unwrap_or(0);
        end(code)
            .and_then(|end| self.bytes.get(start..end))
            .unwrap_or(&[])
    }

    /// A text. One that is not text is empty.
    #[must_use]
    pub fn text(&self, code: usize) -> &str {
        core::str::from_utf8(self.bytes_of(code)).unwrap_or("")
    }

    fn memory(&self) -> usize {
        self.ends
            .capacity()
            .saturating_mul(4)
            .saturating_add(self.bytes.capacity())
    }
}

/// A dictionary that is being read.
#[derive(Debug)]
struct Loading {
    ends: Vec<u8>,
    dictionary: Dictionary,
}

#[derive(Debug, Default)]
enum Held {
    #[default]
    None,
    Loading(Loading),
    Ready(Dictionary),
}

type PageMap = HashMap<u32, u32, BuildHasherDefault<FxHasher>>;

/// The pages that were read last.
#[derive(Debug, Default)]
struct PageCache {
    bytes: Vec<u8>,
    owners: Vec<u32>,
    used: Vec<bool>,
    map: PageMap,
    hand: usize,
}

impl PageCache {
    fn page(&self, slot: u32) -> Option<&[u8]> {
        let start = (slot as usize).checked_mul(PAGE)?;
        self.bytes.get(start..start.checked_add(PAGE)?)
    }

    fn find(&mut self, page: u32) -> Option<u32> {
        let slot = *self.map.get(&page)?;
        if let Some(used) = self.used.get_mut(slot as usize) {
            *used = true;
        }
        Some(slot)
    }

    /// A slot for a page that is not here: a new one while there is room,
    /// otherwise that of a page nobody asked for lately.
    fn slot(&mut self, page: u32) -> Option<u32> {
        let count = self.owners.len();
        let slot = if self.bytes.len() < CACHE_BYTES {
            self.bytes.try_reserve(PAGE).ok()?;
            self.bytes.resize(self.bytes.len().saturating_add(PAGE), 0);
            self.owners.push(page);
            self.used.push(false);
            count
        } else {
            let mut slot = self.hand;
            // Two rounds at most: the first takes the marks away.
            for _ in 0..count.saturating_mul(2) {
                slot = self.hand;
                self.hand = self.hand.saturating_add(1);
                if self.hand >= count {
                    self.hand = 0;
                }
                match self.used.get_mut(slot) {
                    Some(used) if *used => *used = false,
                    _ => break,
                }
            }
            let owner = self.owners.get_mut(slot)?;
            self.map.remove(owner);
            *owner = page;
            slot
        };
        let slot = u32::try_from(slot).ok()?;
        self.map.insert(page, slot);
        Some(slot)
    }

    fn insert(&mut self, page: u32, from: &[u8]) -> Option<u32> {
        let slot = self.slot(page)?;
        let start = (slot as usize).checked_mul(PAGE)?;
        self.bytes
            .get_mut(start..start.checked_add(PAGE)?)?
            .copy_from_slice(from);
        Some(slot)
    }
}

/// A block as it was read, and its values unpacked.
#[derive(Debug)]
pub struct BlockBuffer {
    raw: Vec<u8>,
    /// Where the block begins in `raw`, which holds whole pages.
    skip: usize,
    words: Vec<u64>,
    nulls: Vec<u64>,
    block: Block,
    factor: u64,
    rows: usize,
    /// Where the texts begin in the block.
    texts: usize,
}

impl Default for BlockBuffer {
    fn default() -> Self {
        Self {
            raw: Vec::new(),
            skip: 0,
            words: Vec::new(),
            nulls: Vec::new(),
            block: Block::BLANK,
            factor: 1,
            rows: 0,
            texts: 0,
        }
    }
}

impl BlockBuffer {
    /// The block's record.
    #[must_use]
    pub const fn block(&self) -> &Block {
        &self.block
    }

    /// Number of rows.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// One word for each row: the packed number of an integer, the bits of a
    /// floating-point number, or where a text ends. Empty in a blank block.
    #[must_use]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// The blank cells, one bit each, in 64-bit words.
    #[must_use]
    pub fn nulls(&self) -> &[u64] {
        &self.nulls
    }

    /// What the packed numbers are to be multiplied with.
    #[must_use]
    pub const fn factor(&self) -> u64 {
        self.factor
    }

    /// Whether a cell is blank.
    #[must_use]
    pub fn is_blank(&self, row: usize) -> bool {
        self.nulls
            .get(row >> 6)
            .is_none_or(|word| word.wrapping_shr(low6(row)) & 1 == 1)
    }

    /// The integer of a packed cell.
    #[must_use]
    pub fn int(&self, row: usize) -> i64 {
        self.block
            .value(self.words.get(row).copied().unwrap_or(0), self.factor)
    }

    /// The code of a cell of a block of codes.
    #[must_use]
    pub fn code(&self, row: usize) -> usize {
        usize::try_from(self.int(row)).unwrap_or(usize::MAX)
    }

    /// The number of a cell of a raw block.
    #[must_use]
    pub fn float(&self, row: usize) -> f64 {
        f64::from_bits(self.words.get(row).copied().unwrap_or(0))
    }

    /// The bytes of a cell of a block of texts.
    #[must_use]
    pub fn text(&self, row: usize) -> &[u8] {
        let end = |row: usize| {
            self.words
                .get(row)
                .and_then(|word| usize::try_from(*word).ok())
        };
        let base = self.skip.saturating_add(self.texts);
        let start = row.checked_sub(1).and_then(end).unwrap_or(0);
        end(row)
            .and_then(|end| {
                self.raw
                    .get(base.checked_add(start)?..base.checked_add(end)?)
            })
            .unwrap_or(&[])
    }
}

fn low6(row: usize) -> u32 {
    u32::try_from(row & 63).unwrap_or(0)
}

/// An open table.
#[derive(Debug)]
pub struct Reader {
    directory: Directory,
    cache: PageCache,
    /// One bit for each page that was checked.
    checked: Vec<u64>,
    dictionaries: Vec<Held>,
    missing: Vec<ByteRange>,
    scratch: Vec<u8>,
    /// Bytes of the dictionaries that were kept because they are small.
    small: usize,
}

/// What opening a file came to.
#[derive(Debug)]
pub enum Opened {
    /// It is open.
    Ready(Box<Reader>),
    /// These bytes have to be here first.
    Need(ByteRange),
}

impl Reader {
    /// Opens a file of that length.
    ///
    /// # Errors
    ///
    /// [`ReadError`] when it is not a table file, is damaged, or cannot be read.
    pub fn open<S: Source>(source: &mut S, file_len: u64) -> Result<Opened, ReadError> {
        let shortest = HEADER_LEN.saturating_add(TRAILER_LEN);
        if file_len < shortest || file_len > layout::MAX_FILE.saturating_add(layout::MAX_DIRECTORY)
        {
            return Err(ReadError::Corrupt("not a table file"));
        }
        let mut bytes = [0_u8; 64];
        let at = file_len.saturating_sub(TRAILER_LEN);
        match source.read(at, &mut bytes) {
            Ok(()) => {}
            Err(SourceError::Missing) => {
                // The directory is likely to be in the same piece.
                let from = file_len.saturating_sub(256 << 10);
                return Ok(Opened::Need(ByteRange {
                    at: from,
                    len: file_len.saturating_sub(from),
                }));
            }
            Err(SourceError::Failed) => return Err(ReadError::Failed),
        }
        let trailer = Trailer::decode(&bytes, file_len)?;
        let len = usize::try_from(trailer.directory_len)
            .map_err(|_| ReadError::Corrupt("length of the directory"))?;
        let mut encoded = Vec::new();
        reserve(&mut encoded, len)?;
        encoded.resize(len, 0);
        match source.read(trailer.directory_at, &mut encoded) {
            Ok(()) => {}
            Err(SourceError::Missing) => {
                return Ok(Opened::Need(ByteRange {
                    at: trailer.directory_at,
                    len: file_len.saturating_sub(trailer.directory_at),
                }));
            }
            Err(SourceError::Failed) => return Err(ReadError::Failed),
        }
        let sha256: [u8; 32] = Sha256::digest(&encoded).into();
        if sha256 != trailer.directory_sha256 {
            return Err(ReadError::Corrupt(
                "the directory does not match its checksum",
            ));
        }
        let directory = Directory::decode(&encoded, trailer.directory_at)?;
        let mut head = [0_u8; 16];
        match source.read(0, &mut head) {
            Ok(()) if head != layout::header() => {
                return Err(ReadError::Corrupt("not a table file"));
            }
            // It says nothing that the directory does not.
            Ok(()) | Err(SourceError::Missing) => {}
            Err(SourceError::Failed) => return Err(ReadError::Failed),
        }
        let mut checked = Vec::new();
        reserve(&mut checked, directory.sums.len().div_ceil(64))?;
        checked.resize(directory.sums.len().div_ceil(64), 0);
        let mut dictionaries = Vec::new();
        dictionaries.resize_with(directory.columns.len(), Held::default);
        Ok(Opened::Ready(Box::new(Self {
            directory,
            cache: PageCache::default(),
            checked,
            dictionaries,
            missing: Vec::new(),
            scratch: Vec::new(),
            small: 0,
        })))
    }

    /// What the table holds, and where.
    #[must_use]
    pub const fn directory(&self) -> &Directory {
        &self.directory
    }

    /// Number of rows.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.directory.rows
    }

    /// The columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.directory.columns
    }

    /// The bytes that reads since the last call were missing, in order and
    /// without overlaps.
    pub fn take_missing(&mut self) -> Vec<ByteRange> {
        let mut all = core::mem::take(&mut self.missing);
        all.sort_unstable();
        let mut merged: Vec<ByteRange> = Vec::new();
        for range in all {
            match merged.last_mut() {
                Some(last) if last.end() >= range.at => {
                    last.len = last.end().max(range.end()).saturating_sub(last.at);
                }
                _ => merged.push(range),
            }
        }
        merged
    }

    /// Bytes this takes in memory, about.
    #[must_use]
    pub fn memory(&self) -> usize {
        let dictionaries: usize = self
            .dictionaries
            .iter()
            .map(|held| match held {
                Held::None => 0,
                Held::Loading(loading) => loading
                    .dictionary
                    .memory()
                    .saturating_add(loading.ends.capacity()),
                Held::Ready(dictionary) => dictionary.memory(),
            })
            .fold(0, usize::saturating_add);
        self.cache
            .bytes
            .capacity()
            .saturating_add(dictionaries)
            .saturating_add(self.scratch.capacity())
            .saturating_add(
                self.directory
                    .blocks
                    .capacity()
                    .saturating_mul(size_of::<Block>()),
            )
            .saturating_add(self.directory.sums.capacity().saturating_mul(4))
    }

    /// Checks the pages from that one on against their checksums.
    fn check(&mut self, first: u64, bytes: &[u8]) -> Result<(), ReadError> {
        let (pages, _) = bytes.as_chunks::<PAGE>();
        for (page, content) in (first..).zip(pages) {
            let page = usize::try_from(page).map_err(|_| ReadError::Corrupt("offset"))?;
            let bit = 1_u64.wrapping_shl(low6(page));
            let word = self
                .checked
                .get_mut(page >> 6)
                .ok_or(ReadError::Corrupt("offset"))?;
            if *word & bit != 0 {
                continue;
            }
            if self.directory.sums.get(page) != Some(&hash::checksum(content)) {
                return Err(ReadError::Corrupt("a page does not match its checksum"));
            }
            *word |= bit;
        }
        Ok(())
    }

    fn miss(&mut self, error: SourceError, at: u64, len: u64) -> ReadError {
        match error {
            SourceError::Missing => {
                self.missing.push(ByteRange { at, len });
                ReadError::Missing
            }
            SourceError::Failed => ReadError::Failed,
        }
    }

    /// Reads the whole pages around some bytes into `into`. Gives where the
    /// bytes begin in it.
    fn read_pages<S: Source>(
        &mut self,
        source: &mut S,
        at: u64,
        len: u64,
        into: &mut Vec<u8>,
    ) -> Result<usize, ReadError> {
        let first = at.checked_div(PAGE_BYTES).unwrap_or(0);
        let end = at
            .checked_add(len)
            .filter(|end| *end <= self.directory.data_len)
            .ok_or(ReadError::Corrupt("offset"))?;
        let from = first.saturating_mul(PAGE_BYTES);
        let size = end.next_multiple_of(PAGE_BYTES).saturating_sub(from);
        let size = usize::try_from(size).map_err(|_| ReadError::OutOfMemory)?;
        if into.len() < size {
            let more = size.saturating_sub(into.len());
            into.try_reserve(more).map_err(|_| ReadError::OutOfMemory)?;
        }
        into.resize(size, 0);
        source
            .read(from, into)
            .map_err(|error| self.miss(error, from, u64::try_from(size).unwrap_or(0)))?;
        self.check(first, into)?;
        usize::try_from(at.saturating_sub(from)).map_err(|_| ReadError::Corrupt("offset"))
    }

    /// The slot of a page, which is read when it is not here, with the pages
    /// next to it.
    fn page<S: Source>(&mut self, source: &mut S, page: u64) -> Result<u32, ReadError> {
        let key = u32::try_from(page).map_err(|_| ReadError::Corrupt("offset"))?;
        if let Some(slot) = self.cache.find(key) {
            return Ok(slot);
        }
        let first = page.saturating_sub(page.checked_rem(CLUSTER).unwrap_or(0));
        let count = u64::try_from(self.directory.sums.len())
            .unwrap_or(0)
            .saturating_sub(first)
            .min(CLUSTER);
        let mut scratch = core::mem::take(&mut self.scratch);
        let read = self.read_pages(
            source,
            first.saturating_mul(PAGE_BYTES),
            count.saturating_mul(PAGE_BYTES),
            &mut scratch,
        );
        if read.is_ok() {
            let (pages, _) = scratch.as_chunks::<PAGE>();
            for (number, content) in (first..).zip(pages) {
                let Ok(number) = u32::try_from(number) else {
                    break;
                };
                if number != key && !self.cache.map.contains_key(&number) {
                    self.cache.insert(number, content);
                }
            }
        }
        // The page that was asked for comes last, so that none of the others
        // can take its place.
        let wanted = usize::try_from(page.saturating_sub(first)).unwrap_or(usize::MAX);
        let found = read.is_ok().then(|| {
            let (pages, _) = scratch.as_chunks::<PAGE>();
            let content = pages.get(wanted)?;
            self.cache.insert(key, content)
        });
        self.scratch = scratch;
        read?;
        found.flatten().ok_or(ReadError::OutOfMemory)
    }

    /// Copies bytes out of the cache.
    fn bytes<S: Source>(
        &mut self,
        source: &mut S,
        at: u64,
        into: &mut [u8],
    ) -> Result<(), ReadError> {
        let mut at = at;
        let mut rest = into;
        while !rest.is_empty() {
            let page = at.checked_div(PAGE_BYTES).unwrap_or(0);
            let offset = usize::try_from(at.checked_rem(PAGE_BYTES).unwrap_or(0)).unwrap_or(0);
            let slot = self.page(source, page)?;
            let from = self
                .cache
                .page(slot)
                .and_then(|content| content.get(offset..))
                .ok_or(ReadError::Corrupt("offset"))?;
            let len = from.len().min(rest.len());
            let (now, later) = rest.split_at_mut_checked(len).unwrap_or((&mut [], &mut []));
            now.copy_from_slice(from.get(..len).unwrap_or(&[]));
            at = at.saturating_add(u64::try_from(len).unwrap_or(0));
            rest = later;
        }
        Ok(())
    }

    /// One packed number.
    fn packed<S: Source>(
        &mut self,
        source: &mut S,
        at: u64,
        len: u64,
        index: u64,
        width: u32,
    ) -> Result<u64, ReadError> {
        if width == 0 {
            return Ok(0);
        }
        let (byte, shift) = bits::place(index, width);
        let mut buffer = [0_u8; 9];
        let take = usize::try_from(len.saturating_sub(byte).min(9)).unwrap_or(0);
        let part = buffer.get_mut(..take).unwrap_or(&mut []);
        self.bytes(source, at.saturating_add(byte), part)?;
        Ok(bits::extract(&buffer, shift, width))
    }

    /// Adds at most `limit` bytes of a text to `text`.
    fn text_at<S: Source>(
        &mut self,
        source: &mut S,
        at: u64,
        len: u64,
        limit: usize,
        text: &mut String,
    ) -> Result<(), ReadError> {
        let len = usize::try_from(len).map_err(|_| ReadError::Corrupt("length of a text"))?;
        let whole = len <= limit;
        let len = len.min(limit);
        let mut buffer = Vec::new();
        reserve(&mut buffer, len)?;
        buffer.resize(len, 0);
        self.bytes(source, at, &mut buffer)?;
        push_utf8(&buffer, whole, text)
    }

    /// Reads one cell. Its text is added to `text`, at most `limit` bytes of it.
    ///
    /// # Errors
    ///
    /// [`ReadError`] when the cell cannot be read.
    pub fn cell<S: Source>(
        &mut self,
        source: &mut S,
        column: usize,
        row: u32,
        limit: usize,
        text: &mut String,
    ) -> Result<CellValue, ReadError> {
        if row >= self.directory.rows {
            return Ok(CellValue::Blank);
        }
        let (group, within) = self.directory.group_of(row);
        let Some(block) = self.directory.block(column, group).copied() else {
            return Ok(CellValue::Blank);
        };
        if block.encoding == Encoding::Blank {
            return Ok(CellValue::Blank);
        }
        let index = u64::from(within);
        if block.nulls_at != 0 {
            let mut byte = [0_u8; 1];
            self.bytes(source, block.nulls_at.saturating_add(index >> 3), &mut byte)?;
            if byte
                .first()
                .is_some_and(|byte| byte.wrapping_shr(within & 7) & 1 == 1)
            {
                return Ok(CellValue::Blank);
            }
        }
        let width = u32::from(block.width);
        let len = u64::from(block.len);
        match block.encoding {
            Encoding::Blank => Ok(CellValue::Blank),
            Encoding::Packed => {
                let packed = self.packed(source, block.at, len, index, width)?;
                let value = block.value(packed, block.factor());
                let place = self
                    .directory
                    .columns
                    .get(column)
                    .and_then(|column| column.dictionary);
                match place {
                    None => Ok(CellValue::Int(value)),
                    Some(place) => {
                        self.dictionary_text(source, column, place, value, limit, text)?;
                        Ok(CellValue::Text)
                    }
                }
            }
            Encoding::Raw => {
                let mut bytes = [0_u8; 8];
                self.bytes(
                    source,
                    block.at.saturating_add(index.saturating_mul(8)),
                    &mut bytes,
                )?;
                finite(f64::from_le_bytes(bytes)).map(CellValue::Float)
            }
            Encoding::Text => {
                let rows = self.directory.rows_in(group);
                let ends_len = bits::packed_len(rows as usize, width);
                let start = match index.checked_sub(1) {
                    Some(before) => self.packed(source, block.at, ends_len, before, width)?,
                    None => 0,
                };
                let end = self.packed(source, block.at, ends_len, index, width)?;
                let len = end
                    .checked_sub(start)
                    .filter(|_| end <= len.saturating_sub(ends_len))
                    .ok_or(ReadError::Corrupt("length of a text"))?;
                let at = block.at.saturating_add(ends_len).saturating_add(start);
                self.text_at(source, at, len, limit, text)?;
                Ok(CellValue::Text)
            }
        }
    }

    fn dictionary_text<S: Source>(
        &mut self,
        source: &mut S,
        column: usize,
        place: DictionaryPlace,
        code: i64,
        limit: usize,
        text: &mut String,
    ) -> Result<(), ReadError> {
        let code = u32::try_from(code)
            .ok()
            .filter(|code| *code < place.entries)
            .ok_or(ReadError::Corrupt("code of a text"))?;
        if place.bytes_len.saturating_add(place.ends_len()) <= SMALL_DICTIONARY
            && place.entries <= SMALL_ENTRIES
            && self.small < SMALL_DICTIONARIES
            && self.dictionary(column).is_none()
        {
            while !self.load_dictionary(source, column)? {}
            let memory = self.dictionary(column).map_or(0, Dictionary::memory);
            self.small = self.small.saturating_add(memory);
        }
        if let Some(Held::Ready(dictionary)) = self.dictionaries.get(column) {
            let bytes = dictionary.bytes_of(code as usize);
            let whole = bytes.len() <= limit;
            let bytes = bytes.get(..limit.min(bytes.len())).unwrap_or(&[]);
            return push_utf8(bytes, whole, text);
        }
        let width = u32::from(place.ends_width);
        let index = u64::from(code);
        let start = match index.checked_sub(1) {
            Some(before) => self.packed(source, place.ends_at, place.ends_len(), before, width)?,
            None => 0,
        };
        let end = self.packed(source, place.ends_at, place.ends_len(), index, width)?;
        let len = end
            .checked_sub(start)
            .filter(|_| end <= place.bytes_len)
            .ok_or(ReadError::Corrupt("length of a text"))?;
        self.text_at(
            source,
            place.bytes_at.saturating_add(start),
            len,
            limit,
            text,
        )
    }

    /// The dictionary of a column, when all of it is in memory.
    #[must_use]
    pub fn dictionary(&self, column: usize) -> Option<&Dictionary> {
        match self.dictionaries.get(column) {
            Some(Held::Ready(dictionary)) => Some(dictionary),
            _ => None,
        }
    }

    /// Reads another piece of a column's dictionary. Gives whether all of it
    /// is in memory, which it is at once for a column that has none.
    ///
    /// # Errors
    ///
    /// [`ReadError`] when it cannot be read. What was read already is kept
    /// when only bytes were missing.
    pub fn load_dictionary<S: Source>(
        &mut self,
        source: &mut S,
        column: usize,
    ) -> Result<bool, ReadError> {
        const PIECE: usize = 256 << 10;
        let Some(place) = self
            .directory
            .columns
            .get(column)
            .and_then(|column| column.dictionary)
        else {
            return Ok(true);
        };
        let mut loading = match self.dictionaries.get_mut(column).map(core::mem::take) {
            Some(Held::Ready(dictionary)) => {
                if let Some(slot) = self.dictionaries.get_mut(column) {
                    *slot = Held::Ready(dictionary);
                }
                return Ok(true);
            }
            Some(Held::Loading(loading)) => loading,
            Some(Held::None) | None => {
                let mut loading = Loading {
                    ends: Vec::new(),
                    dictionary: Dictionary::default(),
                };
                let ends = usize::try_from(place.ends_len()).map_err(|_| ReadError::OutOfMemory)?;
                let bytes = usize::try_from(place.bytes_len).map_err(|_| ReadError::OutOfMemory)?;
                reserve(&mut loading.ends, ends)?;
                reserve(&mut loading.dictionary.bytes, bytes)?;
                reserve(&mut loading.dictionary.ends, place.entries as usize)?;
                loading
            }
        };
        let mut scratch = core::mem::take(&mut self.scratch);
        let result = self.dictionary_piece(source, place, &mut loading, &mut scratch, PIECE);
        self.scratch = scratch;
        let done = matches!(result, Ok(true));
        let more = matches!(result, Ok(false) | Err(ReadError::Missing));
        if let Some(slot) = self.dictionaries.get_mut(column) {
            *slot = if done {
                Held::Ready(loading.dictionary)
            } else if more {
                Held::Loading(loading)
            } else {
                // What is damaged is no better the next time.
                Held::None
            };
        }
        result
    }

    fn dictionary_piece<S: Source>(
        &mut self,
        source: &mut S,
        place: DictionaryPlace,
        loading: &mut Loading,
        scratch: &mut Vec<u8>,
        piece: usize,
    ) -> Result<bool, ReadError> {
        let have = u64::try_from(loading.ends.len()).unwrap_or(0);
        let piece = u64::try_from(piece).unwrap_or(0);
        let dictionary = &mut loading.dictionary;
        if have < place.ends_len() {
            let len = place.ends_len().saturating_sub(have).min(piece);
            let skip = self.read_pages(source, place.ends_at.saturating_add(have), len, scratch)?;
            let end = skip.saturating_add(usize::try_from(len).unwrap_or(0));
            loading
                .ends
                .extend_from_slice(scratch.get(skip..end).unwrap_or(&[]));
            return Ok(false);
        }
        if dictionary.ends.is_empty() {
            let mut words = Vec::new();
            reserve(&mut words, place.entries as usize)?;
            words.resize(place.entries as usize, 0);
            bits::unpack(&loading.ends, u32::from(place.ends_width), &mut words);
            let mut last = 0_u64;
            for word in &words {
                if *word < last || *word > place.bytes_len {
                    return Err(ReadError::Corrupt("length of a text"));
                }
                last = *word;
            }
            if last != place.bytes_len || u32::try_from(last).is_err() {
                return Err(ReadError::Corrupt("length of a dictionary"));
            }
            dictionary.ends.extend(
                words
                    .iter()
                    .map(|word| u32::try_from(*word).unwrap_or(u32::MAX)),
            );
            loading.ends = Vec::new();
        }
        let have = u64::try_from(dictionary.bytes.len()).unwrap_or(0);
        if have < place.bytes_len {
            let len = place.bytes_len.saturating_sub(have).min(piece);
            let skip =
                self.read_pages(source, place.bytes_at.saturating_add(have), len, scratch)?;
            let end = skip.saturating_add(usize::try_from(len).unwrap_or(0));
            dictionary
                .bytes
                .extend_from_slice(scratch.get(skip..end).unwrap_or(&[]));
        }
        Ok(u64::try_from(dictionary.bytes.len()) == Ok(place.bytes_len))
    }

    /// Lets go of a dictionary.
    pub fn drop_dictionary(&mut self, column: usize) {
        if let Some(slot) = self.dictionaries.get_mut(column) {
            if let Held::Ready(dictionary) = slot {
                self.small = self.small.saturating_sub(dictionary.memory());
            }
            *slot = Held::None;
        }
    }

    /// Reads all cells of a column in a group.
    ///
    /// # Errors
    ///
    /// [`ReadError`] when they cannot be read.
    pub fn block<S: Source>(
        &mut self,
        source: &mut S,
        column: usize,
        group: u32,
        into: &mut BlockBuffer,
    ) -> Result<(), ReadError> {
        let block = self
            .directory
            .block(column, group)
            .copied()
            .ok_or(ReadError::Corrupt("no such block"))?;
        let rows = self.directory.rows_in(group);
        into.block = block;
        into.factor = block.factor();
        into.rows = rows as usize;
        into.texts = 0;
        let null_words = into.rows.div_ceil(64);
        into.nulls.clear();
        into.words.clear();
        if block.encoding == Encoding::Blank {
            into.nulls.resize(null_words, u64::MAX);
            return Ok(());
        }
        if block.nulls_at == 0 {
            into.nulls.resize(null_words, 0);
        } else {
            let len = layout::nulls_len(rows);
            let skip = self.read_pages(source, block.nulls_at, len, &mut into.raw)?;
            into.nulls.extend(
                into.raw
                    .get(skip..)
                    .unwrap_or(&[])
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .take(null_words)
                    .map(|chunk| u64::from_le_bytes(*chunk)),
            );
        }
        into.skip = self.read_pages(source, block.at, u64::from(block.len), &mut into.raw)?;
        let bytes = into.raw.get(into.skip..).unwrap_or(&[]);
        reserve(&mut into.words, into.rows)?;
        into.words.resize(into.rows, 0);
        let width = u32::from(block.width);
        match block.encoding {
            Encoding::Blank => {}
            Encoding::Packed => bits::unpack(bytes, width, &mut into.words),
            Encoding::Raw => {
                bits::unpack(bytes, 64, &mut into.words);
                if into
                    .words
                    .iter()
                    .any(|word| !f64::from_bits(*word).is_finite())
                {
                    return Err(ReadError::Corrupt("a number that is not finite"));
                }
            }
            Encoding::Text => {
                bits::unpack(bytes, width, &mut into.words);
                let ends_len = bits::packed_len(into.rows, width);
                into.texts = usize::try_from(ends_len).unwrap_or(usize::MAX);
                let limit = u64::from(block.len).saturating_sub(ends_len);
                let mut last = 0;
                for word in &into.words {
                    if *word < last || *word > limit {
                        return Err(ReadError::Corrupt("length of a text"));
                    }
                    last = *word;
                }
            }
        }
        Ok(())
    }
}

fn finite(value: f64) -> Result<f64, ReadError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ReadError::Corrupt("a number that is not finite"))
    }
}

fn reserve<T>(into: &mut Vec<T>, len: usize) -> Result<(), ReadError> {
    into.try_reserve(len).map_err(|_| ReadError::OutOfMemory)
}

/// Adds bytes as text. Where they were cut, they may end in the middle of a
/// character, which is left out.
fn push_utf8(bytes: &[u8], whole: bool, text: &mut String) -> Result<(), ReadError> {
    match core::str::from_utf8(bytes) {
        Ok(valid) => text.push_str(valid),
        Err(error) if !whole && error.error_len().is_none() => {
            let valid = bytes.get(..error.valid_up_to()).unwrap_or(&[]);
            text.push_str(core::str::from_utf8(valid).unwrap_or(""));
        }
        Err(_) => return Err(ReadError::Corrupt("text that is not UTF-8")),
    }
    Ok(())
}

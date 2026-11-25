//! A prototype of a read-only column store: how fast can it be in WebAssembly,
//! on one thread? Types are given, errors are not handled. It is measured by
//! spikes/tables/proto.mjs and goes away when the real one exists.
#![allow(clippy::all, static_mut_refs)]

use std::collections::HashMap;

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn file_write(at: f64, pointer: *const u8, length: usize);
    fn file_read(at: f64, pointer: *mut u8, length: usize);
}

const CHUNK: usize = 65_536;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Int,
    Decimal2,
    DateTime,
    Date,
    Bool,
    Text,
}

#[derive(Clone, Copy, Default)]
struct Block {
    at: u64,
    bytes: u32,
    base: i64,
    width: u8,
    nulls_at: u64,
    has_nulls: bool,
    min: i64,
    max: i64,
}

struct Dictionary {
    map: HashMap<u64, Vec<u32>, std::hash::BuildHasherDefault<Identity>>,
    starts: Vec<u32>,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Identity(u64);
impl std::hash::Hasher for Identity {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {}
    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ bytes.len() as u64;
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    let mut last = [0u8; 8];
    last[..chunks.remainder().len()].copy_from_slice(chunks.remainder());
    h = (h.rotate_left(5) ^ u64::from_le_bytes(last)).wrapping_mul(0x517c_c1b7_2722_0a95);
    h ^ (h >> 32)
}

impl Dictionary {
    fn new() -> Self {
        Self { map: HashMap::default(), starts: vec![0], bytes: Vec::new() }
    }
    fn get(&self, code: usize) -> &[u8] {
        &self.bytes[self.starts[code] as usize..self.starts[code + 1] as usize]
    }
    fn len(&self) -> usize {
        self.starts.len() - 1
    }
    fn code(&mut self, text: &[u8]) -> u32 {
        let h = hash(text);
        if let Some(codes) = self.map.get(&h) {
            for &code in codes {
                if &self.bytes[self.starts[code as usize] as usize..self.starts[code as usize + 1] as usize] == text {
                    return code;
                }
            }
        }
        let code = self.len() as u32;
        self.bytes.extend_from_slice(text);
        self.starts.push(self.bytes.len() as u32);
        self.map.entry(h).or_default().push(code);
        code
    }
}

struct Column {
    kind: Kind,
    values: Vec<i64>,
    nulls: Vec<u64>,
    any_null: bool,
    blocks: Vec<Block>,
    dictionary: Dictionary,
}

struct Store {
    columns: Vec<Column>,
    rows: usize,
    end: u64,
    carry: Vec<u8>,
    header_seen: bool,
    pack: Vec<u8>,
    cache: HashMap<u64, usize>,
    pages: Vec<(u64, Vec<u8>)>,
    hand: usize,
    perm: Vec<u32>,
    out: Vec<u8>,
    bitmap: Vec<u64>,
}

static mut STORE: Option<Store> = None;
static mut INPUT: Vec<u8> = Vec::new();

fn store() -> &'static mut Store {
    unsafe { STORE.as_mut().unwrap() }
}

#[unsafe(no_mangle)]
pub extern "C" fn input(length: usize) -> *mut u8 {
    unsafe {
        INPUT.resize(length, 0);
        INPUT.as_mut_ptr()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn begin(count: usize) {
    let kinds = unsafe { INPUT[..count].to_vec() };
    let columns = kinds
        .iter()
        .map(|k| Column {
            kind: match k {
                0 => Kind::Int,
                1 => Kind::Decimal2,
                2 => Kind::DateTime,
                3 => Kind::Date,
                4 => Kind::Bool,
                _ => Kind::Text,
            },
            values: Vec::with_capacity(CHUNK),
            nulls: vec![0; CHUNK / 64],
            any_null: false,
            blocks: Vec::new(),
            dictionary: Dictionary::new(),
        })
        .collect();
    unsafe {
        STORE = Some(Store { columns, rows: 0, end: 0, carry: Vec::new(), header_seen: false, pack: Vec::new(), cache: HashMap::new(), pages: Vec::new(), hand: 0, perm: Vec::new(), out: Vec::new(), bitmap: Vec::new() });
    }
}

fn digits(bytes: &[u8]) -> i64 {
    let mut value = 0i64;
    for b in bytes {
        value = value * 10 + i64::from(b - b'0');
    }
    value
}

fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn parse(kind: Kind, text: &[u8], dictionary: &mut Dictionary) -> i64 {
    match kind {
        Kind::Int => {
            let (negative, rest) = if text[0] == b'-' { (true, &text[1..]) } else { (false, text) };
            let value = digits(rest);
            if negative { -value } else { value }
        }
        Kind::Decimal2 => {
            let (negative, rest) = if text[0] == b'-' { (true, &text[1..]) } else { (false, text) };
            let mut value = 0i64;
            let mut seen = 0;
            let mut point = false;
            for &b in rest {
                if b == b'.' {
                    point = true;
                } else {
                    value = value * 10 + i64::from(b - b'0');
                    if point {
                        seen += 1;
                    }
                }
            }
            while seen < 2 {
                value *= 10;
                seen += 1;
            }
            if negative { -value } else { value }
        }
        Kind::Date => days(digits(&text[0..4]), digits(&text[5..7]), digits(&text[8..10])),
        Kind::DateTime => days(digits(&text[0..4]), digits(&text[5..7]), digits(&text[8..10])) * 86_400 + digits(&text[11..13]) * 3600 + digits(&text[14..16]) * 60 + digits(&text[17..19]),
        Kind::Bool => i64::from(text[0] == b't'),
        Kind::Text => i64::from(dictionary.code(text)),
    }
}

fn flush(column: &mut Column, end: &mut u64, pack: &mut Vec<u8>) {
    let count = column.values.len();
    if count == 0 {
        return;
    }
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for &v in &column.values {
        min = min.min(v);
        max = max.max(v);
    }
    let range = (max as u64).wrapping_sub(min as u64);
    let width = 64 - range.leading_zeros();
    pack.clear();
    pack.resize((count * width as usize).div_ceil(64) * 8 + 8, 0);
    if width > 0 {
        let mut bit = 0usize;
        for &v in &column.values {
            let value = (v as u64).wrapping_sub(min as u64);
            let byte = bit >> 3;
            let shift = bit & 7;
            let old = u64::from_le_bytes(pack[byte..byte + 8].try_into().unwrap());
            pack[byte..byte + 8].copy_from_slice(&(old | (value << shift)).to_le_bytes());
            if shift + width as usize > 64 {
                pack[byte + 8] |= (value >> (64 - shift)) as u8;
            }
            bit += width as usize;
        }
    }
    let mut block = Block { at: *end, bytes: pack.len() as u32, base: min, width: width as u8, nulls_at: 0, has_nulls: column.any_null, min, max };
    unsafe { file_write(*end as f64, pack.as_ptr(), pack.len()) };
    *end += pack.len() as u64;
    if column.any_null {
        block.nulls_at = *end;
        unsafe { file_write(*end as f64, column.nulls.as_ptr().cast(), CHUNK / 8) };
        *end += (CHUNK / 8) as u64;
        column.nulls.iter_mut().for_each(|w| *w = 0);
        column.any_null = false;
    }
    column.blocks.push(block);
    column.values.clear();
}

impl Store {
    fn field(&mut self, index: usize, text: &[u8]) {
        let column = &mut self.columns[index];
        if text.is_empty() {
            let row = column.values.len();
            column.nulls[row >> 6] |= 1 << (row & 63);
            column.any_null = true;
            let last = column.values.last().copied().unwrap_or(0);
            column.values.push(last);
        } else {
            let value = parse(column.kind, text, &mut column.dictionary);
            column.values.push(value);
        }
    }
    fn row_done(&mut self) {
        self.rows += 1;
        if self.rows % CHUNK == 0 {
            for column in &mut self.columns {
                flush(column, &mut self.end, &mut self.pack);
            }
        }
    }
    /// Parses whole lines; gives back how many bytes it took.
    fn lines(&mut self, data: &[u8], last: bool) -> usize {
        let mut at = 0;
        let mut quoted = Vec::new();
        let count = self.columns.len();
        'line: while at < data.len() {
            // Is the whole line here? Quotes make that more than a search for a newline.
            let line = at;
            let mut end = at;
            let mut inside = false;
            loop {
                if end >= data.len() {
                    if last {
                        break;
                    }
                    return line;
                }
                let b = data[end];
                if b == b'"' {
                    inside = !inside;
                } else if b == b'\n' && !inside {
                    break;
                }
                end += 1;
            }
            let row = &data[line..end];
            at = end + 1;
            if !self.header_seen {
                self.header_seen = true;
                continue 'line;
            }
            let mut p = 0;
            for index in 0..count {
                if p < row.len() && row[p] == b'"' {
                    quoted.clear();
                    p += 1;
                    loop {
                        if row[p] == b'"' {
                            if p + 1 < row.len() && row[p + 1] == b'"' {
                                quoted.push(b'"');
                                p += 2;
                                continue;
                            }
                            p += 1;
                            break;
                        }
                        quoted.push(row[p]);
                        p += 1;
                    }
                    let text = std::mem::take(&mut quoted);
                    self.field(index, &text);
                    quoted = text;
                } else {
                    let start = p;
                    while p < row.len() && row[p] != b',' {
                        p += 1;
                    }
                    let text = &row[start..p];
                    // The borrow of the input ends here.
                    let text: &[u8] = unsafe { std::slice::from_raw_parts(text.as_ptr(), text.len()) };
                    self.field(index, text);
                }
                p += 1;
            }
            self.row_done();
        }
        at.min(data.len())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn feed(length: usize, last: bool) {
    let s = store();
    let data = unsafe { &INPUT[..length] };
    if s.carry.is_empty() {
        let used = s.lines(data, last);
        s.carry.extend_from_slice(&data[used..]);
    } else {
        let mut all = std::mem::take(&mut s.carry);
        // Finish the first line in a small buffer, then read the rest in place.
        let cut = data.iter().position(|&b| b == b'\n').map_or(data.len(), |p| p + 1);
        all.extend_from_slice(&data[..cut]);
        let used = s.lines(&all, false);
        if used < all.len() {
            all.drain(..used);
            all.extend_from_slice(&data[cut..]);
            let used = s.lines(&all, last);
            all.drain(..used);
            s.carry = all;
            return;
        }
        let used = s.lines(&data[cut..], last);
        s.carry.clear();
        s.carry.extend_from_slice(&data[cut + used..]);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn finish() -> usize {
    let s = store();
    for column in &mut s.columns {
        flush(column, &mut s.end, &mut s.pack);
        column.dictionary.map = HashMap::default();
    }
    s.rows
}

#[unsafe(no_mangle)]
pub extern "C" fn file_bytes() -> f64 {
    let s = store();
    s.end as f64 + s.columns.iter().map(|c| c.dictionary.bytes.len() as f64 + c.dictionary.starts.len() as f64 * 4.0).sum::<f64>()
}

#[unsafe(no_mangle)]
pub extern "C" fn dictionary_size(column: usize) -> usize {
    store().columns[column].dictionary.len()
}

fn unpack(bytes: &[u8], width: usize, count: usize, out: &mut [u64]) {
    if width == 0 {
        out[..count].fill(0);
        return;
    }
    let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    if width <= 56 {
        let mut bit = 0usize;
        for slot in &mut out[..count] {
            let byte = bit >> 3;
            let word = u64::from_le_bytes(bytes[byte..byte + 8].try_into().unwrap());
            *slot = (word >> (bit & 7)) & mask;
            bit += width;
        }
    } else {
        for (i, slot) in out[..count].iter_mut().enumerate() {
            let bit = i * width;
            let byte = bit >> 3;
            let shift = bit & 7;
            let mut word = u64::from_le_bytes(bytes[byte..byte + 8].try_into().unwrap()) >> shift;
            if shift + width > 64 {
                word |= u64::from(bytes[byte + 8]) << (64 - shift);
            }
            *slot = word & mask;
        }
    }
}

struct Scan {
    raw: Vec<u8>,
    values: Vec<u64>,
    nulls: Vec<u64>,
}

impl Scan {
    fn new() -> Self {
        Self { raw: Vec::new(), values: vec![0; CHUNK], nulls: vec![0; CHUNK / 64] }
    }
    fn load(&mut self, column: &Column, index: usize, count: usize) -> Block {
        let block = column.blocks[index];
        self.raw.resize(block.bytes as usize, 0);
        unsafe { file_read(block.at as f64, self.raw.as_mut_ptr(), self.raw.len()) };
        unpack(&self.raw, block.width as usize, count, &mut self.values);
        if block.has_nulls {
            unsafe { file_read(block.nulls_at as f64, self.nulls.as_mut_ptr().cast(), CHUNK / 8) };
        } else {
            self.nulls.fill(0);
        }
        block
    }
}

fn rows_of(s: &Store, chunk: usize) -> usize {
    (s.rows - chunk * CHUNK).min(CHUNK)
}

/// count, sum, min, max of a column, over the rows of the bitmap when there is one.
#[unsafe(no_mangle)]
pub extern "C" fn aggregate(column: usize) -> f64 {
    let s = store();
    let mut scan = Scan::new();
    let (mut count, mut sum, mut min, mut max) = (0u64, 0i128, i64::MAX, i64::MIN);
    for chunk in 0..s.columns[column].blocks.len() {
        let rows = rows_of(s, chunk);
        let block = scan.load(&s.columns[column], chunk, rows);
        let mut part = 0u64;
        let mut n = 0u64;
        if !block.has_nulls {
            for &v in &scan.values[..rows] {
                part += v;
            }
            n = rows as u64;
            min = min.min(block.min);
            max = max.max(block.max);
        } else {
            for (i, &v) in scan.values[..rows].iter().enumerate() {
                if scan.nulls[i >> 6] >> (i & 63) & 1 == 0 {
                    part += v;
                    n += 1;
                    let value = block.base.wrapping_add(v as i64);
                    min = min.min(value);
                    max = max.max(value);
                }
            }
        }
        sum += i128::from(part) + i128::from(block.base) * i128::from(n);
        count += n;
    }
    sum as f64 + (count + min as u64 + max as u64) as f64 * 1e-30
}

/// Orders the rows by one column, stably from the order there is; gives the number of passes.
#[unsafe(no_mangle)]
pub extern "C" fn sort(column: usize, descending: bool, fresh: bool) -> usize {
    let s = store();
    let rows = s.rows;
    let mut low = i64::MAX;
    let mut high = i64::MIN;
    let text = s.columns[column].kind == Kind::Text;
    let mut rank: Vec<u32> = Vec::new();
    if text {
        let d = &s.columns[column].dictionary;
        // Orders the dictionary by an eight-byte prefix first.
        let mut order: Vec<(u64, u32)> = (0..d.len())
            .map(|c| {
                let t = d.get(c);
                let mut p = [0u8; 8];
                p[..t.len().min(8)].copy_from_slice(&t[..t.len().min(8)]);
                (u64::from_be_bytes(p), c as u32)
            })
            .collect();
        order.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| d.get(a.1 as usize).cmp(d.get(b.1 as usize))));
        rank = vec![0; d.len()];
        for (i, (_, c)) in order.iter().enumerate() {
            rank[*c as usize] = i as u32;
        }
        low = 0;
        high = d.len() as i64;
    } else {
        for b in &s.columns[column].blocks {
            low = low.min(b.min);
            high = high.max(b.max);
        }
    }
    let range = (high - low) as u64 + 1;
    let bits = 64 - range.leading_zeros() as usize;
    // Blanks sort last: they get the largest key.
    let blank = range as u32;
    let mut keys = vec![0u32; rows];
    let mut scan = Scan::new();
    for chunk in 0..s.columns[column].blocks.len() {
        let n = rows_of(s, chunk);
        let block = scan.load(&s.columns[column], chunk, n);
        let target = &mut keys[chunk * CHUNK..chunk * CHUNK + n];
        let base = block.base - low;
        for (i, slot) in target.iter_mut().enumerate() {
            let mut key = (base + scan.values[i] as i64) as u32;
            if text {
                key = rank[key as usize];
            }
            if descending {
                key = (range as u32 - 1) - key;
            }
            if scan.nulls[i >> 6] >> (i & 63) & 1 == 1 {
                key = blank;
            }
            *slot = key;
        }
    }
    let passes = bits.div_ceil(11).max(1);
    let mut from: Vec<u32> = if fresh || s.perm.len() != rows { (0..rows as u32).collect() } else { std::mem::take(&mut s.perm) };
    let mut to = vec![0u32; rows];
    for pass in 0..passes {
        let shift = pass * 11;
        let mut counts = [0u32; 2048];
        for &k in &keys {
            counts[(k >> shift & 2047) as usize] += 1;
        }
        if counts.iter().any(|&c| c as usize == rows) {
            continue;
        }
        let mut sum = 0;
        for c in counts.iter_mut() {
            let n = *c;
            *c = sum;
            sum += n;
        }
        for &row in &from {
            let k = (keys[row as usize] >> shift & 2047) as usize;
            to[counts[k] as usize] = row;
            counts[k] += 1;
        }
        std::mem::swap(&mut from, &mut to);
    }
    s.perm = from;
    passes
}

/// region = text AND amount > limit, as a bitmap; gives how many.
#[unsafe(no_mangle)]
pub extern "C" fn filter_equal_greater(text_column: usize, length: usize, number: usize, limit: f64) -> usize {
    let s = store();
    let wanted = unsafe { INPUT[..length].to_vec() };
    let d = &s.columns[text_column].dictionary;
    let Some(code) = (0..d.len()).find(|&c| d.get(c) == wanted.as_slice()) else { return 0 };
    let (mut a, mut b) = (Scan::new(), Scan::new());
    s.bitmap = vec![0; s.rows.div_ceil(64)];
    let mut total = 0;
    for chunk in 0..s.columns[text_column].blocks.len() {
        let rows = rows_of(s, chunk);
        let x = a.load(&s.columns[text_column], chunk, rows);
        let y = b.load(&s.columns[number], chunk, rows);
        let code = (code as i64 - x.base) as u64;
        let limit = (limit as i64 - y.base).max(0) as u64;
        for (w, (va, vb)) in a.values[..rows].chunks(64).zip(b.values[..rows].chunks(64)).enumerate() {
            let mut bits = 0u64;
            for i in 0..va.len() {
                bits |= u64::from((va[i] == code) & (vb[i] > limit)) << i;
            }
            bits &= !a.nulls[w] & !b.nulls[w];
            s.bitmap[chunk * (CHUNK / 64) + w] = bits;
            total += bits.count_ones() as usize;
        }
    }
    total
}

/// The text contains: asked of the dictionary, then of the codes.
#[unsafe(no_mangle)]
pub extern "C" fn filter_contains(column: usize, length: usize) -> usize {
    let s = store();
    let wanted = unsafe { INPUT[..length].to_vec() };
    let d = &s.columns[column].dictionary;
    let hit: Vec<bool> = (0..d.len()).map(|c| d.get(c).windows(wanted.len()).any(|w| w == wanted.as_slice())).collect();
    let mut scan = Scan::new();
    s.bitmap = vec![0; s.rows.div_ceil(64)];
    let mut total = 0;
    for chunk in 0..s.columns[column].blocks.len() {
        let rows = rows_of(s, chunk);
        let block = scan.load(&s.columns[column], chunk, rows);
        for (w, values) in scan.values[..rows].chunks(64).enumerate() {
            let mut bits = 0u64;
            for (i, &v) in values.iter().enumerate() {
                bits |= u64::from(hit[(block.base + v as i64) as usize]) << i;
            }
            bits &= !scan.nulls[w];
            s.bitmap[chunk * (CHUNK / 64) + w] = bits;
            total += bits.count_ones() as usize;
        }
    }
    total
}

/// Keeps the rows of the bitmap, in the order there is.
#[unsafe(no_mangle)]
pub extern "C" fn project(sorted: bool) -> usize {
    let s = store();
    if sorted {
        let bitmap = &s.bitmap;
        s.perm.retain(|&r| bitmap[(r >> 6) as usize] >> (r & 63) & 1 == 1);
    } else {
        let mut perm = Vec::new();
        for (w, &bits) in s.bitmap.iter().enumerate() {
            let mut bits = bits;
            while bits != 0 {
                perm.push((w * 64) as u32 + bits.trailing_zeros());
                bits &= bits - 1;
            }
        }
        s.perm = perm;
    }
    s.perm.len()
}

const PAGE: usize = 16_384;
const PAGES: usize = 4096;

impl Store {
    fn bytes(&mut self, at: u64, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            let position = at + done as u64;
            let number = position / PAGE as u64;
            let offset = (position % PAGE as u64) as usize;
            let slot = match self.cache.get(&number) {
                Some(&slot) => slot,
                None => {
                    let slot = if self.pages.len() < PAGES {
                        self.pages.push((number, vec![0; PAGE]));
                        self.pages.len() - 1
                    } else {
                        let slot = self.hand;
                        self.hand = (self.hand + 1) % PAGES;
                        let old = self.pages[slot].0;
                        self.cache.remove(&old);
                        self.pages[slot].0 = number;
                        slot
                    };
                    unsafe { file_read(number as f64 * PAGE as f64, self.pages[slot].1.as_mut_ptr(), PAGE) };
                    self.cache.insert(number, slot);
                    slot
                }
            };
            let take = (PAGE - offset).min(out.len() - done);
            out[done..done + take].copy_from_slice(&self.pages[slot].1[offset..offset + take]);
            done += take;
        }
    }
    fn cell(&mut self, column: usize, row: usize) -> Option<i64> {
        let block = self.columns[column].blocks[row / CHUNK];
        let within = row % CHUNK;
        if block.has_nulls {
            let mut byte = [0u8];
            self.bytes(block.nulls_at + (within >> 3) as u64, &mut byte);
            if byte[0] >> (within & 7) & 1 == 1 {
                return None;
            }
        }
        if block.width == 0 {
            return Some(block.base);
        }
        let bit = within * block.width as usize;
        let mut raw = [0u8; 9];
        self.bytes(block.at + (bit >> 3) as u64, &mut raw);
        let mut word = u64::from_le_bytes(raw[..8].try_into().unwrap()) >> (bit & 7);
        if (bit & 7) + block.width as usize > 64 {
            word |= u64::from(raw[8]) << (64 - (bit & 7));
        }
        let mask = if block.width == 64 { u64::MAX } else { (1 << block.width) - 1 };
        Some(block.base.wrapping_add((word & mask) as i64))
    }
}

/// The text of a screen of rows; gives its length.
#[unsafe(no_mangle)]
pub extern "C" fn screen(first: usize, count: usize, through_order: bool) -> usize {
    use std::io::Write;
    let s = store();
    let mut out = std::mem::take(&mut s.out);
    out.clear();
    for i in first..first + count {
        let row = if through_order { s.perm[i] as usize } else { i };
        for column in 0..s.columns.len() {
            if let Some(value) = s.cell(column, row) {
                match s.columns[column].kind {
                    Kind::Text => out.extend_from_slice(s.columns[column].dictionary.get(value as usize)),
                    Kind::Decimal2 => {
                        let _ = write!(out, "{}.{:02}", value / 100, value % 100);
                    }
                    _ => {
                        let _ = write!(out, "{value}");
                    }
                }
            }
            out.push(b'\t');
        }
    }
    s.out = out;
    s.out.len()
}

#[unsafe(no_mangle)]
pub extern "C" fn clear_cache() {
    let s = store();
    s.cache.clear();
    s.pages.clear();
    s.hand = 0;
}

#[unsafe(no_mangle)]
pub extern "C" fn rows() -> usize {
    store().perm.len()
}

//! Writes Parquet files for the tests: everything that can be read, and some
//! things that cannot.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use reticula_core::rng::Rng;

use super::codec::{Codec, tests as codecs};
use super::crc32::Crc32;
use super::meta::{
    BIT_PACKED, BYTE_STREAM_SPLIT, DELTA_BINARY_PACKED, DELTA_BYTE_ARRAY, DELTA_LENGTH_BYTE_ARRAY,
    PLAIN, PLAIN_DICTIONARY, RLE, RLE_DICTIONARY,
};
use super::runs::tests as runs;
use super::thrift::{BINARY, FALSE, I8, I32, I64, LIST, STRUCT, TRUE};
use super::value::Physical;
use crate::bits;

/// Writes Thrift structures.
#[derive(Default)]
pub(super) struct Put {
    pub(super) out: Vec<u8>,
    last: Vec<i64>,
}

impl Put {
    pub(super) fn begin_element(&mut self) {
        self.last.push(0);
    }

    pub(super) fn end(&mut self) {
        self.out.push(0);
        self.last.pop();
    }

    fn field(&mut self, id: i64, kind: u8) {
        let last = self.last.last_mut().unwrap();
        let delta = id - *last;
        *last = id;
        if (1..=15).contains(&delta) {
            self.out.push((delta as u8) << 4 | kind);
        } else {
            self.out.push(kind);
            runs::put_zigzag(id, &mut self.out);
        }
    }

    pub(super) fn begin(&mut self, id: i64) {
        self.field(id, STRUCT);
        self.begin_element();
    }

    pub(super) fn int32(&mut self, id: i64, value: i64) {
        self.field(id, I32);
        runs::put_zigzag(value, &mut self.out);
    }

    pub(super) fn int64(&mut self, id: i64, value: i64) {
        self.field(id, I64);
        runs::put_zigzag(value, &mut self.out);
    }

    fn int8(&mut self, id: i64, value: i8) {
        self.field(id, I8);
        self.out.push(value.cast_unsigned());
    }

    fn truth(&mut self, id: i64, value: bool) {
        self.field(id, if value { TRUE } else { FALSE });
    }

    pub(super) fn binary(&mut self, id: i64, bytes: &[u8]) {
        self.field(id, BINARY);
        runs::put_varint(bytes.len() as u64, &mut self.out);
        self.out.extend_from_slice(bytes);
    }

    pub(super) fn list(&mut self, id: i64, kind: u8, count: usize) {
        self.field(id, LIST);
        if count < 15 {
            self.out.push((count as u8) << 4 | kind);
        } else {
            self.out.push(0xf0 | kind);
            runs::put_varint(count as u64, &mut self.out);
        }
    }
}

/// A value as it is stored.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Raw {
    Number(u64),
    Bytes(Vec<u8>),
}

/// What a column says its values mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Means {
    Nothing,
    /// The older way: a converted type, with scale and precision.
    Converted(i64, i64, i64),
    /// The newer way: the number of the field in the union.
    Logical(i64),
    Decimal {
        scale: i64,
        precision: i64,
    },
    /// Of a time or a timestamp: the number of the field, and of the unit.
    Time(i64, i64),
    Integer {
        bits: i8,
        signed: bool,
    },
}

/// A column to be written.
#[derive(Clone, Debug)]
pub(super) struct Field {
    pub(super) name: String,
    pub(super) physical: Physical,
    pub(super) means: Means,
    pub(super) optional: bool,
    pub(super) values: Vec<Option<Raw>>,
    pub(super) codec: Codec,
    /// The number of the codec that is written, when it is another.
    pub(super) claimed_codec: Option<i64>,
    /// The encoding of the values.
    pub(super) encoding: i64,
    /// The encoding that is said to be used besides.
    pub(super) claimed_encoding: Option<i64>,
    /// Whether the values are in a list, which is not written.
    pub(super) nested: bool,
    /// Whether the values are said to lie in another file.
    pub(super) external: bool,
    /// Entries of the dictionary that no row has.
    pub(super) unused: Vec<Raw>,
    /// Whether the dictionary is written twice.
    pub(super) twice: bool,
}

impl Field {
    pub(super) fn new(name: &str, physical: Physical, means: Means) -> Self {
        Self {
            name: String::from(name),
            physical,
            means,
            optional: false,
            values: Vec::new(),
            codec: Codec::Plain,
            claimed_codec: None,
            encoding: PLAIN,
            claimed_encoding: None,
            nested: false,
            external: false,
            unused: Vec::new(),
            twice: false,
        }
    }
}

/// How a file is written.
#[derive(Clone, Debug)]
pub(super) struct Layout {
    /// Rows of the groups; the rest goes to the last one.
    pub(super) groups: Vec<usize>,
    /// Most rows of a page.
    pub(super) page_rows: usize,
    pub(super) version_2: bool,
    pub(super) checksums: bool,
}

impl Layout {
    pub(super) fn simple() -> Self {
        Self {
            groups: Vec::new(),
            page_rows: 1 << 20,
            version_2: false,
            checksums: false,
        }
    }
}

const fn physical_number(physical: Physical) -> i64 {
    match physical {
        Physical::Boolean => 0,
        Physical::Int32 => 1,
        Physical::Int64 => 2,
        Physical::Int96 => 3,
        Physical::Float => 4,
        Physical::Double => 5,
        Physical::Bytes => 6,
        Physical::Fixed(_) => 7,
    }
}

pub(super) const fn codec_number(codec: Codec) -> i64 {
    match codec {
        Codec::Plain => 0,
        Codec::Snappy => 1,
        Codec::Gzip => 2,
        Codec::Zstd => 6,
        Codec::Lz4 => 7,
    }
}

/// The encodings a column can have.
pub(super) fn encodings(physical: Physical) -> Vec<i64> {
    let mut all = vec![PLAIN, RLE_DICTIONARY, PLAIN_DICTIONARY];
    match physical {
        Physical::Boolean => return vec![PLAIN, RLE],
        Physical::Int32 | Physical::Int64 => {
            all.extend([DELTA_BINARY_PACKED, BYTE_STREAM_SPLIT]);
        }
        Physical::Int96 => {}
        Physical::Float | Physical::Double => all.push(BYTE_STREAM_SPLIT),
        Physical::Bytes => all.extend([DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY]),
        Physical::Fixed(_) => all.extend([DELTA_BYTE_ARRAY, BYTE_STREAM_SPLIT]),
    }
    all
}

fn number(value: &Raw) -> u64 {
    match value {
        Raw::Number(number) => *number,
        Raw::Bytes(_) => panic!("bytes where a number was expected"),
    }
}

/// The bytes of a value that takes the same number of them as all others.
fn fixed(physical: Physical, value: &Raw) -> Vec<u8> {
    match value {
        Raw::Bytes(bytes) => bytes.clone(),
        Raw::Number(number) => number.to_le_bytes()[..physical.width().unwrap()].to_vec(),
    }
}

fn bytes(value: &Raw) -> &[u8] {
    match value {
        Raw::Bytes(bytes) => bytes,
        Raw::Number(_) => panic!("a number where bytes were expected"),
    }
}

fn plain(physical: Physical, values: &[Raw]) -> Vec<u8> {
    match physical {
        Physical::Boolean => {
            let truths: Vec<u64> = values.iter().map(number).collect();
            runs::pack(&truths, 1)
        }
        Physical::Bytes => {
            let mut out = Vec::new();
            for value in values {
                out.extend_from_slice(&(bytes(value).len() as u32).to_le_bytes());
                out.extend_from_slice(bytes(value));
            }
            out
        }
        _ => values
            .iter()
            .flat_map(|value| fixed(physical, value))
            .collect(),
    }
}

fn lengths(values: &[&[u8]], rng: &mut Rng) -> Vec<u8> {
    let lens: Vec<u64> = values.iter().map(|value| value.len() as u64).collect();
    let (block, parts) = [(128, 4), (256, 2)][rng.next_below(2) as usize];
    runs::delta(&lens, block, parts)
}

fn encode(physical: Physical, encoding: i64, values: &[Raw], rng: &mut Rng) -> Vec<u8> {
    match encoding {
        PLAIN => plain(physical, values),
        RLE => {
            let truths: Vec<u64> = values.iter().map(number).collect();
            let mut body = Vec::new();
            runs::hybrid(&truths, 1, rng, &mut body);
            let mut out = (body.len() as u32).to_le_bytes().to_vec();
            out.extend(body);
            out
        }
        DELTA_BINARY_PACKED => {
            let numbers: Vec<u64> = values
                .iter()
                .map(|value| match physical {
                    Physical::Int32 => {
                        i64::from((number(value) as u32).cast_signed()).cast_unsigned()
                    }
                    _ => number(value),
                })
                .collect();
            runs::delta(&numbers, 128, 4)
        }
        DELTA_LENGTH_BYTE_ARRAY => {
            let all: Vec<&[u8]> = values.iter().map(bytes).collect();
            let mut out = lengths(&all, rng);
            out.extend(all.concat());
            out
        }
        DELTA_BYTE_ARRAY => {
            let mut shared = Vec::new();
            let mut rests: Vec<&[u8]> = Vec::new();
            let mut before: &[u8] = &[];
            for value in values {
                let value = bytes(value);
                let mut same = before
                    .iter()
                    .zip(value)
                    .take_while(|(left, right)| left == right)
                    .count();
                if rng.next_below(4) == 0 {
                    same /= 2;
                }
                shared.push(same as u64);
                rests.push(&value[same..]);
                before = value;
            }
            let mut out = runs::delta(&shared, 128, 4);
            out.extend(lengths(&rests, rng));
            out.extend(rests.concat());
            out
        }
        BYTE_STREAM_SPLIT => {
            let all: Vec<Vec<u8>> = values.iter().map(|value| fixed(physical, value)).collect();
            let mut out = Vec::new();
            for byte in 0..physical.width().unwrap() {
                out.extend(all.iter().map(|value| value[byte]));
            }
            out
        }
        other => panic!("encoding {other}"),
    }
}

/// Writes the file.
pub(super) fn write(fields: &[Field], layout: &Layout, rng: &mut Rng) -> Vec<u8> {
    let mut file = b"PAR1".to_vec();
    let rows = fields
        .iter()
        .find(|field| !field.nested)
        .map_or(0, |field| field.values.len());
    let mut groups = layout.groups.clone();
    let before: usize = groups.iter().sum();
    groups.push(rows - before);
    let mut meta = Put::default();
    meta.begin_element();
    meta.int32(1, 2);
    write_schema(fields, &mut meta);
    meta.int64(3, rows as i64);
    meta.list(4, STRUCT, groups.len());
    let mut from = 0;
    for count in groups {
        meta.begin_element();
        meta.list(1, STRUCT, fields.len());
        for field in fields {
            meta.begin_element();
            if field.external {
                meta.binary(1, b"other.parquet");
            }
            meta.int64(2, 0);
            if field.nested {
                write_nested(field, &mut meta);
            } else {
                let values = &field.values[from..from + count];
                write_chunk(field, values, layout, rng, &mut file, &mut meta);
            }
            meta.end();
        }
        meta.int64(2, 0);
        meta.int64(3, count as i64);
        meta.end();
        from += count;
    }
    meta.binary(6, b"tests");
    meta.end();
    finish(&mut file, &meta.out);
    file
}

/// Writes what comes after the pages.
pub(super) fn finish(file: &mut Vec<u8>, metadata: &[u8]) {
    file.extend_from_slice(metadata);
    file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
}

fn write_schema(fields: &[Field], meta: &mut Put) {
    let nested = fields.iter().filter(|field| field.nested).count();
    meta.list(2, STRUCT, 1 + fields.len() + nested);
    meta.begin_element();
    meta.binary(4, b"schema");
    meta.int32(5, fields.len() as i64);
    meta.end();
    for field in fields {
        if field.nested {
            meta.begin_element();
            meta.int32(3, 1);
            meta.binary(4, field.name.as_bytes());
            meta.int32(5, 1);
            meta.int32(6, 3);
            meta.end();
        }
        meta.begin_element();
        meta.int32(1, physical_number(field.physical));
        if let Physical::Fixed(len) = field.physical {
            meta.int32(2, i64::from(len));
        }
        let repetition = if field.nested {
            2
        } else {
            i64::from(field.optional)
        };
        meta.int32(3, repetition);
        meta.binary(4, field.name.as_bytes());
        match field.means {
            Means::Nothing => {}
            Means::Converted(kind, scale, precision) => {
                meta.int32(6, kind);
                if kind == 5 {
                    meta.int32(7, scale);
                    meta.int32(8, precision);
                }
            }
            Means::Logical(number) => {
                meta.begin(10);
                meta.begin(number);
                meta.end();
                meta.end();
            }
            Means::Decimal { scale, precision } => {
                meta.begin(10);
                meta.begin(5);
                meta.int32(1, scale);
                meta.int32(2, precision);
                meta.end();
                meta.end();
            }
            Means::Time(number, unit) => {
                meta.begin(10);
                meta.begin(number);
                meta.truth(1, true);
                meta.begin(2);
                meta.begin(unit);
                meta.end();
                meta.end();
                meta.end();
                meta.end();
            }
            Means::Integer { bits, signed } => {
                meta.begin(10);
                meta.begin(10);
                meta.int8(1, bits);
                meta.truth(2, signed);
                meta.end();
                meta.end();
            }
        }
        meta.end();
    }
}

/// The metadata of a chunk that is not there.
fn write_nested(field: &Field, meta: &mut Put) {
    meta.begin(3);
    meta.int32(1, physical_number(field.physical));
    meta.list(2, I32, 1);
    runs::put_zigzag(RLE, &mut meta.out);
    meta.int32(4, 99);
    meta.int64(5, 12_345);
    meta.int64(7, 1 << 40);
    meta.int64(9, 1 << 41);
    meta.end();
}

fn write_chunk(
    field: &Field,
    values: &[Option<Raw>],
    layout: &Layout,
    rng: &mut Rng,
    file: &mut Vec<u8>,
    meta: &mut Put,
) {
    let start = file.len();
    let with_dictionary = matches!(field.encoding, RLE_DICTIONARY | PLAIN_DICTIONARY);
    let mut codes: BTreeMap<&Raw, u64> = BTreeMap::new();
    let mut dictionary_at = None;
    if with_dictionary {
        let mut entries = field.unused.clone();
        for value in values.iter().flatten() {
            codes.entry(value).or_insert_with(|| {
                entries.push(value.clone());
                entries.len() as u64 - 1
            });
        }
        dictionary_at = Some(start);
        let body = plain(field.physical, &entries);
        let encoding = if field.encoding == PLAIN_DICTIONARY {
            PLAIN_DICTIONARY
        } else {
            PLAIN
        };
        for _ in 0..=usize::from(field.twice) {
            write_page(field, layout, rng, file, &body, &[], 2, |put| {
                put.begin(7);
                put.int32(1, entries.len() as i64);
                put.int32(2, encoding);
                put.end();
            });
        }
    }
    let data_at = file.len();
    let mut used = vec![RLE, BIT_PACKED];
    let mut at = 0;
    while at < values.len() {
        if rng.next_below(9) == 0 {
            // A page of something else, and one without any rows.
            write_page(field, layout, rng, file, b"index", &[], 1, |put| {
                put.begin(6);
                put.end();
            });
            write_data(field, &[], field.encoding, &codes, layout, rng, file);
        }
        let most = layout.page_rows.min(values.len() - at);
        let count = 1 + rng.next_below(most as u64) as usize;
        // A writer gives up on a dictionary that becomes too large.
        let encoding = if with_dictionary && at > 0 && rng.next_below(4) == 0 {
            PLAIN
        } else {
            field.encoding
        };
        used.push(encoding);
        write_data(
            field,
            &values[at..at + count],
            encoding,
            &codes,
            layout,
            rng,
            file,
        );
        at += count;
    }
    used.extend(field.claimed_encoding);
    used.sort_unstable();
    used.dedup();
    meta.begin(3);
    meta.int32(1, physical_number(field.physical));
    meta.list(2, I32, used.len());
    for encoding in used {
        runs::put_zigzag(encoding, &mut meta.out);
    }
    meta.list(3, BINARY, 1);
    runs::put_varint(field.name.len() as u64, &mut meta.out);
    meta.out.extend_from_slice(field.name.as_bytes());
    meta.int32(4, field.claimed_codec.unwrap_or(codec_number(field.codec)));
    meta.int64(5, values.len() as i64);
    meta.int64(6, (file.len() - start) as i64);
    meta.int64(7, (file.len() - start) as i64);
    meta.int64(9, data_at as i64);
    match dictionary_at {
        Some(at) => meta.int64(11, at as i64),
        None if rng.next_below(2) == 0 => meta.int64(11, 0),
        None => {}
    }
    // Statistics, which nobody reads.
    meta.begin(12);
    meta.binary(1, &[0; 40]);
    meta.int64(3, 5);
    meta.end();
    meta.end();
}

fn write_data(
    field: &Field,
    values: &[Option<Raw>],
    encoding: i64,
    codes: &BTreeMap<&Raw, u64>,
    layout: &Layout,
    rng: &mut Rng,
    file: &mut Vec<u8>,
) {
    let present: Vec<Raw> = values.iter().flatten().cloned().collect();
    let blanks = values.len() - present.len();
    let body = if matches!(encoding, RLE_DICTIONARY | PLAIN_DICTIONARY) {
        let width = bits::width_of((field.unused.len() + codes.len()).saturating_sub(1) as u64);
        let all: Vec<u64> = present.iter().map(|value| codes[value]).collect();
        let mut body = vec![width as u8];
        runs::hybrid(&all, width, rng, &mut body);
        body
    } else if present.is_empty() && rng.next_below(2) == 0 {
        Vec::new()
    } else {
        encode(field.physical, encoding, &present, rng)
    };
    let defined: Vec<u64> = values
        .iter()
        .map(|value| u64::from(value.is_some()))
        .collect();
    let mut levels = Vec::new();
    if layout.version_2 {
        let mixed = blanks > 0 && !present.is_empty();
        if field.optional && (mixed || rng.next_below(2) == 0) {
            runs::hybrid(&defined, 1, rng, &mut levels);
        }
        let len = levels.len();
        let packed = rng.next_below(4) > 0;
        let mut plain_field = field.clone();
        if !packed {
            plain_field.codec = Codec::Plain;
        }
        write_page(&plain_field, layout, rng, file, &body, &levels, 3, |put| {
            put.begin(8);
            put.int32(1, values.len() as i64);
            put.int32(2, blanks as i64);
            put.int32(3, values.len() as i64);
            put.int32(4, encoding);
            put.int32(5, len as i64);
            put.int32(6, 0);
            if !packed {
                put.truth(7, false);
            }
            put.end();
        });
        return;
    }
    let bit_packed = rng.next_below(3) == 0;
    let mut whole = Vec::new();
    if field.optional {
        if bit_packed {
            let mut packed = vec![0_u8; values.len().div_ceil(8)];
            for (index, level) in defined.iter().enumerate() {
                packed[index / 8] |= (*level as u8) << (7 - index % 8);
            }
            whole = packed;
        } else {
            runs::hybrid(&defined, 1, rng, &mut levels);
            whole.extend_from_slice(&(levels.len() as u32).to_le_bytes());
            whole.extend_from_slice(&levels);
        }
    }
    whole.extend(body);
    let statistics = vec![b'm'; rng.next_below(400) as usize];
    write_page(field, layout, rng, file, &whole, &[], 0, |put| {
        put.begin(5);
        put.int32(1, values.len() as i64);
        put.int32(2, encoding);
        put.int32(3, if bit_packed { BIT_PACKED } else { RLE });
        put.int32(4, BIT_PACKED);
        put.begin(5);
        put.binary(5, &statistics);
        put.end();
        put.end();
    });
}

#[expect(clippy::too_many_arguments, reason = "all of it is needed")]
fn write_page(
    field: &Field,
    layout: &Layout,
    rng: &mut Rng,
    file: &mut Vec<u8>,
    body: &[u8],
    levels: &[u8],
    kind: i64,
    header: impl FnOnce(&mut Put),
) {
    let mut whole = levels.to_vec();
    // Nothing is sometimes not even packed.
    if !body.is_empty() || rng.next_below(2) == 0 {
        whole.extend(codecs::pack(field.codec, body));
    }
    let mut put = Put::default();
    put.begin_element();
    put.int32(1, kind);
    put.int32(2, (levels.len() + body.len()) as i64);
    put.int32(3, whole.len() as i64);
    if layout.checksums {
        let checksum = Crc32::new().of(&whole);
        put.int32(4, i64::from(checksum.cast_signed()));
    }
    header(&mut put);
    put.end();
    file.extend(put.out);
    file.extend(whole);
}

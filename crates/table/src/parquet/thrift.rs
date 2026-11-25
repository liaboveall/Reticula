//! The Thrift compact protocol, as far as Parquet's metadata needs it.

/// The types a field or an element can have.
pub(super) const TRUE: u8 = 1;
pub(super) const FALSE: u8 = 2;
pub(super) const I8: u8 = 3;
pub(super) const I16: u8 = 4;
pub(super) const I32: u8 = 5;
pub(super) const I64: u8 = 6;
pub(super) const DOUBLE: u8 = 7;
pub(super) const BINARY: u8 = 8;
pub(super) const LIST: u8 = 9;
pub(super) const SET: u8 = 10;
pub(super) const MAP: u8 = 11;
pub(super) const STRUCT: u8 = 12;
pub(super) const UUID: u8 = 13;

/// How deep structures may lie in one another.
const MAX_DEPTH: u32 = 24;

/// Why a structure cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ThriftError {
    /// It goes on after the last byte.
    Ends,
    /// It is not one.
    Bad,
}

/// Reads values, one after another.
///
/// After one that cannot be read there are no more bytes, and all that comes
/// is zero; whether that happened is asked once, at the end.
#[derive(Clone, Copy, Debug)]
pub(super) struct Reader<'a> {
    rest: &'a [u8],
    error: Option<ThriftError>,
}

impl<'a> Reader<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self {
            rest: bytes,
            error: None,
        }
    }

    /// Bytes that were not read yet.
    pub(super) const fn left(&self) -> usize {
        self.rest.len()
    }

    /// Whether everything could be read so far.
    pub(super) const fn check(&self) -> Result<(), ThriftError> {
        match self.error {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }

    #[cold]
    pub(super) fn fail(&mut self, error: ThriftError) {
        self.rest = &[];
        self.error = self.error.or(Some(error));
    }

    fn byte(&mut self) -> u8 {
        let Some((byte, rest)) = self.rest.split_first() else {
            self.fail(ThriftError::Ends);
            return 0;
        };
        self.rest = rest;
        *byte
    }

    fn take(&mut self, len: u64) -> &'a [u8] {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let Some((taken, rest)) = self.rest.split_at_checked(len) else {
            self.fail(ThriftError::Ends);
            return &[];
        };
        self.rest = rest;
        taken
    }

    #[inline(never)]
    fn varint(&mut self) -> u64 {
        let mut value = 0_u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte();
            value |= u64::from(byte & 0x7f).wrapping_shl(shift);
            if byte & 0x80 == 0 {
                return value;
            }
        }
        self.fail(ThriftError::Bad);
        0
    }

    /// The next field of a structure, and its type; nothing after the last one.
    /// `last` is the field before it, 0 at first.
    #[inline(never)]
    pub(super) fn field(&mut self, last: &mut i64) -> Option<(i64, u8)> {
        let byte = self.byte();
        if byte == 0 {
            return None;
        }
        let kind = byte & 0x0f;
        let delta = i64::from(byte >> 4);
        *last = if delta == 0 {
            self.int()
        } else {
            last.wrapping_add(delta)
        };
        Some((*last, kind))
    }

    /// A whole number of 16, 32 or 64 bits.
    pub(super) fn int(&mut self) -> i64 {
        let value = self.varint();
        (value >> 1).cast_signed() ^ (value & 1).cast_signed().wrapping_neg()
    }

    /// A text, or bytes.
    pub(super) fn binary(&mut self) -> &'a [u8] {
        let len = self.varint();
        self.take(len)
    }

    /// The start of a list or set: how many elements, and of which type.
    /// There are no more elements than bytes left.
    #[inline(never)]
    pub(super) fn list(&mut self) -> (usize, u8) {
        let byte = self.byte();
        let kind = byte & 0x0f;
        let count = match byte >> 4 {
            15 => self.varint(),
            count => u64::from(count),
        };
        match usize::try_from(count) {
            Ok(count) if count <= self.rest.len() => (count, kind),
            _ => {
                self.fail(ThriftError::Ends);
                (0, kind)
            }
        }
    }

    /// Puts a field that is a whole number into the place of that index. Leaves
    /// out one that is something else, or has no place.
    #[inline(never)]
    pub(super) fn keep(&mut self, field: i64, kind: u8, places: &mut [i64]) {
        let place = usize::try_from(field)
            .ok()
            .and_then(|field| places.get_mut(field));
        match (place, kind) {
            (Some(place), I16 | I32 | I64) => *place = self.int(),
            _ => self.skip(kind),
        }
    }

    /// Leaves out a value that is a field. The value of a truth is its type.
    pub(super) fn skip(&mut self, kind: u8) {
        self.skip_value(kind, false, 0);
    }

    /// Leaves out a value that is an element of a list.
    pub(super) fn skip_element(&mut self, kind: u8) {
        self.skip_value(kind, true, 0);
    }

    fn skip_value(&mut self, kind: u8, element: bool, depth: u32) {
        if depth > MAX_DEPTH {
            self.fail(ThriftError::Bad);
            return;
        }
        let deeper = depth.saturating_add(1);
        match kind {
            TRUE | FALSE if !element => {}
            TRUE | FALSE | I8 => {
                self.byte();
            }
            I16 | I32 | I64 => {
                self.varint();
            }
            DOUBLE => {
                self.take(8);
            }
            UUID => {
                self.take(16);
            }
            BINARY => {
                self.binary();
            }
            // Every element takes a byte at least, so this ends.
            LIST | SET => {
                let (mut count, kind) = self.list();
                while count > 0 && self.error.is_none() {
                    self.skip_value(kind, true, deeper);
                    count = count.wrapping_sub(1);
                }
            }
            MAP => {
                let mut count = self.varint();
                let kinds = if count > 0 { self.byte() } else { 0 };
                while count > 0 && self.error.is_none() {
                    self.skip_value(kinds >> 4, true, deeper);
                    self.skip_value(kinds & 0x0f, true, deeper);
                    count = count.wrapping_sub(1);
                }
            }
            STRUCT => {
                let mut last = 0;
                while let Some((_, kind)) = self.field(&mut last) {
                    self.skip_value(kind, false, deeper);
                }
            }
            _ => self.fail(ThriftError::Bad),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "tests"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use alloc::vec::Vec;
    use reticula_core::rng::Rng;

    /// What reading gives, when it can be read.
    fn read<'a, T>(
        bytes: &'a [u8],
        read: impl FnOnce(&mut Reader<'a>) -> T,
    ) -> Result<T, ThriftError> {
        let mut reader = Reader::new(bytes);
        let value = read(&mut reader);
        reader.check().map(|()| value)
    }

    #[test]
    fn numbers() {
        let int = |bytes: &[u8]| read(bytes, Reader::int);
        assert_eq!(int(&[0x00]), Ok(0));
        assert_eq!(int(&[0x01]), Ok(-1));
        assert_eq!(int(&[0x02]), Ok(1));
        assert_eq!(int(&[0xac, 0x02]), Ok(150));
        let mut largest = [0xff_u8; 10];
        largest[9] = 0x01;
        assert_eq!(int(&largest), Ok(i64::MIN));
        assert_eq!(int(&[0x80]), Err(ThriftError::Ends));
        assert_eq!(int(&[0x80; 11]), Err(ThriftError::Bad));
    }

    #[test]
    fn fields_count_up_or_say_their_number() {
        // Field 1 of 32 bits, field 3 a truth, field 100 a text, the end.
        let bytes = [0x15, 0x0a, 0x21, 0x08, 0xc8, 0x01, 0x02, b'h', b'i', 0x00];
        let mut reader = Reader::new(&bytes);
        let mut last = 0;
        assert_eq!(reader.field(&mut last), Some((1, I32)));
        assert_eq!(reader.int(), 5);
        assert_eq!(reader.field(&mut last), Some((3, TRUE)));
        assert_eq!(reader.field(&mut last), Some((100, BINARY)));
        assert_eq!(reader.binary(), b"hi");
        assert_eq!(reader.field(&mut last), None);
        assert_eq!(reader.left(), 0);
        assert_eq!(reader.check(), Ok(()));
        // Nothing comes after the end, and that it did not is remembered.
        assert_eq!(reader.field(&mut last), None);
        assert_eq!(reader.check(), Err(ThriftError::Ends));
    }

    #[test]
    fn numbers_are_kept_by_their_field() {
        // Fields 1 and 3 are numbers, 2 is a text, 4 has no place.
        let bytes = [0x15, 0x0a, 0x18, 0x01, b'x', 0x16, 0x03, 0x15, 0x08, 0x00];
        let mut reader = Reader::new(&bytes);
        let mut places = [7; 4];
        let mut last = 0;
        while let Some((field, kind)) = reader.field(&mut last) {
            reader.keep(field, kind, &mut places);
        }
        assert_eq!(reader.check(), Ok(()));
        assert_eq!(places, [7, 5, 7, -2]);
    }

    #[test]
    fn unknown_values_are_left_out() {
        // A structure of: a list of two truths, a map of a number to a text,
        // a structure, a double, a uuid, an empty map.
        let mut bytes = Vec::from([0x19, 0x21, 0x01, 0x02]);
        bytes.extend([0x1b, 0x01, 0x58, 0x02, 0x01, b'x']);
        bytes.extend([0x1c, 0x13, 0x07, 0x00]);
        bytes.extend([0x17, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.extend([0x1d]);
        bytes.extend([7; 16]);
        bytes.extend([0x1b, 0x00, 0x00]);
        bytes.push(0xaa);
        let mut reader = Reader::new(&bytes);
        reader.skip(STRUCT);
        assert_eq!(reader.check(), Ok(()));
        assert_eq!(reader.left(), 1);
        for len in 0..bytes.len() - 1 {
            let skipped = read(&bytes[..len], |reader| reader.skip(STRUCT));
            assert_eq!(skipped, Err(ThriftError::Ends), "{len} bytes");
        }
    }

    #[test]
    fn depth_and_size_have_limits() {
        // Structures in structures, without an end.
        let deep = [0x1c_u8; 10_000];
        let skipped = read(&deep, |reader| reader.skip(STRUCT));
        assert_eq!(skipped, Err(ThriftError::Bad));
        // A list that claims more elements than there are bytes.
        let long = [0xf5_u8, 0xff, 0xff, 0xff, 0x0f, 0x00];
        assert_eq!(read(&long, Reader::list), Err(ThriftError::Ends));
        // A map that does, which ends with the bytes.
        let long = [0xff_u8, 0xff, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x55, 0x00];
        let skipped = read(&long, |reader| reader.skip(MAP));
        assert_eq!(skipped, Err(ThriftError::Ends));
        // A text that claims to be longer than memory.
        let mut text = [0xff_u8; 10];
        text[9] = 0x01;
        assert_eq!(read(&text, Reader::binary), Err(ThriftError::Ends));
    }

    #[test]
    fn any_bytes_are_survived() {
        let mut rng = Rng::from_seed(1);
        for _ in 0..20_000 {
            let len = rng.next_below(40);
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64().to_le_bytes()[0]).collect();
            let mut reader = Reader::new(&bytes);
            reader.skip(STRUCT);
        }
    }
}

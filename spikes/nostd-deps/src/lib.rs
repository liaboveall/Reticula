//! Do the dependencies planned for the kernel build without the standard
//! library? Not part of the engine.
//!
//! Kernel crates are `no_std`. That only holds if everything they depend on
//! is too, with the features the kernel would use. This crate uses each planned
//! dependency the way the kernel will, and is checked for a target that has no
//! standard library at all (`wasm32v1-none`), where a dependency that reaches
//! for `std` fails to compile.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use core::hash::{BuildHasherDefault, Hasher};

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// A record with the shapes the kernel will persist: integers, a string, and
/// a small inline vector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// An identifier.
    pub id: u64,
    /// A short run of values, stored inline when it fits.
    pub values: SmallVec<[u32; 4]>,
    /// Free text.
    pub label: String,
}

/// Encodes with postcard, compresses with LZ4, and reverses both.
#[must_use]
pub fn round_trip(record: &Record) -> Option<Record> {
    let encoded: Vec<u8> = postcard::to_allocvec(record).ok()?;
    let compressed = lz4_flex::compress_prepend_size(&encoded);
    let decompressed = lz4_flex::decompress_size_prepended(&compressed).ok()?;
    postcard::from_bytes(&decompressed).ok()
}

/// A hasher of our own: without `std` there is no default one.
#[derive(Default)]
pub struct Multiply(u64);

impl Hasher for Multiply {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
}

/// Builds a hash map and looks a key up in it.
#[must_use]
pub fn lookup(key: u64) -> Option<f64> {
    let mut squares: HashMap<u64, f64, BuildHasherDefault<Multiply>> = HashMap::default();
    for index in 0..64_u32 {
        let value = f64::from(index);
        squares.insert(u64::from(index), libm::sqrt(value * value));
    }
    squares.get(&key).copied()
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use smallvec::smallvec;

    use super::{Record, lookup, round_trip};

    #[test]
    fn everything_works_together() {
        let record = Record {
            id: u64::MAX,
            values: smallvec![1, 2, 3, 4, 5],
            label: "格子 lattice".to_string(),
        };
        assert_eq!(round_trip(&record), Some(record));
        assert_eq!(lookup(7), Some(7.0));
        assert_eq!(lookup(64), None);
    }
}

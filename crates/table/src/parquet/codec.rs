//! How pages are packed.

use alloc::boxed::Box;
use core::fmt;

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};
use ruzstd::decoding::FrameDecoder;

use super::crc32::Crc32;
use super::snappy;

/// The window of a Zstandard frame that is always allowed, as its
/// specification asks for.
const ZSTD_WINDOW: u64 = 8 << 20;

/// A way to pack pages that can be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Codec {
    Plain,
    Snappy,
    Gzip,
    Zstd,
    Lz4,
}

impl Codec {
    /// The codec of a number in the metadata.
    pub(super) const fn from_number(number: i64) -> Option<Self> {
        match number {
            0 => Some(Self::Plain),
            1 => Some(Self::Snappy),
            2 => Some(Self::Gzip),
            6 => Some(Self::Zstd),
            7 => Some(Self::Lz4),
            _ => None,
        }
    }
}

/// Unpacks pages. What the codecs need besides is kept from one to the next.
#[derive(Default)]
pub(super) struct Unpacker {
    crc: Option<Crc32>,
    inflater: Option<Box<DecompressorOxide>>,
    zstd: Option<Box<FrameDecoder>>,
}

impl fmt::Debug for Unpacker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Unpacker").finish_non_exhaustive()
    }
}

impl Unpacker {
    /// The CRC-32 of a page.
    pub(super) fn checksum(&mut self, bytes: &[u8]) -> u32 {
        self.crc.get_or_insert_with(Crc32::new).of(bytes)
    }

    /// Unpacks `input`, which has to fill `out` exactly.
    pub(super) fn unpack(&mut self, codec: Codec, input: &[u8], out: &mut [u8]) -> Option<()> {
        // Where there are no values, some writers do not even pack nothing.
        if out.is_empty() && input.is_empty() {
            return Some(());
        }
        match codec {
            Codec::Plain => {
                if input.len() != out.len() {
                    return None;
                }
                out.copy_from_slice(input);
                Some(())
            }
            Codec::Snappy => snappy::decompress(input, out),
            Codec::Gzip => self.gunzip(input, out),
            Codec::Zstd => self.unzstd(input, out),
            Codec::Lz4 => {
                let written = lz4_flex::block::decompress_into(input, out).ok()?;
                (written == out.len()).then_some(())
            }
        }
    }

    fn unzstd(&mut self, input: &[u8], out: &mut [u8]) -> Option<()> {
        let decoder = self.zstd.get_or_insert_with(Box::default);
        // A window longer than the page is of no use to anybody.
        let window = u64::try_from(out.len()).ok()?.max(ZSTD_WINDOW);
        decoder.set_max_window_size(window);
        let written = decoder.decode_all(input, out).ok()?;
        (written == out.len()).then_some(())
    }

    /// Members of the gzip format one after another, or a zlib stream, which
    /// some writers take for the same.
    fn gunzip(&mut self, input: &[u8], out: &mut [u8]) -> Option<()> {
        let mut rest = input;
        let mut at = 0_usize;
        let mut first = true;
        while let Some(deflated) = gzip_header(rest) {
            let (used, written) = self.inflate(deflated, out, at, 0)?;
            let member = out.get(at..at.checked_add(written)?)?;
            let sum = self.checksum(member);
            let (trailer, after) = deflated.get(used..)?.split_first_chunk::<8>()?;
            let [s0, s1, s2, s3, l0, l1, l2, l3] = *trailer;
            let len = u32::from_le_bytes([l0, l1, l2, l3]);
            if u32::from_le_bytes([s0, s1, s2, s3]) != sum
                || u64::try_from(written).ok()? & 0xffff_ffff != u64::from(len)
            {
                return None;
            }
            at = at.checked_add(written)?;
            rest = after;
            first = false;
        }
        if first && let Some([method, flags]) = rest.first_chunk::<2>() {
            let check = u32::from(u16::from_be_bytes([*method, *flags]));
            if method & 0x0f != 8 || check.checked_rem(31) != Some(0) {
                return None;
            }
            let (_, written) =
                self.inflate(rest, out, 0, inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER)?;
            at = written;
        }
        (at == out.len()).then_some(())
    }

    /// Inflates to `out` from `at` on. Gives how many bytes were read and written.
    fn inflate(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        at: usize,
        flags: u32,
    ) -> Option<(usize, usize)> {
        let inflater = self.inflater.get_or_insert_with(Box::default);
        inflater.init();
        let flags = flags | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, used, written) = decompress(inflater, input, out, at, flags);
        (status == TINFLStatus::Done).then_some((used, written))
    }
}

/// What comes after the header of a gzip member, if there is one.
fn gzip_header(bytes: &[u8]) -> Option<&[u8]> {
    let ([0x1f, 0x8b, 8, flags, _, _, _, _, _, _], mut rest) = bytes.split_first_chunk::<10>()?
    else {
        return None;
    };
    if flags & 4 != 0 {
        let (len, after) = rest.split_first_chunk::<2>()?;
        rest = after.get(usize::from(u16::from_le_bytes(*len))..)?;
    }
    // A name and a comment, each with a zero at its end.
    for flag in [8, 16] {
        if flags & flag != 0 {
            let end = rest.iter().position(|byte| *byte == 0)?;
            rest = rest.get(end.checked_add(1)?..)?;
        }
    }
    if flags & 2 != 0 {
        rest = rest.get(2..)?;
    }
    Some(rest)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "tests"
)]
pub(super) mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use reticula_core::rng::Rng;
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};

    /// Deflates without making anything shorter.
    fn stored(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let count = bytes.len().div_ceil(40_000).max(1);
        for index in 0..count {
            let from = index * 40_000;
            let piece = &bytes[from..bytes.len().min(from + 40_000)];
            out.push(u8::from(index + 1 == count));
            out.extend_from_slice(&(piece.len() as u16).to_le_bytes());
            out.extend_from_slice(&(!(piece.len() as u16)).to_le_bytes());
            out.extend_from_slice(piece);
        }
        out
    }

    pub(in crate::parquet) fn gzip(bytes: &[u8], flags: u8) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 255];
        if flags & 4 != 0 {
            out.extend_from_slice(&[3, 0, 1, 2, 3]);
        }
        if flags & 8 != 0 {
            out.extend_from_slice(b"name\0");
        }
        if flags & 16 != 0 {
            out.extend_from_slice(b"comment\0");
        }
        if flags & 2 != 0 {
            out.extend_from_slice(&[0, 0]);
        }
        out.extend_from_slice(&stored(bytes));
        out.extend_from_slice(&Crc32::new().of(bytes).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out
    }

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        out.extend_from_slice(&stored(bytes));
        let (mut a, mut b) = (1_u32, 0_u32);
        for byte in bytes {
            a = (a + u32::from(*byte)) % 65_521;
            b = (b + a) % 65_521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    /// A frame of raw blocks, or one that really packs.
    pub(in crate::parquet) fn zstd(bytes: &[u8], raw: bool) -> Vec<u8> {
        compress_to_vec(
            bytes,
            if raw {
                CompressionLevel::Uncompressed
            } else {
                CompressionLevel::Fastest
            },
        )
    }

    pub(in crate::parquet) fn pack(codec: Codec, bytes: &[u8]) -> Vec<u8> {
        match codec {
            Codec::Plain => bytes.to_vec(),
            Codec::Snappy => snappy::tests::compress(bytes),
            Codec::Gzip => gzip(bytes, 0),
            Codec::Zstd => zstd(bytes, false),
            Codec::Lz4 => lz4_flex::block::compress(bytes),
        }
    }

    pub(in crate::parquet) const ALL: [Codec; 5] = [
        Codec::Plain,
        Codec::Snappy,
        Codec::Gzip,
        Codec::Zstd,
        Codec::Lz4,
    ];

    fn some_bytes(rng: &mut Rng, len: usize) -> Vec<u8> {
        let kinds = 1 + rng.next_below(256);
        (0..len)
            .map(|_| rng.next_below(kinds).to_le_bytes()[0])
            .collect()
    }

    #[test]
    fn what_was_packed_is_unpacked() {
        let mut rng = Rng::from_seed(5);
        let mut unpacker = Unpacker::default();
        for len in [0, 1, 2, 100, 4_096, 40_000, 40_001, 300_000] {
            let bytes = some_bytes(&mut rng, len);
            let mut packed: Vec<(Codec, Vec<u8>)> = ALL
                .iter()
                .map(|codec| (*codec, pack(*codec, &bytes)))
                .collect();
            packed.push((Codec::Gzip, gzip(&bytes, 2 | 4 | 8 | 16)));
            packed.push((Codec::Gzip, zlib(&bytes)));
            packed.push((Codec::Zstd, zstd(&bytes, true)));
            // Two members, and two frames, are one page.
            let half = len / 2;
            let mut members = gzip(&bytes[..half], 0);
            members.extend(gzip(&bytes[half..], 8));
            packed.push((Codec::Gzip, members));
            let mut frames = zstd(&bytes[..half], false);
            // A frame that is to be left out lies between them.
            frames.extend([0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 9, 9, 9]);
            frames.extend(zstd(&bytes[half..], true));
            packed.push((Codec::Zstd, frames));
            for (codec, packed) in packed {
                let mut out = vec![0xee; len];
                assert_eq!(
                    unpacker.unpack(codec, &packed, &mut out),
                    Some(()),
                    "{codec:?}, {len} bytes"
                );
                assert_eq!(out, bytes);
                for other in [len + 1, len.saturating_sub(1)] {
                    if other != len {
                        let mut out = vec![0; other];
                        assert_eq!(unpacker.unpack(codec, &packed, &mut out), None);
                    }
                }
            }
        }
    }

    #[test]
    fn a_wrong_checksum_is_noticed() {
        let bytes = b"one two three".as_slice();
        let mut packed = gzip(bytes, 0);
        let at = packed.len() - 8;
        packed[at] ^= 1;
        let mut out = vec![0; bytes.len()];
        assert_eq!(
            Unpacker::default().unpack(Codec::Gzip, &packed, &mut out),
            None
        );
        let mut packed = zlib(bytes);
        let at = packed.len() - 1;
        packed[at] ^= 1;
        assert_eq!(
            Unpacker::default().unpack(Codec::Gzip, &packed, &mut out),
            None
        );
    }

    #[test]
    fn damage_is_survived() {
        let mut rng = Rng::from_seed(6);
        let mut unpacker = Unpacker::default();
        for round in 0..6_000 {
            let len = rng.next_below(3_000) as usize;
            let bytes = some_bytes(&mut rng, len);
            let codec = ALL[round % ALL.len()];
            let mut packed = if codec == Codec::Gzip && round % 2 == 0 {
                zlib(&bytes)
            } else {
                pack(codec, &bytes)
            };
            match rng.next_below(4) {
                0 => packed.truncate(rng.next_below(packed.len() as u64 + 1) as usize),
                1 => {
                    for byte in packed.iter_mut().skip(rng.next_below(8) as usize).take(20) {
                        *byte = rng.next_u64().to_le_bytes()[0];
                    }
                }
                _ => {
                    for _ in 0..=rng.next_below(4) {
                        if !packed.is_empty() {
                            let at = rng.next_below(packed.len() as u64) as usize;
                            packed[at] = rng.next_u64().to_le_bytes()[0];
                        }
                    }
                }
            }
            let mut out = vec![0; len];
            let _ = unpacker.unpack(codec, &packed, &mut out);
        }
    }
}

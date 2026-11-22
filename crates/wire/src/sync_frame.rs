//! Versioned binary WebSocket frame encoder/decoder (`SYNC_PROTOCOL_VERSION = 1`
//! and `SYNC_PROTOCOL_VERSION_V2 = 2`).
//!
//! Layout: `[protocol_version: u8][postcard::to_allocvec(WireSyncFrame)]`,
//! bounded by [`MAX_SYNC_FRAME_BYTES`] (`4 MiB`) prior to deserialisation.

use alloc::vec::Vec;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::v2::{SYNC_PROTOCOL_VERSION, WireSyncFrameV1};
use crate::v3::{SYNC_PROTOCOL_VERSION_V2, WireSyncFrameV2};

/// Maximum encoded size of a single WebSocket binary sync frame (`4 MiB`).
pub const MAX_SYNC_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Failure encoding or decoding a binary sync protocol frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncFrameError {
    /// Input byte slice was empty (missing protocol version prefix byte).
    Empty,
    /// Encoded or incoming frame exceeded [`MAX_SYNC_FRAME_BYTES`].
    FrameTooLarge {
        /// Observed frame byte length.
        len: usize,
        /// Maximum permitted length.
        max: usize,
    },
    /// Frame carried an unsupported protocol version prefix byte.
    UnsupportedVersion(u8),
    /// `postcard` serialization failed.
    Encode,
    /// `postcard` deserialisation failed.
    Decode,
}

fn encode_versioned_frame<T: Serialize>(version: u8, frame: &T) -> Result<Vec<u8>, SyncFrameError> {
    let payload = postcard::to_allocvec(frame).map_err(|_| SyncFrameError::Encode)?;
    let total = payload.len().saturating_add(1);
    if total > MAX_SYNC_FRAME_BYTES {
        return Err(SyncFrameError::FrameTooLarge {
            len: total,
            max: MAX_SYNC_FRAME_BYTES,
        });
    }
    let mut out = Vec::with_capacity(total);
    out.push(version);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn validate_incoming_header(bytes: &[u8]) -> Result<(u8, &[u8]), SyncFrameError> {
    let Some(&ver) = bytes.first() else {
        return Err(SyncFrameError::Empty);
    };
    if bytes.len() > MAX_SYNC_FRAME_BYTES {
        return Err(SyncFrameError::FrameTooLarge {
            len: bytes.len(),
            max: MAX_SYNC_FRAME_BYTES,
        });
    }
    Ok((ver, bytes.get(1..).unwrap_or(&[])))
}

fn decode_versioned_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, SyncFrameError> {
    postcard::from_bytes::<T>(body).map_err(|_| SyncFrameError::Decode)
}

/// Encodes `frame` with the 1-byte [`SYNC_PROTOCOL_VERSION`] header and validates
/// that the resulting packet stays within [`MAX_SYNC_FRAME_BYTES`].
pub fn encode_sync_frame(frame: &WireSyncFrameV1) -> Result<Vec<u8>, SyncFrameError> {
    encode_versioned_frame(SYNC_PROTOCOL_VERSION, frame)
}

/// Validates version header and [`MAX_SYNC_FRAME_BYTES`] limit before decoding a
/// [`WireSyncFrameV1`] from `bytes`.
pub fn decode_sync_frame(bytes: &[u8]) -> Result<WireSyncFrameV1, SyncFrameError> {
    let (ver, body) = validate_incoming_header(bytes)?;
    if ver != SYNC_PROTOCOL_VERSION {
        return Err(SyncFrameError::UnsupportedVersion(ver));
    }
    decode_versioned_body(body)
}

/// Encodes a [`WireSyncFrameV2`] with [`SYNC_PROTOCOL_VERSION_V2`] (`2`).
pub fn encode_sync_frame_v2(frame: &WireSyncFrameV2) -> Result<Vec<u8>, SyncFrameError> {
    encode_versioned_frame(SYNC_PROTOCOL_VERSION_V2, frame)
}

/// Validates framing bounds and protocol version byte before decoding a [`WireSyncFrameV2`].
pub fn decode_sync_frame_v2(bytes: &[u8]) -> Result<WireSyncFrameV2, SyncFrameError> {
    let (ver, body) = validate_incoming_header(bytes)?;
    if ver != SYNC_PROTOCOL_VERSION_V2 {
        return Err(SyncFrameError::UnsupportedVersion(ver));
    }
    decode_versioned_body(body)
}

/// Decodes either a protocol `v1` frame (transparently upgraded to
/// [`WireSyncFrameV2`]) or a protocol `v2` frame, returning `(wire_version, frame)`.
pub fn decode_sync_frame_any(bytes: &[u8]) -> Result<(u8, WireSyncFrameV2), SyncFrameError> {
    let (ver, body) = validate_incoming_header(bytes)?;
    match ver {
        SYNC_PROTOCOL_VERSION => {
            let v1: WireSyncFrameV1 = decode_versioned_body(body)?;
            Ok((ver, v1.into()))
        }
        SYNC_PROTOCOL_VERSION_V2 => {
            let v2: WireSyncFrameV2 = decode_versioned_body(body)?;
            Ok((ver, v2))
        }
        _ => Err(SyncFrameError::UnsupportedVersion(ver)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::vec;
    use alloc::vec::Vec;

    use super::{
        MAX_SYNC_FRAME_BYTES, SyncFrameError, decode_sync_frame, decode_sync_frame_any,
        decode_sync_frame_v2, encode_sync_frame, encode_sync_frame_v2,
    };
    use crate::v2::{SYNC_PROTOCOL_VERSION, WireSyncFrameV1};
    use crate::v3::{SYNC_PROTOCOL_VERSION_V2, WireOpV3, WireSyncFrameV2, WireTxnV3};

    #[test]
    fn round_trips_and_rejects_bad_version_empty_or_oversized() {
        let frame = WireSyncFrameV1::Ping(42);
        let bytes = encode_sync_frame(&frame).unwrap();
        assert_eq!(bytes[0], SYNC_PROTOCOL_VERSION);
        assert_eq!(decode_sync_frame(&bytes), Ok(frame.clone()));
        assert_eq!(
            decode_sync_frame_any(&bytes),
            Ok((SYNC_PROTOCOL_VERSION, WireSyncFrameV2::from(frame)))
        );

        let v2_frame = WireSyncFrameV2::Commit {
            seq: 1,
            replica: 1,
            client_seq: 1,
            user_id: [0x22; 16],
            timestamp_ms: 123,
            txn: WireTxnV3 {
                label: alloc::string::String::from("Sort"),
                ops: vec![WireOpV3::PermuteContent {
                    sheet: 0,
                    rows: vec![0, 1],
                    cols: vec![0],
                    row_perm: vec![1, 0],
                }],
            },
        };
        let bytes_v2 = encode_sync_frame_v2(&v2_frame).unwrap();
        assert_eq!(bytes_v2[0], SYNC_PROTOCOL_VERSION_V2);
        assert_eq!(decode_sync_frame_v2(&bytes_v2), Ok(v2_frame.clone()));
        assert_eq!(
            decode_sync_frame_any(&bytes_v2),
            Ok((SYNC_PROTOCOL_VERSION_V2, v2_frame))
        );
        assert_eq!(
            decode_sync_frame(&bytes_v2),
            Err(SyncFrameError::UnsupportedVersion(SYNC_PROTOCOL_VERSION_V2))
        );

        assert_eq!(decode_sync_frame(&[]), Err(SyncFrameError::Empty));
        assert_eq!(decode_sync_frame_any(&[]), Err(SyncFrameError::Empty));
        assert_eq!(
            decode_sync_frame(&[99, 0]),
            Err(SyncFrameError::UnsupportedVersion(99))
        );
        assert_eq!(
            decode_sync_frame_any(&[99, 0]),
            Err(SyncFrameError::UnsupportedVersion(99))
        );
        let huge: Vec<u8> = vec![SYNC_PROTOCOL_VERSION; MAX_SYNC_FRAME_BYTES + 1];
        assert!(matches!(
            decode_sync_frame(&huge),
            Err(SyncFrameError::FrameTooLarge { .. })
        ));
    }
}

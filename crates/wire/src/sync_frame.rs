//! Versioned binary WebSocket frame encoder/decoder (`SYNC_PROTOCOL_VERSION = 1`).
//!
//! Layout: `[protocol_version: u8 (0x01)][postcard::to_allocvec(WireSyncFrameV1)]`,
//! bounded by [`MAX_SYNC_FRAME_BYTES`] (`4 MiB`) prior to deserialisation.

use alloc::vec::Vec;

use crate::v2::{SYNC_PROTOCOL_VERSION, WireSyncFrameV1};

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

/// Encodes `frame` with the 1-byte [`SYNC_PROTOCOL_VERSION`] header and validates
/// that the resulting packet stays within [`MAX_SYNC_FRAME_BYTES`].
pub fn encode_sync_frame(frame: &WireSyncFrameV1) -> Result<Vec<u8>, SyncFrameError> {
    let payload = postcard::to_allocvec(frame).map_err(|_| SyncFrameError::Encode)?;
    let total = payload.len().saturating_add(1);
    if total > MAX_SYNC_FRAME_BYTES {
        return Err(SyncFrameError::FrameTooLarge {
            len: total,
            max: MAX_SYNC_FRAME_BYTES,
        });
    }
    let mut out = Vec::with_capacity(total);
    out.push(SYNC_PROTOCOL_VERSION);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Validates version header and [`MAX_SYNC_FRAME_BYTES`] limit before decoding a
/// [`WireSyncFrameV1`] from `bytes`.
pub fn decode_sync_frame(bytes: &[u8]) -> Result<WireSyncFrameV1, SyncFrameError> {
    let Some(&ver) = bytes.first() else {
        return Err(SyncFrameError::Empty);
    };
    if bytes.len() > MAX_SYNC_FRAME_BYTES {
        return Err(SyncFrameError::FrameTooLarge {
            len: bytes.len(),
            max: MAX_SYNC_FRAME_BYTES,
        });
    }
    if ver != SYNC_PROTOCOL_VERSION {
        return Err(SyncFrameError::UnsupportedVersion(ver));
    }
    let body = bytes.get(1..).unwrap_or(&[]);
    postcard::from_bytes::<WireSyncFrameV1>(body).map_err(|_| SyncFrameError::Decode)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::vec;
    use alloc::vec::Vec;

    use super::{MAX_SYNC_FRAME_BYTES, SyncFrameError, decode_sync_frame, encode_sync_frame};
    use crate::v2::{SYNC_PROTOCOL_VERSION, WireSyncFrameV1};

    #[test]
    fn round_trips_and_rejects_bad_version_empty_or_oversized() {
        let frame = WireSyncFrameV1::Ping(42);
        let bytes = encode_sync_frame(&frame).unwrap();
        assert_eq!(bytes[0], SYNC_PROTOCOL_VERSION);
        assert_eq!(decode_sync_frame(&bytes), Ok(frame));

        assert_eq!(decode_sync_frame(&[]), Err(SyncFrameError::Empty));
        assert_eq!(
            decode_sync_frame(&[99, 0]),
            Err(SyncFrameError::UnsupportedVersion(99))
        );
        let huge: Vec<u8> = vec![SYNC_PROTOCOL_VERSION; MAX_SYNC_FRAME_BYTES + 1];
        assert!(matches!(
            decode_sync_frame(&huge),
            Err(SyncFrameError::FrameTooLarge { .. })
        ));
    }
}

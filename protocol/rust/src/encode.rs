//! Frame encoding: header, CRC, COBS, delimiter.

use crate::{
    CRC_LEN, DELIMITER, HEADER_LEN, MAX_ENCODED, MAX_FRAME, MAX_PAYLOAD,
    cobs_frame::{CobsError, cobs_encode, max_encoded_len},
    crc32::checksum,
    frame::FrameType,
};

/// Why a frame could not be encoded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// The payload exceeded [`crate::MAX_PAYLOAD`].
    PayloadTooLarge { len: usize },
    /// The destination slice was too small; it needs [`encoded_len`] bytes.
    DestTooSmall { need: usize, got: usize },
}

impl From<CobsError> for EncodeError {
    fn from(e: CobsError) -> Self {
        match e {
            // Only reachable if the caller's slice was short, which we check first.
            CobsError::DestTooSmall | CobsError::Malformed => EncodeError::DestTooSmall {
                need: MAX_ENCODED,
                got: 0,
            },
        }
    }
}

/// Worst-case encoded size, including the trailing delimiter, for a payload of `payload_len`.
#[inline]
pub const fn encoded_len(payload_len: usize) -> usize {
    max_encoded_len(HEADER_LEN + payload_len + CRC_LEN) + 1
}

/// Encode one frame into `dst`, returning the number of bytes written.
///
/// The written bytes are the COBS-encoded logical frame followed by a single `0x00`
/// delimiter, ready to hand to a transport.
pub fn encode_frame(
    ty: FrameType,
    seq: u8,
    payload: &[u8],
    dst: &mut [u8],
) -> Result<usize, EncodeError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(EncodeError::PayloadTooLarge { len: payload.len() });
    }
    let need = encoded_len(payload.len());
    if dst.len() < need {
        return Err(EncodeError::DestTooSmall {
            need,
            got: dst.len(),
        });
    }

    // Build the logical frame on the stack: 4-byte header, payload, 4-byte CRC.
    let mut logical = [0u8; MAX_FRAME];
    let body_len = HEADER_LEN + payload.len();
    logical[0] = ty.as_u8();
    logical[1..3].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    logical[3] = seq;
    logical[HEADER_LEN..body_len].copy_from_slice(payload);
    let crc = checksum(&logical[..body_len]);
    logical[body_len..body_len + CRC_LEN].copy_from_slice(&crc.to_le_bytes());
    let total = body_len + CRC_LEN;

    let written = cobs_encode(&logical[..total], dst)?;
    dst[written] = DELIMITER;
    Ok(written + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_output_has_exactly_one_delimiter_at_the_end() {
        let mut dst = [0u8; MAX_ENCODED];
        let payload = [0u8; 300]; // all delimiters pre-encoding, worst case for COBS
        let n = encode_frame(FrameType::CurrentSamples, 7, &payload, &mut dst).unwrap();
        assert_eq!(dst[n - 1], DELIMITER);
        assert!(
            !dst[..n - 1].contains(&DELIMITER),
            "COBS must not leave an embedded zero"
        );
    }

    #[test]
    fn oversize_payload_rejected() {
        let mut dst = [0u8; MAX_ENCODED];
        let big = [0u8; MAX_PAYLOAD + 1];
        assert_eq!(
            encode_frame(FrameType::Event, 0, &big, &mut dst),
            Err(EncodeError::PayloadTooLarge {
                len: MAX_PAYLOAD + 1
            })
        );
    }

    #[test]
    fn small_dest_rejected_with_required_size() {
        let mut dst = [0u8; 4];
        let err =
            encode_frame(FrameType::Hello, 0, &[1, 2, 3, 4, 5, 6, 7, 8], &mut dst).unwrap_err();
        match err {
            EncodeError::DestTooSmall { need, got } => {
                assert_eq!(got, 4);
                assert!(need > 4);
            }
            other => panic!("expected DestTooSmall, got {other:?}"),
        }
    }

    #[test]
    fn empty_payload_is_legal() {
        let mut dst = [0u8; 32];
        let n = encode_frame(FrameType::StartCapture, 0, &[], &mut dst).unwrap();
        assert!(n > 0 && n <= encoded_len(0));
    }
}

//! Binary framing shared by the on-disk log and the WebSocket link.
//!
//! A frame is `[len: u32 LE][crc32: u32 LE][payload: len bytes]`, the payload
//! being a `bitcode` value. The CRC lets the event log recover from a torn
//! write after a crash: replay stops at the first frame whose length runs past
//! the end of the file or whose checksum does not match, and the file is
//! truncated there.

use bitcode::{Decode, DecodeOwned, Encode};

/// Size of the frame header in bytes.
pub const FRAME_HEADER: usize = 8;

/// Upper bound on one frame, so a corrupted length can never make a reader
/// allocate gigabytes.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    /// More bytes are needed before the next frame is complete.
    Incomplete,
    /// The length field exceeds [`MAX_FRAME`].
    TooLarge(usize),
    /// The payload does not match its checksum.
    Checksum,
    /// The payload is not a valid value of the expected type.
    Decode(bitcode::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete => f.write_str("incomplete frame"),
            Self::TooLarge(len) => write!(f, "frame of {len} bytes exceeds the limit"),
            Self::Checksum => f.write_str("frame checksum mismatch"),
            Self::Decode(e) => write!(f, "frame payload: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encodes `value` as a bare `bitcode` payload (no frame header).
pub fn encode<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    bitcode::encode(value)
}

/// Decodes a bare `bitcode` payload.
pub fn decode<T: DecodeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    bitcode::decode(bytes).map_err(FrameError::Decode)
}

/// Appends one framed value to `out`. Returns the number of bytes written.
pub fn write_frame<T: Encode + ?Sized>(value: &T, out: &mut Vec<u8>) -> usize {
    let payload = bitcode::encode(value);
    let start = out.len();
    out.reserve(FRAME_HEADER + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out.len() - start
}

/// Reads framed values out of a byte buffer without copying payloads.
pub struct FrameReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> FrameReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Offset just past the last complete, valid frame.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The next payload, checksum verified.
    pub fn next_payload(&mut self) -> Result<&'a [u8], FrameError> {
        let rest = &self.buf[self.pos..];
        if rest.len() < FRAME_HEADER {
            return Err(FrameError::Incomplete);
        }
        let len = u32::from_le_bytes(rest[0..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(FrameError::TooLarge(len));
        }
        let crc = u32::from_le_bytes(rest[4..8].try_into().unwrap());
        let Some(payload) = rest.get(FRAME_HEADER..FRAME_HEADER + len) else {
            return Err(FrameError::Incomplete);
        };
        if crc32fast::hash(payload) != crc {
            return Err(FrameError::Checksum);
        }
        self.pos += FRAME_HEADER + len;
        Ok(payload)
    }

    /// The next decoded value.
    pub fn read<T: for<'de> Decode<'de>>(&mut self) -> Result<T, FrameError> {
        let start = self.pos;
        let payload = self.next_payload()?;
        bitcode::decode(payload).map_err(|e| {
            self.pos = start;
            FrameError::Decode(e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Sample {
        a: u64,
        b: String,
    }

    #[test]
    fn frames_round_trip_and_torn_tail_is_detected() {
        let mut buf = Vec::new();
        write_frame(&Sample { a: 1, b: "one".into() }, &mut buf);
        let keep = buf.len();
        write_frame(&Sample { a: 2, b: "two".into() }, &mut buf);
        buf.truncate(buf.len() - 2); // simulate a crash mid-write

        let mut reader = FrameReader::new(&buf);
        assert_eq!(reader.read::<Sample>().unwrap(), Sample { a: 1, b: "one".into() });
        assert!(matches!(reader.read::<Sample>(), Err(FrameError::Incomplete)));
        assert_eq!(reader.position(), keep);
    }

    #[test]
    fn corrupted_payload_fails_checksum() {
        let mut buf = Vec::new();
        write_frame(&Sample { a: 7, b: "seven".into() }, &mut buf);
        let last = buf.len() - 1;
        buf[last] ^= 0xff;
        assert!(matches!(FrameReader::new(&buf).read::<Sample>(), Err(FrameError::Checksum)));
    }
}

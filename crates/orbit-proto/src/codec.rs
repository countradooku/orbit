//! Binary framing shared by the on-disk logs and the WebSocket link.
//!
//! A frame is `[len: u32 LE][crc32: u32 LE][payload: len bytes]`, the payload
//! being a value in Orbit's wire format. The CRC lets the event log recover
//! from a torn write after a crash: replay stops at the first frame whose
//! length runs past the end of the file or whose checksum does not match,
//! and the file is truncated there.
//!
//! The wire format is `wincode` with variable-length integers and one-byte
//! enum tags ([`WireConfig`]). Its cost follows the value, not the size of
//! the type: a call or a log record takes tens of nanoseconds (see
//! `docs/research/codecs.md`). Data written before schema 5 is `bitcode`;
//! [`bitcode_decode`] reads it so old data folders can be migrated.

use wincode::config::Configuration;
use wincode::int_encoding::{LittleEndian, VarInt};
use wincode::len::BincodeLen;

/// Size of the frame header in bytes.
pub const FRAME_HEADER: usize = 8;

/// Upper bound on one frame, so a corrupted length can never make a reader
/// allocate gigabytes.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Orbit's wire format: wincode, little-endian varints, `u8` enum tags, and
/// collections preallocated up to [`MAX_FRAME`] (wincode's 4 MiB default
/// would refuse a long thread's timeline).
pub type WireConfig = Configuration<true, MAX_FRAME, BincodeLen, LittleEndian, VarInt, u8>;

/// The [`WireConfig`] value.
pub const fn wire_config() -> WireConfig {
    Configuration::default()
        .with_varint_encoding()
        .with_tag_encoding::<u8>()
        .with_preallocation_size_limit::<MAX_FRAME>()
}

/// A value in Orbit's wire format.
pub trait Wire:
    wincode::SchemaWrite<WireConfig, Src = Self> + for<'de> wincode::SchemaRead<'de, WireConfig, Dst = Self>
{
}

impl<T> Wire for T where
    T: wincode::SchemaWrite<WireConfig, Src = T> + for<'de> wincode::SchemaRead<'de, WireConfig, Dst = T>
{
}

#[derive(Debug)]
pub enum FrameError {
    /// More bytes are needed before the next frame is complete.
    Incomplete,
    /// The length field exceeds [`MAX_FRAME`].
    TooLarge(usize),
    /// The payload does not match its checksum.
    Checksum,
    /// The payload is not a valid value of the expected type.
    Decode(String),
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

/// An encoder that keeps its output buffer, so a connection or log writer
/// does not allocate per message.
#[derive(Default)]
pub struct Codec {
    buf: Vec<u8>,
}

impl Codec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes `value`; the bytes are valid until the next call.
    pub fn encode<T: Wire>(&mut self, value: &T) -> &[u8] {
        self.buf.clear();
        encode_into(value, &mut self.buf);
        &self.buf
    }

    pub fn decode<T: Wire>(&mut self, bytes: &[u8]) -> Result<T, FrameError> {
        decode(bytes)
    }

    /// Appends one framed value to `out`. Returns the number of bytes written.
    pub fn write_frame<T: Wire>(&mut self, value: &T, out: &mut Vec<u8>) -> usize {
        self.buf.clear();
        encode_into(value, &mut self.buf);
        let start = out.len();
        write_raw_frame(&self.buf, out);
        out.len() - start
    }
}

/// Appends `value` to `out`.
pub fn encode_into<T: Wire>(value: &T, out: &mut Vec<u8>) {
    // Writing to a growable Vec cannot fail; an error would be a bug in a
    // schema, not bad input.
    let at = out.len();
    wincode::config::serialize_into(wincode::io::Cursor::new_at(out, at), value, wire_config())
        .expect("encoding into a Vec cannot fail");
}

/// Encodes `value` as a bare payload (no frame header).
pub fn encode<T: Wire>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

/// Decodes a bare payload.
pub fn decode<T: Wire>(bytes: &[u8]) -> Result<T, FrameError> {
    wincode::config::deserialize::<T, _>(bytes, wire_config()).map_err(|e| FrameError::Decode(e.to_string()))
}

/// Decodes a payload written before schema 5, in `bitcode`.
pub fn bitcode_decode<T: bitcode::DecodeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    bitcode::decode(bytes).map_err(|e| FrameError::Decode(e.to_string()))
}

/// Appends one framed value to `out`. Returns the number of bytes written.
pub fn write_frame<T: Wire>(value: &T, out: &mut Vec<u8>) -> usize {
    let start = out.len();
    // Reserve the header, write the payload after it, then fill it in.
    out.extend_from_slice(&[0; FRAME_HEADER]);
    encode_into(value, out);
    let len = out.len() - start - FRAME_HEADER;
    let crc = crc32fast::hash(&out[start + FRAME_HEADER..]);
    out[start..start + 4].copy_from_slice(&(len as u32).to_le_bytes());
    out[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
    out.len() - start
}

/// Appends one frame around raw `payload` bytes (headers that are not a
/// wire value).
pub fn write_raw_frame(payload: &[u8], out: &mut Vec<u8>) {
    out.reserve(FRAME_HEADER + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    out.extend_from_slice(payload);
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
    pub fn read<T: Wire>(&mut self) -> Result<T, FrameError> {
        let start = self.pos;
        let payload = self.next_payload()?;
        decode(payload).inspect_err(|_| self.pos = start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(wincode::SchemaWrite, wincode::SchemaRead, PartialEq, Debug)]
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

    #[test]
    fn a_reused_codec_and_appended_frames_match_one_shot_encoding() {
        let sample = Sample { a: 300, b: "x".repeat(70_000) };
        let mut codec = Codec::new();
        assert_eq!(codec.encode(&sample), encode(&sample).as_slice());
        let mut a = vec![9, 9];
        let mut b = vec![9, 9];
        codec.write_frame(&sample, &mut a);
        write_frame(&sample, &mut b);
        assert_eq!(a, b);
        assert_eq!(FrameReader::new(&a[2..]).read::<Sample>().unwrap(), sample);
    }
}

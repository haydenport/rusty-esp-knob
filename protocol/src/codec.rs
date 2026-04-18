//! Wire framing: `postcard` serialization + CRC-8 + COBS framing.
//!
//! Frame layout on the wire (before COBS):
//! ```text
//! [ postcard-encoded message bytes ][ crc8 ]
//! ```
//! This is then COBS-encoded and terminated with a 0x00 byte. The 0x00
//! byte never appears inside a COBS-encoded frame, so the decoder can
//! split the stream on 0x00 boundaries.
//!
//! The buffered [`Decoder`] lets you feed bytes as they arrive (e.g. from
//! USB reads) and pull complete, validated messages out one at a time.

use alloc::vec::Vec;
use serde::{de::DeserializeOwned, Serialize};

/// All codec errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// postcard serialization failed.
    Serialize,
    /// postcard deserialization failed.
    Deserialize,
    /// COBS decode failed (corrupt framing).
    Cobs,
    /// CRC-8 mismatch.
    BadCrc,
    /// Frame was too short to contain even a CRC byte.
    FrameTooShort,
    /// Output buffer would overflow.
    BufferFull,
}

/// CRC-8 (SMBus polynomial 0x07, init 0x00).
///
/// Small, fast, plenty for catching bit errors on a short USB frame.
fn crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Serialize, append CRC-8, COBS-encode, and terminate with 0x00.
///
/// Returns the wire bytes ready to write to the transport.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, Error> {
    // 1. postcard serialize
    let payload = postcard::to_allocvec(msg).map_err(|_| Error::Serialize)?;

    // 2. Append CRC-8
    let mut with_crc = Vec::with_capacity(payload.len() + 1);
    with_crc.extend_from_slice(&payload);
    with_crc.push(crc8(&payload));

    // 3. COBS encode with 0x00 terminator
    let max_cobs_len = cobs::max_encoding_length(with_crc.len());
    let mut out = alloc::vec![0u8; max_cobs_len + 1];
    let encoded_len = cobs::encode(&with_crc, &mut out);
    out.truncate(encoded_len + 1);
    out[encoded_len] = 0x00;

    Ok(out)
}

/// Decode a single COBS-framed payload (without the 0x00 terminator).
///
/// Use this if you already have the trimmed frame; otherwise use [`Decoder`]
/// to handle streaming buffers.
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, Error> {
    // 1. COBS decode
    let mut decoded = alloc::vec![0u8; frame.len()];
    let decoded_len = cobs::decode(frame, &mut decoded).map_err(|_| Error::Cobs)?;
    decoded.truncate(decoded_len);

    // 2. Split off CRC-8
    if decoded.len() < 2 {
        return Err(Error::FrameTooShort);
    }
    let (payload, crc_bytes) = decoded.split_at(decoded.len() - 1);
    if crc8(payload) != crc_bytes[0] {
        return Err(Error::BadCrc);
    }

    // 3. postcard deserialize
    postcard::from_bytes(payload).map_err(|_| Error::Deserialize)
}

/// Streaming decoder — accumulates bytes and yields complete frames as they arrive.
///
/// Push arbitrary byte chunks from your transport via [`Decoder::push`], then
/// call [`Decoder::next_frame`] in a loop until it returns `None` to drain all
/// pending frames. The internal buffer is bounded by `max_frame_len` to prevent
/// runaway growth from a stuck sender.
pub struct Decoder {
    buf: Vec<u8>,
    max_frame_len: usize,
}

impl Decoder {
    pub fn new(max_frame_len: usize) -> Self {
        Self {
            buf: Vec::with_capacity(max_frame_len),
            max_frame_len,
        }
    }

    /// Append bytes from the transport. Returns `Err` if the internal buffer
    /// would exceed `max_frame_len` — caller should drop and reset.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.buf.len() + bytes.len() > self.max_frame_len {
            self.buf.clear();
            return Err(Error::BufferFull);
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Try to decode one frame. Returns `Ok(Some(msg))` on success,
    /// `Ok(None)` if no complete frame is buffered yet, or `Err` if the
    /// next frame was corrupt (which is still consumed so decoding can progress).
    ///
    /// Decodes COBS **in place** inside `self.buf` — for an 8 KB icon frame
    /// this saves ~16 KB of transient heap versus draining into a fresh Vec
    /// and copying to a second COBS output buffer. The firmware has only
    /// ~18 KB of free heap after the framebuffer, so allocating two 8 KB
    /// scratch buffers was hitting OOM mid-decode.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        let Some(end) = self.buf.iter().position(|&b| b == 0x00) else {
            return Ok(None);
        };

        // Empty frame (just a lone terminator) — skip it.
        if end == 0 {
            self.buf.drain(..=end);
            return Ok(None);
        }

        let result = decode_in_place::<T>(&mut self.buf[..end]);
        // Consume the frame bytes regardless of outcome so a bad frame does
        // not wedge the decoder.
        self.buf.drain(..=end);
        result.map(Some)
    }
}

/// COBS-decode `buf` in place, validate the trailing CRC, and postcard-deserialize.
fn decode_in_place<T: DeserializeOwned>(buf: &mut [u8]) -> Result<T, Error> {
    let decoded_len = cobs::decode_in_place(buf).map_err(|_| Error::Cobs)?;
    if decoded_len < 2 {
        return Err(Error::FrameTooShort);
    }
    let (payload, crc_bytes) = buf[..decoded_len].split_at(decoded_len - 1);
    if crc8(payload) != crc_bytes[0] {
        return Err(Error::BadCrc);
    }
    postcard::from_bytes(payload).map_err(|_| Error::Deserialize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{DeviceToHost, HostToDevice};

    #[test]
    fn round_trip_ping() {
        let frame = encode(&HostToDevice::Ping).unwrap();
        // Should end with 0x00 terminator
        assert_eq!(*frame.last().unwrap(), 0x00);
        let decoded: HostToDevice = decode(&frame[..frame.len() - 1]).unwrap();
        assert_eq!(decoded, HostToDevice::Ping);
    }

    #[test]
    fn round_trip_echo_payload() {
        let msg = HostToDevice::Echo(alloc::vec![1, 2, 3, 0, 4, 5]);
        let frame = encode(&msg).unwrap();
        let decoded: HostToDevice = decode(&frame[..frame.len() - 1]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn streaming_decoder_two_frames() {
        let a = encode(&DeviceToHost::Pong).unwrap();
        let b = encode(&DeviceToHost::EncoderDelta(42)).unwrap();
        let mut combined = a.clone();
        combined.extend_from_slice(&b);

        let mut dec = Decoder::new(256);
        dec.push(&combined).unwrap();
        let m1: DeviceToHost = dec.next_frame().unwrap().unwrap();
        let m2: DeviceToHost = dec.next_frame().unwrap().unwrap();
        assert_eq!(m1, DeviceToHost::Pong);
        assert_eq!(m2, DeviceToHost::EncoderDelta(42));
        assert!(dec.next_frame::<DeviceToHost>().unwrap().is_none());
    }

    #[test]
    fn corrupt_frame_rejected() {
        // Use a payload with enough bytes to flip one without breaking COBS framing.
        let msg = HostToDevice::Echo(alloc::vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let mut frame = encode(&msg).unwrap();
        // Flip a bit inside the payload (not the terminator, not the COBS overhead).
        frame[3] ^= 0x01;
        assert!(decode::<HostToDevice>(&frame[..frame.len() - 1]).is_err());
    }
}

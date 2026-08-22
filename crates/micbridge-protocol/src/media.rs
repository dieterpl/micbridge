//! The media datagram: a fixed 16-byte header followed by interleaved PCM.
//!
//! Media is one-way and never retransmitted. A datagram that arrives late is
//! worth less than the silence it would displace, so the receiver drops it
//! rather than stalling to put it back in order.

use std::fmt;

/// `"RP"`. Present so a stray datagram on the media port is rejected cheaply
/// rather than being interpreted as audio.
pub const MAGIC: u16 = 0x5250;

/// Length of the encoded header, in bytes.
pub const HEADER_LEN: usize = 16;

/// Interleaved PCM in the format negotiated during the control handshake.
pub const CHANNEL_AUDIO: u8 = 0;

/// Reserved for HID reports. Out of scope for this version — the constant
/// exists so the channel byte is not quietly reused for something else, and so
/// a receiver that sees one can say what it is while ignoring it.
pub const CHANNEL_HID: u8 = 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("datagram is {0} bytes, shorter than the {HEADER_LEN}-byte header")]
    TooShort(usize),
    #[error("bad magic {0:#06x}, expected {MAGIC:#06x}")]
    BadMagic(u16),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
}

/// The header of a media datagram.
///
/// Layout is fixed, big-endian, and exactly [`HEADER_LEN`] bytes:
///
/// ```text
/// offset  size  field
///      0     2  magic       = 0x5250
///      2     1  version
///      3     1  channel
///      4     4  seq
///      8     8  sample_idx
/// ```
///
/// There is no flags byte. Extension goes through `channel` for new payload
/// kinds and `version` for incompatible changes to an existing one, which
/// covers the cases a flags byte would have and keeps `sample_idx` at a natural
/// 8-byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub version: u8,
    pub channel: u8,
    /// Increments by one per datagram and wraps. The receiver uses it only to
    /// count loss and spot reordering; it is never used for placement, because
    /// `sample_idx` says where the payload belongs.
    pub seq: u32,
    /// Index of the first frame in this payload, counted from the start of the
    /// sender's capture stream.
    ///
    /// This is a **frame counter, not a clock**. The two machines therefore
    /// never have to agree on a time base, and the receiver's placement of a
    /// datagram is unaffected by how long it spent in flight. Drift is measured
    /// from buffer occupancy instead — see `micbridge_core::DriftController`.
    ///
    /// At 48 kHz a `u64` lasts about twelve million years, so wrap is not a
    /// case the receiver handles.
    pub sample_idx: u64,
}

impl MediaHeader {
    pub fn new(channel: u8, seq: u32, sample_idx: u64) -> Self {
        Self { version: super::PROTOCOL_VERSION as u8, channel, seq, sample_idx }
    }

    /// Writes the header into the first [`HEADER_LEN`] bytes of `out`.
    ///
    /// # Panics
    /// If `out` is shorter than [`HEADER_LEN`].
    pub fn encode_into(&self, out: &mut [u8]) {
        assert!(out.len() >= HEADER_LEN, "encode_into needs {HEADER_LEN} bytes, got {}", out.len());
        out[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        out[2] = self.version;
        out[3] = self.channel;
        out[4..8].copy_from_slice(&self.seq.to_be_bytes());
        out[8..16].copy_from_slice(&self.sample_idx.to_be_bytes());
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        self.encode_into(&mut out);
        out
    }

    /// Parses a header and returns it alongside the remaining payload bytes.
    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), Error> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::TooShort(bytes.len()));
        }
        let magic = u16::from_be_bytes([bytes[0], bytes[1]]);
        if magic != MAGIC {
            return Err(Error::BadMagic(magic));
        }
        let version = bytes[2];
        if version as u32 != super::PROTOCOL_VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let header = Self {
            version,
            channel: bytes[3],
            seq: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            sample_idx: u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes")),
        };
        Ok((header, &bytes[HEADER_LEN..]))
    }
}

impl fmt::Display for MediaHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let channel = match self.channel {
            CHANNEL_AUDIO => "audio",
            CHANNEL_HID => "hid",
            _ => "unknown",
        };
        write!(f, "{channel} seq={} idx={}", self.seq, self.sample_idx)
    }
}

/// Frames that fit in one datagram, given a payload budget and channel count.
///
/// Used to keep the sender's packetisation inside a safe MTU. The default
/// 5 ms / 48 kHz / stereo packet is 960 payload bytes, well inside any path.
pub fn max_frames_per_packet(
    payload_budget: usize,
    channels: u16,
    bytes_per_sample: usize,
) -> usize {
    let per_frame = channels as usize * bytes_per_sample;
    // `checked_div` rather than a guard, so a zero-channel or zero-width format
    // yields zero frames instead of dividing by zero.
    payload_budget.checked_div(per_frame).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let header = MediaHeader::new(CHANNEL_AUDIO, 0xDEAD_BEEF, 0x0123_4567_89AB_CDEF);
        let bytes = header.encode();
        let (decoded, payload) = MediaHeader::decode(&bytes).expect("decodes");
        assert_eq!(header, decoded);
        assert!(payload.is_empty());
    }

    #[test]
    fn payload_is_returned_after_the_header() {
        let mut buf = vec![0u8; HEADER_LEN + 4];
        MediaHeader::new(CHANNEL_AUDIO, 1, 2).encode_into(&mut buf);
        buf[HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);
        let (_, payload) = MediaHeader::decode(&buf).expect("decodes");
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn header_is_exactly_sixteen_bytes() {
        assert_eq!(MediaHeader::new(CHANNEL_AUDIO, 0, 0).encode().len(), 16);
    }

    #[test]
    fn rejects_short_bad_magic_and_bad_version() {
        assert_eq!(MediaHeader::decode(&[0u8; 4]), Err(Error::TooShort(4)));

        let mut buf = MediaHeader::new(CHANNEL_AUDIO, 1, 1).encode();
        buf[0] = 0xFF;
        assert!(matches!(MediaHeader::decode(&buf), Err(Error::BadMagic(_))));

        let mut buf = MediaHeader::new(CHANNEL_AUDIO, 1, 1).encode();
        buf[2] = 99;
        assert_eq!(MediaHeader::decode(&buf), Err(Error::UnsupportedVersion(99)));
    }

    #[test]
    fn wire_layout_is_pinned() {
        // Guards the byte layout against an accidental field reorder. If this
        // changes, docs/protocol.md changes with it.
        let bytes = MediaHeader::new(CHANNEL_AUDIO, 0x0000_0102, 0x0000_0000_0000_03E8).encode();
        insta::assert_debug_snapshot!(bytes);
    }

    #[test]
    fn packet_sizing_matches_the_documented_default() {
        // 5 ms at 48 kHz stereo i16 = 240 frames = 960 payload bytes.
        assert_eq!(max_frames_per_packet(1200, 2, 2), 300);
        // The whole datagram, header included, against a 1500-byte Ethernet MTU.
        // This margin is the reason 240 frames is the default.
        assert_eq!(240 * 2 * 2 + HEADER_LEN, 976);
    }
}

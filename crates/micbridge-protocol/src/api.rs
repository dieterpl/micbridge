//! Control-channel messages.
//!
//! Serialised as MessagePack maps with named fields, so adding a field is
//! backward compatible in both directions: an older peer ignores a key it does
//! not know, and `serde`'s `default` fills one that is absent. Only a change
//! that an existing peer would misparse needs [`crate::PROTOCOL_VERSION`]
//! bumped.

use serde::{Deserialize, Serialize};

/// Sample format on the wire.
///
/// Only signed 16-bit little-endian exists in version 1. The enum is here so a
/// future float or packed format is an added variant rather than a reinterpreted
/// field — a receiver that meets an unknown variant fails the handshake with a
/// clear error instead of rendering noise at full scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireSampleFormat {
    S16Le,
}

impl WireSampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::S16Le => 2,
        }
    }
}

/// The shape of the PCM stream carried on the media channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: WireSampleFormat,
    /// Frames per media datagram. The receiver needs it to size its buffers
    /// before the first datagram arrives, so it is negotiated rather than
    /// inferred.
    pub frames_per_packet: u32,
}

impl StreamFormat {
    pub fn bytes_per_frame(&self) -> usize {
        self.channels as usize * self.sample_format.bytes_per_sample()
    }

    pub fn payload_bytes_per_packet(&self) -> usize {
        self.frames_per_packet as usize * self.bytes_per_frame()
    }

    /// Duration of one packet in milliseconds, for logs and buffer maths.
    pub fn packet_ms(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.frames_per_packet as f64 * 1000.0 / self.sample_rate as f64
    }
}

/// Sender's opening message. Must be the first frame on the control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    /// Distinguishes a reconnect from a second concurrent sender. The receiver
    /// accepts one session at a time and rejects the other with
    /// [`ServerMessage::Error`].
    pub session_id: u64,
    /// Human-readable capture device name, for the receiver's logs. Purely
    /// informational — the receiver never matches on it.
    pub device_name: String,
    pub format: StreamFormat,
}

/// Receiver's answer to [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub protocol_version: u32,
    /// Where to send media. The receiver reports the port it actually bound,
    /// which may differ from the default if that was already taken.
    pub media_port: u16,
    /// Jitter-buffer target the receiver settled on. Echoed so the sender can
    /// log the end-to-end latency budget without being told twice.
    pub target_buffer_ms: u32,
    /// The format the receiver will decode. Version 1 requires this to equal
    /// the format in `Hello`; a receiver that cannot honour the request fails
    /// the handshake rather than silently converting, because a mismatch here
    /// would show up as pitch-shifted audio rather than as an error.
    pub format: StreamFormat,
}

/// Sender-side counters, reported periodically for observability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SenderStats {
    pub frames_captured: u64,
    pub packets_sent: u64,
    /// Frames dropped because the capture callback outran the network thread.
    /// Non-zero means the send ring is too small or the socket is blocking.
    pub frames_dropped: u64,
}

/// Receiver-side counters. These are the numbers the soak test asserts on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ReceiverStats {
    pub packets_received: u64,
    /// Gaps inferred from `sample_idx`, in frames.
    pub frames_lost: u64,
    /// Datagrams that arrived after their position had already been played.
    pub packets_late: u64,
    /// Datagrams that arrived out of order but early enough to be placed.
    pub packets_reordered: u64,
    /// Render callbacks that found too little buffered audio.
    pub underruns: u64,
    /// Frames discarded because the buffer filled faster than it drained.
    pub overruns: u64,
    pub buffer_fill_ms: f32,
    pub resample_ratio: f64,
}

/// Sender to receiver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(Hello),
    Heartbeat,
    Stats(SenderStats),
    Goodbye,
}

/// Receiver to sender.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMessage {
    HelloAck(HelloAck),
    Heartbeat,
    Stats(ReceiverStats),
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing;

    fn format() -> StreamFormat {
        StreamFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_format: WireSampleFormat::S16Le,
            frames_per_packet: 240,
        }
    }

    #[test]
    fn default_packet_is_five_milliseconds_and_960_bytes() {
        let f = format();
        assert_eq!(f.packet_ms(), 5.0);
        assert_eq!(f.payload_bytes_per_packet(), 960);
    }

    #[test]
    fn client_messages_round_trip() {
        let hello = ClientMessage::Hello(Hello {
            protocol_version: crate::PROTOCOL_VERSION,
            session_id: 0x1234,
            device_name: "UMC204HD 192k".to_string(),
            format: format(),
        });
        let bytes = framing::encode(&hello).expect("encodes");
        assert_eq!(framing::decode::<ClientMessage>(&bytes).expect("decodes"), hello);
    }

    #[test]
    fn unit_variants_round_trip() {
        for msg in [ClientMessage::Heartbeat, ClientMessage::Goodbye] {
            let bytes = framing::encode(&msg).expect("encodes");
            assert_eq!(framing::decode::<ClientMessage>(&bytes).expect("decodes"), msg);
        }
    }

    #[test]
    fn server_messages_round_trip() {
        let ack = ServerMessage::HelloAck(HelloAck {
            protocol_version: crate::PROTOCOL_VERSION,
            media_port: crate::DEFAULT_MEDIA_PORT,
            target_buffer_ms: 20,
            format: format(),
        });
        let bytes = framing::encode(&ack).expect("encodes");
        assert_eq!(framing::decode::<ServerMessage>(&bytes).expect("decodes"), ack);

        let err = ServerMessage::Error { message: "session already active".to_string() };
        let bytes = framing::encode(&err).expect("encodes");
        assert_eq!(framing::decode::<ServerMessage>(&bytes).expect("decodes"), err);
    }

    #[test]
    fn stats_round_trip_with_floats() {
        let stats = ServerMessage::Stats(ReceiverStats {
            packets_received: 12_000,
            buffer_fill_ms: 19.5,
            resample_ratio: 1.000_042,
            ..Default::default()
        });
        let bytes = framing::encode(&stats).expect("encodes");
        assert_eq!(framing::decode::<ServerMessage>(&bytes).expect("decodes"), stats);
    }
}

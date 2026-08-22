//! Wire types and framing for micbridge.
//!
//! This crate is deliberately free of platform and transport dependencies: it
//! knows how to turn messages into bytes and back, and nothing about sockets,
//! audio devices, or threads. That is what lets the protocol be snapshot-tested
//! on any target, including the platform the code will never run on.
//!
//! Two channels, described in full in `docs/protocol.md`:
//!
//! * **Control** — TCP, length-prefixed MessagePack ([`framing`], [`api`]).
//!   Session setup, format negotiation, heartbeats, statistics.
//! * **Media** — UDP, fixed 16-byte header plus PCM ([`media`]). One-way,
//!   loss-tolerant, never retransmitted.

pub mod api;
pub mod discovery;
pub mod framing;
pub mod media;

pub use api::{
    ClientMessage, Hello, HelloAck, ReceiverStats, SenderStats, ServerMessage, StreamFormat,
    WireSampleFormat,
};
pub use framing::{Error as FramingError, FrameReader, MAX_FRAME_LEN};
pub use media::{Error as MediaError, MediaHeader, CHANNEL_AUDIO, CHANNEL_HID, HEADER_LEN, MAGIC};

/// Bumped only for changes that a peer cannot parse. Additive fields on control
/// messages do not require a bump: MessagePack maps are keyed by name, so an
/// older peer ignores what it does not recognise.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default control port. Chosen to sit clear of the GameStream range
/// (47984-47990 and 48010) that Sunshine and Moonlight already occupy on the
/// same two machines.
pub const DEFAULT_CONTROL_PORT: u16 = 42100;

/// Default media port.
pub const DEFAULT_MEDIA_PORT: u16 = 42101;

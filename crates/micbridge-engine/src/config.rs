//! Session configuration.
//!
//! Plain data with no dependency on `clap` or on any GUI framework, so the CLI
//! and the GUI build the same structs rather than each carrying its own notion of
//! what a session is.

use std::path::PathBuf;

/// Frames per media datagram. 240 at 48 kHz is 5 ms, which keeps a stereo packet
/// at 960 payload bytes — comfortably inside any path's MTU.
pub const DEFAULT_PACKET_FRAMES: u32 = 240;

/// Jitter-buffer target in milliseconds. The dominant term in end-to-end latency
/// and the budget for network jitter.
pub const DEFAULT_TARGET_BUFFER_MS: u32 = 20;

/// Gain in decibels. Zero is unity — the signal is passed through untouched.
pub const DEFAULT_GAIN_DB: f32 = 0.0;

/// The gain range a frontend should offer. Re-exported here so the CLI and the GUI
/// do not each need a dependency on `micbridge-core` to draw one slider.
pub use micbridge_core::gain::{MAX_DB as MAX_GAIN_DB, MIN_DB as MIN_GAIN_DB};

/// Where a sender gets its audio.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// A capture device, matched by case-insensitive substring. `None` uses the
    /// system default input.
    Device(Option<String>),
    /// A synthetic sine wave at this frequency. Needs no device and no microphone
    /// permission, which is what makes it the right first step when bringing a
    /// link up across two machines.
    Tone(f64),
}

impl Default for Source {
    fn default() -> Self {
        Self::Device(None)
    }
}

/// Where a receiver puts audio.
#[derive(Debug, Clone, PartialEq)]
pub enum Sink {
    /// An output device, matched by case-insensitive substring. On Windows this is
    /// where `"CABLE Input"` goes. `None` uses the system default output.
    Device(Option<String>),
    /// A WAV file. Needs no audio hardware at all.
    Wav(PathBuf),
}

impl Default for Sink {
    fn default() -> Self {
        Self::Device(None)
    }
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub host: String,
    pub port: u16,
    pub source: Source,
    /// Override the capture rate. `None` accepts the device's own, which avoids
    /// making CoreAudio resample before we ever see the samples.
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub packet_frames: u32,
    /// Capture callback size in frames. `None` lets the host decide.
    pub capture_frames: Option<u32>,
    /// Stop after this many seconds. `None` runs until stopped.
    pub duration_secs: Option<u64>,
    /// Applied to captured audio before it is sent. Positive amplifies.
    pub gain_db: f32,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: micbridge_protocol::DEFAULT_CONTROL_PORT,
            source: Source::default(),
            sample_rate: None,
            channels: None,
            packet_frames: DEFAULT_PACKET_FRAMES,
            capture_frames: None,
            duration_secs: None,
            gain_db: DEFAULT_GAIN_DB,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub bind: String,
    pub port: u16,
    /// Preferred media port. Reported to the sender in the handshake, so it never
    /// has to be configured on both sides.
    pub media_port: u16,
    pub sink: Sink,
    pub target_buffer_ms: u32,
    /// Render callback size in frames. `None` lets the host decide.
    pub render_frames: Option<u32>,
    /// Chunk size for the WAV sink, standing in for a device callback.
    pub wav_chunk_frames: u32,
    /// Serve one session and return, rather than waiting for the next sender.
    pub once: bool,
    pub duration_secs: Option<u64>,
    /// Answer discovery probes so a sender can find this machine without being told
    /// an address. Harmless to leave on: it replies to probes and initiates nothing.
    pub announce: bool,
    /// Port to answer probes on. Zero picks any free port, which is only useful in
    /// tests — a prober broadcasts to the default and would not find it.
    pub discovery_port: u16,
    /// Something human-readable in the reply, to tell two receivers apart.
    pub label: String,
    /// Applied to received audio before it reaches the device. Positive amplifies.
    ///
    /// Offered on both ends deliberately: the machine where "too quiet" is noticed
    /// is often not the machine holding the interface, and this program exists
    /// precisely because those two are in different places.
    pub gain_db: f32,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            port: micbridge_protocol::DEFAULT_CONTROL_PORT,
            media_port: micbridge_protocol::DEFAULT_MEDIA_PORT,
            sink: Sink::default(),
            target_buffer_ms: DEFAULT_TARGET_BUFFER_MS,
            render_frames: None,
            wav_chunk_frames: DEFAULT_PACKET_FRAMES,
            once: false,
            duration_secs: None,
            announce: true,
            discovery_port: micbridge_protocol::discovery::DEFAULT_DISCOVERY_PORT,
            label: String::new(),
            gain_db: DEFAULT_GAIN_DB,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let sender = SenderConfig::default();
        assert_eq!(sender.packet_frames, 240);
        assert_eq!(sender.port, 42100);
        assert_eq!(sender.source, Source::Device(None));

        assert_eq!(sender.gain_db, 0.0, "a default session must not alter the signal");

        let receiver = ReceiverConfig::default();
        assert_eq!(receiver.target_buffer_ms, 20);
        assert_eq!(receiver.gain_db, 0.0);
        assert_eq!(receiver.port, 42100);
        assert_eq!(receiver.media_port, 42101);
        assert_eq!(receiver.bind, "0.0.0.0");
    }

    #[test]
    fn a_default_packet_is_five_milliseconds_at_48k() {
        assert_eq!(DEFAULT_PACKET_FRAMES as f64 / 48_000.0 * 1000.0, 5.0);
    }
}

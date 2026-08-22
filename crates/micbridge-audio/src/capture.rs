//! Input capture.
//!
//! The capture callback does one thing: copy the device's frames into a
//! lock-free ring. Everything else — packetising, converting to `i16`, talking
//! to a socket — happens on the network thread, because a syscall in a CoreAudio
//! callback is a dropout.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use micbridge_core::ring::FrameProducer;
use micbridge_core::{Gain, LevelMeter};
use micbridge_protocol::{StreamFormat, WireSampleFormat};

use crate::devices::{find_device, Direction};

#[derive(Debug, Clone, Default)]
pub struct CaptureConfig {
    /// Case-insensitive substring of the device name. `None` takes the default.
    pub device: Option<String>,
    /// Requested rate. `None` accepts the device's default, which avoids forcing
    /// a rate the hardware would have to resample internally.
    pub sample_rate: Option<u32>,
    /// Requested channel count. `None` accepts the device's default.
    pub channels: Option<u16>,
    /// Requested callback size in frames. Smaller is lower latency and more
    /// callbacks; `None` lets the host decide.
    pub buffer_frames: Option<u32>,
    /// Applied to every captured sample before it goes on the wire. Shared, so it
    /// can be turned while a session runs.
    pub gain: Arc<Gain>,
}

/// A running capture stream.
///
/// The `cpal::Stream` is not `Send` on every platform, so this must be kept on
/// the thread that built it. Dropping it stops capture.
pub struct Capture {
    pub stream: cpal::Stream,
    pub device_name: String,
    /// The format actually negotiated, which is what the control handshake
    /// advertises.
    pub format: StreamFormat,
    /// Frames the callback had to discard because the network thread was not
    /// keeping up. Non-zero means a real defect, not a transient.
    pub frames_dropped: Arc<AtomicU64>,
    /// Peak input magnitude, for a level display. Answers "is the microphone
    /// actually producing anything", which a packet counter cannot.
    pub level: Arc<LevelMeter>,
}

/// Opens a capture device and starts feeding `producer`.
///
/// `frames_per_packet` is recorded in the returned [`StreamFormat`] so the
/// receiver can size its buffers before the first datagram; it does not affect
/// the device's own callback size.
pub fn start_capture(
    config: &CaptureConfig,
    frames_per_packet: u32,
    mut producer: FrameProducer,
    level: Arc<LevelMeter>,
) -> Result<Capture> {
    let device = find_device(Direction::Input, config.device.as_deref())?;
    let device_name = device.name().unwrap_or_else(|_| "<unnamed>".into());

    let default = device
        .default_input_config()
        .with_context(|| format!("reading default input config for {device_name:?}"))?;

    if default.sample_format() != crate::SAMPLE_FORMAT {
        return Err(anyhow!(
            "{device_name:?} offers {:?} samples; this build only handles f32",
            default.sample_format()
        ));
    }

    let channels = config.channels.unwrap_or_else(|| default.channels());
    let sample_rate = config.sample_rate.unwrap_or_else(|| default.sample_rate().0);

    if channels as usize != producer.channels() {
        return Err(anyhow!(
            "capture ring was built for {} channels but the device gives {channels}",
            producer.channels()
        ));
    }

    let stream_config = cpal::StreamConfig {
        channels,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: match config.buffer_frames {
            Some(frames) => cpal::BufferSize::Fixed(frames),
            None => cpal::BufferSize::Default,
        },
    };

    let frames_dropped = Arc::new(AtomicU64::new(0));
    let dropped = Arc::clone(&frames_dropped);
    let meter = Arc::clone(&level);
    let gain = Arc::clone(&config.gain);

    let stream = device
        .build_input_stream(
            &stream_config,
            move |samples: &[f32], _info: &cpal::InputCallbackInfo| {
                let factor = gain.factor();
                // Metering after gain, not before: the meter's job is to show what
                // is actually being sent, and its clip indicator is the only warning
                // that the gain has been turned up too far. Peak scales linearly, so
                // this needs no second pass over the buffer.
                meter.record_scaled(samples, factor);

                let lost = if factor == 1.0 {
                    producer.push_frames(samples)
                } else {
                    producer.push_frames_scaled(samples, factor)
                };
                if lost > 0 {
                    // Counted, never logged: `tracing` allocates, and this is a
                    // realtime callback.
                    dropped.fetch_add(lost as u64, Ordering::Relaxed);
                }
            },
            |err| tracing::error!(%err, "capture stream error"),
            None,
        )
        .with_context(|| format!("opening capture stream on {device_name:?}"))?;

    stream.play().context("starting capture stream")?;

    Ok(Capture {
        stream,
        device_name,
        format: StreamFormat {
            sample_rate,
            channels,
            sample_format: WireSampleFormat::S16Le,
            frames_per_packet,
        },
        frames_dropped,
        level,
    })
}

/// The format a device would be opened with, without opening it.
///
/// Used by `micbridge send --probe` so a user can see what will be negotiated before
/// committing to a session.
pub fn probe(config: &CaptureConfig, frames_per_packet: u32) -> Result<(String, StreamFormat)> {
    let device = find_device(Direction::Input, config.device.as_deref())?;
    let device_name = device.name().unwrap_or_else(|_| "<unnamed>".into());
    let default = device
        .default_input_config()
        .with_context(|| format!("reading default input config for {device_name:?}"))?;
    Ok((
        device_name,
        StreamFormat {
            sample_rate: config.sample_rate.unwrap_or_else(|| default.sample_rate().0),
            channels: config.channels.unwrap_or_else(|| default.channels()),
            sample_format: WireSampleFormat::S16Le,
            frames_per_packet,
        },
    ))
}

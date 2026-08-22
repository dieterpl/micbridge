//! Output rendering.
//!
//! The render callback runs the drift controller and the resampler, because both
//! have to be driven by the device's own clock — that clock is the thing being
//! measured against. Both are pure arithmetic over pre-allocated buffers, so there
//! is nothing here that can block.
//!
//! The device is resolved **once**, into an [`OutputTarget`], and the caller is
//! expected to do that before it acknowledges a session. Two reasons:
//!
//! * WASAPI in shared mode refuses to open a stream at a channel count other than
//!   the endpoint's own, so the count has to be known before the pipeline is built —
//!   discovering it afterwards means failing a session that was already accepted.
//! * Device enumeration is not instant. Doing it after the handshake means the
//!   sender is already streaming into a jitter buffer that nobody is draining yet.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use micbridge_core::pipeline::PlaybackSource;
use micbridge_core::{Gain, LevelMeter};

use crate::devices::{find_device, Direction};

#[derive(Debug, Clone, Default)]
pub struct RenderConfig {
    /// Case-insensitive substring of the device name. On Windows this is where
    /// `"CABLE Input"` goes.
    pub device: Option<String>,
    /// Override the rate. `None` — the normal case — takes the device's own.
    pub sample_rate: Option<u32>,
    /// Override the channel count.
    ///
    /// Leave this `None`. WASAPI in shared mode only supports the endpoint's own
    /// count: cpal never sets `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, so the audio
    /// engine does no channel conversion and `IsFormatSupported` rejects anything
    /// else. CoreAudio converts silently, which is why forcing a count appears to
    /// work on a Mac and fails on every Windows endpoint whose mix format differs.
    pub channels: Option<u16>,
    /// Render callback size in frames. `None` lets the host decide.
    pub buffer_frames: Option<u32>,
}

/// A resolved output device, opened but not yet streaming.
pub struct OutputTarget {
    device: cpal::Device,
    pub name: String,
    /// The rate the stream will run at.
    pub sample_rate: u32,
    /// The channel count the stream will run at. The pipeline maps onto this.
    pub channels: u16,
    buffer_frames: Option<u32>,
}

/// Finds the device and reads the configuration a stream would use.
///
/// Call this before accepting a session: everything that can fail because of the
/// device fails here, while the peer is still waiting for an answer.
pub fn open(config: &RenderConfig) -> Result<OutputTarget> {
    let device = find_device(Direction::Output, config.device.as_deref())?;
    let name = device.name().unwrap_or_else(|_| "<unnamed>".into());

    let default = device
        .default_output_config()
        .with_context(|| format!("reading default output config for {name:?}"))?;

    if default.sample_format() != crate::SAMPLE_FORMAT {
        return Err(anyhow!(
            "{name:?} wants {:?} samples; this build only handles f32",
            default.sample_format()
        ));
    }

    Ok(OutputTarget {
        device,
        name,
        sample_rate: config.sample_rate.unwrap_or_else(|| default.sample_rate().0),
        channels: config.channels.unwrap_or_else(|| default.channels()),
        buffer_frames: config.buffer_frames,
    })
}

pub struct Render {
    pub stream: cpal::Stream,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Starts rendering from `source` into the already-resolved target.
///
/// `level` is fed from what was actually handed to the device, after resampling and
/// channel mapping. Measuring there rather than on arrival is what makes the meter
/// honest: a meter fed from the network keeps moving during an underrun, when the
/// device is in fact receiving silence.
pub fn start(
    target: OutputTarget,
    mut source: PlaybackSource,
    level: Arc<LevelMeter>,
    gain: Arc<Gain>,
) -> Result<Render> {
    if target.channels as usize != source.output_channels() {
        return Err(anyhow!(
            "pipeline renders {} channels but {:?} wants {}",
            source.output_channels(),
            target.name,
            target.channels
        ));
    }

    let stream_config = cpal::StreamConfig {
        channels: target.channels,
        sample_rate: cpal::SampleRate(target.sample_rate),
        buffer_size: match target.buffer_frames {
            Some(frames) => cpal::BufferSize::Fixed(frames),
            None => cpal::BufferSize::Default,
        },
    };

    let name = target.name.clone();
    let stream = target
        .device
        .build_output_stream(
            &stream_config,
            move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                source.fill(out);
                // Gain before the meter, so what is displayed is what the device
                // is being handed rather than what arrived over the network.
                gain.apply(out);
                level.record(out);
            },
            |err| tracing::error!(%err, "render stream error"),
            None,
        )
        .with_context(|| format!("opening render stream on {name:?}"))?;

    stream.play().context("starting render stream")?;

    Ok(Render {
        stream,
        device_name: target.name,
        sample_rate: target.sample_rate,
        channels: target.channels,
    })
}

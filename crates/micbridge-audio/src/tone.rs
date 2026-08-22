//! A synthetic capture source.
//!
//! Bringing this up across two machines has several things that can be wrong at
//! once: the capture device, the network path, the receiver's output routing, and
//! on Windows whether VB-CABLE is installed and selected. A known signal collapses
//! that: if `micbridge send --tone 1000` shows a level on the Windows recording meter,
//! everything except the microphone is working, and if it does not, the microphone
//! was never the problem.
//!
//! It also gives the receiver something to render on a machine with no audio input
//! at all, which is most CI runners.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use micbridge_core::ring::FrameProducer;
use micbridge_core::LevelMeter;
use micbridge_protocol::{StreamFormat, WireSampleFormat};

/// Amplitude of the generated tone, about -12 dBFS.
///
/// Loud enough to move a level meter unmistakably, quiet enough that it is
/// obviously not clipping — so a meter pinned at full scale means something is
/// wrong rather than "the tone is loud".
const AMPLITUDE: f32 = 0.25;

/// A running tone generator. Dropping the handle does not stop it; the caller
/// sets the shared stop flag and joins.
pub struct Tone {
    pub thread: JoinHandle<()>,
    pub format: StreamFormat,
    pub frames_dropped: Arc<AtomicU64>,
    /// Peak magnitude, so the GUI's meter behaves the same whether the source is
    /// a device or the tone.
    pub level: Arc<LevelMeter>,
}

/// Builds the format a tone source will advertise.
///
/// Defaults to 48 kHz stereo, which is what a Windows endpoint runs at natively,
/// so the receiver's nominal resample ratio is exactly 1.0 and the tone tests the
/// transport rather than the interpolator.
pub fn format(
    sample_rate: Option<u32>,
    channels: Option<u16>,
    frames_per_packet: u32,
) -> StreamFormat {
    StreamFormat {
        sample_rate: sample_rate.unwrap_or(48_000),
        channels: channels.unwrap_or(2),
        sample_format: WireSampleFormat::S16Le,
        frames_per_packet,
    }
}

/// Starts generating a sine wave into `producer` at real-time pace.
///
/// Real-time pacing is the point: a generator running flat out would fill the
/// ring, report a huge overrun, and prove nothing about a path that has to carry
/// audio at exactly the rate it is produced.
pub fn start(
    hz: f64,
    format: StreamFormat,
    mut producer: FrameProducer,
    stop: Arc<AtomicBool>,
    level: Arc<LevelMeter>,
) -> Result<Tone> {
    let frames_dropped = Arc::new(AtomicU64::new(0));
    let dropped = Arc::clone(&frames_dropped);
    let meter = Arc::clone(&level);

    let channels = format.channels as usize;
    let chunk_frames = format.frames_per_packet as usize;
    let sample_rate = format.sample_rate as f64;

    let thread = std::thread::Builder::new()
        .name("micbridge-tone".into())
        .spawn(move || {
            let mut buffer = vec![0.0f32; chunk_frames * channels];
            let chunk_duration = Duration::from_secs_f64(chunk_frames as f64 / sample_rate);
            let start = Instant::now();
            let mut chunks: u32 = 0;
            // Phase is carried as a sample counter rather than accumulated
            // radians, so it cannot drift or lose precision over a long run.
            let mut frame_index: u64 = 0;

            while !stop.load(Ordering::Relaxed) {
                for frame in buffer.chunks_mut(channels) {
                    let t = frame_index as f64 / sample_rate;
                    let value = (t * hz * std::f64::consts::TAU).sin() as f32 * AMPLITUDE;
                    frame.fill(value);
                    frame_index += 1;
                }

                meter.record(&buffer);
                let lost = producer.push_frames(&buffer);
                if lost > 0 {
                    dropped.fetch_add(lost as u64, Ordering::Relaxed);
                }

                chunks += 1;
                let target = start + chunk_duration * chunks;
                if let Some(remaining) = target.checked_duration_since(Instant::now()) {
                    std::thread::sleep(remaining);
                }
            }
        })
        .context("spawning tone thread")?;

    Ok(Tone { thread, format, frames_dropped, level })
}

#[cfg(test)]
mod tests {
    use micbridge_core::ring::frame_channel;

    use super::*;

    #[test]
    fn generates_a_paced_tone_on_both_channels() {
        let format = format(Some(48_000), Some(2), 240);
        let (producer, mut consumer) = frame_channel(2, 48_000);
        let stop = Arc::new(AtomicBool::new(false));

        let level = Arc::new(LevelMeter::new());
        let tone = start(1_000.0, format, producer, Arc::clone(&stop), Arc::clone(&level))
            .expect("starts");
        std::thread::sleep(Duration::from_millis(250));
        stop.store(true, Ordering::Relaxed);
        tone.thread.join().expect("tone thread finished");

        let available = consumer.occupied_frames();
        // A quarter second at 48 kHz is 12 000 frames. Pacing is not exact, so
        // this checks the order of magnitude rather than the count: running flat
        // out would have produced hundreds of thousands.
        assert!(
            (4_000..20_000).contains(&available),
            "expected roughly a quarter second of frames, got {available}"
        );
        assert_eq!(tone.frames_dropped.load(Ordering::Relaxed), 0, "ring was large enough");

        let mut out = vec![0.0f32; available * 2];
        consumer.pop_frames(&mut out);

        let peak = out.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!((peak - AMPLITUDE).abs() < 0.02, "peak should be near {AMPLITUDE}, was {peak}");

        // The meter must agree with the samples, since it is what the GUI draws.
        let metered = level.peek();
        assert!(
            (metered - AMPLITUDE).abs() < 0.02,
            "meter read {metered}, samples peaked at {peak}"
        );

        // Both channels carry the same signal, so a receiver rendering only one
        // still hears it.
        for frame in out.chunks(2) {
            assert_eq!(frame[0], frame[1]);
        }

        // And it is a sine, not a constant.
        let crossings = out.chunks(2).map(|f| f[0]).collect::<Vec<_>>();
        let sign_changes = crossings.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert!(sign_changes > 100, "expected many zero crossings, saw {sign_changes}");
    }

    #[test]
    fn default_format_is_48k_stereo() {
        // 48 kHz keeps the receiver's nominal ratio at exactly 1.0 against a
        // Windows endpoint, so a tone test exercises the transport and not the
        // resampler.
        let f = format(None, None, 240);
        assert_eq!(f.sample_rate, 48_000);
        assert_eq!(f.channels, 2);
        assert_eq!(f.packet_ms(), 5.0);
    }
}

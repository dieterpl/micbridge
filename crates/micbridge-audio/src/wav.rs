//! A file sink standing in for an output device.
//!
//! This exists so the whole receive path can be exercised with no audio
//! hardware, no VB-CABLE, and no second machine — which means it runs in CI, on
//! either platform, and is the first thing to reach for when something sounds
//! wrong and the question is whether the network or the device is at fault.
//!
//! It paces itself in real time rather than running flat out. A sink that drained
//! the pipeline as fast as it could would never see an underrun and would never
//! exercise the drift controller, so it would prove nothing about the parts most
//! likely to be broken.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use micbridge_core::pcm::f32_to_i16;
use micbridge_core::pipeline::PlaybackSource;
use micbridge_core::{Gain, LevelMeter};

/// A 16-bit PCM WAV writer.
///
/// 16-bit because it matches the wire format exactly, so a captured file shows
/// what was actually transmitted rather than what a float conversion made of it.
pub struct WavSink {
    writer: hound::WavWriter<BufWriter<File>>,
    channels: usize,
    frames_written: u64,
}

impl WavSink {
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> Result<Self> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { writer, channels: channels as usize, frames_written: 0 })
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &sample in samples {
            self.writer.write_sample(f32_to_i16(sample)).context("writing sample")?;
        }
        self.frames_written += (samples.len() / self.channels) as u64;
        Ok(())
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// Writes the WAV header lengths. Skipping this leaves a file most players
    /// treat as empty, so it is not optional.
    pub fn finalize(self) -> Result<()> {
        self.writer.finalize().context("finalizing wav file")
    }
}

/// `chunk_frames` plays the part of the device's callback size, so it also sets how
/// often the drift controller updates.
/// Drains `source` into an already-open sink at real-time pace until `stop` is set.
///
/// The sink is created by the caller, not here, so a locked or unwritable file fails
/// while the session is still being negotiated rather than on a background thread
/// whose error nobody sees until the session ends.
pub fn run(
    mut sink: WavSink,
    mut source: PlaybackSource,
    chunk_frames: usize,
    sample_rate: u32,
    stop: Arc<AtomicBool>,
    level: Arc<LevelMeter>,
    gain: Arc<Gain>,
) -> Result<u64> {
    let channels = source.output_channels();
    let mut buffer = vec![0.0f32; chunk_frames * channels];

    let chunk_duration = Duration::from_secs_f64(chunk_frames as f64 / sample_rate as f64);
    let start = Instant::now();
    let mut chunks: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        source.fill(&mut buffer);
        gain.apply(&mut buffer);
        level.record(&buffer);
        sink.write(&buffer)?;
        chunks += 1;

        // Pace against elapsed time from a fixed origin rather than sleeping a
        // fixed interval each pass, so the writing time does not accumulate into
        // a growing rate error that the drift controller would then chase.
        let target = start + chunk_duration * chunks;
        if let Some(remaining) = target.checked_duration_since(Instant::now()) {
            std::thread::sleep(remaining);
        }
    }

    let frames = sink.frames_written();
    sink.finalize()?;
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use micbridge_core::pcm::f32_to_i16;
    use micbridge_core::pipeline::{build, PipelineConfig};
    use micbridge_protocol::{StreamFormat, WireSampleFormat};

    use super::*;

    const RATE: u32 = 48_000;
    const CH: u16 = 2;
    const FPP: u32 = 240;

    fn format() -> StreamFormat {
        StreamFormat {
            sample_rate: RATE,
            channels: CH,
            sample_format: WireSampleFormat::S16Le,
            frames_per_packet: FPP,
        }
    }

    /// One packet of a 1 kHz sine at half scale.
    fn tone_packet(first_frame: u64) -> Vec<i16> {
        (0..FPP as u64)
            .flat_map(|f| {
                let t = (first_frame + f) as f64 / RATE as f64;
                let v = (t * 1_000.0 * std::f64::consts::TAU).sin() * 0.5;
                let s = f32_to_i16(v as f32);
                [s, s]
            })
            .collect()
    }

    #[test]
    fn wav_sink_writes_a_readable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.wav");

        let mut sink = WavSink::create(&path, RATE, CH).expect("create");
        sink.write(&[0.5, -0.5, 0.25, -0.25]).expect("write");
        assert_eq!(sink.frames_written(), 2);
        sink.finalize().expect("finalize");

        let reader = hound::WavReader::open(&path).expect("reopen");
        assert_eq!(reader.spec().channels, CH);
        assert_eq!(reader.spec().sample_rate, RATE);
        let samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.expect("sample")).collect();
        assert_eq!(samples, vec![16_384, -16_384, 8_192, -8_192]);
    }

    /// Datagrams in one end, audio out of the other, with no network and no
    /// hardware. Returns the meter's peak and the samples written, so a caller can
    /// assert about either.
    fn pipeline_through_a_file(gain_db: f32) -> (f32, Vec<i16>, u64) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tone.wav");

        let (mut media, source, stats) = build(PipelineConfig::new(format(), RATE, 20));
        let stop = Arc::new(AtomicBool::new(false));
        let fed = Arc::new(AtomicU64::new(0));

        // Feed from another thread, as the real network path does.
        let feeder = {
            let stop = Arc::clone(&stop);
            let fed = Arc::clone(&fed);
            std::thread::spawn(move || {
                let mut next = 0u64;
                let packet_duration = Duration::from_secs_f64(FPP as f64 / RATE as f64);
                let start = Instant::now();
                let mut sent: u32 = 0;
                while !stop.load(Ordering::Relaxed) {
                    media.accept(next, &tone_packet(next));
                    next += FPP as u64;
                    sent += 1;
                    fed.store(next, Ordering::Relaxed);
                    let target = start + packet_duration * sent;
                    if let Some(rest) = target.checked_duration_since(Instant::now()) {
                        std::thread::sleep(rest);
                    }
                }
            })
        };

        let stopper = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(600));
                stop.store(true, Ordering::Relaxed);
            })
        };

        let level = Arc::new(LevelMeter::new());
        let file = WavSink::create(&path, RATE, CH).expect("create");
        let frames = run(
            file,
            source,
            240,
            RATE,
            Arc::clone(&stop),
            Arc::clone(&level),
            Arc::new(Gain::new(gain_db)),
        )
        .expect("sink ran");
        feeder.join().expect("feeder finished");
        stopper.join().expect("stopper finished");

        // A property of the harness rather than of any one test: the feeder runs in
        // real time, so a ring sized for 400 ms must never overflow.
        assert_eq!(stats.overrun_frames(), 0, "real-time feed should not overrun a 400 ms ring");

        let reader = hound::WavReader::open(&path).expect("reopen");
        let samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.expect("sample")).collect();
        (level.peek(), samples, frames)
    }

    #[test]
    fn end_to_end_pipeline_into_a_file_preserves_the_tone() {
        // The M4 acceptance test, minus the network and the hardware.
        let (peak, samples, frames) = pipeline_through_a_file(0.0);

        // The meter is what the GUI shows, so it is worth asserting rather than
        // assuming: a packet counter climbs happily while the input is muted.
        assert!(
            (peak - 0.5).abs() < 0.05,
            "meter should have seen the half-scale tone, saw {peak}"
        );

        assert!(
            frames > RATE as u64 / 10,
            "expected at least 100 ms of audio, got {frames} frames"
        );

        // Skip the pre-buffering silence at the head, then require real signal.
        let tail = &samples[samples.len() / 2..];
        let peak = tail.iter().map(|s| s.abs()).max().expect("non-empty");
        assert!(peak > 8_000, "tone should survive at close to half scale, peak was {peak}");

        // And it should be a tone, not a stuck value.
        let distinct = tail.iter().collect::<std::collections::HashSet<_>>().len();
        assert!(distinct > 20, "output looks constant, only {distinct} distinct values");
    }

    /// Gain has to be proved where it actually runs, not only as a multiply in a
    /// unit test: the point is that it reaches the file, and that the meter agrees
    /// with what was written rather than with what arrived.
    #[test]
    fn gain_amplifies_what_reaches_the_file() {
        let (unity_peak, unity, _) = pipeline_through_a_file(0.0);
        let (boosted_peak, boosted, _) = pipeline_through_a_file(6.0);

        let peak_of = |s: &[i16]| s[s.len() / 2..].iter().map(|v| v.abs()).max().expect("samples");
        let (quiet, loud) = (peak_of(&unity), peak_of(&boosted));

        // +6 dB is a factor of two, and the source is at half scale, so the boosted
        // tone should land near full scale without being clipped flat.
        let ratio = loud as f32 / quiet as f32;
        assert!(
            (ratio - 2.0).abs() < 0.15,
            "+6 dB should roughly double the output, got {quiet} -> {loud} (x{ratio:.2})"
        );

        assert!(
            boosted_peak > unity_peak,
            "the meter must reflect the gain, saw {unity_peak} then {boosted_peak}"
        );

        // Still a waveform, not a square: clipping the whole tone flat would also
        // raise the peak, and would be the wrong outcome at this level.
        let tail = &boosted[boosted.len() / 2..];
        let distinct = tail.iter().collect::<std::collections::HashSet<_>>().len();
        assert!(distinct > 20, "boosted output looks clipped flat, {distinct} distinct values");
    }
}

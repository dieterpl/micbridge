//! The receive pipeline, split across the two threads that drive it.
//!
//! ```text
//!  network thread                   ring                audio callback
//!  ──────────────                   ────                ──────────────
//!  MediaSink                                            PlaybackSource
//!    Sequencer  ──in-order frames──> FrameProducer ──>  FrameConsumer
//!                                                        DriftController
//!                                                        VariableResampler
//! ```
//!
//! The split is deliberate. Everything that can allocate, hold a `BTreeMap`, or
//! take an unbounded amount of time lives on the network side; the audio
//! callback does arithmetic on pre-allocated buffers and nothing else.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use micbridge_protocol::{ReceiverStats, StreamFormat};

use crate::channels::{ChannelMap, Mapping};
use crate::drift::{DriftController, DriftGains};
use crate::resample::VariableResampler;
use crate::ring::{frame_channel, FrameConsumer, FrameProducer};
use crate::sequencer::Sequencer;

/// How the receive pipeline is sized and tuned.
#[derive(Debug, Clone, Copy)]
pub struct PipelineConfig {
    /// The negotiated stream format, which fixes the sender's rate, the channel
    /// count, and the datagram size.
    pub format: StreamFormat,
    /// Rate of the local output device. Equal to `format.sample_rate` in the
    /// normal case; when it differs, the resampler's nominal ratio absorbs the
    /// conversion.
    pub output_rate: u32,
    /// Channel count of the local output device.
    ///
    /// Authoritative, like `output_rate`: WASAPI in shared mode refuses to open a
    /// stream at any other count, so a mismatch with the stream's channels is
    /// resolved by a [`ChannelMap`] rather than pushed onto the driver. Zero means
    /// "same as the stream".
    pub output_channels: u16,
    /// Jitter-buffer target. The dominant term in end-to-end latency, and the
    /// budget for network jitter — 20 ms is comfortable on wired Ethernet, and
    /// 10 ms is reachable.
    pub target_buffer_ms: u32,
    /// Datagrams held while waiting for a missing one before giving up and
    /// substituting silence.
    ///
    /// Bounded by the buffer target, not chosen freely — see
    /// [`PipelineConfig::default_reorder_window`]. Waiting costs exactly the
    /// latency it buys.
    pub reorder_window_packets: usize,
    /// Ring capacity. Only has to exceed the target by enough to absorb a burst;
    /// generous here costs memory, not latency, because the drift controller
    /// holds the *fill* at the target regardless of capacity.
    pub ring_capacity_ms: u32,
    pub gains: DriftGains,
}

impl PipelineConfig {
    pub fn new(format: StreamFormat, output_rate: u32, target_buffer_ms: u32) -> Self {
        Self {
            format,
            output_rate,
            // Defaults to the stream's own count, so an existing caller that does
            // not care gets the previous behaviour.
            output_channels: format.channels,
            target_buffer_ms,
            reorder_window_packets: Self::default_reorder_window(format, target_buffer_ms),
            ring_capacity_ms: 400,
            gains: DriftGains::default(),
        }
    }

    /// Effective output channel count, resolving zero to the stream's own.
    pub fn resolved_output_channels(&self) -> usize {
        if self.output_channels == 0 {
            self.format.channels as usize
        } else {
            self.output_channels as usize
        }
    }

    /// The largest reorder window the buffer target can actually afford.
    ///
    /// Holding datagrams back to wait for a missing one stalls the stream for as
    /// long as the wait lasts. A window of `w` packets means up to `w + 1` packet
    /// intervals with nothing entering the ring, drawn entirely from the jitter
    /// buffer — so a window sized at or above the target guarantees the underrun
    /// it was meant to prevent. That is not a hypothetical: a 20 ms target with a
    /// 4-packet window at 5 ms per packet underran on every single lost packet.
    ///
    /// Half the target is the budget, leaving the other half as margin for the
    /// network jitter the buffer is mainly there for. Below about three packets
    /// of target there is no room for any tolerance at all, and the window falls
    /// to one — a straight swap of two adjacent datagrams is still recovered,
    /// and anything deeper becomes silence. Wanting more reorder tolerance means
    /// raising `target_buffer_ms`, and paying for it in latency.
    pub fn default_reorder_window(format: StreamFormat, target_buffer_ms: u32) -> usize {
        let target_frames = target_buffer_ms as u64 * format.sample_rate as u64 / 1000;
        let per_packet = format.frames_per_packet.max(1) as u64;
        ((target_frames / (2 * per_packet)).saturating_sub(1)).max(1) as usize
    }

    fn target_frames(&self) -> f64 {
        (self.target_buffer_ms as f64 * self.output_rate as f64 / 1000.0).max(1.0)
    }

    fn ring_capacity_frames(&self) -> usize {
        let frames = self.ring_capacity_ms as usize * self.format.sample_rate as usize / 1000;
        // Never smaller than the target plus a few datagrams, whatever the
        // caller asked for — a ring that cannot hold the target would make the
        // drift controller chase a level it can never reach.
        frames.max(self.target_frames() as usize + 4 * self.format.frames_per_packet as usize)
    }
}

/// Counters shared by both halves and read by the control thread.
///
/// Held behind atomics rather than a lock because the audio callback is one of
/// the writers. Each counter is independent; a snapshot is not a consistent
/// instant, which is fine for observability and would not be worth a lock.
#[derive(Debug, Default)]
pub struct PipelineStats {
    packets_received: AtomicU64,
    frames_lost: AtomicU64,
    packets_late: AtomicU64,
    packets_duplicate: AtomicU64,
    packets_reordered: AtomicU64,
    underruns: AtomicU64,
    overrun_frames: AtomicU64,
    /// `f64` bits. Read by the control thread only for reporting.
    fill_frames: AtomicU64,
    ratio_bits: AtomicU64,
}

impl PipelineStats {
    pub fn snapshot(&self, output_rate: u32) -> ReceiverStats {
        let fill_frames = f64::from_bits(self.fill_frames.load(Ordering::Relaxed));
        ReceiverStats {
            packets_received: self.packets_received.load(Ordering::Relaxed),
            frames_lost: self.frames_lost.load(Ordering::Relaxed),
            packets_late: self.packets_late.load(Ordering::Relaxed),
            packets_reordered: self.packets_reordered.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overrun_frames.load(Ordering::Relaxed),
            buffer_fill_ms: (fill_frames * 1000.0 / output_rate.max(1) as f64) as f32,
            resample_ratio: f64::from_bits(self.ratio_bits.load(Ordering::Relaxed)),
        }
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn overrun_frames(&self) -> u64 {
        self.overrun_frames.load(Ordering::Relaxed)
    }

    pub fn frames_lost(&self) -> u64 {
        self.frames_lost.load(Ordering::Relaxed)
    }

    pub fn packets_duplicate(&self) -> u64 {
        self.packets_duplicate.load(Ordering::Relaxed)
    }
}

/// Network-thread half: datagrams in, in-order frames into the ring.
pub struct MediaSink {
    sequencer: Sequencer,
    producer: FrameProducer,
    stats: Arc<PipelineStats>,
    /// Reused across datagrams so the network thread settles into a steady state
    /// with no allocation.
    staging: Vec<f32>,
    channels: usize,
}

impl MediaSink {
    /// Places one datagram's samples and forwards whatever became contiguous.
    ///
    /// `samples` is interleaved `i16` as decoded from the payload, and
    /// `sample_idx` is the header's frame index.
    pub fn accept(&mut self, sample_idx: u64, samples: &[i16]) {
        if samples.is_empty() || !samples.len().is_multiple_of(self.channels) {
            // A payload that is not a whole number of frames cannot be placed
            // without shifting the interleave for the rest of the session, so it
            // is discarded whole.
            return;
        }

        self.staging.clear();
        let before = self.sequencer.stats();
        self.sequencer.push(sample_idx, samples, &mut self.staging);
        let after = self.sequencer.stats();

        let stats = &self.stats;
        stats
            .packets_received
            .fetch_add(after.packets_accepted - before.packets_accepted, Ordering::Relaxed);
        stats.packets_late.fetch_add(after.packets_late - before.packets_late, Ordering::Relaxed);
        stats
            .packets_duplicate
            .fetch_add(after.packets_duplicate - before.packets_duplicate, Ordering::Relaxed);
        stats
            .packets_reordered
            .fetch_add(after.packets_buffered - before.packets_buffered, Ordering::Relaxed);
        stats.frames_lost.fetch_add(after.frames_lost - before.frames_lost, Ordering::Relaxed);

        if !self.staging.is_empty() {
            let dropped = self.producer.push_frames(&self.staging);
            if dropped > 0 {
                // The renderer is not draining. Either it has not started or the
                // output device has stalled; either way the frames are gone and
                // the count is the only honest record of it.
                stats.overrun_frames.fetch_add(dropped as u64, Ordering::Relaxed);
            }
        }
    }

    /// Discards buffered state for a new session.
    pub fn reset(&mut self) {
        self.sequencer.reset();
        self.staging.clear();
    }

    pub fn stats(&self) -> &Arc<PipelineStats> {
        &self.stats
    }
}

/// Audio-callback half: ring out, drift-corrected frames into the device buffer.
pub struct PlaybackSource {
    consumer: FrameConsumer,
    resampler: VariableResampler,
    drift: DriftController,
    stats: Arc<PipelineStats>,
    /// The stream's channel count, which is what the ring and the resampler work in.
    channels: usize,
    /// The output device's channel count, which is what `fill` must write.
    out_channels: usize,
    map: ChannelMap,
    /// Staging buffer for the mapped path, pre-allocated because `fill` runs in the
    /// audio callback and must not allocate. Unused when the counts match.
    scratch: Vec<f32>,
    output_rate: u32,
    /// Playback waits for the buffer to reach its target before starting.
    ///
    /// Without this the first seconds are a fight: the controller sees an empty
    /// buffer, commands its maximum slow-down, and takes several seconds to
    /// recover. Pre-buffering starts the loop already at its setpoint.
    started: bool,
    prebuffer_frames: usize,
}

/// Frames of staging the mapped render path pre-allocates.
///
/// A callback larger than this is rendered in several passes rather than growing
/// the buffer, because growing it would allocate on the realtime thread.
const SCRATCH_FRAMES: usize = 4096;

/// What one `fill` call did, for logging and for the tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FillReport {
    /// Frames the resampler produced from real input.
    pub frames_rendered: usize,
    /// Frames that had to be filled with silence.
    pub frames_silent: usize,
    /// True while pre-buffering. Silence during this phase is expected and is
    /// not counted as an underrun.
    pub prebuffering: bool,
    pub ratio: f64,
    pub fill_frames: f64,
}

impl PlaybackSource {
    /// Renders into an interleaved output buffer, which is fully written on
    /// every call — with silence where input was unavailable.
    ///
    /// Called from the realtime audio callback: no allocation, no locks, no
    /// syscalls.
    pub fn fill(&mut self, out: &mut [f32]) -> FillReport {
        // Sized by the *device's* channel count, since that is the buffer's shape.
        let want_frames = out.len() / self.out_channels;
        let available = self.consumer.occupied_frames();

        if !self.started {
            if available < self.prebuffer_frames {
                out.fill(0.0);
                return FillReport {
                    frames_rendered: 0,
                    frames_silent: want_frames,
                    prebuffering: true,
                    ratio: self.drift.ratio(),
                    fill_frames: available as f64,
                };
            }
            self.started = true;
        }

        // Buffered audio ahead of the renderer includes what the resampler is
        // holding in its kernel, which is a constant the controller would
        // otherwise have to absorb as a steady-state error.
        let fill_frames = (available + VariableResampler::latency_frames()) as f64;
        let dt = want_frames as f64 / self.output_rate as f64;
        let ratio = self.drift.update(fill_frames, dt);

        let produced = if self.map.is_passthrough() {
            // Split the borrow so the pull closure and the resampler can both be
            // held mutably.
            let Self { consumer, resampler, .. } = self;
            resampler.process(out, ratio, |frame| consumer.pop_frame(frame))
        } else {
            self.render_mapped(out, ratio, want_frames)
        };

        let silent = want_frames - produced;
        if silent > 0 {
            out[produced * self.out_channels..].fill(0.0);
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
        }

        self.stats.fill_frames.store(fill_frames.to_bits(), Ordering::Relaxed);
        self.stats.ratio_bits.store(ratio.to_bits(), Ordering::Relaxed);

        FillReport {
            frames_rendered: produced,
            frames_silent: silent,
            prebuffering: false,
            ratio,
            fill_frames,
        }
    }

    /// Renders through the channel map, in passes bounded by the staging buffer.
    ///
    /// Passes rather than one call so a callback larger than [`SCRATCH_FRAMES`] does
    /// not need a bigger buffer, which on this thread would mean allocating.
    fn render_mapped(&mut self, out: &mut [f32], ratio: f64, want_frames: usize) -> usize {
        let Self { consumer, resampler, scratch, channels, out_channels, map, .. } = self;
        let mut done = 0;

        while done < want_frames {
            let pass = (want_frames - done).min(SCRATCH_FRAMES);
            let staged = &mut scratch[..pass * *channels];
            let produced = resampler.process(staged, ratio, |frame| consumer.pop_frame(frame));
            if produced == 0 {
                break;
            }

            map.apply(
                &staged[..produced * *channels],
                &mut out[done * *out_channels..(done + produced) * *out_channels],
            );
            done += produced;

            // A short pass means the ring ran dry; the caller fills the rest with
            // silence and counts an underrun.
            if produced < pass {
                break;
            }
        }

        done
    }

    /// Returns to pre-buffering and forgets the drift estimate.
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.drift.reset();
        self.started = false;
        let mut frame = vec![0.0; self.channels];
        while self.consumer.pop_frame(&mut frame) {}
    }

    /// The output device's channel count — what [`PlaybackSource::fill`] expects its
    /// buffer to be laid out in.
    ///
    /// Named `output_channels` rather than `channels` because there are two counts
    /// here and confusing them silently shifts the interleave.
    pub fn output_channels(&self) -> usize {
        self.out_channels
    }

    /// The stream's channel count, before mapping.
    pub fn stream_channels(&self) -> usize {
        self.channels
    }

    /// What the channel map will do, for logging.
    pub fn mapping(&self) -> Mapping {
        self.map.mapping()
    }

    pub fn stats(&self) -> &Arc<PipelineStats> {
        &self.stats
    }
}

/// Builds both halves of the receive pipeline plus the stats they share.
pub fn build(config: PipelineConfig) -> (MediaSink, PlaybackSource, Arc<PipelineStats>) {
    let channels = config.format.channels as usize;
    let target_frames = config.target_frames();
    let stats = Arc::new(PipelineStats::default());

    let (producer, consumer) = frame_channel(channels, config.ring_capacity_frames());

    let sink = MediaSink {
        sequencer: Sequencer::new(
            channels,
            config.format.frames_per_packet as usize,
            config.reorder_window_packets,
        ),
        producer,
        stats: Arc::clone(&stats),
        staging: Vec::with_capacity(channels * config.format.frames_per_packet as usize * 8),
        channels,
    };

    let drift = DriftController::with_gains(
        config.format.sample_rate,
        config.output_rate,
        target_frames,
        config.gains,
    );

    let out_channels = config.resolved_output_channels();
    let map = ChannelMap::new(channels, out_channels);

    let source = PlaybackSource {
        consumer,
        resampler: VariableResampler::new(channels),
        drift,
        stats: Arc::clone(&stats),
        channels,
        out_channels,
        map,
        // Only the mapped path needs staging, so the matched case — which is almost
        // every case — allocates nothing.
        scratch: if map.is_passthrough() {
            Vec::new()
        } else {
            vec![0.0; SCRATCH_FRAMES * channels]
        },
        output_rate: config.output_rate,
        started: false,
        prebuffer_frames: target_frames as usize,
    };

    (sink, source, stats)
}

#[cfg(test)]
mod tests {
    use micbridge_protocol::WireSampleFormat;

    use super::*;

    const CH: usize = 2;
    const FPP: usize = 240;
    const RATE: u32 = 48_000;

    fn format() -> StreamFormat {
        StreamFormat {
            sample_rate: RATE,
            channels: CH as u16,
            sample_format: WireSampleFormat::S16Le,
            frames_per_packet: FPP as u32,
        }
    }

    fn pipeline(target_ms: u32) -> (MediaSink, PlaybackSource, Arc<PipelineStats>) {
        build(PipelineConfig::new(format(), RATE, target_ms))
    }

    /// A packet of a 1 kHz-ish sine, so output can be checked for being audio
    /// rather than only for being non-zero.
    fn tone_packet(first_frame: u64) -> Vec<i16> {
        (0..FPP)
            .flat_map(|f| {
                let t = (first_frame as usize + f) as f64 / RATE as f64;
                let v = (t * 1_000.0 * std::f64::consts::TAU).sin() * 0.5;
                let s = crate::pcm::f32_to_i16(v as f32);
                [s, s]
            })
            .collect()
    }

    #[test]
    fn renders_silence_until_the_prebuffer_target_is_reached() {
        let (mut sink, mut source, _) = pipeline(20); // 960 frames
        let mut out = vec![9.0; 128 * CH];

        // One 240-frame packet is well short of the 960-frame target.
        sink.accept(0, &tone_packet(0));
        let report = source.fill(&mut out);
        assert!(report.prebuffering);
        assert_eq!(report.frames_rendered, 0);
        assert!(out.iter().all(|&s| s == 0.0), "prebuffering must write silence, not stale data");
    }

    #[test]
    fn prebuffering_is_not_counted_as_an_underrun() {
        let (_sink, mut source, stats) = pipeline(20);
        let mut out = vec![0.0; 128 * CH];
        for _ in 0..10 {
            source.fill(&mut out);
        }
        assert_eq!(stats.underruns(), 0, "startup silence is expected, not a fault");
    }

    #[test]
    fn renders_audio_once_primed() {
        let (mut sink, mut source, stats) = pipeline(10); // 480 frames
        for i in 0..4u64 {
            sink.accept(i * FPP as u64, &tone_packet(i * FPP as u64));
        }
        let mut out = vec![0.0; 256 * CH];
        let report = source.fill(&mut out);
        assert!(!report.prebuffering);
        assert_eq!(report.frames_rendered, 256);
        assert_eq!(report.frames_silent, 0);
        assert_eq!(stats.underruns(), 0);
        assert!(out.iter().any(|&s| s.abs() > 0.1), "should be carrying the tone");
    }

    #[test]
    fn a_starved_callback_is_fully_written_and_counted() {
        let (mut sink, mut source, stats) = pipeline(5); // 240 frames
        for i in 0..2u64 {
            sink.accept(i * FPP as u64, &tone_packet(i * FPP as u64));
        }
        // Ask for far more than has arrived.
        let mut out = vec![9.0; 4_000 * CH];
        let report = source.fill(&mut out);
        assert!(report.frames_silent > 0, "should have starved");
        assert_eq!(stats.underruns(), 1);
        // Every sample must be written; leaving the tail untouched would replay
        // whatever the device buffer held last.
        assert!(out.iter().all(|&s| s != 9.0), "tail was left unwritten");
        assert!(
            out[report.frames_rendered * CH..].iter().all(|&s| s == 0.0),
            "the starved tail must be silence"
        );
    }

    #[test]
    fn steady_state_holds_the_buffer_near_target_over_a_simulated_minute() {
        // The condensed form of the soak test: a matched sender and receiver
        // should produce no underruns and no overruns at all.
        let (mut sink, mut source, stats) = pipeline(20);
        let callback = 240; // 5 ms
        let mut out = vec![0.0; callback * CH];
        let mut next_frame = 0u64;

        // 60 s at 48 kHz is 12 000 callbacks of 240 frames.
        for step in 0..12_000 {
            // One packet per callback keeps the rates matched exactly.
            sink.accept(next_frame, &tone_packet(next_frame));
            next_frame += FPP as u64;
            let report = source.fill(&mut out);
            if step > 100 {
                assert_eq!(report.frames_silent, 0, "underran at step {step}");
            }
        }

        assert_eq!(stats.underruns(), 0);
        assert_eq!(stats.overrun_frames(), 0);
        let final_stats = stats.snapshot(RATE);
        assert!(
            (final_stats.buffer_fill_ms - 20.0).abs() < 5.0,
            "fill drifted to {} ms",
            final_stats.buffer_fill_ms
        );
    }

    #[test]
    fn a_sender_running_fast_is_absorbed_without_overrunning() {
        // A *sustained* rate offset, which is the thing drift correction exists
        // for. Getting one out of integer buffer sizes means putting the mismatch
        // on the render side: 240 frames arrive per callback and 239 are
        // consumed, so the sender is permanently 240/239 fast — about 4184 ppm.
        //
        // That is far larger than any real crystal pair, which sits nearer
        // 50 ppm; it is the smallest sustained offset expressible with whole
        // packets and whole callbacks at this scale. Realistic magnitudes are
        // covered in `drift.rs`, where the simulation is continuous. What matters
        // here is that the assembled pipeline holds, and that 4184 ppm still fits
        // inside the controller's clamp with room over.
        //
        // The earlier version of this test fed 1.0003 packets per callback, which
        // is not a rate offset at all — it is three extra packets spaced twenty
        // seconds apart. The controller correctly absorbed each step and returned
        // to a ratio of 1.0, so the test proved nothing.
        const RENDER_FRAMES: usize = 239;
        let expected_trim = 240.0 / RENDER_FRAMES as f64;

        let (mut sink, mut source, stats) = pipeline(20);
        let mut out = vec![0.0; RENDER_FRAMES * CH];
        let mut next_frame = 0u64;

        for _ in 0..20_000 {
            sink.accept(next_frame, &tone_packet(next_frame));
            next_frame += FPP as u64;
            source.fill(&mut out);
        }

        assert_eq!(stats.overrun_frames(), 0, "drift correction should have drained the surplus");
        assert_eq!(stats.underruns(), 0);

        let snapshot = stats.snapshot(RATE);
        assert!(
            (snapshot.resample_ratio - expected_trim).abs() < 200e-6,
            "should have converged on the sender's true rate: ratio {} ({:+.0} ppm), want {:+.0} ppm, fill {:.1} ms",
            snapshot.resample_ratio,
            (snapshot.resample_ratio - 1.0) * 1e6,
            (expected_trim - 1.0) * 1e6,
            snapshot.buffer_fill_ms
        );
        assert!(
            (snapshot.buffer_fill_ms - 20.0).abs() < 5.0,
            "buffer should still be at target, is {:.1} ms",
            snapshot.buffer_fill_ms
        );
    }

    #[test]
    fn the_reorder_window_always_fits_inside_the_buffer_target() {
        // The invariant the loss test exposed: a window of w packets stalls the
        // stream for up to w + 1 packet intervals, and that stall is paid for out
        // of the jitter buffer.
        for target_ms in [5, 10, 20, 40, 100, 250] {
            let window = PipelineConfig::default_reorder_window(format(), target_ms);
            let stall_ms = (window + 1) as f64 * format().packet_ms();
            assert!(window >= 1, "{target_ms} ms: window must allow a simple swap");
            assert!(
                stall_ms <= target_ms as f64 || window == 1,
                "{target_ms} ms target: a {window}-packet window stalls {stall_ms} ms"
            );
        }
    }

    #[test]
    fn a_bigger_buffer_buys_more_reorder_tolerance() {
        let small = PipelineConfig::default_reorder_window(format(), 20);
        let large = PipelineConfig::default_reorder_window(format(), 200);
        assert!(large > small, "{large} should exceed {small}");
    }

    #[test]
    fn lost_packets_are_reported_but_playback_continues() {
        let (mut sink, mut source, stats) = pipeline(20);
        let mut out = vec![0.0; 240 * CH];
        let mut next_frame = 0u64;

        for step in 0..2_000 {
            // Drop every fiftieth packet.
            if step % 50 != 49 {
                sink.accept(next_frame, &tone_packet(next_frame));
            }
            next_frame += FPP as u64;
            source.fill(&mut out);
        }

        assert!(stats.frames_lost() > 0, "the drops should be visible");
        assert_eq!(stats.underruns(), 0, "loss becomes silence, not an underrun");
    }

    #[test]
    fn reset_returns_to_prebuffering() {
        let (mut sink, mut source, _) = pipeline(5);
        for i in 0..4u64 {
            sink.accept(i * FPP as u64, &tone_packet(i * FPP as u64));
        }
        let mut out = vec![0.0; 128 * CH];
        assert!(!source.fill(&mut out).prebuffering);

        source.reset();
        sink.reset();
        let report = source.fill(&mut out);
        assert!(report.prebuffering, "a new session starts by refilling the buffer");
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn ring_capacity_is_never_smaller_than_the_target() {
        // A caller asking for a 5 ms ring with a 100 ms target would otherwise
        // build a pipeline that can never prime.
        let mut config = PipelineConfig::new(format(), RATE, 100);
        config.ring_capacity_ms = 5;
        assert!(config.ring_capacity_frames() > config.target_frames() as usize);
    }

    /// A mono stream against a stereo device: the exact configuration that fails on
    /// WASAPI if the receiver demands the stream's channel count instead of adapting.
    fn mono_pipeline_to_stereo_device() -> (MediaSink, PlaybackSource, Arc<PipelineStats>) {
        let format = StreamFormat {
            sample_rate: RATE,
            channels: 1,
            sample_format: WireSampleFormat::S16Le,
            frames_per_packet: FPP as u32,
        };
        let mut config = PipelineConfig::new(format, RATE, 20);
        config.output_channels = 2;
        build(config)
    }

    fn mono_tone_packet(first_frame: u64) -> Vec<i16> {
        (0..FPP)
            .map(|f| {
                let t = (first_frame as usize + f) as f64 / RATE as f64;
                crate::pcm::f32_to_i16((t * 1_000.0 * std::f64::consts::TAU).sin() as f32 * 0.5)
            })
            .collect()
    }

    #[test]
    fn a_mono_stream_renders_to_a_stereo_device() {
        let (mut sink, mut source, stats) = mono_pipeline_to_stereo_device();
        assert_eq!(source.output_channels(), 2, "fill writes at the device's channel count");
        assert_eq!(source.stream_channels(), 1);
        assert_eq!(source.mapping(), Mapping::FanOutMono);

        // Prime past the 20 ms target with mono packets.
        for i in 0..8u64 {
            sink.accept(i * FPP as u64, &mono_tone_packet(i * FPP as u64));
        }

        // A stereo device buffer: 256 frames is 512 samples.
        let mut out = vec![0.0; 256 * 2];
        let report = source.fill(&mut out);

        assert!(!report.prebuffering);
        assert_eq!(report.frames_rendered, 256, "should have filled the whole callback");
        assert_eq!(stats.underruns(), 0);
        assert!(out.iter().any(|s| s.abs() > 0.1), "should be carrying the tone");

        // Mono fanned out: both channels of every frame identical.
        for frame in out.chunks(2) {
            assert_eq!(frame[0], frame[1], "mono should appear on both channels");
        }
    }

    #[test]
    fn a_mapped_pipeline_survives_a_simulated_minute_without_faults() {
        let (mut sink, mut source, stats) = mono_pipeline_to_stereo_device();
        let mut out = vec![0.0; FPP * 2];
        let mut next = 0u64;

        for step in 0..12_000 {
            sink.accept(next, &mono_tone_packet(next));
            next += FPP as u64;
            let report = source.fill(&mut out);
            if step > 100 {
                assert_eq!(report.frames_silent, 0, "underran at step {step}");
            }
        }

        assert_eq!(stats.underruns(), 0);
        assert_eq!(stats.overrun_frames(), 0);
    }

    #[test]
    fn a_callback_larger_than_the_staging_buffer_is_still_fully_written() {
        // The mapped path renders in passes bounded by SCRATCH_FRAMES, because
        // growing the buffer would allocate on the audio thread. A callback past that
        // bound must still come out whole rather than truncated.
        let (mut sink, mut source, _) = mono_pipeline_to_stereo_device();
        for i in 0..400u64 {
            sink.accept(i * FPP as u64, &mono_tone_packet(i * FPP as u64));
        }

        let frames = SCRATCH_FRAMES + 1_000;
        let mut out = vec![f32::NAN; frames * 2];
        let report = source.fill(&mut out);

        assert!(report.frames_rendered > SCRATCH_FRAMES, "should have made several passes");
        assert!(out.iter().all(|s| s.is_finite()), "the whole buffer must be written");
        for frame in out[..report.frames_rendered * 2].chunks(2) {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn a_matched_pipeline_allocates_no_staging_buffer() {
        // The overwhelmingly common case must not pay for the mapped path.
        let (_, source, _) = pipeline(20);
        assert!(source.map.is_passthrough());
        assert!(source.scratch.is_empty(), "matched channels should need no staging");
    }

    #[test]
    fn output_channels_defaults_to_the_stream_and_zero_means_the_same() {
        let config = PipelineConfig::new(format(), RATE, 20);
        assert_eq!(config.output_channels, format().channels);
        assert_eq!(config.resolved_output_channels(), CH);

        let mut zeroed = config;
        zeroed.output_channels = 0;
        assert_eq!(zeroed.resolved_output_channels(), CH, "zero means follow the stream");
    }

    #[test]
    fn malformed_payloads_are_discarded_rather_than_shifting_the_interleave() {
        let (mut sink, _source, stats) = pipeline(20);
        sink.accept(0, &[1, 2, 3]); // not a whole number of stereo frames
        sink.accept(0, &[]);
        assert_eq!(stats.snapshot(RATE).packets_received, 0);
    }
}

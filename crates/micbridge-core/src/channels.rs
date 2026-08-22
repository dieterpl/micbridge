//! Mapping a stream's channel count onto an output device's.
//!
//! This exists because WASAPI in shared mode will not open a stream at a channel
//! count other than the endpoint's own. cpal never sets
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, so the audio engine does no channel
//! conversion and `IsFormatSupported` rejects any other count outright. CoreAudio's
//! AUHAL happily converts for you, which is exactly why the problem is invisible on
//! the Mac and fatal on Windows.
//!
//! So the device's channel count is authoritative, the same way its sample rate
//! already is, and a mismatch is resolved here rather than being pushed onto the
//! driver.
//!
//! The concrete case this was written for: a mono capture source on the Mac — a
//! single-input interface, or a USB microphone as the system default — negotiating
//! one channel, against VB-CABLE's stereo endpoint.

/// How a stream's channels are laid out across an output device's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelMap {
    source: usize,
    target: usize,
}

/// What a [`ChannelMap`] will actually do, for logging.
///
/// Worth logging: a silent mono-to-stereo fan-out and a silent
/// drop-the-rear-channels both sound plausible until someone wonders why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// Counts match; samples are copied through untouched.
    Passthrough,
    /// One source channel replicated to every output channel.
    FanOutMono,
    /// All source channels averaged into one.
    DownmixToMono,
    /// Source channels copied in order, remaining output channels silenced.
    PadWithSilence,
    /// Leading output channels copied in order, extra source channels discarded.
    TruncateExtra,
}

impl Mapping {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::FanOutMono => "mono fanned out to every output channel",
            Self::DownmixToMono => "downmixed to mono",
            Self::PadWithSilence => "copied in order, extra output channels silent",
            Self::TruncateExtra => "extra source channels discarded",
        }
    }
}

impl ChannelMap {
    pub fn new(source: usize, target: usize) -> Self {
        assert!(source > 0 && target > 0, "channel counts must be non-zero");
        Self { source, target }
    }

    pub fn source(&self) -> usize {
        self.source
    }

    pub fn target(&self) -> usize {
        self.target
    }

    pub fn is_passthrough(&self) -> bool {
        self.source == self.target
    }

    pub fn mapping(&self) -> Mapping {
        if self.source == self.target {
            Mapping::Passthrough
        } else if self.source == 1 {
            Mapping::FanOutMono
        } else if self.target == 1 {
            Mapping::DownmixToMono
        } else if self.source < self.target {
            Mapping::PadWithSilence
        } else {
            Mapping::TruncateExtra
        }
    }

    /// Maps `src` frames into `dst`.
    ///
    /// Both slices must hold the same number of frames at their respective channel
    /// counts. Realtime-safe: no allocation, one pass.
    pub fn apply(&self, src: &[f32], dst: &mut [f32]) {
        debug_assert_eq!(src.len() / self.source, dst.len() / self.target, "frame counts differ");

        match self.mapping() {
            Mapping::Passthrough => dst.copy_from_slice(src),
            Mapping::FanOutMono => {
                // Mono to N: every output channel gets the same sample, so the
                // signal is audible whichever channel the far end listens to.
                for (frame_in, frame_out) in src.iter().zip(dst.chunks_exact_mut(self.target)) {
                    frame_out.fill(*frame_in);
                }
            }
            Mapping::DownmixToMono => {
                // Average rather than take the first channel, so a signal present
                // only on the right does not vanish.
                let scale = 1.0 / self.source as f32;
                for (frame_in, frame_out) in src.chunks_exact(self.source).zip(dst.iter_mut()) {
                    *frame_out = frame_in.iter().sum::<f32>() * scale;
                }
            }
            Mapping::PadWithSilence => {
                for (frame_in, frame_out) in
                    src.chunks_exact(self.source).zip(dst.chunks_exact_mut(self.target))
                {
                    frame_out[..self.source].copy_from_slice(frame_in);
                    frame_out[self.source..].fill(0.0);
                }
            }
            Mapping::TruncateExtra => {
                for (frame_in, frame_out) in
                    src.chunks_exact(self.source).zip(dst.chunks_exact_mut(self.target))
                {
                    frame_out.copy_from_slice(&frame_in[..self.target]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_counts_pass_through_untouched() {
        let map = ChannelMap::new(2, 2);
        assert!(map.is_passthrough());
        assert_eq!(map.mapping(), Mapping::Passthrough);

        let src = [0.1, -0.2, 0.3, -0.4];
        let mut dst = [0.0; 4];
        map.apply(&src, &mut dst);
        assert_eq!(dst, src);
    }

    #[test]
    fn mono_fans_out_to_every_channel() {
        // The case this module was written for: a mono Mac input against VB-CABLE's
        // stereo endpoint.
        let map = ChannelMap::new(1, 2);
        assert_eq!(map.mapping(), Mapping::FanOutMono);

        let mut dst = [0.0; 6];
        map.apply(&[0.5, -0.25, 1.0], &mut dst);
        assert_eq!(dst, [0.5, 0.5, -0.25, -0.25, 1.0, 1.0]);
    }

    #[test]
    fn mono_fans_out_to_more_than_two() {
        let map = ChannelMap::new(1, 4);
        let mut dst = [0.0; 8];
        map.apply(&[0.5, -0.5], &mut dst);
        assert_eq!(dst, [0.5, 0.5, 0.5, 0.5, -0.5, -0.5, -0.5, -0.5]);
    }

    #[test]
    fn downmix_averages_rather_than_dropping_a_channel() {
        // Taking the first channel would silence anything panned hard right.
        let map = ChannelMap::new(2, 1);
        assert_eq!(map.mapping(), Mapping::DownmixToMono);

        let mut dst = [0.0; 2];
        map.apply(&[0.0, 1.0, 0.5, 0.5], &mut dst);
        assert_eq!(dst, [0.5, 0.5]);

        // A hard-right signal survives at half amplitude rather than disappearing.
        let mut dst = [0.0; 1];
        map.apply(&[0.0, 0.8], &mut dst);
        assert!((dst[0] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn widening_pads_the_extra_channels_with_silence() {
        let map = ChannelMap::new(2, 4);
        assert_eq!(map.mapping(), Mapping::PadWithSilence);

        let mut dst = [9.0; 8];
        map.apply(&[0.1, 0.2, 0.3, 0.4], &mut dst);
        assert_eq!(dst, [0.1, 0.2, 0.0, 0.0, 0.3, 0.4, 0.0, 0.0]);
    }

    #[test]
    fn narrowing_keeps_the_leading_channels() {
        let map = ChannelMap::new(4, 2);
        assert_eq!(map.mapping(), Mapping::TruncateExtra);

        let mut dst = [0.0; 4];
        map.apply(&[0.1, 0.2, 0.7, 0.8, 0.3, 0.4, 0.9, 1.0], &mut dst);
        assert_eq!(dst, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn every_mapping_writes_the_whole_destination() {
        // A mapping that leaves part of the device buffer untouched replays whatever
        // was there last, which is audible as a buzz.
        for (source, target) in [(1, 2), (2, 1), (2, 4), (4, 2), (3, 3), (1, 8), (8, 1)] {
            let map = ChannelMap::new(source, target);
            let frames = 5;
            let src: Vec<f32> = (0..frames * source).map(|i| (i as f32 + 1.0) / 100.0).collect();
            let mut dst = vec![f32::NAN; frames * target];
            map.apply(&src, &mut dst);
            assert!(
                dst.iter().all(|s| s.is_finite()),
                "{source}->{target} left part of the buffer unwritten"
            );
        }
    }

    #[test]
    fn mappings_have_readable_descriptions() {
        assert_eq!(
            ChannelMap::new(1, 2).mapping().describe(),
            "mono fanned out to every output channel"
        );
        assert_eq!(ChannelMap::new(2, 2).mapping().describe(), "passthrough");
    }

    #[test]
    #[should_panic(expected = "channel counts must be non-zero")]
    fn zero_channels_is_rejected_loudly() {
        ChannelMap::new(0, 2);
    }
}

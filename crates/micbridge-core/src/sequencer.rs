//! Turns a stream of datagrams that may arrive out of order, twice, or not at
//! all into a single in-order run of samples.
//!
//! Placement is by `sample_idx`, never by arrival order, so a datagram's
//! position is independent of how long it spent in flight. The only thing time
//! decides is whether a datagram is still worth having.

use std::collections::BTreeMap;

/// Counters describing what the sequencer had to do to keep the stream
/// contiguous. All of these should be zero on a quiet wired LAN; a non-zero
/// `frames_lost` after a soak run is the signal that something upstream is
/// wrong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SequencerStats {
    pub packets_accepted: u64,
    /// Arrived entirely in the past — its samples had already been played.
    pub packets_late: u64,
    /// Same `sample_idx` seen twice.
    pub packets_duplicate: u64,
    /// Arrived ahead of the next expected index and had to be held back.
    /// A gap that later fills in is counted here and *not* in `frames_lost`.
    pub packets_buffered: u64,
    /// Frames the sequencer had to invent as silence because the datagram
    /// carrying them never showed up before the window expired.
    pub frames_lost: u64,
}

/// Reassembles datagrams into a contiguous sample stream.
///
/// Assumes a fixed `frames_per_packet`, which the control handshake negotiates.
/// A datagram whose `sample_idx` falls before the next expected index is treated
/// as wholly late rather than partially usable — with a fixed packet size the
/// partial-overlap case cannot arise, and pretending otherwise would add a
/// branch that never runs.
pub struct Sequencer {
    channels: usize,
    frames_per_packet: usize,
    /// How many future datagrams may be held before the sequencer gives up on
    /// the missing one and fills silence. This is the reorder tolerance, and it
    /// costs latency only when reordering actually happens.
    window_packets: usize,
    /// Index of the next frame to emit. `None` until the first datagram sets
    /// the origin — the sender may have been running long before we started
    /// listening, so the stream does not begin at zero.
    next_idx: Option<u64>,
    pending: BTreeMap<u64, Vec<f32>>,
    stats: SequencerStats,
}

impl Sequencer {
    pub fn new(channels: usize, frames_per_packet: usize, window_packets: usize) -> Self {
        assert!(channels > 0, "channels must be non-zero");
        assert!(frames_per_packet > 0, "frames_per_packet must be non-zero");
        Self {
            channels,
            frames_per_packet,
            window_packets: window_packets.max(1),
            next_idx: None,
            pending: BTreeMap::new(),
            stats: SequencerStats::default(),
        }
    }

    pub fn stats(&self) -> SequencerStats {
        self.stats
    }

    /// Frames currently held back waiting for a gap to fill.
    pub fn pending_frames(&self) -> usize {
        self.pending.len() * self.frames_per_packet
    }

    /// Accepts one datagram's payload and appends whatever became contiguous to
    /// `out` as interleaved `f32` in `[-1.0, 1.0)`.
    ///
    /// `out` is appended to, not cleared, so a caller can accumulate several
    /// datagrams before handing the result on.
    pub fn push(&mut self, sample_idx: u64, samples: &[i16], out: &mut Vec<f32>) {
        let frames = samples.len() / self.channels;
        if frames == 0 {
            return;
        }

        let next = *self.next_idx.get_or_insert(sample_idx);

        if sample_idx < next {
            self.stats.packets_late += 1;
            return;
        }
        if self.pending.contains_key(&sample_idx) {
            self.stats.packets_duplicate += 1;
            return;
        }

        self.stats.packets_accepted += 1;

        if sample_idx == next {
            // The common case: extend the stream directly and then see whether
            // this unblocked anything already waiting.
            append_converted(samples, out);
            self.next_idx = Some(next + frames as u64);
            self.drain_contiguous(out);
            return;
        }

        self.stats.packets_buffered += 1;
        self.pending.insert(sample_idx, to_converted(samples));

        if self.pending.len() > self.window_packets {
            self.give_up_on_gap(out);
        }
    }

    /// Emits silence up to the oldest pending datagram, then drains everything
    /// that is now contiguous.
    ///
    /// Called when the reorder window is full: continuing to wait would cost
    /// more than the missing audio is worth.
    fn give_up_on_gap(&mut self, out: &mut Vec<f32>) {
        let Some(&oldest) = self.pending.keys().next() else { return };
        let next = self.next_idx.expect("next_idx is set once a datagram has arrived");
        let missing = oldest.saturating_sub(next);
        if missing > 0 {
            self.stats.frames_lost += missing;
            out.resize(out.len() + missing as usize * self.channels, 0.0);
            self.next_idx = Some(oldest);
        }
        self.drain_contiguous(out);
    }

    fn drain_contiguous(&mut self, out: &mut Vec<f32>) {
        loop {
            let next = self.next_idx.expect("next_idx is set once a datagram has arrived");
            let Some(samples) = self.pending.remove(&next) else { break };
            let frames = samples.len() / self.channels;
            out.extend_from_slice(&samples);
            self.next_idx = Some(next + frames as u64);
        }
    }

    /// Discards all state so the next datagram re-establishes the origin.
    ///
    /// Used when the control channel reports a new session rather than trying
    /// to reconcile two senders' frame counters.
    pub fn reset(&mut self) {
        self.next_idx = None;
        self.pending.clear();
    }
}

fn to_converted(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| convert(s)).collect()
}

fn append_converted(samples: &[i16], out: &mut Vec<f32>) {
    out.extend(samples.iter().map(|&s| convert(s)));
}

/// Divides by 32768 rather than 32767 so the conversion is an exact power-of-two
/// scale: every `i16` maps to a distinct `f32` and back without rounding, and
/// full-scale negative does not clip.
#[inline]
fn convert(sample: i16) -> f32 {
    sample as f32 / 32_768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH: usize = 2;
    const FPP: usize = 4;

    /// Builds a packet whose samples encode their own frame index, so a test can
    /// assert on ordering by looking at the output values.
    fn packet(first_frame: u64) -> Vec<i16> {
        (0..FPP)
            .flat_map(|f| {
                let idx = (first_frame as usize + f) as i16;
                [idx * 10, idx * 10 + 1]
            })
            .collect()
    }

    fn frame_indices(out: &[f32]) -> Vec<i16> {
        out.chunks(CH).map(|frame| (frame[0] * 32_768.0).round() as i16 / 10).collect()
    }

    fn sequencer() -> Sequencer {
        Sequencer::new(CH, FPP, 4)
    }

    #[test]
    fn in_order_packets_pass_straight_through() {
        let mut seq = sequencer();
        let mut out = Vec::new();
        for i in 0..3u64 {
            seq.push(i * FPP as u64, &packet(i * FPP as u64), &mut out);
        }
        assert_eq!(frame_indices(&out), (0..12).collect::<Vec<i16>>());
        assert_eq!(seq.stats().frames_lost, 0);
        assert_eq!(seq.stats().packets_buffered, 0);
    }

    #[test]
    fn stream_origin_comes_from_the_first_packet_not_from_zero() {
        // The sender may have been capturing for hours before we connected.
        let mut seq = sequencer();
        let mut out = Vec::new();
        seq.push(1_000_000, &packet(0), &mut out);
        assert_eq!(out.len(), FPP * CH);
        assert_eq!(seq.stats().frames_lost, 0);
    }

    #[test]
    fn a_reordered_packet_is_held_and_then_released_in_order() {
        let mut seq = sequencer();
        let mut out = Vec::new();

        seq.push(0, &packet(0), &mut out);
        // Packet 2 overtakes packet 1.
        seq.push(8, &packet(8), &mut out);
        assert_eq!(
            frame_indices(&out),
            (0..4).collect::<Vec<i16>>(),
            "held back, nothing emitted yet"
        );

        seq.push(4, &packet(4), &mut out);
        assert_eq!(frame_indices(&out), (0..12).collect::<Vec<i16>>(), "gap filled, both released");
        assert_eq!(seq.stats().frames_lost, 0, "reordering is not loss");
        assert_eq!(seq.stats().packets_buffered, 1);
    }

    #[test]
    fn a_lost_packet_becomes_silence_once_the_window_fills() {
        let mut seq = sequencer();
        let mut out = Vec::new();

        seq.push(0, &packet(0), &mut out);
        // Packet at frame 4 never arrives. Five more turn up, exceeding the
        // window of four.
        for i in 2..=6u64 {
            seq.push(i * FPP as u64, &packet(i * FPP as u64), &mut out);
        }

        assert_eq!(seq.stats().frames_lost, FPP as u64, "exactly the missing packet");
        let silent_frames = out.chunks(CH).filter(|f| f[0] == 0.0 && f[1] == 0.0).count();
        assert_eq!(silent_frames, FPP, "the gap is filled with silence, not skipped");
        // Six real packets plus the one packet's worth of substituted silence.
        assert_eq!(out.len() / CH, 7 * FPP);
    }

    #[test]
    fn late_packets_are_dropped_rather_than_stalling_the_stream() {
        let mut seq = sequencer();
        let mut out = Vec::new();
        seq.push(0, &packet(0), &mut out);
        seq.push(4, &packet(4), &mut out);
        out.clear();

        seq.push(0, &packet(0), &mut out); // arrives after its slot was played
        assert!(out.is_empty());
        assert_eq!(seq.stats().packets_late, 1);
    }

    #[test]
    fn duplicates_are_counted_and_ignored() {
        let mut seq = sequencer();
        let mut out = Vec::new();
        seq.push(0, &packet(0), &mut out);
        seq.push(8, &packet(8), &mut out);
        seq.push(8, &packet(8), &mut out); // duplicate of a pending packet
        assert_eq!(seq.stats().packets_duplicate, 1);
        assert_eq!(seq.pending_frames(), FPP);
    }

    #[test]
    fn conversion_is_lossless_and_symmetric() {
        assert_eq!(convert(0), 0.0);
        assert_eq!(convert(-32_768), -1.0);
        assert_eq!(convert(16_384), 0.5);
        // Round-trips exactly for every representable value.
        for s in [i16::MIN, -1, 0, 1, 12_345, i16::MAX] {
            assert_eq!((convert(s) * 32_768.0) as i16, s);
        }
    }

    #[test]
    fn reset_reestablishes_the_origin() {
        let mut seq = sequencer();
        let mut out = Vec::new();
        seq.push(0, &packet(0), &mut out);
        seq.reset();
        out.clear();
        // A frame index far in the past would be "late" without the reset.
        seq.push(0, &packet(0), &mut out);
        assert_eq!(out.len(), FPP * CH);
        assert_eq!(seq.stats().packets_late, 0);
    }
}

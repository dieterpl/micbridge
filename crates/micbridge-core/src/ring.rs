//! A lock-free single-producer single-consumer channel of interleaved frames.
//!
//! One side of every ring here is a realtime audio callback, which may not
//! allocate, lock, or block. That rules out a `Mutex<VecDeque<_>>`: even a
//! `try_lock` that fails only occasionally turns into an audible click every few
//! minutes, which is precisely the class of bug this project is trying to avoid.
//!
//! Frame alignment is the invariant worth stating: the ring stores samples, but
//! every operation moves whole frames. A partial frame would shift the channel
//! interleave for the rest of the session.

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// Producer half. Lives on whichever side generates audio — the capture
/// callback on the sender, the network thread on the receiver.
pub struct FrameProducer {
    inner: HeapProd<f32>,
    channels: usize,
}

/// Consumer half.
pub struct FrameConsumer {
    inner: HeapCons<f32>,
    channels: usize,
}

/// Creates a ring holding `capacity_frames` interleaved frames.
pub fn frame_channel(channels: usize, capacity_frames: usize) -> (FrameProducer, FrameConsumer) {
    assert!(channels > 0, "channels must be non-zero");
    assert!(capacity_frames > 0, "capacity must be non-zero");
    let (prod, cons) = HeapRb::<f32>::new(capacity_frames * channels).split();
    (FrameProducer { inner: prod, channels }, FrameConsumer { inner: cons, channels })
}

impl FrameProducer {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames that can be written without dropping any.
    pub fn vacant_frames(&self) -> usize {
        self.inner.vacant_len() / self.channels
    }

    /// Writes as many whole frames from `samples` as fit, returning the number
    /// of frames **dropped**.
    ///
    /// Dropping rather than blocking is the only option available to a realtime
    /// callback. A non-zero return is a real defect — the ring is too small or
    /// the consumer has stalled — so the caller counts it rather than ignoring
    /// it.
    pub fn push_frames(&mut self, samples: &[f32]) -> usize {
        let offered = samples.len() / self.channels;
        let room = self.vacant_frames();
        let fitting = offered.min(room);
        let pushed = self.inner.push_slice(&samples[..fitting * self.channels]);
        debug_assert_eq!(pushed, fitting * self.channels, "push_slice honoured vacant_len");
        offered - fitting
    }

    /// [`Self::push_frames`], scaling by `factor` and saturating at full scale.
    ///
    /// Scaling during the copy rather than before it, because the caller is a
    /// capture callback holding an immutable `&[f32]` from the host: applying gain
    /// first would mean a second buffer, and allocating one there is forbidden.
    /// The scratch below is on the stack and fixed, so this allocates nothing at
    /// any buffer size.
    pub fn push_frames_scaled(&mut self, samples: &[f32], factor: f32) -> usize {
        const CHUNK: usize = 512;

        let offered = samples.len() / self.channels;
        let fitting = offered.min(self.vacant_frames());
        let wanted = &samples[..fitting * self.channels];

        let mut scratch = [0.0f32; CHUNK];
        for block in wanted.chunks(CHUNK) {
            let scaled = &mut scratch[..block.len()];
            for (out, &sample) in scaled.iter_mut().zip(block) {
                *out = (sample * factor).clamp(-1.0, 1.0);
            }
            let pushed = self.inner.push_slice(scaled);
            debug_assert_eq!(pushed, scaled.len(), "push_slice honoured vacant_len");
        }
        offered - fitting
    }
}

impl FrameConsumer {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames available to read.
    pub fn occupied_frames(&self) -> usize {
        self.inner.occupied_len() / self.channels
    }

    /// Reads exactly one frame into `frame`, or returns `false` and reads
    /// nothing.
    ///
    /// The occupancy check before the read is what preserves frame alignment: a
    /// bare `pop_slice` would happily return a single sample of a stereo frame
    /// and desynchronise the channels permanently.
    pub fn pop_frame(&mut self, frame: &mut [f32]) -> bool {
        debug_assert_eq!(frame.len(), self.channels);
        if self.inner.occupied_len() < self.channels {
            return false;
        }
        self.inner.pop_slice(&mut frame[..self.channels]) == self.channels
    }

    /// Reads whole frames into `out`, returning how many frames were read.
    pub fn pop_frames(&mut self, out: &mut [f32]) -> usize {
        let wanted = out.len() / self.channels;
        let have = self.occupied_frames();
        let taking = wanted.min(have);
        self.inner.pop_slice(&mut out[..taking * self.channels]) / self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_the_round_trip_in_order() {
        let (mut prod, mut cons) = frame_channel(2, 8);
        let dropped = prod.push_frames(&[1.0, -1.0, 2.0, -2.0]);
        assert_eq!(dropped, 0);
        assert_eq!(cons.occupied_frames(), 2);

        let mut out = vec![0.0; 4];
        assert_eq!(cons.pop_frames(&mut out), 2);
        assert_eq!(out, vec![1.0, -1.0, 2.0, -2.0]);
        assert_eq!(cons.occupied_frames(), 0);
    }

    #[test]
    fn a_full_ring_drops_whole_frames_and_reports_them() {
        let (mut prod, _cons) = frame_channel(2, 2);
        let dropped = prod.push_frames(&[1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
        assert_eq!(dropped, 1, "one frame past capacity");
    }

    #[test]
    fn partial_frames_are_never_handed_out() {
        // The alignment invariant. With one sample of a stereo frame available,
        // a reader must get nothing rather than half a frame.
        let (mut prod, mut cons) = frame_channel(2, 4);
        prod.inner.push_slice(&[0.5]); // deliberately unaligned write
        let mut frame = vec![0.0; 2];
        assert!(!cons.pop_frame(&mut frame), "should refuse a partial frame");
        assert_eq!(cons.occupied_frames(), 0, "half a frame is not a frame");
    }

    #[test]
    fn pop_frame_reports_empty_without_touching_the_buffer() {
        let (_prod, mut cons) = frame_channel(2, 4);
        let mut frame = vec![7.0, 7.0];
        assert!(!cons.pop_frame(&mut frame));
        assert_eq!(frame, vec![7.0, 7.0], "left the caller's buffer alone");
    }

    #[test]
    fn wraps_around_capacity_repeatedly() {
        let (mut prod, mut cons) = frame_channel(1, 4);
        let mut frame = vec![0.0; 1];
        for i in 0..100 {
            assert_eq!(prod.push_frames(&[i as f32]), 0);
            assert!(cons.pop_frame(&mut frame));
            assert_eq!(frame[0], i as f32);
        }
    }

    #[test]
    fn vacant_and_occupied_agree_with_capacity() {
        let (mut prod, cons) = frame_channel(2, 10);
        assert_eq!(prod.vacant_frames(), 10);
        prod.push_frames(&[0.0; 8]); // 4 frames
        assert_eq!(cons.occupied_frames(), 4);
        assert_eq!(prod.vacant_frames(), 6);
    }

    #[test]
    fn moves_between_threads() {
        // The whole point is a cross-thread handoff, so make sure the halves are
        // actually `Send`.
        let (mut prod, mut cons) = frame_channel(1, 64);
        let writer = std::thread::spawn(move || {
            for i in 0..1_000 {
                while prod.push_frames(&[i as f32]) != 0 {
                    std::thread::yield_now();
                }
            }
        });
        let mut seen = 0;
        let mut frame = vec![0.0; 1];
        while seen < 1_000 {
            if cons.pop_frame(&mut frame) {
                assert_eq!(frame[0], seen as f32, "order preserved across threads");
                seen += 1;
            } else {
                std::thread::yield_now();
            }
        }
        writer.join().expect("writer finished");
    }
}

//! A continuously variable-ratio resampler.
//!
//! Drift correction needs the ratio to be adjustable by parts per million on
//! every callback, which rules out the usual fixed-ratio designs. What is left
//! is the direct approach: keep a fractional read position through the input and
//! interpolate. Four-point cubic Hermite is the interpolator — it costs a
//! handful of multiplies per sample and its error at ratios near 1.0, which is
//! where drift correction lives, is far below the noise floor of the capture
//! device.
//!
//! This is not a high-quality rate converter. For 48 kHz to 48 kHz with drift
//! trim it is inaudible; for a large fixed conversion such as 44.1 to 48 kHz it
//! is adequate for voice but will not satisfy a mastering engineer. Setting the
//! interface to 48 kHz avoids the question entirely, which is why the README
//! recommends it. A band-limited sinc converter is the upgrade path if the
//! large-ratio case ever matters.

/// The number of input frames the interpolator keeps as history. Four is the
/// width of the cubic kernel: one before the read position and two after.
const KERNEL: usize = 4;

/// Interpolates a stream of interleaved frames at an arbitrary, changeable rate.
///
/// The resampler pulls input on demand through a closure rather than taking a
/// slice, because the number of input frames it needs depends on a ratio that
/// changes while it runs. Pulling also means starvation is a return value
/// instead of a precondition the caller has to compute.
pub struct VariableResampler {
    channels: usize,
    /// Per channel, the `KERNEL` most recent input frames, oldest first.
    history: Vec<[f32; KERNEL]>,
    /// Read position between `history[1]` and `history[2]`, in input frames.
    /// Starts at `KERNEL` so the first call primes the kernel before producing
    /// anything, rather than needing a separate priming path.
    position: f64,
    scratch: Vec<f32>,
}

impl VariableResampler {
    pub fn new(channels: usize) -> Self {
        assert!(channels > 0, "channels must be non-zero");
        Self {
            channels,
            history: vec![[0.0; KERNEL]; channels],
            position: KERNEL as f64,
            scratch: vec![0.0; channels],
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Input frames held in the kernel. The drift controller adds this to the
    /// ring occupancy so a change in buffering target is not fought by a
    /// constant offset.
    pub const fn latency_frames() -> usize {
        KERNEL
    }

    /// Fills `out` with interleaved frames, consuming input through `next_frame`
    /// at `ratio` input frames per output frame.
    ///
    /// `next_frame` writes one interleaved input frame and returns `false` when
    /// no input is available. Returns the number of output **frames** written;
    /// anything less than `out.len() / channels` means the input starved and the
    /// caller should treat the remainder as an underrun.
    pub fn process<F>(&mut self, out: &mut [f32], ratio: f64, mut next_frame: F) -> usize
    where
        F: FnMut(&mut [f32]) -> bool,
    {
        debug_assert!(ratio > 0.0, "ratio must be positive");
        let want = out.len() / self.channels;
        let mut produced = 0;

        while produced < want {
            // Advance the kernel until the read position sits inside it. With a
            // ratio above 1.0 this consumes more than one input frame per output
            // frame, which is exactly how the buffer drains faster than it fills.
            while self.position >= 1.0 {
                if !next_frame(&mut self.scratch) {
                    return produced;
                }
                for ch in 0..self.channels {
                    let h = &mut self.history[ch];
                    h[0] = h[1];
                    h[1] = h[2];
                    h[2] = h[3];
                    h[3] = self.scratch[ch];
                }
                self.position -= 1.0;
            }

            let x = self.position as f32;
            let base = produced * self.channels;
            for ch in 0..self.channels {
                out[base + ch] = hermite(&self.history[ch], x);
            }
            produced += 1;
            self.position += ratio;
        }

        produced
    }

    /// Drops history and the fractional position.
    ///
    /// Called on a session change. Without it the first frames of a new stream
    /// would be interpolated against the tail of the old one, which is audible
    /// as a click.
    pub fn reset(&mut self) {
        for h in &mut self.history {
            *h = [0.0; KERNEL];
        }
        self.position = KERNEL as f64;
    }
}

/// Four-point cubic Hermite interpolation, Catmull-Rom tangents.
///
/// `y` holds the samples at positions -1, 0, 1 and 2; `x` in `[0, 1)` is the
/// offset from `y[1]`. Written in Horner form to keep it to three multiplies
/// per sample in the inner loop.
#[inline]
fn hermite(y: &[f32; KERNEL], x: f32) -> f32 {
    let c0 = y[1];
    let c1 = 0.5 * (y[2] - y[0]);
    let c2 = y[0] - 2.5 * y[1] + 2.0 * y[2] - 0.5 * y[3];
    let c3 = 0.5 * (y[3] - y[0]) + 1.5 * (y[1] - y[2]);
    ((c3 * x + c2) * x + c1) * x + c0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds frames whose every channel carries the same ramping value, so an
    /// interpolated output can be checked against the position it claims to be
    /// reading from.
    fn ramp_source(channels: usize, total: usize) -> impl FnMut(&mut [f32]) -> bool {
        let mut n = 0usize;
        move |frame: &mut [f32]| {
            if n >= total {
                return false;
            }
            frame[..channels].fill(n as f32);
            n += 1;
            true
        }
    }

    #[test]
    fn passthrough_ratio_reproduces_the_input_exactly() {
        // At ratio 1.0 the read position never leaves an integer, so the
        // interpolator must return the sample itself rather than a blend.
        let mut r = VariableResampler::new(1);
        let mut out = vec![0.0; 16];
        let produced = r.process(&mut out, 1.0, ramp_source(1, 64));
        assert_eq!(produced, 16);
        // Two frames of kernel latency: output n is input n + 1.
        for (i, &v) in out.iter().enumerate() {
            assert!((v - (i as f32 + 1.0)).abs() < 1e-4, "out[{i}] = {v}");
        }
    }

    #[test]
    fn a_linear_ramp_is_interpolated_exactly_at_any_ratio() {
        // Cubic interpolation is exact for polynomials up to degree three, so a
        // ramp pins down the read position with no tolerance for a phase error.
        for ratio in [0.5, 0.75, 1.0, 1.5, 2.0] {
            let mut r = VariableResampler::new(1);
            let mut out = vec![0.0; 32];
            let produced = r.process(&mut out, ratio, ramp_source(1, 512));
            assert_eq!(produced, 32, "ratio {ratio}");
            for (i, &v) in out.iter().enumerate() {
                let expected = 1.0 + i as f32 * ratio as f32;
                assert!(
                    (v - expected).abs() < 1e-3,
                    "ratio {ratio} out[{i}] = {v}, want {expected}"
                );
            }
        }
    }

    #[test]
    fn ratio_controls_how_fast_input_is_consumed() {
        // This is the property drift correction depends on: a ratio above 1.0
        // must drain the buffer faster than it fills.
        //
        // The run has to be long to say anything. Producing N output frames
        // advances the position N-1 times, so at ratio 1.01 and N = 100 the
        // total advance is 99.99 — indistinguishable from ratio 1.0. Drift trims
        // are parts per million, so a test that cannot resolve one percent over
        // its window would not catch a sign error either.
        const N: usize = 10_000;

        for ratio in [0.99, 1.0, 1.01] {
            let mut r = VariableResampler::new(2);
            let mut consumed = 0usize;
            let mut out = vec![0.0; N * 2];
            let produced = r.process(&mut out, ratio, |frame| {
                consumed += 1;
                frame.fill(0.25);
                true
            });
            assert_eq!(produced, N);

            let net = consumed - VariableResampler::latency_frames();
            let want = (N - 1) as f64 * ratio;
            assert!(
                (net as f64 - want).abs() <= 1.0,
                "ratio {ratio}: consumed {net} input frames for {N} output, want ~{want}"
            );
        }
    }

    #[test]
    fn starvation_reports_a_short_write_instead_of_inventing_audio() {
        let mut r = VariableResampler::new(2);
        let mut out = vec![99.0; 40 * 2];
        // Only 12 input frames, 4 of which prime the kernel.
        let produced = r.process(&mut out, 1.0, ramp_source(2, 12));
        assert!(produced < 40, "should have starved, produced {produced}");
        assert!(produced >= 8, "should have produced what it could, got {produced}");
        // The caller owns the tail; the resampler must not have touched it.
        assert_eq!(out[produced * 2], 99.0);
    }

    #[test]
    fn resuming_after_starvation_continues_the_stream() {
        let mut r = VariableResampler::new(1);
        let mut source = ramp_source(1, 8);
        let mut out = vec![0.0; 16];
        let first = r.process(&mut out, 1.0, &mut source);

        let mut source = ramp_source(1, 64);
        let mut more = vec![0.0; 8];
        let second = r.process(&mut more, 1.0, &mut source);
        assert!(first > 0 && second > 0, "both passes produced output");
        // The second source restarts its ramp at 0, so continuity across the
        // boundary is not checkable here; what matters is that the resampler
        // kept producing rather than wedging.
        assert_eq!(second, 8);
    }

    #[test]
    fn channels_stay_independent() {
        let mut r = VariableResampler::new(2);
        let mut n = 0.0f32;
        let mut out = vec![0.0; 16 * 2];
        r.process(&mut out, 1.0, |frame| {
            frame[0] = n;
            frame[1] = -n;
            n += 1.0;
            true
        });
        for f in out.chunks(2) {
            assert!(
                (f[0] + f[1]).abs() < 1e-4,
                "left {} and right {} should be negatives",
                f[0],
                f[1]
            );
        }
    }

    #[test]
    fn reset_clears_history() {
        let mut r = VariableResampler::new(1);
        let mut out = vec![0.0; 8];
        r.process(&mut out, 1.0, ramp_source(1, 64));
        r.reset();

        // After a reset, a silent source must produce silence — any residue from
        // the previous stream would be an audible click.
        let mut out = vec![0.0; 8];
        r.process(&mut out, 1.0, |frame| {
            frame.fill(0.0);
            true
        });
        assert!(out.iter().all(|&v| v == 0.0), "reset left residue: {out:?}");
    }

    #[test]
    fn hermite_returns_the_sample_itself_at_zero_offset() {
        let y = [1.0, 2.0, 3.0, 4.5];
        assert!((hermite(&y, 0.0) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn hermite_is_bounded_on_a_step() {
        // Cubic interpolation overshoots on a step; the point of this test is
        // that the overshoot is small and bounded, not that it is absent.
        let y = [0.0, 0.0, 1.0, 1.0];
        for i in 0..=10 {
            let v = hermite(&y, i as f32 / 10.0);
            assert!(v > -0.2 && v < 1.2, "overshoot at {i}: {v}");
        }
    }
}

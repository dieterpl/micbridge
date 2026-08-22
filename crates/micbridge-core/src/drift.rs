//! Clock-drift correction.
//!
//! The sender's capture clock and the receiver's render clock are different
//! crystals, typically tens of parts per million apart. Nothing in the protocol
//! can fix that: over an hour, 50 ppm is 180 ms of audio that either has to go
//! somewhere or has to come from somewhere. Left alone the jitter buffer walks
//! steadily toward empty or full, which is why an uncorrected implementation
//! feels like it works for several minutes and then starts clicking.
//!
//! The correction is to make the *consumption* rate track the arrival rate, and
//! the only observable that reveals the mismatch is buffer occupancy. So this is
//! a PI controller on fill level whose output is the resampler ratio.
//!
//! Deliberately slow. A ratio that moves quickly is a pitch wobble, and the
//! error being corrected accumulates over minutes — there is nothing to be
//! gained by reacting in milliseconds.

/// Gains and limits for [`DriftController`].
///
/// The defaults come from treating the loop as a second-order system. With
/// `K = sample_rate / target_frames` as the loop gain, the closed-loop poles are
/// `s² + K·kp·s + K·ki`, giving a natural frequency of `sqrt(K·ki)` and damping
/// `kp·sqrt(K/ki)/2`. At the default 20 ms target and 48 kHz that lands near
/// 0.2 Hz with damping around 0.8: settles in a few seconds, far too slow to
/// hear as pitch movement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftGains {
    /// Proportional gain on normalised fill error.
    pub kp: f64,
    /// Integral gain per second. This term is what actually cancels a constant
    /// rate offset — proportional control alone would settle at a permanent fill
    /// error instead.
    pub ki: f64,
    /// Hard bound on the fractional rate correction, in either direction.
    ///
    /// 0.005 is 5000 ppm, two orders of magnitude more headroom than any real
    /// crystal pair needs. It exists to stop a pathological fill reading — a
    /// stalled sender, a session change — from commanding an audible rate jump.
    pub max_deviation: f64,
}

impl Default for DriftGains {
    fn default() -> Self {
        Self { kp: 0.04, ki: 0.03, max_deviation: 0.005 }
    }
}

/// Maps jitter-buffer occupancy to a resampler ratio.
#[derive(Debug, Clone)]
pub struct DriftController {
    /// Ratio that would be correct if both clocks were exact: the rate
    /// conversion, with no drift trim applied.
    nominal: f64,
    target_frames: f64,
    gains: DriftGains,
    integral: f64,
    ratio: f64,
}

impl DriftController {
    /// `input_rate` is the sender's declared rate, `output_rate` the local
    /// device's. `target_frames` is the fill level the controller holds.
    pub fn new(input_rate: u32, output_rate: u32, target_frames: f64) -> Self {
        Self::with_gains(input_rate, output_rate, target_frames, DriftGains::default())
    }

    pub fn with_gains(
        input_rate: u32,
        output_rate: u32,
        target_frames: f64,
        gains: DriftGains,
    ) -> Self {
        assert!(output_rate > 0, "output rate must be non-zero");
        assert!(target_frames > 0.0, "target fill must be positive");
        let nominal = input_rate as f64 / output_rate as f64;
        Self { nominal, target_frames, gains, integral: 0.0, ratio: nominal }
    }

    /// The ratio that would apply with no drift trim.
    pub fn nominal_ratio(&self) -> f64 {
        self.nominal
    }

    pub fn target_frames(&self) -> f64 {
        self.target_frames
    }

    /// Most recently commanded ratio.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The trim currently being applied, as a fraction of the nominal rate.
    ///
    /// Reported in logs because it is the direct estimate of the two clocks'
    /// disagreement: parked at 60e-6 means the sender's crystal is 60 ppm fast.
    pub fn deviation(&self) -> f64 {
        self.ratio / self.nominal - 1.0
    }

    /// Folds one fill measurement in and returns the ratio to use.
    ///
    /// `fill_frames` is everything buffered ahead of the renderer, including the
    /// resampler's kernel. `dt` is the time since the previous call, in seconds,
    /// so the gains stay meaningful whatever the callback size.
    pub fn update(&mut self, fill_frames: f64, dt: f64) -> f64 {
        // Normalising by the target makes the gains dimensionless, so they do
        // not need retuning when the buffer target changes.
        let error = (fill_frames - self.target_frames) / self.target_frames;

        // Integrate with the limit applied to the accumulator itself, not only
        // to the output. Clamping the output alone lets the integral wind up
        // during a long excursion and then overshoot on the way back.
        self.integral = (self.integral + error * self.gains.ki * dt)
            .clamp(-self.gains.max_deviation, self.gains.max_deviation);

        let correction = (self.gains.kp * error + self.integral)
            .clamp(-self.gains.max_deviation, self.gains.max_deviation);

        self.ratio = self.nominal * (1.0 + correction);
        self.ratio
    }

    /// Returns to the nominal ratio and forgets the accumulated estimate.
    ///
    /// Only on a session change. Surviving a brief underrun with the estimate
    /// intact is the whole point of the integral term — resetting on every
    /// hiccup would throw away minutes of convergence.
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.ratio = self.nominal;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const TARGET: f64 = 960.0; // 20 ms at 48 kHz
    const CALLBACK: f64 = 0.01; // 10 ms render callbacks

    /// Runs the loop against a sender whose clock is `ppm` parts per million
    /// fast, returning the fill history in frames.
    ///
    /// `controlled` selects whether the ratio is fed back or pinned at nominal,
    /// which is how the tests below show the controller is load-bearing rather
    /// than decorative.
    fn simulate(ppm: f64, seconds: f64, controlled: bool) -> (Vec<f64>, DriftController) {
        let mut ctl = DriftController::new(RATE, RATE, TARGET);
        let in_rate = RATE as f64 * (1.0 + ppm * 1e-6);
        let mut fill = TARGET;
        let mut history = Vec::new();

        let steps = (seconds / CALLBACK) as usize;
        for _ in 0..steps {
            let ratio = if controlled { ctl.update(fill, CALLBACK) } else { ctl.nominal_ratio() };
            fill += in_rate * CALLBACK - ratio * RATE as f64 * CALLBACK;
            history.push(fill);
        }
        (history, ctl)
    }

    #[test]
    fn uncorrected_drift_drains_the_buffer_dry() {
        // The motivation for the whole module. 200 ppm is a plausible pair of
        // consumer crystals, and five minutes is a short game session.
        let (history, _) = simulate(-200.0, 300.0, false);
        let final_fill = *history.last().expect("simulation ran");
        assert!(
            final_fill < -TARGET,
            "expected the buffer to run dry, ended at {final_fill} frames"
        );
    }

    #[test]
    fn correction_holds_the_buffer_at_target() {
        let (history, ctl) = simulate(200.0, 300.0, true);
        let final_fill = *history.last().expect("simulation ran");
        assert!(
            (final_fill - TARGET).abs() < TARGET * 0.05,
            "fill should sit near {TARGET}, ended at {final_fill}"
        );
        // The controller has effectively measured the clock offset.
        assert!(
            (ctl.deviation() - 200e-6).abs() < 20e-6,
            "expected a ~200 ppm trim, got {:.1} ppm",
            ctl.deviation() * 1e6
        );
    }

    #[test]
    fn works_in_both_directions() {
        for ppm in [-500.0, -50.0, 50.0, 500.0] {
            let (history, ctl) = simulate(ppm, 300.0, true);
            let final_fill = *history.last().expect("simulation ran");
            assert!(
                (final_fill - TARGET).abs() < TARGET * 0.05,
                "{ppm} ppm: fill ended at {final_fill}"
            );
            assert!(
                (ctl.deviation() - ppm * 1e-6).abs() < 50e-6,
                "{ppm} ppm: trim was {:.1} ppm",
                ctl.deviation() * 1e6
            );
        }
    }

    #[test]
    fn settles_without_large_overshoot() {
        let (history, _) = simulate(200.0, 300.0, true);
        let peak = history.iter().cloned().fold(f64::MIN, f64::max);
        assert!(peak < TARGET * 1.5, "overshot to {peak} frames, target {TARGET}");
        let trough = history.iter().cloned().fold(f64::MAX, f64::min);
        assert!(trough > 0.0, "undershot to {trough} frames — that is an underrun");
    }

    #[test]
    fn settles_within_a_few_seconds() {
        // Deliberately slow, but not so slow that a session change takes a
        // minute to recover from.
        let (history, _) = simulate(200.0, 60.0, true);
        let settled_at = history
            .iter()
            .position(|&f| (f - TARGET).abs() < TARGET * 0.02)
            .expect("should settle");
        let seconds = settled_at as f64 * CALLBACK;
        assert!(seconds < 20.0, "took {seconds}s to settle");
    }

    #[test]
    fn ratio_never_leaves_the_clamp_even_on_absurd_input() {
        let mut ctl = DriftController::new(RATE, RATE, TARGET);
        let max = DriftGains::default().max_deviation;
        // A stalled sender reads as an empty buffer forever.
        for _ in 0..10_000 {
            let ratio = ctl.update(0.0, CALLBACK);
            assert!(
                ratio >= 1.0 - max - 1e-9 && ratio <= 1.0 + max + 1e-9,
                "ratio {ratio} escaped"
            );
        }
        // And the far side: a buffer pinned full.
        for _ in 0..10_000 {
            let ratio = ctl.update(TARGET * 100.0, CALLBACK);
            assert!(
                ratio >= 1.0 - max - 1e-9 && ratio <= 1.0 + max + 1e-9,
                "ratio {ratio} escaped"
            );
        }
    }

    #[test]
    fn integral_recovers_promptly_after_being_pinned() {
        // Anti-windup check. After a long stall the accumulator must not have
        // charged up so far that it overshoots on the way back.
        let mut ctl = DriftController::new(RATE, RATE, TARGET);
        for _ in 0..30_000 {
            ctl.update(0.0, CALLBACK); // five minutes of empty buffer
        }
        let mut fill = TARGET;
        for _ in 0..2_000 {
            let ratio = ctl.update(fill, CALLBACK);
            fill += RATE as f64 * CALLBACK - ratio * RATE as f64 * CALLBACK;
        }
        assert!((fill - TARGET).abs() < TARGET * 0.1, "recovered to {fill}, want ~{TARGET}");
    }

    #[test]
    fn nominal_ratio_carries_the_rate_conversion() {
        // The interface's current 44.1 kHz against a 48 kHz Windows endpoint.
        let ctl = DriftController::new(44_100, 48_000, TARGET);
        assert!((ctl.nominal_ratio() - 0.918_75).abs() < 1e-9);
        assert_eq!(ctl.ratio(), ctl.nominal_ratio(), "starts with no trim applied");
        assert_eq!(ctl.deviation(), 0.0);
    }

    #[test]
    fn drift_correction_composes_with_rate_conversion() {
        // Trim is relative to nominal, so a converting stream must still
        // converge on the same ppm estimate.
        let mut ctl = DriftController::new(44_100, 48_000, TARGET);
        let in_rate = 44_100.0 * (1.0 + 300e-6);
        let mut fill = TARGET;
        for _ in 0..30_000 {
            let ratio = ctl.update(fill, CALLBACK);
            fill += in_rate * CALLBACK - ratio * 48_000.0 * CALLBACK;
        }
        assert!((fill - TARGET).abs() < TARGET * 0.05, "fill ended at {fill}");
        assert!(
            (ctl.deviation() - 300e-6).abs() < 30e-6,
            "trim was {:.1} ppm, want 300",
            ctl.deviation() * 1e6
        );
    }

    #[test]
    fn reset_returns_to_nominal() {
        let mut ctl = DriftController::new(RATE, RATE, TARGET);
        for _ in 0..1_000 {
            ctl.update(TARGET * 2.0, CALLBACK);
        }
        assert_ne!(ctl.ratio(), ctl.nominal_ratio());
        ctl.reset();
        assert_eq!(ctl.ratio(), ctl.nominal_ratio());
        assert_eq!(ctl.deviation(), 0.0);
    }
}

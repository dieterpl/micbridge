//! Output gain, adjustable while a session is running.
//!
//! An interface whose knob is set low, or a quiet talker, arrives at the other end
//! too quiet to be useful, and the fix is not always reachable — the whole point of
//! this program is that the two machines are in different places. So the gain lives
//! here, in the signal path, where either end can change it.
//!
//! Held as an atomic rather than behind a lock because both call sites are audio
//! callbacks. `CONTRIBUTING.md` forbids locking there, including `try_lock`: a lock
//! that fails one time in ten thousand is an audible click every few minutes. One
//! relaxed load per block is the entire synchronisation cost.
//!
//! Applying gain can push samples past full scale, so [`Gain::apply`] saturates. That
//! is deliberate and it is why the GUI's clip indicator latches: hard clipping is
//! audible, and a user who turned the gain up too far needs to be told rather than
//! left to wonder why it sounds harsh.

use std::sync::atomic::{AtomicU32, Ordering};

/// Quietest setting that is still a gain rather than a mute.
pub const MIN_DB: f32 = -30.0;
/// Loudest setting offered.
///
/// +30 dB is a factor of about 32. Past that, anything quiet enough to need it is
/// dominated by its own noise floor, so a larger number would promise something the
/// signal cannot deliver.
pub const MAX_DB: f32 = 30.0;

/// A shared, realtime-safe gain.
#[derive(Debug)]
pub struct Gain {
    /// `f32` bits: there is no stable `AtomicF32`.
    factor: AtomicU32,
}

impl Default for Gain {
    fn default() -> Self {
        Self::unity()
    }
}

impl Gain {
    pub const fn unity() -> Self {
        Self { factor: AtomicU32::new(1.0f32.to_bits()) }
    }

    pub fn new(db: f32) -> Self {
        let gain = Self::unity();
        gain.set_db(db);
        gain
    }

    /// Sets the gain in decibels, clamped to [`MIN_DB`]..=[`MAX_DB`].
    pub fn set_db(&self, db: f32) {
        // NaN would propagate into every sample. Treat it as unity, which is the
        // only safe reading of "no meaningful value".
        let db = if db.is_nan() { 0.0 } else { db.clamp(MIN_DB, MAX_DB) };
        self.factor.store(db_to_factor(db).to_bits(), Ordering::Relaxed);
    }

    pub fn db(&self) -> f32 {
        factor_to_db(self.factor())
    }

    /// The linear multiplier. One relaxed load; safe in an audio callback.
    #[inline]
    pub fn factor(&self) -> f32 {
        f32::from_bits(self.factor.load(Ordering::Relaxed))
    }

    /// True when the gain would not change a sample, so callers can skip the work
    /// entirely rather than multiplying a whole buffer by 1.0.
    #[inline]
    pub fn is_unity(&self) -> bool {
        self.factor() == 1.0
    }

    /// Scales a buffer in place, saturating at full scale.
    #[inline]
    pub fn apply(&self, samples: &mut [f32]) {
        let factor = self.factor();
        if factor == 1.0 {
            return;
        }
        for sample in samples {
            *sample = (*sample * factor).clamp(-1.0, 1.0);
        }
    }
}

/// Decibels to a linear amplitude multiplier.
pub fn db_to_factor(db: f32) -> f32 {
    // Amplitude, not power, so 20 rather than 10: +6 dB is twice as loud a signal.
    10.0f32.powf(db / 20.0)
}

/// Linear amplitude multiplier back to decibels.
pub fn factor_to_db(factor: f32) -> f32 {
    if factor <= 0.0 {
        MIN_DB
    } else {
        20.0 * factor.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_leaves_every_sample_exactly_alone() {
        // Exactly, not approximately: a gain control at 0 dB that perturbs the
        // signal is a bug that would be inaudible and permanent.
        let gain = Gain::new(0.0);
        assert!(gain.is_unity());

        let original = [-1.0, -0.5, 0.0, 0.25, 0.999];
        let mut samples = original;
        gain.apply(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn six_decibels_is_a_factor_of_two() {
        // The relationship a user is relying on when they read the slider.
        assert!((db_to_factor(6.0) - 2.0).abs() < 0.01, "{}", db_to_factor(6.0));
        assert!((db_to_factor(-6.0) - 0.5).abs() < 0.01);
        assert!((db_to_factor(0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn decibels_survive_a_round_trip() {
        for db in [-30.0, -12.0, -0.5, 0.0, 3.0, 12.0, 30.0] {
            let gain = Gain::new(db);
            assert!((gain.db() - db).abs() < 0.01, "{db} came back as {}", gain.db());
        }
    }

    /// The reason `apply` saturates rather than wrapping: a wrapped sample is not
    /// merely loud, it inverts, and full-scale noise is the result.
    #[test]
    fn boosting_saturates_instead_of_wrapping() {
        let gain = Gain::new(20.0); // x10
        let mut samples = [0.5, -0.5, 0.05, -0.05];
        gain.apply(&mut samples);

        assert_eq!(samples[0], 1.0, "positive overshoot clamps to full scale");
        assert_eq!(samples[1], -1.0, "negative overshoot clamps to full scale");
        assert!((samples[2] - 0.5).abs() < 1e-5, "in-range samples still scale");
        assert!((samples[3] + 0.5).abs() < 1e-5);
    }

    #[test]
    fn the_range_is_clamped_rather_than_trusted() {
        assert_eq!(Gain::new(1000.0).db(), MAX_DB);
        assert_eq!(Gain::new(-1000.0).db(), MIN_DB);
    }

    /// NaN in an audio path is permanent: it propagates through every later sample
    /// and the stream never recovers. A UI that produces one must not be able to
    /// silence the program.
    #[test]
    fn a_nan_setting_is_treated_as_unity() {
        let gain = Gain::new(f32::NAN);
        assert!(gain.is_unity(), "NaN should fall back to unity, got {}", gain.db());

        let mut samples = [0.3, -0.7];
        gain.apply(&mut samples);
        assert_eq!(samples, [0.3, -0.7]);
    }

    #[test]
    fn gain_can_be_changed_while_another_thread_reads_it() {
        // Not a race detector, just proof the type is shareable without a lock —
        // which is the property that lets an audio callback read it at all.
        use std::sync::Arc;
        let gain = Arc::new(Gain::new(0.0));
        let writer = Arc::clone(&gain);
        let handle = std::thread::spawn(move || {
            for db in 0..20 {
                writer.set_db(db as f32);
            }
        });
        for _ in 0..1000 {
            let mut samples = [0.1; 64];
            gain.apply(&mut samples);
        }
        handle.join().expect("writer thread");
        assert!(gain.db() >= 0.0);
    }
}

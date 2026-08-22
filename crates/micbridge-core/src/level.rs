//! A peak level meter that an audio callback can write to.
//!
//! Exists for the GUI: "is audio actually flowing" is the first question anyone
//! asks when bringing this up, and a packet counter does not answer it — a
//! counter climbs just as happily when the input is muted.
//!
//! Peak-hold with reset-on-read. The writer keeps the largest magnitude it has
//! seen; the reader takes that value and clears it, so each poll reports the peak
//! since the previous poll rather than an instantaneous sample that would mostly
//! catch zero crossings.

use std::sync::atomic::{AtomicU32, Ordering};

/// Realtime-safe peak meter.
///
/// One relaxed load and at most one relaxed store per block, no allocation and no
/// locking, so it is safe to call from a CoreAudio or WASAPI callback.
#[derive(Debug, Default)]
pub struct LevelMeter {
    /// `f32` bits. Held as an integer because there is no stable `AtomicF32`.
    peak: AtomicU32,
}

impl LevelMeter {
    pub const fn new() -> Self {
        Self { peak: AtomicU32::new(0) }
    }

    /// Folds a block of interleaved samples into the held peak.
    ///
    /// Channels are deliberately not separated: a single bar is what the GUI
    /// shows, and a per-channel meter would be more state for no extra answer to
    /// the question being asked.
    #[inline]
    pub fn record(&self, samples: &[f32]) {
        let mut peak = 0.0f32;
        for &sample in samples {
            let magnitude = sample.abs();
            if magnitude > peak {
                peak = magnitude;
            }
        }
        self.record_peak(peak);
    }

    /// [`Self::record`] for a block that is about to be scaled by `factor`.
    ///
    /// Peak is linear in gain, so the scaled peak is the raw peak times the factor,
    /// saturated the same way the samples themselves will be. That makes a post-gain
    /// meter free rather than a second pass over the buffer.
    #[inline]
    pub fn record_scaled(&self, samples: &[f32], factor: f32) {
        let mut peak = 0.0f32;
        for &sample in samples {
            let magnitude = sample.abs();
            if magnitude > peak {
                peak = magnitude;
            }
        }
        self.record_peak((peak * factor).min(1.0));
    }

    /// Folds a single already-computed magnitude into the held peak.
    #[inline]
    pub fn record_peak(&self, peak: f32) {
        // NaN would poison the meter permanently, since every later comparison
        // against it is false. Drop it instead.
        if !peak.is_finite() || peak <= 0.0 {
            return;
        }
        let candidate = peak.to_bits();
        let held = self.peak.load(Ordering::Relaxed);
        // Comparing bit patterns rather than floats is valid here because both
        // values are finite and non-negative, and IEEE-754 orders those the same
        // way as their bit patterns do.
        if candidate > held {
            self.peak.store(candidate, Ordering::Relaxed);
        }
    }

    /// Returns the peak since the last call and resets the hold.
    pub fn take(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }

    /// Reads the held peak without clearing it.
    pub fn peek(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    /// Converts a linear magnitude to dBFS, floored at `floor_db`.
    ///
    /// A meter drawn on a linear scale spends almost all its travel in the top
    /// few dB and shows nothing at all for quiet speech; dB is what makes the
    /// difference between "silent" and "quiet but present" visible. The floor
    /// keeps digital silence from mapping to negative infinity.
    pub fn to_dbfs(magnitude: f32, floor_db: f32) -> f32 {
        if magnitude <= 0.0 {
            return floor_db;
        }
        (20.0 * magnitude.log10()).max(floor_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_the_largest_magnitude_seen() {
        let meter = LevelMeter::new();
        meter.record(&[0.1, -0.5, 0.2]);
        meter.record(&[0.3, 0.05]);
        assert_eq!(meter.peek(), 0.5, "should hold the peak, not the most recent block");
    }

    #[test]
    fn take_resets_so_each_poll_covers_one_interval() {
        let meter = LevelMeter::new();
        meter.record(&[0.75]);
        assert_eq!(meter.take(), 0.75);
        assert_eq!(meter.take(), 0.0, "second poll sees nothing new");
        meter.record(&[0.25]);
        assert_eq!(meter.take(), 0.25);
    }

    #[test]
    fn magnitude_is_absolute() {
        let meter = LevelMeter::new();
        meter.record(&[-0.9]);
        assert_eq!(meter.take(), 0.9);
    }

    #[test]
    fn silence_reads_as_zero() {
        let meter = LevelMeter::new();
        meter.record(&[0.0, 0.0, 0.0]);
        assert_eq!(meter.take(), 0.0);
    }

    #[test]
    fn empty_block_is_harmless() {
        let meter = LevelMeter::new();
        meter.record(&[]);
        assert_eq!(meter.take(), 0.0);
    }

    #[test]
    fn nan_does_not_poison_the_meter() {
        // Every comparison against a held NaN is false, so a single bad sample
        // would freeze the meter for the rest of the session.
        let meter = LevelMeter::new();
        meter.record(&[f32::NAN, 0.4]);
        assert_eq!(meter.take(), 0.4);

        meter.record(&[f32::INFINITY]);
        assert_eq!(meter.take(), 0.0, "infinity is rejected too");
    }

    #[test]
    fn dbfs_conversion_matches_known_points() {
        assert!((LevelMeter::to_dbfs(1.0, -90.0) - 0.0).abs() < 1e-4);
        assert!((LevelMeter::to_dbfs(0.5, -90.0) + 6.0206).abs() < 1e-3);
        assert!((LevelMeter::to_dbfs(0.25, -90.0) + 12.0412).abs() < 1e-3);
    }

    #[test]
    fn dbfs_floors_instead_of_returning_negative_infinity() {
        assert_eq!(LevelMeter::to_dbfs(0.0, -90.0), -90.0);
        assert_eq!(LevelMeter::to_dbfs(1e-12, -90.0), -90.0);
    }

    #[test]
    fn is_shareable_across_threads() {
        use std::sync::Arc;

        let meter = Arc::new(LevelMeter::new());
        let writer = {
            let meter = Arc::clone(&meter);
            std::thread::spawn(move || {
                for i in 0..1_000 {
                    meter.record(&[i as f32 / 1_000.0]);
                }
            })
        };
        writer.join().expect("writer finished");
        assert!(meter.take() > 0.99, "should have seen the largest value written");
    }
}

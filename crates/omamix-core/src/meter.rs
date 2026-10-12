//! Block peak and RMS, and the UI hold and decay.
//!
//! The audio callback publishes one peak and one RMS per block, in dBFS.
//! It does not hold or decay. [`MeterDisplay`] is what a UI timer steps.
//!
//! Peak is the largest absolute sample on either channel. RMS is the
//! root-mean-square of the louder channel, so a silent side does not pull
//! the reading down. A sine whose peak is −20 dBFS reads about −23 dB RMS,
//! which is −20 minus 10 log10(2). Non-finite samples are skipped. Silence
//! is −∞ dB, not 0 dBFS. A block louder than [`crate::db::MAX_BOOST_DB`] is
//! published at that ceiling, the same cap as a gain. Peak and RMS share one
//! atomic word, so a reader cannot pair values from two blocks. The sum of
//! squares is `f64`, so a block around 1e19 does not overflow into silence.
//!
//! The hold time, the fall rate, and [`METER_BAR_FLOOR_DB`] are display
//! constants. They are not a reading from a meter on any particular machine.
//! The engine still publishes −∞ for silence. A later bar draws empty below
//! that floor. Nothing here clamps to it.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::db::{self, BelowMin};

/// How long the displayed peak stays up before it falls.
pub const PEAK_HOLD_SECS: f32 = 1.0;

/// How fast the displayed peak falls after the hold, in dB per second.
pub const PEAK_DECAY_DB_PER_SEC: f32 = 20.0;

/// Lowest dBFS a later meter bar draws. Below this the bar is empty.
///
/// About −60 dBFS, chosen for the display. Not a measured noise floor.
/// [`BlockMeter`] and [`MeterDisplay`] keep publishing −∞ for silence.
pub const METER_BAR_FLOOR_DB: f32 = -60.0;

const SILENCE_PAIR: u64 = {
    let bits = f32::NEG_INFINITY.to_bits() as u64;
    bits | (bits << 32)
};

/// Peak and RMS of one block, in dBFS, published as one word.
///
/// Starts at silence (−∞ dB). A store sanitizes NaN to silence and +∞ to
/// [`db::MAX_BOOST_DB`]. One store publishes both numbers.
#[derive(Debug)]
pub struct BlockMeter {
    packed: AtomicU64,
}

impl BlockMeter {
    pub const fn new() -> Self {
        Self {
            packed: AtomicU64::new(SILENCE_PAIR),
        }
    }

    pub fn store(&self, peak_db: f32, rms_db: f32) {
        self.packed.store(
            pack(sanitize_meter_db(peak_db), sanitize_meter_db(rms_db)),
            Ordering::Release,
        );
    }

    pub fn load(&self) -> (f32, f32) {
        unpack(self.packed.load(Ordering::Acquire))
    }

    pub fn reset(&self) {
        self.store(f32::NEG_INFINITY, f32::NEG_INFINITY);
    }
}

impl Default for BlockMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// Peak and RMS of one block.
///
/// Each channel is measured on its own. The result is the louder peak and
/// the louder RMS. An empty channel is silence and does not lower the other
/// side. Non-finite samples are left out of that channel's count.
pub fn block_peak_rms(left: &[f32], right: &[f32]) -> (f32, f32) {
    let (peak_l, rms_l) = channel_peak_rms(left);
    let (peak_r, rms_r) = channel_peak_rms(right);
    (
        db::linear_to_db(peak_l.max(peak_r)),
        db::linear_to_db(rms_l.max(rms_r)),
    )
}

/// Peak and RMS from a running sum of squares.
///
/// `frames` is the count for both channels. The mixer uses this so it does
/// not keep the block. Every frame it counted is finite. For a finite block
/// this matches [`block_peak_rms`].
pub(crate) fn levels_from_power(
    peak_l: f32,
    sum_l: f64,
    peak_r: f32,
    sum_r: f64,
    frames: usize,
) -> (f32, f32) {
    let count = frames as f64;
    (
        db::linear_to_db(finite_peak(peak_l).max(finite_peak(peak_r))),
        db::linear_to_db(rms_linear(sum_l, count).max(rms_linear(sum_r, count))),
    )
}

fn channel_peak_rms(samples: &[f32]) -> (f32, f32) {
    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    let mut count = 0.0f64;
    for sample in samples {
        if !sample.is_finite() {
            continue;
        }
        let abs = sample.abs();
        if abs > peak {
            peak = abs;
        }
        sum += f64::from(*sample) * f64::from(*sample);
        count += 1.0;
    }
    (peak, rms_linear(sum, count))
}

fn finite_peak(peak: f32) -> f32 {
    if peak.is_finite() && peak > 0.0 {
        peak
    } else {
        0.0
    }
}

fn rms_linear(sum_sq: f64, count: f64) -> f32 {
    if count <= 0.0 {
        return 0.0;
    }
    // A non-finite sum is an overflowed loud block, not silence.
    if !sum_sq.is_finite() {
        return f32::INFINITY;
    }
    if sum_sq <= 0.0 {
        return 0.0;
    }
    let mean = sum_sq / count;
    if !mean.is_finite() {
        return f32::INFINITY;
    }
    if mean <= 0.0 {
        return 0.0;
    }
    let rms = mean.sqrt();
    if rms.is_finite() {
        rms as f32
    } else {
        f32::INFINITY
    }
}

fn pack(peak: f32, rms: f32) -> u64 {
    (peak.to_bits() as u64) | ((rms.to_bits() as u64) << 32)
}

fn unpack(word: u64) -> (f32, f32) {
    (
        f32::from_bits(word as u32),
        f32::from_bits((word >> 32) as u32),
    )
}

fn sanitize_meter_db(db: f32) -> f32 {
    db::sanitize_db(db, f32::NEG_INFINITY, db::MAX_BOOST_DB, BelowMin::Clamp)
}

/// UI ballistics for one meter.
///
/// The audio callback does not call this. Peak snaps up, holds for
/// [`PEAK_HOLD_SECS`], then falls at [`PEAK_DECAY_DB_PER_SEC`] and does not
/// fall below the current block peak. RMS is the latest block value, with
/// no hold.
#[derive(Clone, Copy, Debug)]
pub struct MeterDisplay {
    peak_db: f32,
    rms_db: f32,
    hold_secs: f32,
}

impl MeterDisplay {
    pub fn new() -> Self {
        Self {
            peak_db: f32::NEG_INFINITY,
            rms_db: f32::NEG_INFINITY,
            hold_secs: 0.0,
        }
    }

    pub fn peak_db(&self) -> f32 {
        self.peak_db
    }

    pub fn rms_db(&self) -> f32 {
        self.rms_db
    }

    /// Step the display by `dt_secs`. A non-finite or negative `dt_secs` does
    /// not move the hold or the fall. A higher block peak still snaps up.
    pub fn update(&mut self, block_peak_db: f32, block_rms_db: f32, dt_secs: f32) {
        let block_peak = sanitize_meter_db(block_peak_db);
        let block_rms = sanitize_meter_db(block_rms_db);
        self.rms_db = block_rms;
        let dt = if dt_secs.is_finite() && dt_secs > 0.0 {
            dt_secs
        } else {
            0.0
        };
        // Equal peaks refresh the hold, so a steady tone does not fall.
        // −∞ >= −∞ is true, and refreshing silence is a no-op.
        if block_peak >= self.peak_db {
            self.peak_db = block_peak;
            self.hold_secs = PEAK_HOLD_SECS;
            return;
        }
        let mut fall = dt;
        if self.hold_secs > 0.0 {
            if fall <= self.hold_secs {
                self.hold_secs -= fall;
                return;
            }
            fall -= self.hold_secs;
            self.hold_secs = 0.0;
        }
        if fall > 0.0 && self.peak_db.is_finite() {
            let decayed = self.peak_db - PEAK_DECAY_DB_PER_SEC * fall;
            self.peak_db = sanitize_meter_db(decayed.max(block_peak));
        }
    }
}

impl Default for MeterDisplay {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(actual: f32, expected: f32, tol: f32) {
        assert!(
            (actual - expected).abs() <= tol,
            "{actual} != {expected} (tol {tol})"
        );
    }

    fn assert_minus_20_sine(peak: f32, rms: f32) {
        near(peak, -20.0, 0.1);
        let analytic = -20.0 - 10.0 * 2.0f32.log10();
        near(rms, analytic, 0.1);
        near(rms, -23.0, 0.1);
    }

    /// `cycles` periods of a −20 dBFS sine. 10 cycles in 480 frames is 1 kHz
    /// at 48 kHz. 10 cycles in 441 frames is 1 kHz at 44.1 kHz.
    fn minus_20_sine(frames: usize, cycles: f32) -> Vec<f32> {
        let amp = db::db_to_linear(-20.0);
        (0..frames)
            .map(|index| {
                let phase = cycles * index as f32 / frames as f32;
                (phase * std::f32::consts::TAU).sin() * amp
            })
            .collect()
    }

    #[test]
    fn meter_starts_at_silence_and_nan_store_is_silent_or_capped() {
        let meter = BlockMeter::new();
        let (peak, rms) = meter.load();
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert!(rms.is_infinite() && rms.is_sign_negative());
        assert_eq!(METER_BAR_FLOOR_DB, -60.0);
        assert!(peak < METER_BAR_FLOOR_DB);
        meter.store(f32::NAN, f32::INFINITY);
        let (peak, rms) = meter.load();
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert_eq!(rms, db::MAX_BOOST_DB);
        meter.reset();
        let (peak, rms) = meter.load();
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert!(rms.is_infinite() && rms.is_sign_negative());
    }

    #[test]
    fn minus_20_dbfs_sine_reads_minus_20_peak_and_about_minus_23_rms() {
        for (frames, cycles) in [(480, 10.0), (441, 10.0)] {
            let sine = minus_20_sine(frames, cycles);
            let (peak, rms) = block_peak_rms(&sine, &sine);
            assert_minus_20_sine(peak, rms);
            let zeros = vec![0.0f32; frames];
            let (hard, hard_rms) = block_peak_rms(&sine, &zeros);
            assert_minus_20_sine(hard, hard_rms);
        }
    }

    #[test]
    fn silence_and_non_finite_samples_do_not_read_as_full_scale() {
        let (peak, rms) = block_peak_rms(&[], &[]);
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert!(rms.is_infinite() && rms.is_sign_negative());
        let zeros = [0.0f32; 8];
        let (peak, rms) = block_peak_rms(&zeros, &zeros);
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert!(rms.is_infinite() && rms.is_sign_negative());
        let (peak, rms) = block_peak_rms(&[f32::NAN, 0.1], &[f32::INFINITY, f32::NEG_INFINITY]);
        near(peak, db::linear_to_db(0.1), 1.0e-4);
        near(rms, db::linear_to_db(0.1), 1.0e-4);
    }

    #[test]
    fn levels_from_power_matches_block_peak_rms_for_finite_samples() {
        let left = [0.1f32, -0.2, 0.0, 0.5, -0.0];
        let right = [-0.1f32, 0.0, 0.25, -0.5, 0.05];
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        let mut sum_l = 0.0f64;
        let mut sum_r = 0.0f64;
        for sample in left {
            let abs = sample.abs();
            if abs > peak_l {
                peak_l = abs;
            }
            sum_l += f64::from(sample) * f64::from(sample);
        }
        for sample in right {
            let abs = sample.abs();
            if abs > peak_r {
                peak_r = abs;
            }
            sum_r += f64::from(sample) * f64::from(sample);
        }
        let from_sums = levels_from_power(peak_l, sum_l, peak_r, sum_r, left.len());
        let from_buf = block_peak_rms(&left, &right);
        assert_eq!(from_sums.0.to_bits(), from_buf.0.to_bits());
        assert_eq!(from_sums.1.to_bits(), from_buf.1.to_bits());
    }

    #[test]
    fn display_holds_then_decays_and_rms_follows_the_block() {
        let mut display = MeterDisplay::new();
        assert!(display.peak_db().is_infinite());
        display.update(-20.0, -23.0, 0.0);
        near(display.peak_db(), -20.0, 1.0e-4);
        near(display.rms_db(), -23.0, 1.0e-4);

        display.update(f32::NEG_INFINITY, f32::NEG_INFINITY, 0.5);
        near(display.peak_db(), -20.0, 1.0e-4);
        assert!(display.rms_db().is_infinite() && display.rms_db().is_sign_negative());

        display.update(f32::NEG_INFINITY, f32::NEG_INFINITY, 0.5);
        near(display.peak_db(), -20.0, 1.0e-4);

        display.update(f32::NEG_INFINITY, f32::NEG_INFINITY, 0.5);
        near(display.peak_db(), -30.0, 1.0e-3);

        display.update(-10.0, -13.0, 0.2);
        near(display.peak_db(), -10.0, 1.0e-4);
        near(display.rms_db(), -13.0, 1.0e-4);
        display.update(f32::NEG_INFINITY, f32::NEG_INFINITY, 0.5);
        near(display.peak_db(), -10.0, 1.0e-4);

        display.update(-22.0, -25.0, 1.0 + 5.0);
        near(display.peak_db(), -22.0, 1.0e-3);
        near(display.rms_db(), -25.0, 1.0e-4);

        let mut steady = MeterDisplay::new();
        steady.update(-20.0, -23.0, 0.1);
        steady.update(-20.0, -23.0, 0.1);
        near(steady.peak_db(), -20.0, 1.0e-4);

        let mut stuck = MeterDisplay::new();
        stuck.update(-20.0, -23.0, 0.0);
        stuck.update(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NAN);
        near(stuck.peak_db(), -20.0, 1.0e-4);
    }

    #[test]
    fn sum_of_squares_around_1e19_does_not_read_as_silence() {
        let hot = [1.0e19f32; 8];
        let (peak, rms) = block_peak_rms(&hot, &hot);
        assert!(peak > 300.0, "{peak}");
        assert!(rms > 300.0, "{rms}");
        let meter = BlockMeter::new();
        meter.store(peak, rms);
        let (peak, rms) = meter.load();
        assert_eq!(peak, db::MAX_BOOST_DB);
        assert_eq!(rms, db::MAX_BOOST_DB);
    }

    #[test]
    fn load_never_pairs_peak_and_rms_from_different_blocks() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::thread;

        let meter = Arc::new(BlockMeter::new());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let meter = Arc::clone(&meter);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let pairs = [(-20.0f32, -23.0f32), (-6.0, -9.0)];
                while !stop.load(Ordering::Relaxed) {
                    for pair in pairs {
                        meter.store(pair.0, pair.1);
                    }
                }
            })
        };
        for _ in 0..50_000 {
            let (peak, rms) = meter.load();
            let pair = (peak.to_bits(), rms.to_bits());
            let silence = (f32::NEG_INFINITY.to_bits(), f32::NEG_INFINITY.to_bits());
            let low = ((-20.0f32).to_bits(), (-23.0f32).to_bits());
            let high = ((-6.0f32).to_bits(), (-9.0f32).to_bits());
            assert!(
                pair == silence || pair == low || pair == high,
                "torn snapshot peak {peak} rms {rms}"
            );
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }
}

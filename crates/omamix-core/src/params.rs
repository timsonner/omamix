//! Per-strip parameters and the realtime smoother.
//!
//! The control thread stores trim, fader, pan, mute, solo, and enable as
//! atomics. [`StripMixer::process`] loads them once per block and smooths per
//! sample. That function does not lock, allocate, or read the command channel.
//!
//! The smoother is a one-pole with a 10 ms time constant:
//!
//! ```text
//! coeff = 1 - exp(-1 / (tau * sample_rate))
//! y += coeff * (target - y)
//! ```
//!
//! `tau` is [`SMOOTH_TAU_SECS`]. The same seconds apply at 44.1 kHz and 48 kHz.
//! A unit step reaches `1 - 1/e` after one time constant. After five time
//! constants the residual is `exp(-5)` (about 0.67% of the step), which is
//! within 1% of the target distance.
//!
//! Gain is trim times fader, in linear amplitude, gated by mute, solo, and
//! enable. Pan is smoothed as a position in `-1..=1`, then the balance law in
//! [`crate::dsp::balance_gains`] is applied: left `min(1, 1 - pan)`, right
//! `min(1, 1 + pan)`. Center is gain 1 on both channels, so a default stereo
//! strip does not change the level. Neither side goes above 1. The law is
//! skipped once the smoothed pan has settled on the same value.
//!
//! Solo matches [`crate::graph::Strip::audible`]. A solo on an enabled strip
//! makes the other strips target silence, and mute still silences a soloed
//! strip. A disabled strip is bypassed: its gain target is 0, and its solo
//! flag is ignored, so it does not silence the others. Defaults are trim
//! 0 dB, fader 0 dB, pan center, not muted, not soloed, and enabled. Those
//! defaults are not a microphone level.
//!
//! For a constant input of amplitude `A` and a settled pan, a gain step from
//! `g0` to `g1` moves each output sample by at most
//! [`one_pole_max_jump`]`(coeff, g0, g1) * A`. That first one-pole step is the
//! click threshold the step test checks. A raw jump of `|g1 - g0| * A` would
//! be the click this ramp removes.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::db::{self, BelowMin};
use crate::dsp::balance_gains;
use crate::fader;
use crate::graph::{INPUT_GAIN_MAX_DB, INPUT_GAIN_MIN_DB};

/// One-pole time constant. About 10 ms.
pub const SMOOTH_TAU_SECS: f32 = 0.010;

/// Slots in the mix engine. Matches the filter's tap table.
pub const STRIP_SLOT_COUNT: usize = 64;

const FLAG_MUTE: u32 = 1 << 0;
const FLAG_SOLO: u32 = 1 << 1;
const FLAG_ENABLED: u32 = 1 << 2;

/// `1 - exp(-1 / (tau * sample_rate))`.
///
/// A non-positive `tau` or a zero sample rate snaps (`coeff` 1): the next
/// sample is the target. The result is finite and in `0..=1`.
pub fn one_pole_coeff(sample_rate: u32, tau_secs: f32) -> f32 {
    if sample_rate == 0 || !tau_secs.is_finite() || tau_secs <= 0.0 {
        return 1.0;
    }
    let denom = tau_secs * sample_rate as f32;
    if !denom.is_finite() || denom <= 0.0 {
        return 1.0;
    }
    let coeff = 1.0 - (-1.0 / denom).exp();
    clamp_coeff(coeff)
}

/// Largest `|y[n] - y[n-1]|` of a one-pole stepping from `from` toward `to`.
///
/// The first sample takes this whole step. Later samples are smaller.
pub fn one_pole_max_jump(coeff: f32, from: f32, to: f32) -> f32 {
    clamp_coeff(coeff) * (to - from).abs()
}

/// Samples that cover `time_constants` of [`SMOOTH_TAU_SECS`] at `sample_rate`.
///
/// Five constants is the 1% settle point (`exp(-5) < 0.01`). Four is not
/// (`exp(-4) > 0.01`).
pub fn settle_samples(sample_rate: u32, time_constants: f32) -> usize {
    let n = time_constants * SMOOTH_TAU_SECS * sample_rate as f32;
    if n.is_finite() && n > 0.0 {
        n.ceil() as usize
    } else {
        1
    }
}

/// Bound on a balance-law pan step, for a constant input.
///
/// `|d(left)/d(pan)|` and `|d(right)/d(pan)|` are at most 1 (the side that is
/// turning down). The pan one-pole itself moves at most `coeff * |to - from|`
/// on the first sample.
pub fn pan_step_max_jump(coeff: f32, from_pan: f32, to_pan: f32, gain: f32, amplitude: f32) -> f32 {
    one_pole_max_jump(coeff, from_pan, to_pan) * gain.abs() * amplitude.abs()
}

fn clamp_coeff(coeff: f32) -> f32 {
    if coeff.is_finite() {
        coeff.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// One-pole state. The realtime thread owns this. It is not an atomic.
#[derive(Clone, Copy, Debug)]
struct OnePole {
    y: f32,
    coeff: f32,
}

impl OnePole {
    fn new(initial: f32, coeff: f32) -> Self {
        Self {
            y: finite_or(initial, 0.0),
            coeff: clamp_coeff(coeff),
        }
    }

    fn set_coeff(&mut self, coeff: f32) {
        self.coeff = clamp_coeff(coeff);
    }

    fn reset(&mut self, value: f32) {
        self.y = finite_or(value, 0.0);
    }

    fn next(&mut self, target: f32) -> f32 {
        let target = finite_or(target, 0.0);
        let y = self.y + self.coeff * (target - self.y);
        // Flush denormals. The 1% settle point is far above this floor.
        let y = if y.is_finite() {
            if y.abs() < 1.0e-15 { 0.0 } else { y }
        } else {
            target
        };
        self.y = y;
        y
    }
}

/// What the control thread asks to store. Applied outside `process`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StripParamUpdate {
    TrimDb(f32),
    FaderDb(f32),
    Pan(f32),
    Mute(bool),
    Solo(bool),
    Enabled(bool),
}

/// One strip. Four atomics: trim dB, fader dB, pan, and the mute/solo/enable bits.
#[derive(Debug)]
struct AtomicStripParams {
    trim_db: AtomicU32,
    fader_db: AtomicU32,
    pan: AtomicU32,
    flags: AtomicU32,
}

impl AtomicStripParams {
    fn neutral() -> Self {
        Self {
            trim_db: AtomicU32::new(0.0f32.to_bits()),
            fader_db: AtomicU32::new(0.0f32.to_bits()),
            pan: AtomicU32::new(0.0f32.to_bits()),
            flags: AtomicU32::new(FLAG_ENABLED),
        }
    }

    fn set_trim_db(&self, db: f32) {
        let db = db::sanitize_db(db, INPUT_GAIN_MIN_DB, INPUT_GAIN_MAX_DB, BelowMin::Clamp);
        self.trim_db.store(db.to_bits(), Ordering::Release);
    }

    fn set_fader_db(&self, db: f32) {
        let db = fader::sanitize_fader_db(db);
        self.fader_db.store(db.to_bits(), Ordering::Release);
    }

    fn set_pan(&self, pan: f32) {
        let pan = if pan.is_finite() {
            pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        self.pan.store(pan.to_bits(), Ordering::Release);
    }

    fn set_flag(&self, bit: u32, on: bool) {
        if on {
            self.flags.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.flags.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    fn trim_db(&self) -> f32 {
        f32::from_bits(self.trim_db.load(Ordering::Acquire))
    }

    fn fader_db(&self) -> f32 {
        f32::from_bits(self.fader_db.load(Ordering::Acquire))
    }

    fn pan(&self) -> f32 {
        let pan = f32::from_bits(self.pan.load(Ordering::Acquire));
        if pan.is_finite() {
            pan.clamp(-1.0, 1.0)
        } else {
            0.0
        }
    }

    fn flag(&self, bit: u32) -> bool {
        self.flags.load(Ordering::Acquire) & bit != 0
    }
}

impl Default for AtomicStripParams {
    fn default() -> Self {
        Self::neutral()
    }
}

/// Lock-free strip parameters, indexed by mix slot.
///
/// The control thread calls [`Self::apply`]. The realtime thread only loads.
#[derive(Debug)]
pub struct StripParamBank {
    strips: [AtomicStripParams; STRIP_SLOT_COUNT],
}

impl StripParamBank {
    pub fn new() -> Self {
        Self {
            strips: std::array::from_fn(|_| AtomicStripParams::neutral()),
        }
    }

    /// Store one field. Returns false when `slot` is outside the bank.
    pub fn apply(&self, slot: u16, update: StripParamUpdate) -> bool {
        let Some(strip) = self.strips.get(slot as usize) else {
            return false;
        };
        match update {
            StripParamUpdate::TrimDb(db) => strip.set_trim_db(db),
            StripParamUpdate::FaderDb(db) => strip.set_fader_db(db),
            StripParamUpdate::Pan(pan) => strip.set_pan(pan),
            StripParamUpdate::Mute(on) => strip.set_flag(FLAG_MUTE, on),
            StripParamUpdate::Solo(on) => strip.set_flag(FLAG_SOLO, on),
            StripParamUpdate::Enabled(on) => strip.set_flag(FLAG_ENABLED, on),
        }
        true
    }

    /// Neutral defaults: 0 dB, center, audible, enabled.
    pub fn reset_slot(&self, slot: usize) {
        let Some(strip) = self.strips.get(slot) else {
            return;
        };
        strip.set_trim_db(0.0);
        strip.set_fader_db(0.0);
        strip.set_pan(0.0);
        strip.set_flag(FLAG_MUTE, false);
        strip.set_flag(FLAG_SOLO, false);
        strip.set_flag(FLAG_ENABLED, true);
    }

    pub fn trim_db(&self, slot: usize) -> f32 {
        self.strips
            .get(slot)
            .map(|strip| strip.trim_db())
            .unwrap_or(0.0)
    }

    pub fn fader_db(&self, slot: usize) -> f32 {
        self.strips
            .get(slot)
            .map(|strip| strip.fader_db())
            .unwrap_or(0.0)
    }

    pub fn pan(&self, slot: usize) -> f32 {
        self.strips
            .get(slot)
            .map(|strip| strip.pan())
            .unwrap_or(0.0)
    }

    pub fn mute(&self, slot: usize) -> bool {
        self.strips
            .get(slot)
            .is_some_and(|strip| strip.flag(FLAG_MUTE))
    }

    pub fn solo(&self, slot: usize) -> bool {
        self.strips
            .get(slot)
            .is_some_and(|strip| strip.flag(FLAG_SOLO))
    }

    /// Solo that should silence the other strips.
    ///
    /// A disabled strip is bypassed, so its solo flag does not count.
    pub fn solo_is_active(&self, slot: usize) -> bool {
        self.enabled(slot) && self.solo(slot)
    }

    pub fn enabled(&self, slot: usize) -> bool {
        self.strips
            .get(slot)
            .is_some_and(|strip| strip.flag(FLAG_ENABLED))
    }

    /// Linear amplitude the smoother aims at for this block.
    ///
    /// `any_solo` is the caller's scan of the slots included in the block.
    pub fn target_gain(&self, slot: usize, any_solo: bool) -> f32 {
        let Some(strip) = self.strips.get(slot) else {
            return 0.0;
        };
        let audible = strip.flag(FLAG_ENABLED)
            && !strip.flag(FLAG_MUTE)
            && (!any_solo || strip.flag(FLAG_SOLO));
        if !audible {
            return 0.0;
        }
        let gain = db::db_to_linear(strip.trim_db()) * db::db_to_linear(strip.fader_db());
        if gain.is_finite() { gain } else { 0.0 }
    }

    pub fn target_pan(&self, slot: usize) -> f32 {
        self.pan(slot)
    }
}

impl Default for StripParamBank {
    fn default() -> Self {
        Self::new()
    }
}

/// One stereo strip for one block. Empty slices read as silence.
#[derive(Clone, Copy, Debug)]
pub struct StripBlockInput<'a> {
    pub left: &'a [f32],
    pub right: &'a [f32],
}

/// Realtime smoother for every slot.
///
/// Only the thread that calls [`Self::process`] may touch this value.
#[derive(Debug)]
pub struct StripMixer {
    gains: [OnePole; STRIP_SLOT_COUNT],
    pans: [OnePole; STRIP_SLOT_COUNT],
    /// Pan the balance law was last evaluated at. NaN means unset.
    pan_key: [f32; STRIP_SLOT_COUNT],
    pan_left: [f32; STRIP_SLOT_COUNT],
    pan_right: [f32; STRIP_SLOT_COUNT],
    sample_rate: u32,
}

impl StripMixer {
    /// Smoothers start at unity gain and center pan, matching a neutral bank.
    pub fn new(sample_rate: u32) -> Self {
        let coeff = one_pole_coeff(sample_rate, SMOOTH_TAU_SECS);
        Self {
            gains: [OnePole::new(1.0, coeff); STRIP_SLOT_COUNT],
            pans: [OnePole::new(0.0, coeff); STRIP_SLOT_COUNT],
            pan_key: [f32::NAN; STRIP_SLOT_COUNT],
            pan_left: [1.0; STRIP_SLOT_COUNT],
            pan_right: [1.0; STRIP_SLOT_COUNT],
            sample_rate,
        }
    }

    /// Snap slots `0..slots` to the bank's current targets.
    ///
    /// The audio callback does not call this. A test uses it so a later step
    /// is measured from a settled value. Slot reuse resets through
    /// [`Self::reset_slot`] instead, back to the neutral start.
    pub fn align_to_targets(&mut self, bank: &StripParamBank, slots: usize) {
        let slots = slots.min(STRIP_SLOT_COUNT);
        let any_solo = (0..slots).any(|slot| bank.solo_is_active(slot));
        for slot in 0..slots {
            self.gains[slot].reset(bank.target_gain(slot, any_solo));
            self.pans[slot].reset(bank.target_pan(slot));
            self.pan_key[slot] = f32::NAN;
        }
    }

    /// Neutral gain and center pan. Used when a tap slot is freed.
    pub fn reset_slot(&mut self, slot: usize) {
        if slot >= STRIP_SLOT_COUNT {
            return;
        }
        self.gains[slot].reset(1.0);
        self.pans[slot].reset(0.0);
        self.pan_key[slot] = f32::NAN;
    }

    pub fn smoothed_gain(&self, slot: usize) -> f32 {
        self.gains.get(slot).map(|pole| pole.y).unwrap_or(0.0)
    }

    pub fn smoothed_pan(&self, slot: usize) -> f32 {
        self.pans.get(slot).map(|pole| pole.y).unwrap_or(0.0)
    }

    /// Mix `inputs` into `out_left` and `out_right`, overwriting those buffers.
    ///
    /// `inputs.len()` is how many slots participate, including holes (`None`
    /// still smooths). A solo bit counts only while that strip is enabled, so
    /// a disabled strip does not silence the others. Atomics are loaded once
    /// here. The one-pole then runs on every sample. No allocation.
    pub fn process(
        &mut self,
        bank: &StripParamBank,
        inputs: &[Option<StripBlockInput<'_>>],
        out_left: &mut [f32],
        out_right: &mut [f32],
        sample_rate: u32,
    ) {
        let frames = out_left.len().min(out_right.len());
        if frames == 0 {
            return;
        }
        self.ensure_rate(sample_rate);
        let slots = inputs.len().min(STRIP_SLOT_COUNT);

        let mut any_solo = false;
        for slot in 0..slots {
            if bank.solo_is_active(slot) {
                any_solo = true;
                break;
            }
        }

        let mut gain_target = [0.0f32; STRIP_SLOT_COUNT];
        let mut pan_target = [0.0f32; STRIP_SLOT_COUNT];
        for slot in 0..slots {
            gain_target[slot] = bank.target_gain(slot, any_solo);
            pan_target[slot] = bank.target_pan(slot);
        }

        for frame in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for slot in 0..slots {
                let gain = self.gains[slot].next(gain_target[slot]);
                let pan = self.pans[slot].next(pan_target[slot]);
                let (pan_l, pan_r) = self.balance_for(slot, pan);
                let (in_l, in_r) = match inputs[slot] {
                    Some(input) => (sample_at(input.left, frame), sample_at(input.right, frame)),
                    None => (0.0, 0.0),
                };
                left += in_l * gain * pan_l;
                right += in_r * gain * pan_r;
            }
            out_left[frame] = finite_or(left, 0.0);
            out_right[frame] = finite_or(right, 0.0);
        }
    }

    /// Balance gains for this pan. A settled pan skips the law.
    fn balance_for(&mut self, slot: usize, pan: f32) -> (f32, f32) {
        if self.pan_key[slot].to_bits() == pan.to_bits() {
            return (self.pan_left[slot], self.pan_right[slot]);
        }
        let (left, right) = balance_gains(pan);
        self.pan_key[slot] = pan;
        self.pan_left[slot] = left;
        self.pan_right[slot] = right;
        (left, right)
    }

    fn ensure_rate(&mut self, sample_rate: u32) {
        if self.sample_rate == sample_rate {
            return;
        }
        let coeff = one_pole_coeff(sample_rate, SMOOTH_TAU_SECS);
        self.sample_rate = sample_rate;
        for pole in self.gains.iter_mut() {
            pole.set_coeff(coeff);
        }
        for pole in self.pans.iter_mut() {
            pole.set_coeff(coeff);
        }
    }
}

impl Default for StripMixer {
    fn default() -> Self {
        Self::new(48_000)
    }
}

fn sample_at(buf: &[f32], frame: usize) -> f32 {
    if frame < buf.len() {
        finite_or(buf[frame], 0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JUMP_SLACK: f32 = 1.0e-5;
    const SETTLE_SLACK: f32 = 1.0e-4;

    fn near(actual: f32, expected: f32, tol: f32) {
        assert!(
            (actual - expected).abs() <= tol,
            "{actual} != {expected} (tol {tol})"
        );
    }

    fn hard_left(slots: usize) -> StripParamBank {
        let bank = StripParamBank::new();
        for slot in 0..slots {
            assert!(bank.apply(slot as u16, StripParamUpdate::Pan(-1.0)));
        }
        bank
    }

    fn process_constant(
        mixer: &mut StripMixer,
        bank: &StripParamBank,
        slots: usize,
        sample: f32,
        frames: usize,
        rate: u32,
    ) -> (Vec<f32>, Vec<f32>) {
        let frames = frames.max(1);
        let input = vec![sample; frames];
        let inputs: Vec<Option<StripBlockInput<'_>>> = (0..slots)
            .map(|_| {
                Some(StripBlockInput {
                    left: &input,
                    right: &input,
                })
            })
            .collect();
        let mut left = vec![0.0; frames];
        let mut right = vec![0.0; frames];
        mixer.process(bank, &inputs, &mut left, &mut right, rate);
        (left, right)
    }

    fn assert_jumps(samples: &[f32], threshold: f32) {
        for pair in samples.windows(2) {
            let jump = (pair[1] - pair[0]).abs();
            assert!(
                jump <= threshold,
                "sample step {jump} exceeds the one-pole threshold {threshold}"
            );
        }
    }

    fn assert_gain_step(rate: u32, update: StripParamUpdate, end_gain: f32) {
        let bank = hard_left(1);
        let mut mixer = StripMixer::new(rate);
        mixer.align_to_targets(&bank, 1);
        let (pre, _) = process_constant(&mut mixer, &bank, 1, 1.0, 1, rate);
        near(pre[0], 1.0, 1.0e-5);

        assert!(bank.apply(0, update));
        let n5 = settle_samples(rate, 5.0);
        let n4 = settle_samples(rate, 4.0);
        assert!(n5 > n4);
        let (block, right) = process_constant(&mut mixer, &bank, 1, 1.0, n5, rate);
        assert!(right.iter().all(|sample| sample.abs() <= 1.0e-5));

        let coeff = one_pole_coeff(rate, SMOOTH_TAU_SECS);
        let raw = (end_gain - 1.0).abs();
        let threshold = one_pole_max_jump(coeff, 1.0, end_gain) + JUMP_SLACK;
        assert!(
            raw > threshold,
            "the unsmoothed step {raw} should exceed the ramp threshold {threshold}"
        );
        let mut series = pre;
        series.extend_from_slice(&block);
        assert_jumps(&series, threshold);

        let span = (end_gain - 1.0).abs();
        let err4 = (block[n4 - 1] - end_gain).abs();
        let err5 = (block[n5 - 1] - end_gain).abs();
        assert!(
            err4 > 0.01 * span,
            "4 time constants should still be outside 1% ({err4} vs {span})"
        );
        assert!(
            err5 <= 0.01 * span + SETTLE_SLACK,
            "5 time constants should be within 1% ({err5} vs {span})"
        );
    }

    #[test]
    fn one_pole_coeff_matches_the_exponential_at_44k_and_48k() {
        for rate in [44_100_u32, 48_000] {
            let expected = 1.0 - (-1.0 / (SMOOTH_TAU_SECS * rate as f32)).exp();
            near(one_pole_coeff(rate, SMOOTH_TAU_SECS), expected, 1.0e-7);
            let five = settle_samples(rate, 5.0);
            let residual = (1.0 - one_pole_coeff(rate, SMOOTH_TAU_SECS)).powi(five as i32);
            assert!(residual < 0.01, "rate {rate} residual {residual}");
            assert!(residual > 0.005, "rate {rate} residual {residual}");
        }
        let c44 = one_pole_coeff(44_100, SMOOTH_TAU_SECS);
        let c48 = one_pole_coeff(48_000, SMOOTH_TAU_SECS);
        assert!(c44 > c48);
        assert_eq!(settle_samples(48_000, 5.0), 2_400);
        assert_eq!(settle_samples(44_100, 5.0), 2_205);
        assert_eq!(one_pole_coeff(0, SMOOTH_TAU_SECS), 1.0);
    }

    #[test]
    fn defaults_are_neutral() {
        let bank = StripParamBank::new();
        assert_eq!(bank.trim_db(0), 0.0);
        assert_eq!(bank.fader_db(0), 0.0);
        assert_eq!(bank.pan(0), 0.0);
        assert!(!bank.mute(0));
        assert!(!bank.solo(0));
        assert!(bank.enabled(0));
        near(bank.target_gain(0, false), 1.0, 1.0e-6);
        assert!(!bank.apply(u16::MAX, StripParamUpdate::Mute(true)));
    }

    #[test]
    fn non_finite_params_stay_finite_in_the_target() {
        let bank = StripParamBank::new();
        bank.apply(0, StripParamUpdate::TrimDb(f32::NAN));
        assert!(bank.trim_db(0).is_infinite() && bank.trim_db(0).is_sign_negative());
        assert_eq!(bank.target_gain(0, false), 0.0);
        bank.apply(0, StripParamUpdate::TrimDb(80.0));
        near(bank.trim_db(0), INPUT_GAIN_MAX_DB, 1.0e-6);
        bank.apply(0, StripParamUpdate::TrimDb(0.0));
        bank.apply(0, StripParamUpdate::FaderDb(f32::NAN));
        assert!(bank.fader_db(0).is_infinite() && bank.fader_db(0).is_sign_negative());
        assert_eq!(bank.target_gain(0, false), 0.0);
        bank.apply(0, StripParamUpdate::FaderDb(f32::INFINITY));
        near(bank.fader_db(0), fader::FADER_MAX_DB, 1.0e-6);
        bank.apply(0, StripParamUpdate::Pan(f32::NAN));
        assert_eq!(bank.pan(0), 0.0);
        bank.apply(0, StripParamUpdate::Pan(4.0));
        assert_eq!(bank.pan(0), 1.0);
        bank.apply(0, StripParamUpdate::Pan(-4.0));
        assert_eq!(bank.pan(0), -1.0);
    }

    #[test]
    fn unity_to_mute_and_gain_steps_stay_under_the_one_pole_jump() {
        for rate in [44_100, 48_000] {
            assert_gain_step(rate, StripParamUpdate::Mute(true), 0.0);
            assert_gain_step(rate, StripParamUpdate::Enabled(false), 0.0);
            assert_gain_step(
                rate,
                StripParamUpdate::FaderDb(fader::FADER_MAX_DB),
                db::db_to_linear(fader::FADER_MAX_DB),
            );
            assert_gain_step(rate, StripParamUpdate::TrimDb(6.0), db::db_to_linear(6.0));
        }
    }

    #[test]
    fn balance_pan_is_unity_at_center_and_never_above_one() {
        for pan in [-1.0, -0.5, 0.0, 0.25, 0.5, 1.0] {
            let (left, right) = balance_gains(pan);
            assert!(left <= 1.0 && right <= 1.0, "pan {pan} exceeded 1");
            assert!(left >= 0.0 && right >= 0.0, "pan {pan} went negative");
        }
        assert_eq!(balance_gains(0.0), (1.0, 1.0));
        assert_eq!(balance_gains(-1.0), (1.0, 0.0));
        assert_eq!(balance_gains(1.0), (0.0, 1.0));
        assert_eq!(balance_gains(0.5), (0.5, 1.0));
        for rate in [44_100, 48_000] {
            for pan in [-1.0, -0.5, 0.0, 0.25, 0.5, 1.0] {
                let bank = StripParamBank::new();
                bank.apply(0, StripParamUpdate::Pan(pan));
                let mut mixer = StripMixer::new(rate);
                mixer.align_to_targets(&bank, 1);
                let (left, right) = process_constant(&mut mixer, &bank, 1, 1.0, 8, rate);
                let (want_l, want_r) = balance_gains(pan);
                for index in 0..left.len() {
                    near(left[index], want_l, 1.0e-6);
                    near(right[index], want_r, 1.0e-6);
                    assert!(left[index] <= 1.0 + 1.0e-6);
                    assert!(right[index] <= 1.0 + 1.0e-6);
                }
            }
        }
    }

    #[test]
    fn default_bank_passes_a_stereo_block_through_unchanged() {
        let samples_l = [0.1f32, -0.25, 0.5, 1.0, -1.0, 0.0, 0.70710677, -0.003];
        let samples_r = [-0.1f32, 0.25, -0.5, 0.125, 0.5, -1.0, 0.0, 0.003];
        for rate in [44_100, 48_000] {
            let bank = StripParamBank::new();
            let mut mixer = StripMixer::new(rate);
            let inputs = [Some(StripBlockInput {
                left: &samples_l,
                right: &samples_r,
            })];
            let mut left = [0.0f32; 8];
            let mut right = [0.0f32; 8];
            mixer.process(&bank, &inputs, &mut left, &mut right, rate);
            assert_eq!(left, samples_l);
            assert_eq!(right, samples_r);
            mixer.process(&bank, &inputs, &mut left, &mut right, rate);
            assert_eq!(left, samples_l);
            assert_eq!(right, samples_r);
        }
    }

    #[test]
    fn pan_step_follows_the_balance_law_and_stays_under_the_bound() {
        for rate in [44_100, 48_000] {
            let bank = StripParamBank::new();
            let mut mixer = StripMixer::new(rate);
            mixer.align_to_targets(&bank, 1);
            let (pre_l, pre_r) = process_constant(&mut mixer, &bank, 1, 1.0, 1, rate);
            near(pre_l[0], 1.0, 1.0e-6);
            near(pre_r[0], 1.0, 1.0e-6);

            bank.apply(0, StripParamUpdate::Pan(1.0));
            let n5 = settle_samples(rate, 5.0);
            let (left, right) = process_constant(&mut mixer, &bank, 1, 1.0, n5, rate);
            let coeff = one_pole_coeff(rate, SMOOTH_TAU_SECS);
            let threshold = pan_step_max_jump(coeff, 0.0, 1.0, 1.0, 1.0) + JUMP_SLACK;
            let mut series_l = pre_l;
            let mut series_r = pre_r;
            series_l.extend_from_slice(&left);
            series_r.extend_from_slice(&right);
            assert_jumps(&series_l, threshold);
            assert_jumps(&series_r, threshold);
            for index in 0..left.len() {
                assert!(left[index] <= 1.0 + 1.0e-6);
                assert!(right[index] <= 1.0 + 1.0e-6);
            }
            near(left[n5 - 1], 0.0, 0.01 + SETTLE_SLACK);
            near(right[n5 - 1], 1.0, 1.0e-6);
        }
    }

    #[test]
    fn solo_mutes_the_other_strip_with_smoothing() {
        for rate in [44_100, 48_000] {
            let bank = hard_left(2);
            let mut mixer = StripMixer::new(rate);
            mixer.align_to_targets(&bank, 2);
            near(mixer.smoothed_gain(0), 1.0, 1.0e-6);
            near(mixer.smoothed_gain(1), 1.0, 1.0e-6);
            let (pre, _) = process_constant(&mut mixer, &bank, 2, 1.0, 1, rate);
            near(pre[0], 2.0, 1.0e-4);

            assert!(bank.apply(0, StripParamUpdate::Solo(true)));
            let n5 = settle_samples(rate, 5.0);
            let n4 = settle_samples(rate, 4.0);
            let (block, _) = process_constant(&mut mixer, &bank, 2, 1.0, n5, rate);
            let coeff = one_pole_coeff(rate, SMOOTH_TAU_SECS);
            let threshold = one_pole_max_jump(coeff, 1.0, 0.0) + JUMP_SLACK;
            let mut series = pre;
            series.extend_from_slice(&block);
            assert_jumps(&series, threshold);

            near(mixer.smoothed_gain(0), 1.0, 1.0e-3);
            assert!(
                (block[n4 - 1] - 1.0).abs() > 0.01,
                "the non-soloed strip should still contribute at 4 time constants"
            );
            assert!(
                mixer.smoothed_gain(1) < 0.01,
                "solo ramp residual {}",
                mixer.smoothed_gain(1)
            );
            near(block[n5 - 1], 1.0, 0.01 + SETTLE_SLACK);

            // A second solo keeps both strips. Mute still wins on the soloed one.
            bank.apply(1, StripParamUpdate::Solo(true));
            mixer.align_to_targets(&bank, 2);
            let (before_mute, _) = process_constant(&mut mixer, &bank, 2, 1.0, 1, rate);
            near(before_mute[0], 2.0, 1.0e-4);
            bank.apply(0, StripParamUpdate::Mute(true));
            let (muted, _) = process_constant(&mut mixer, &bank, 2, 1.0, n5, rate);
            let mut mute_series = before_mute;
            mute_series.extend_from_slice(&muted);
            assert_jumps(&mute_series, threshold);
            assert!(mixer.smoothed_gain(0) < 0.01);
            near(mixer.smoothed_gain(1), 1.0, 1.0e-3);
            near(muted[n5 - 1], 1.0, 0.01 + SETTLE_SLACK);
        }
    }

    #[test]
    fn a_disabled_solo_does_not_silence_the_other_strip() {
        for rate in [44_100, 48_000] {
            let bank = hard_left(2);
            assert!(bank.apply(0, StripParamUpdate::Enabled(false)));
            assert!(bank.apply(0, StripParamUpdate::Solo(true)));
            assert!(bank.solo(0));
            assert!(!bank.solo_is_active(0));
            assert!(!bank.solo_is_active(1));
            let mut mixer = StripMixer::new(rate);
            let n5 = settle_samples(rate, 5.0);
            let (block, _) = process_constant(&mut mixer, &bank, 2, 1.0, n5, rate);
            near(mixer.smoothed_gain(1), 1.0, 1.0e-4);
            assert!(
                mixer.smoothed_gain(0) < 0.01,
                "disabled strip residual {}",
                mixer.smoothed_gain(0)
            );
            near(block[n5 - 1], 1.0, 0.01 + SETTLE_SLACK);
        }
    }

    #[test]
    fn process_allocates_nothing() {
        for rate in [44_100, 48_000] {
            let bank = StripParamBank::new();
            bank.apply(0, StripParamUpdate::Solo(true));
            bank.apply(0, StripParamUpdate::FaderDb(-12.0));
            bank.apply(1, StripParamUpdate::Pan(0.3));
            let mut mixer = StripMixer::new(rate);
            let frames = 256;
            let input = vec![0.5f32; frames];
            let inputs = [
                Some(StripBlockInput {
                    left: input.as_slice(),
                    right: input.as_slice(),
                }),
                Some(StripBlockInput {
                    left: input.as_slice(),
                    right: input.as_slice(),
                }),
            ];
            let mut left = vec![0.0f32; frames];
            let mut right = vec![0.0f32; frames];
            mixer.process(&bank, &inputs, &mut left, &mut right, rate);

            crate::test_alloc::arm();
            mixer.process(&bank, &inputs, &mut left, &mut right, rate);
            let allocations = crate::test_alloc::disarm();
            let heard = left
                .iter()
                .chain(right.iter())
                .any(|sample| sample.abs() > 1.0e-6);
            std::hint::black_box((&left, &right, &mixer, heard));
            assert!(
                heard,
                "process produced silence at {rate}, so a no-op would pass"
            );
            assert_eq!(
                allocations, 0,
                "process allocated {allocations} times at {rate}"
            );
        }
    }

    #[test]
    fn counting_allocator_sees_a_deliberate_allocation() {
        crate::test_alloc::arm();
        let buffer = vec![1u8; 64];
        std::hint::black_box(&buffer);
        let allocations = crate::test_alloc::disarm();
        assert!(allocations > 0, "allocator did not count");
    }
}

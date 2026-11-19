//! High-pass and 10-band graphic EQ.
//!
//! Centers are the ISO 266 octave bands from 31.5 Hz to 16 kHz. Each band is
//! an RBJ peaking section with a bandwidth of one octave, limited to
//! +/-12 dB. The high-pass is an RBJ high-pass with Butterworth Q. Its
//! cutoff starts at [`HPF_DEFAULT_HZ`] and it starts disabled. 80 Hz is a
//! generic mixer default, not a measured microphone or a measured room.
//!
//! Defaults are flat: every band is 0 dB and the high-pass is off. A flat
//! block is an identity bypass, so a default strip does not change samples.
//!
//! The control thread builds coefficients in `f64` and publishes the whole
//! block by swapping an index in a triple buffer. [`EqWriter`] is the only
//! producer. It is not `Clone` and not `Sync`, so safe code cannot publish
//! from two threads. The audio callback holds [`EqReader`] exclusively and
//! only loads that block. The consumer index lives on the reader, and
//! [`crate::params::StripMixer::process`] takes `&mut EqReader`, so two
//! mixers cannot share one. The callback does not build coefficients,
//! allocate, or take a lock.
//! A newly published block is linearly ramped across [`EQ_RAMP_SAMPLES`]
//! samples, including when the quantum is shorter than that. The last sample
//! of the ramp uses the new coefficients.
//!
//! A settled block skips a section whose coefficients are identity. That
//! section's delay is cleared when the skip mask is built. The mask stays
//! off for the whole ramp, including the sample that finishes it, so every
//! section still runs while coefficients are moving.
//!
//! Each band is a peaking section, not a low shelf and not a high shelf.
//! Centers near Nyquist (the 8 kHz and 16 kHz bands) use the RBJ bilinear
//! bandwidth warp, so the skirts are not symmetric in hertz. A center at or
//! above [`NYQUIST_MARGIN`] times Nyquist is disabled: the section is
//! identity, and [`EqSettings::band_disabled`] reports it. At 32 kHz the
//! 16 kHz band is disabled. Coefficients stay `f64` because an `f32` pole
//! at 31.5 Hz drifts at high sample rates.
//!
//! [`response_db`] is the pure magnitude of a setting block. The EQ widget
//! can call it without touching the audio callback.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::params::STRIP_SLOT_COUNT;

/// Octave-band centers, ISO 266, 31.5 Hz through 16 kHz.
pub const EQ_BAND_HZ: [f32; EQ_BANDS] = [
    31.5, 63.0, 125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
];

pub const EQ_BANDS: usize = 10;

/// Lowest and highest band gain. A non-finite request becomes 0 dB.
pub const EQ_GAIN_MIN_DB: f32 = -12.0;
pub const EQ_GAIN_MAX_DB: f32 = 12.0;

/// Bandwidth of each peaking section, in octaves.
pub const EQ_BANDWIDTH_OCTAVES: f32 = 1.0;

/// Cutoff used when the high-pass is switched on and no other value was set.
pub const HPF_DEFAULT_HZ: f32 = 80.0;
pub const HPF_MIN_HZ: f32 = 20.0;
pub const HPF_MAX_HZ: f32 = 400.0;

/// Butterworth Q. The cutoff is then -3 dB.
pub const HPF_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// A band whose center is at or above this fraction of Nyquist is identity.
///
/// 0.99 disables the 16 kHz band at 32 kHz (Nyquist is 16 kHz) and leaves it
/// enabled at 44.1 kHz and above. The 8 kHz and 16 kHz bells that do run are
/// still warped by the RBJ bilinear map. That warp is not a shelf.
pub const NYQUIST_MARGIN: f32 = 0.99;

/// Samples a coefficient change takes. A short quantum does not shorten it.
///
/// 256 samples is about 5.3 ms at 48 kHz and about 5.8 ms at 44.1 kHz. It is
/// a generic crossfade, not a measured click threshold from a particular machine.
pub const EQ_RAMP_SAMPLES: u32 = 256;

const SAMPLE_RATE_MIN: u32 = 8_000;
const SAMPLE_RATE_MAX: u32 = 384_000;

const SECTIONS: usize = 1 + EQ_BANDS;
const _: () = assert!(SECTIONS <= 16);
const COEFFS_PER: usize = 5;
const PACK: usize = SECTIONS * COEFFS_PER;

/// Producer's first buffer. The consumer starts on 1. The spare starts at 2.
const INITIAL_WRITE: u32 = 0;
const INITIAL_READ: u32 = 1;
const INITIAL_SPARE: u32 = 2;
const DIRTY: u32 = 0b100;

/// What the control thread asks to store. Applied outside `process`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EqUpdate {
    Band {
        index: u8,
        gain_db: f32,
    },
    HighPassEnabled(bool),
    HighPassHz(f32),
    /// 0 dB bands, high-pass off, cutoff back to [`HPF_DEFAULT_HZ`].
    Reset,
}

/// One EQ snapshot the widget or a test can turn into coefficients.
///
/// Fields are private so a gain outside +/-12 dB cannot be stored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqSettings {
    sample_rate: u32,
    hpf_enabled: bool,
    hpf_hz: f32,
    band_db: [f32; EQ_BANDS],
}

impl EqSettings {
    /// Flat bypass at `sample_rate`. An unusable rate becomes 48 kHz.
    pub fn flat(sample_rate: u32) -> Self {
        Self {
            sample_rate: usable_rate(sample_rate).unwrap_or(48_000),
            hpf_enabled: false,
            hpf_hz: HPF_DEFAULT_HZ,
            band_db: [0.0; EQ_BANDS],
        }
    }

    pub fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    pub fn hpf_enabled(self) -> bool {
        self.hpf_enabled
    }

    pub fn hpf_hz(self) -> f32 {
        self.hpf_hz
    }

    pub fn band_db(self) -> [f32; EQ_BANDS] {
        self.band_db
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) -> bool {
        match usable_rate(sample_rate) {
            Some(rate) => {
                self.sample_rate = rate;
                true
            }
            None => false,
        }
    }

    pub fn set_hpf_enabled(&mut self, enabled: bool) {
        self.hpf_enabled = enabled;
    }

    pub fn set_hpf_hz(&mut self, hz: f32) {
        self.hpf_hz = sanitize_hz(hz);
    }

    /// Store one band. Returns false when `index` is not a band.
    pub fn set_band(&mut self, index: usize, gain_db: f32) -> bool {
        let Some(slot) = self.band_db.get_mut(index) else {
            return false;
        };
        *slot = sanitize_band(gain_db);
        true
    }

    /// Magnitude at `freq_hz`, in dB. No audio and no shared state.
    pub fn response_db(self, freq_hz: f32) -> f32 {
        response_db(self, freq_hz)
    }

    /// True when band `index` is forced to identity.
    ///
    /// The center is at or above [`NYQUIST_MARGIN`] times Nyquist, or `index`
    /// is not a band. A 0 dB band is not disabled: it is a flat peaking
    /// section the user can still move.
    pub fn band_disabled(self, index: usize) -> bool {
        match EQ_BAND_HZ.get(index) {
            Some(&freq) => center_disabled(self.sample_rate, freq),
            None => true,
        }
    }

    pub fn coefficients(self) -> EqCoeffBlock {
        let mut block = EqCoeffBlock::bypass(self.sample_rate);
        if self.hpf_enabled {
            block.hpf = highpass(self.sample_rate, self.hpf_hz, HPF_Q);
        }
        for (index, gain) in self.band_db.iter().enumerate() {
            block.bands[index] = peaking(self.sample_rate, EQ_BAND_HZ[index], *gain);
        }
        block
    }
}

/// Pure magnitude, in dB, of `settings` at `freq_hz`.
///
/// This is the curve the EQ widget draws. It does not read atomics and it
/// does not run the filter.
pub fn response_db(settings: EqSettings, freq_hz: f32) -> f32 {
    magnitude_db(&settings.coefficients(), freq_hz)
}

/// One biquad, normalized (`a0` is 1). Stored in `f64` so a 31.5 Hz pole
/// still lands at high sample rates. The audio path casts the sample to `f32`
/// after the cascade.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiquadCoeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl BiquadCoeffs {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    fn is_identity(self) -> bool {
        self.b0 == 1.0 && self.b1 == 0.0 && self.b2 == 0.0 && self.a1 == 0.0 && self.a2 == 0.0
    }

    fn is_finite(self) -> bool {
        self.b0.is_finite()
            && self.b1.is_finite()
            && self.b2.is_finite()
            && self.a1.is_finite()
            && self.a2.is_finite()
    }
}

/// High-pass plus ten peaking sections, normalized (`a0` is 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqCoeffBlock {
    pub hpf: BiquadCoeffs,
    pub bands: [BiquadCoeffs; EQ_BANDS],
    pub sample_rate: u32,
}

impl EqCoeffBlock {
    fn bypass(sample_rate: u32) -> Self {
        Self {
            hpf: BiquadCoeffs::IDENTITY,
            bands: [BiquadCoeffs::IDENTITY; EQ_BANDS],
            sample_rate,
        }
    }

    fn is_bypass(self) -> bool {
        self.hpf.is_identity() && self.bands.iter().all(|band| band.is_identity())
    }

    fn pack(self) -> [f64; PACK] {
        let mut out = [0.0; PACK];
        write_section(&mut out, 0, self.hpf);
        for (index, band) in self.bands.iter().enumerate() {
            write_section(&mut out, index + 1, *band);
        }
        out
    }

    #[cfg(test)]
    fn stamp_uniform(&self) -> Option<u64> {
        let bits = self.hpf.b0.to_bits();
        let same = self.hpf.b1.to_bits() == bits
            && self.hpf.b2.to_bits() == bits
            && self.hpf.a1.to_bits() == bits
            && self.hpf.a2.to_bits() == bits
            && self.bands.iter().all(|band| {
                band.b0.to_bits() == bits
                    && band.b1.to_bits() == bits
                    && band.b2.to_bits() == bits
                    && band.a1.to_bits() == bits
                    && band.a2.to_bits() == bits
            });
        if same { Some(bits) } else { None }
    }
}

fn write_section(pack: &mut [f64; PACK], index: usize, coeffs: BiquadCoeffs) {
    let at = index * COEFFS_PER;
    pack[at] = coeffs.b0;
    pack[at + 1] = coeffs.b1;
    pack[at + 2] = coeffs.b2;
    pack[at + 3] = coeffs.a1;
    pack[at + 4] = coeffs.a2;
}

fn identity_pack() -> [f64; PACK] {
    EqCoeffBlock::bypass(48_000).pack()
}

fn is_bypass(pack: &[f64; PACK]) -> bool {
    pack.chunks_exact(COEFFS_PER).all(|section| {
        section[0] == 1.0
            && section[1] == 0.0
            && section[2] == 0.0
            && section[3] == 0.0
            && section[4] == 0.0
    })
}

fn lerp(from: [f64; PACK], to: [f64; PACK], t: f64) -> [f64; PACK] {
    let mut out = [0.0; PACK];
    for index in 0..PACK {
        out[index] = from[index] + (to[index] - from[index]) * t;
    }
    out
}

#[derive(Debug)]
struct EqShared {
    strips: [EqChannel; STRIP_SLOT_COUNT],
    master: EqChannel,
    sample_rate: AtomicU32,
}

/// The only producer. Not `Clone`. Not `Sync`.
///
/// [`open`] returns this and the [`EqReader`] the audio callback loads.
/// Publishing takes `&mut self`, so two callers cannot publish at once, and
/// the type cannot be shared across threads.
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::eq::EqWriter>();
/// }
/// ```
///
/// ```compile_fail
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::eq::EqWriter>();
/// }
/// ```
#[derive(Debug)]
pub struct EqWriter {
    inner: Arc<EqShared>,
    _unshared: PhantomData<Cell<()>>,
}

/// Coefficient publications for the audio callback.
///
/// Not `Clone` and not `Sync`. The consumer index for each channel lives
/// here. [`crate::params::StripMixer::process`] takes `&mut EqReader`, so
/// two mixers cannot hold one reader at once, and a second mixer cannot
/// keep its own index. A second index would read the slot the producer is
/// filling. This handle does not publish.
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::eq::EqReader>();
/// }
/// ```
///
/// ```compile_fail
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::eq::EqReader>();
/// }
/// ```
///
/// ```compile_fail
/// fn demo() {
///     let mut reader = omamix_core::eq::EqReader::flat();
///     let mut a = omamix_core::params::StripMixer::new(48_000);
///     let mut b = omamix_core::params::StripMixer::new(48_000);
///     let bank = omamix_core::params::StripParamBank::new();
///     let meters = omamix_core::params::StripMeterBank::new();
///     let mut scopes = omamix_core::scope::ScopeBank::new();
///     let mut comp = omamix_core::comp::CompReader::bypassed();
///     let mut limit = omamix_core::limit::LimitReader::bypassed();
///     let mut left = [0.0f32; 8];
///     let mut right = [0.0f32; 8];
///     let shared = &reader;
///     a.process(
///         &bank,
///         shared,
///         &mut comp,
///         &mut limit,
///         &meters,
///         &mut scopes,
///         &[],
///         &mut left,
///         &mut right,
///         48_000,
///     );
///     b.process(
///         &bank,
///         shared,
///         &mut comp,
///         &mut limit,
///         &meters,
///         &mut scopes,
///         &[],
///         &mut left,
///         &mut right,
///         48_000,
///     );
/// }
/// ```
#[derive(Debug)]
pub struct EqReader {
    inner: Arc<EqShared>,
    /// One consumer index per strip. Not stored on [`EqRuntime`].
    read_idx: [u32; STRIP_SLOT_COUNT],
    master_read_idx: u32,
    _unshared: PhantomData<Cell<()>>,
}

/// One producer and one consumer. Dropping either does not free the other.
pub fn open() -> (EqWriter, EqReader) {
    let inner = Arc::new(EqShared {
        strips: std::array::from_fn(|_| EqChannel::new()),
        master: EqChannel::new(),
        sample_rate: AtomicU32::new(48_000),
    });
    (
        EqWriter {
            inner: Arc::clone(&inner),
            _unshared: PhantomData,
        },
        EqReader {
            inner,
            read_idx: [INITIAL_READ; STRIP_SLOT_COUNT],
            master_read_idx: INITIAL_READ,
            _unshared: PhantomData,
        },
    )
}

impl EqWriter {
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate.load(Ordering::Acquire)
    }

    /// Rebuild every publication at `sample_rate`.
    ///
    /// Returns false when `sample_rate` is outside 8 kHz..384 kHz. A repeat
    /// of the current rate does not rebuild. The audio callback does not
    /// call this.
    pub fn set_sample_rate(&mut self, sample_rate: u32) -> bool {
        let Some(rate) = usable_rate(sample_rate) else {
            return false;
        };
        let prev = self.inner.sample_rate.swap(rate, Ordering::AcqRel);
        if prev != rate {
            self.republish_all();
        }
        true
    }

    /// Store one field and publish that strip's coefficients.
    ///
    /// Returns false when `slot` or a band index is out of range.
    pub fn apply(&mut self, slot: u16, update: EqUpdate) -> bool {
        let rate = self.sample_rate();
        let Some(channel) = self.inner.strips.get(slot as usize) else {
            return false;
        };
        store_and_publish(channel, update, rate)
    }

    pub fn apply_master(&mut self, update: EqUpdate) -> bool {
        let rate = self.sample_rate();
        store_and_publish(&self.inner.master, update, rate)
    }

    /// Replace one strip's gains and high-pass, then publish once.
    ///
    /// Coefficients use this writer's sample rate, not `settings.sample_rate`.
    pub fn apply_settings(&mut self, slot: u16, settings: EqSettings) -> bool {
        let Some(channel) = self.inner.strips.get(slot as usize) else {
            return false;
        };
        channel.store_settings(settings);
        channel.publish_at(self.sample_rate());
        true
    }

    pub fn apply_master_settings(&mut self, settings: EqSettings) {
        self.inner.master.store_settings(settings);
        self.inner.master.publish_at(self.sample_rate());
    }

    pub fn settings(&self, slot: usize) -> Option<EqSettings> {
        self.inner
            .strips
            .get(slot)
            .map(|channel| channel_settings(self.sample_rate(), channel))
    }

    pub fn master_settings(&self) -> EqSettings {
        channel_settings(self.sample_rate(), &self.inner.master)
    }

    /// Flat gains, high-pass off, and a bypass publication.
    ///
    /// Call this off the audio callback. It builds the bypass block here.
    /// The callback only clears filter memory.
    pub fn reset_slot(&mut self, slot: usize) {
        if let Some(channel) = self.inner.strips.get(slot) {
            channel.store_settings(EqSettings::flat(self.sample_rate()));
            channel.publish_at(self.sample_rate());
        }
    }

    fn republish_all(&mut self) {
        let rate = self.sample_rate();
        for channel in &self.inner.strips {
            channel.publish_at(rate);
        }
        self.inner.master.publish_at(rate);
    }
}

impl EqReader {
    /// A flat reader whose producer has been dropped. Nothing can publish.
    pub fn flat() -> Self {
        open().1
    }

    /// Copy a newly published strip block into `dst`.
    ///
    /// The consumer index for `slot` stays on this reader. Returns false
    /// when nothing new is waiting, or when `slot` is out of range. A failed
    /// poll leaves `dst` unchanged.
    fn poll_strip(&mut self, slot: usize, dst: &mut EqCoeffBlock) -> bool {
        let Some(channel) = self.inner.strips.get(slot) else {
            return false;
        };
        let Some(read_idx) = self.read_idx.get_mut(slot) else {
            return false;
        };
        channel.poll(read_idx, dst)
    }

    fn poll_master(&mut self, dst: &mut EqCoeffBlock) -> bool {
        self.inner.master.poll(&mut self.master_read_idx, dst)
    }
}

fn store_and_publish(channel: &EqChannel, update: EqUpdate, rate: u32) -> bool {
    if !channel.store(update) {
        return false;
    }
    channel.publish_at(rate);
    true
}

fn channel_settings(sample_rate: u32, channel: &EqChannel) -> EqSettings {
    let mut settings = EqSettings::flat(sample_rate);
    settings.hpf_enabled = channel.hpf_enabled.load(Ordering::Acquire);
    settings.hpf_hz = f32::from_bits(channel.hpf_hz.load(Ordering::Acquire));
    if !settings.hpf_hz.is_finite() {
        settings.hpf_hz = HPF_DEFAULT_HZ;
    }
    for (index, band) in channel.bands.iter().enumerate() {
        settings.band_db[index] = sanitize_band(f32::from_bits(band.load(Ordering::Acquire)));
    }
    settings
}

/// Filter memory for every strip and the master.
///
/// The consumer index lives on [`EqReader`]. Only the thread that calls
/// [`crate::params::StripMixer::process`] may touch this value.
#[derive(Debug)]
pub struct EqRuntime {
    strips: [StripEqRt; STRIP_SLOT_COUNT],
    master: StripEqRt,
}

impl EqRuntime {
    pub fn new() -> Self {
        Self {
            strips: std::array::from_fn(|_| StripEqRt::new()),
            master: StripEqRt::new(),
        }
    }

    pub(crate) fn begin(
        &mut self,
        reader: &mut EqReader,
        slots: usize,
        frames: usize,
        sample_rate: u32,
    ) {
        let slots = slots.min(STRIP_SLOT_COUNT);
        for slot in 0..slots {
            let mut loaded = EqCoeffBlock::bypass(sample_rate);
            let fresh = reader.poll_strip(slot, &mut loaded);
            self.strips[slot].begin(fresh, loaded, frames, sample_rate);
        }
        let mut loaded = EqCoeffBlock::bypass(sample_rate);
        let fresh = reader.poll_master(&mut loaded);
        self.master.begin(fresh, loaded, frames, sample_rate);
    }

    pub(crate) fn tick(&mut self, slot: usize, frame: usize, left: f32, right: f32) -> (f32, f32) {
        match self.strips.get_mut(slot) {
            Some(strip) => strip.tick_one(frame, left, right),
            None => (left, right),
        }
    }

    pub(crate) fn apply_master(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.master.process_begun(left, right);
    }

    /// Identity coefficients and cleared filter memory.
    ///
    /// The consumer index stays on [`EqReader`]. Resetting it would alias a
    /// buffer the producer still owns.
    pub(crate) fn reset_slot(&mut self, slot: usize) {
        if let Some(strip) = self.strips.get_mut(slot) {
            strip.reset_filter();
        }
    }

    #[cfg(test)]
    fn process_slot(
        &mut self,
        reader: &mut EqReader,
        slot: usize,
        left: &mut [f32],
        right: &mut [f32],
        sample_rate: u32,
    ) {
        let frames = left.len().min(right.len());
        if frames == 0 || slot >= STRIP_SLOT_COUNT {
            return;
        }
        let mut loaded = EqCoeffBlock::bypass(sample_rate);
        let fresh = reader.poll_strip(slot, &mut loaded);
        self.strips[slot].begin(fresh, loaded, frames, sample_rate);
        self.strips[slot].process_begun(left, right);
    }

    #[cfg(test)]
    fn process_master(
        &mut self,
        reader: &mut EqReader,
        left: &mut [f32],
        right: &mut [f32],
        sample_rate: u32,
    ) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }
        let mut loaded = EqCoeffBlock::bypass(sample_rate);
        let fresh = reader.poll_master(&mut loaded);
        self.master.begin(fresh, loaded, frames, sample_rate);
        self.master.process_begun(left, right);
    }

    #[cfg(test)]
    fn clear_memory(&mut self, slot: usize) {
        if let Some(strip) = self.strips.get_mut(slot) {
            strip.clear_state();
        }
    }

    #[cfg(test)]
    fn strip_bypassed(&self, slot: usize) -> bool {
        self.strips.get(slot).is_none_or(|strip| strip.bypass)
    }

    #[cfg(test)]
    fn master_bypassed(&self) -> bool {
        self.master.bypass
    }

    #[cfg(test)]
    fn ramping(&self, slot: usize) -> bool {
        self.strips.get(slot).is_some_and(|strip| strip.ramping)
    }

    #[cfg(test)]
    fn section_state(&self, slot: usize, section: usize) -> f64 {
        self.strips.get(slot).map_or(0.0, |strip| {
            strip
                .left
                .get(section)
                .map(|state| state.s1.abs() + state.s2.abs())
                .unwrap_or(0.0)
        })
    }
}

impl Default for EqRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct EqChannel {
    bands: [AtomicU32; EQ_BANDS],
    hpf_enabled: AtomicBool,
    hpf_hz: AtomicU32,
    slots: [UnsafeCell<EqCoeffBlock>; 3],
    /// Spare index in bits 0..2. Bit 2 is set when a block is waiting.
    state: AtomicU32,
    write_idx: AtomicU32,
}

impl EqChannel {
    fn new() -> Self {
        Self {
            bands: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            hpf_enabled: AtomicBool::new(false),
            hpf_hz: AtomicU32::new(HPF_DEFAULT_HZ.to_bits()),
            slots: std::array::from_fn(|_| UnsafeCell::new(EqCoeffBlock::bypass(48_000))),
            state: AtomicU32::new(INITIAL_SPARE),
            write_idx: AtomicU32::new(INITIAL_WRITE),
        }
    }

    fn store(&self, update: EqUpdate) -> bool {
        match update {
            EqUpdate::Band { index, gain_db } => {
                let Some(slot) = self.bands.get(index as usize) else {
                    return false;
                };
                slot.store(sanitize_band(gain_db).to_bits(), Ordering::Release);
                true
            }
            EqUpdate::HighPassEnabled(on) => {
                self.hpf_enabled.store(on, Ordering::Release);
                true
            }
            EqUpdate::HighPassHz(hz) => {
                self.hpf_hz
                    .store(sanitize_hz(hz).to_bits(), Ordering::Release);
                true
            }
            EqUpdate::Reset => {
                self.store_settings(EqSettings::flat(48_000));
                true
            }
        }
    }

    fn store_settings(&self, settings: EqSettings) {
        for (slot, gain) in self.bands.iter().zip(settings.band_db) {
            slot.store(sanitize_band(gain).to_bits(), Ordering::Release);
        }
        self.hpf_enabled
            .store(settings.hpf_enabled, Ordering::Release);
        self.hpf_hz
            .store(sanitize_hz(settings.hpf_hz).to_bits(), Ordering::Release);
    }

    fn publish_at(&self, sample_rate: u32) {
        let mut settings = EqSettings::flat(sample_rate);
        settings.hpf_enabled = self.hpf_enabled.load(Ordering::Acquire);
        settings.hpf_hz = sanitize_hz(f32::from_bits(self.hpf_hz.load(Ordering::Acquire)));
        for (index, band) in self.bands.iter().enumerate() {
            settings.band_db[index] = sanitize_band(f32::from_bits(band.load(Ordering::Acquire)));
        }
        self.publish_block(settings.coefficients());
    }

    /// Publish `block` and take the previous spare index as the next write.
    fn publish_block(&self, block: EqCoeffBlock) {
        let write_idx = self.write_idx.load(Ordering::Relaxed);
        if write_idx > 2 {
            return;
        }
        unsafe {
            *self.slots[write_idx as usize].get() = block;
        }
        let prev = self.state.swap(write_idx | DIRTY, Ordering::AcqRel);
        self.write_idx.store(prev & 0b11, Ordering::Relaxed);
    }

    /// Copy a newly published block. `read_idx` is the consumer's buffer.
    ///
    /// Returns false when nothing new is waiting, or when the spare index
    /// would alias the consumer. A failed claim leaves `dst` unchanged.
    fn poll(&self, read_idx: &mut u32, dst: &mut EqCoeffBlock) -> bool {
        let state = self.state.load(Ordering::Acquire);
        if !claim(self, state, read_idx, dst) {
            let state = self.state.load(Ordering::Acquire);
            return claim(self, state, read_idx, dst);
        }
        true
    }
}

fn claim(channel: &EqChannel, state: u32, read_idx: &mut u32, dst: &mut EqCoeffBlock) -> bool {
    if state & DIRTY == 0 {
        return false;
    }
    let spare = state & 0b11;
    if spare > 2 || spare == *read_idx {
        return false;
    }
    if channel
        .state
        .compare_exchange(state, *read_idx, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    *read_idx = spare;
    unsafe {
        *dst = *channel.slots[spare as usize].get();
    }
    true
}

// SAFETY: [`EqWriter`] is the only producer. It is not `Clone` and not `Sync`,
// and publish takes `&mut self`, so safe code cannot publish from two threads.
// The consumer index lives on [`EqReader`], one per channel. `EqReader` is not
// `Clone` and not `Sync`, and `poll_strip`, `poll_master`, and
// [`crate::params::StripMixer::process`] take `&mut EqReader`. Two mixers
// cannot hold one reader at once, and a second mixer cannot keep its own
// index. The producer writes a slot only while it owns `write_idx`. The
// consumer reads a slot only after `poll` has swapped that index out of the
// spare field. Those two indices are distinct, so the cells are not aliased.
// The gain atomics are synchronized on their own.
unsafe impl Sync for EqChannel {}

#[derive(Debug)]
struct StripEqRt {
    current: [f64; PACK],
    target: [f64; PACK],
    origin: [f64; PACK],
    left: [BiquadState; SECTIONS],
    right: [BiquadState; SECTIONS],
    block_frames: u32,
    /// Samples already taken in the current crossfade.
    ramp_pos: u32,
    ramping: bool,
    bypass: bool,
    /// Settled sections that are not identity. Ignored while `mask_on` is false.
    live: u16,
    /// When true, the cascade skips sections missing from `live`.
    mask_on: bool,
}

impl StripEqRt {
    fn new() -> Self {
        let identity = identity_pack();
        Self {
            current: identity,
            target: identity,
            origin: identity,
            left: [BiquadState::new(); SECTIONS],
            right: [BiquadState::new(); SECTIONS],
            block_frames: 1,
            ramp_pos: 0,
            ramping: false,
            bypass: true,
            live: 0,
            mask_on: false,
        }
    }

    fn begin(&mut self, fresh: bool, loaded: EqCoeffBlock, frames: usize, sample_rate: u32) {
        self.block_frames = frames.max(1) as u32;
        let previous = self.target;
        if fresh && (loaded.sample_rate == sample_rate || loaded.is_bypass()) {
            self.target = loaded.pack();
        }
        if self.current == self.target {
            self.ramping = false;
            self.bypass = is_bypass(&self.current);
            if self.bypass {
                self.clear_state();
            }
        } else if self.ramping && self.target == previous {
            self.bypass = false;
        } else {
            self.origin = self.current;
            self.ramp_pos = 0;
            self.ramping = true;
            self.bypass = false;
        }
        self.arm_mask();
    }

    /// Build the settled skip mask, or leave the cascade on the ramp path.
    fn arm_mask(&mut self) {
        if self.ramping || self.bypass {
            self.mask_on = false;
            return;
        }
        let live = live_mask(&self.current);
        for section in 0..SECTIONS {
            if live & (1u16 << section) == 0 {
                self.left[section] = BiquadState::new();
                self.right[section] = BiquadState::new();
            }
        }
        self.live = live;
        self.mask_on = true;
    }

    fn process_begun(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bypass {
            return;
        }
        let frames = left.len().min(right.len()).min(self.block_frames as usize);
        for frame in 0..frames {
            let (sample_l, sample_r) = self.tick_one(frame, left[frame], right[frame]);
            left[frame] = sample_l;
            right[frame] = sample_r;
        }
    }

    fn tick_one(&mut self, _frame: usize, left: f32, right: f32) -> (f32, f32) {
        if self.bypass {
            return (left, right);
        }
        let pack = self.pack_for();
        if self.bypass {
            return (left, right);
        }
        let live = if self.mask_on { Some(self.live) } else { None };
        let left = cascade(&pack, &mut self.left, left, live);
        let right = cascade(&pack, &mut self.right, right, live);
        (left, right)
    }

    fn pack_for(&mut self) -> [f64; PACK] {
        if !self.ramping {
            return self.current;
        }
        let pos = self.ramp_pos.saturating_add(1);
        self.ramp_pos = pos;
        if pos >= EQ_RAMP_SAMPLES.max(1) {
            self.current = self.target;
            self.ramping = false;
            if is_bypass(&self.current) {
                self.clear_state();
                self.bypass = true;
            }
            return self.current;
        }
        let t = f64::from(pos) / f64::from(EQ_RAMP_SAMPLES.max(1));
        self.current = lerp(self.origin, self.target, t);
        self.current
    }

    fn reset_filter(&mut self) {
        let identity = identity_pack();
        self.current = identity;
        self.target = identity;
        self.origin = identity;
        self.ramp_pos = 0;
        self.ramping = false;
        self.bypass = true;
        self.mask_on = false;
        self.live = 0;
        self.clear_state();
    }

    fn clear_state(&mut self) {
        self.left = [BiquadState::new(); SECTIONS];
        self.right = [BiquadState::new(); SECTIONS];
    }
}

#[derive(Clone, Copy, Debug)]
struct BiquadState {
    s1: f64,
    s2: f64,
}

impl BiquadState {
    const fn new() -> Self {
        Self { s1: 0.0, s2: 0.0 }
    }
}

fn section_is_identity(pack: &[f64; PACK], section: usize) -> bool {
    let at = section * COEFFS_PER;
    pack[at] == 1.0
        && pack[at + 1] == 0.0
        && pack[at + 2] == 0.0
        && pack[at + 3] == 0.0
        && pack[at + 4] == 0.0
}

fn live_mask(pack: &[f64; PACK]) -> u16 {
    let mut live = 0u16;
    for section in 0..SECTIONS {
        if !section_is_identity(pack, section) {
            live |= 1u16 << section;
        }
    }
    live
}

fn cascade(
    pack: &[f64; PACK],
    state: &mut [BiquadState; SECTIONS],
    input: f32,
    live: Option<u16>,
) -> f32 {
    let mut y = if input.is_finite() {
        f64::from(input)
    } else {
        0.0
    };
    for (section, memory) in state.iter_mut().enumerate() {
        if live.is_some_and(|mask| mask & (1u16 << section) == 0) {
            continue;
        }
        if live.is_none() && section_is_identity(pack, section) {
            memory.s1 = 0.0;
            memory.s2 = 0.0;
            continue;
        }
        let at = section * COEFFS_PER;
        let b0 = pack[at];
        let b1 = pack[at + 1];
        let b2 = pack[at + 2];
        let a1 = pack[at + 3];
        let a2 = pack[at + 4];
        let out = b0 * y + memory.s1;
        let out = if out.is_finite() { out } else { 0.0 };
        memory.s1 = flush(b1 * y - a1 * out + memory.s2);
        memory.s2 = flush(b2 * y - a2 * out);
        y = out;
    }
    y as f32
}

fn flush(value: f64) -> f64 {
    if value.is_finite() {
        if value.abs() < 1.0e-15 { 0.0 } else { value }
    } else {
        0.0
    }
}

fn usable_rate(sample_rate: u32) -> Option<u32> {
    if (SAMPLE_RATE_MIN..SAMPLE_RATE_MAX).contains(&sample_rate) {
        Some(sample_rate)
    } else {
        None
    }
}

fn sanitize_band(gain_db: f32) -> f32 {
    if gain_db.is_finite() {
        gain_db.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB)
    } else {
        0.0
    }
}

fn sanitize_hz(hz: f32) -> f32 {
    if hz.is_finite() {
        hz.clamp(HPF_MIN_HZ, HPF_MAX_HZ)
    } else {
        HPF_DEFAULT_HZ
    }
}

fn center_disabled(sample_rate: u32, freq_hz: f32) -> bool {
    let Some(rate) = usable_rate(sample_rate) else {
        return true;
    };
    let nyquist = f64::from(rate) * 0.5;
    let freq = f64::from(freq_hz);
    !freq.is_finite() || freq <= 0.0 || freq >= nyquist * f64::from(NYQUIST_MARGIN)
}

fn peaking(sample_rate: u32, freq_hz: f32, gain_db: f32) -> BiquadCoeffs {
    if gain_db.abs() < 1.0e-6 || center_disabled(sample_rate, freq_hz) {
        return BiquadCoeffs::IDENTITY;
    }
    let Some((w0, sin_w, cos_w)) = omega(sample_rate, freq_hz) else {
        return BiquadCoeffs::IDENTITY;
    };
    let Some(alpha) = octave_alpha(sin_w, w0, f64::from(EQ_BANDWIDTH_OCTAVES)) else {
        return BiquadCoeffs::IDENTITY;
    };
    let a = 10f64.powf(f64::from(gain_db) / 40.0);
    if !a.is_finite() || a <= 0.0 {
        return BiquadCoeffs::IDENTITY;
    }
    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cos_w;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cos_w;
    let a2 = 1.0 - alpha / a;
    normalize(b0, b1, b2, a0, a1, a2)
}

fn highpass(sample_rate: u32, freq_hz: f32, q: f32) -> BiquadCoeffs {
    if !q.is_finite() || q <= 0.0 {
        return BiquadCoeffs::IDENTITY;
    }
    let Some((w0, sin_w, cos_w)) = omega(sample_rate, freq_hz) else {
        return BiquadCoeffs::IDENTITY;
    };
    let _ = w0;
    let alpha = sin_w / (2.0 * f64::from(q));
    if !alpha.is_finite() || alpha <= 0.0 {
        return BiquadCoeffs::IDENTITY;
    }
    let b0 = (1.0 + cos_w) * 0.5;
    let b1 = -(1.0 + cos_w);
    let b2 = (1.0 + cos_w) * 0.5;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_w;
    let a2 = 1.0 - alpha;
    normalize(b0, b1, b2, a0, a1, a2)
}

fn omega(sample_rate: u32, freq_hz: f32) -> Option<(f64, f64, f64)> {
    if center_disabled(sample_rate, freq_hz) {
        return None;
    }
    let rate = usable_rate(sample_rate)?;
    let w0 = std::f64::consts::TAU * f64::from(freq_hz) / f64::from(rate);
    let (sin_w, cos_w) = w0.sin_cos();
    if sin_w.abs() < 1.0e-12 || !sin_w.is_finite() || !cos_w.is_finite() {
        None
    } else {
        Some((w0, sin_w, cos_w))
    }
}

fn octave_alpha(sin_w: f64, w0: f64, bandwidth: f64) -> Option<f64> {
    if bandwidth <= 0.0 || !bandwidth.is_finite() {
        return None;
    }
    let shaped = ((std::f64::consts::LN_2 / 2.0) * bandwidth * w0 / sin_w).sinh();
    let alpha = sin_w * shaped;
    if alpha.is_finite() && alpha > 0.0 {
        Some(alpha)
    } else {
        None
    }
}

fn normalize(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> BiquadCoeffs {
    if !a0.is_finite() || a0.abs() < 1.0e-18 {
        return BiquadCoeffs::IDENTITY;
    }
    let inv = 1.0 / a0;
    let coeffs = BiquadCoeffs {
        b0: b0 * inv,
        b1: b1 * inv,
        b2: b2 * inv,
        a1: a1 * inv,
        a2: a2 * inv,
    };
    if coeffs.is_finite() {
        coeffs
    } else {
        BiquadCoeffs::IDENTITY
    }
}

fn magnitude_db(block: &EqCoeffBlock, freq_hz: f32) -> f32 {
    let Some(rate) = usable_rate(block.sample_rate) else {
        return 0.0;
    };
    if !freq_hz.is_finite() || freq_hz <= 0.0 || f64::from(freq_hz) >= f64::from(rate) * 0.5 {
        return 0.0;
    }
    let w = std::f64::consts::TAU * f64::from(freq_hz) / f64::from(rate);
    let mut db = section_db(block.hpf, w);
    for band in block.bands {
        db += section_db(band, w);
    }
    if db.is_finite() { db as f32 } else { 0.0 }
}

fn section_db(coeffs: BiquadCoeffs, w: f64) -> f64 {
    if coeffs.is_identity() {
        return 0.0;
    }
    let (sin_w, cos_w) = w.sin_cos();
    let (sin_2w, cos_2w) = (2.0 * w).sin_cos();
    let num_re = coeffs.b0 + coeffs.b1 * cos_w + coeffs.b2 * cos_2w;
    let num_im = -(coeffs.b1 * sin_w + coeffs.b2 * sin_2w);
    let den_re = 1.0 + coeffs.a1 * cos_w + coeffs.a2 * cos_2w;
    let den_im = -(coeffs.a1 * sin_w + coeffs.a2 * sin_2w);
    let num = num_re * num_re + num_im * num_im;
    let den = den_re * den_re + den_im * den_im;
    if num <= 0.0 || den <= 0.0 || !num.is_finite() || !den.is_finite() {
        return 0.0;
    }
    10.0 * (num / den).log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    const BLOCK: usize = 256;

    #[allow(clippy::too_many_arguments)]
    fn push_sine(
        runtime: &mut EqRuntime,
        reader: &mut EqReader,
        produced: &mut Vec<f32>,
        sample_index: &mut usize,
        frames: usize,
        freq: f32,
        amp: f32,
        rate: u32,
    ) {
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        for (index, sample) in left.iter_mut().enumerate() {
            let phase = f64::from(freq) * (*sample_index + index) as f64 / f64::from(rate);
            *sample = (phase * std::f64::consts::TAU).sin() as f32 * amp;
        }
        right.copy_from_slice(&left);
        runtime.process_slot(reader, 0, &mut left, &mut right, rate);
        produced.extend_from_slice(&left);
        *sample_index += frames;
    }

    fn near(actual: f32, expected: f32, tol: f32) {
        assert!(
            (actual - expected).abs() <= tol,
            "{actual} != {expected} (tol {tol})"
        );
    }

    fn flat_pair(rate: u32) -> (EqWriter, EqReader) {
        let (mut writer, reader) = open();
        assert!(writer.set_sample_rate(rate));
        (writer, reader)
    }

    fn publish_strip(writer: &mut EqWriter, settings: EqSettings) {
        assert!(writer.set_sample_rate(settings.sample_rate()));
        assert!(writer.apply_settings(0, settings));
    }

    /// Steady-state gain of slot 0, in dB, from a sine at `freq_hz`.
    ///
    /// The consumer index lives on `reader`. Memory is cleared so the previous
    /// tone's tail is not part of this measurement. Coefficients stay.
    fn measure_db(reader: &mut EqReader, runtime: &mut EqRuntime, freq_hz: f32, rate: u32) -> f32 {
        runtime.clear_memory(0);
        let cycles = rate as f64 / freq_hz as f64;
        let warmup = ((32.0 * cycles).round() as usize).max(4_096);
        let measure = ((16.0 * cycles).round() as usize).max(2_048);
        let total = warmup + measure;
        let amp = 0.1f32;
        let mut left = vec![0.0f32; total];
        let mut right = vec![0.0f32; total];
        for (index, sample) in left.iter_mut().enumerate() {
            let phase = freq_hz as f64 * index as f64 / rate as f64;
            *sample = (phase * std::f64::consts::TAU).sin() as f32 * amp;
        }
        right.copy_from_slice(&left);
        let mut offset = 0;
        while offset < total {
            let end = (offset + BLOCK).min(total);
            runtime.process_slot(
                reader,
                0,
                &mut left[offset..end],
                &mut right[offset..end],
                rate,
            );
            offset = end;
        }
        let left_db = correlate_db(&left[warmup..], freq_hz, rate, warmup, amp);
        let right_db = correlate_db(&right[warmup..], freq_hz, rate, warmup, amp);
        assert!(
            (left_db - right_db).abs() <= 0.05,
            "channels diverged at {freq_hz} Hz: {left_db} vs {right_db}"
        );
        left_db
    }

    fn correlate_db(samples: &[f32], freq_hz: f32, rate: u32, start: usize, amp: f32) -> f32 {
        let mut re = 0.0f64;
        let mut im = 0.0f64;
        for (index, sample) in samples.iter().enumerate() {
            let w = std::f64::consts::TAU * freq_hz as f64 * (start + index) as f64 / rate as f64;
            let y = f64::from(*sample);
            re += y * w.cos();
            im += y * w.sin();
        }
        let n = samples.len() as f64;
        let peak = 2.0 * (re * re + im * im).sqrt() / n;
        if peak <= 0.0 || !peak.is_finite() {
            return f32::NEG_INFINITY;
        }
        (20.0 * (peak / f64::from(amp)).log10()) as f32
    }

    fn assert_sweep(settings: EqSettings, freqs: &[f32]) {
        let (mut writer, mut reader) = flat_pair(settings.sample_rate());
        publish_strip(&mut writer, settings);
        let mut runtime = EqRuntime::new();
        for &freq in freqs {
            let measured = measure_db(&mut reader, &mut runtime, freq, settings.sample_rate());
            let want = response_db(settings, freq);
            let err = (measured - want).abs();
            assert!(
                err <= 0.5,
                "f={freq} Hz measured {measured} dB, response_db {want} dB, error {err} dB"
            );
        }
    }

    fn mixed(rate: u32) -> EqSettings {
        let mut settings = EqSettings::flat(rate);
        let gains = [3.0, -6.0, 0.0, 9.0, -3.0, 6.0, 0.0, -9.0, 4.0, -2.0];
        for (index, gain) in gains.iter().enumerate() {
            assert!(settings.set_band(index, *gain));
        }
        settings.set_hpf_enabled(true);
        settings
    }

    #[test]
    fn defaults_are_flat_and_generic() {
        let settings = EqSettings::flat(48_000);
        assert!(!settings.hpf_enabled());
        assert_eq!(settings.hpf_hz(), HPF_DEFAULT_HZ);
        assert_eq!(settings.band_db(), [0.0; EQ_BANDS]);
        assert_eq!(EQ_BAND_HZ[0], 31.5);
        assert_eq!(EQ_BAND_HZ[9], 16_000.0);
        assert_eq!(EQ_GAIN_MIN_DB, -12.0);
        assert_eq!(EQ_GAIN_MAX_DB, 12.0);
        for freq in [20.0, 31.5, 80.0, 1_000.0, 16_000.0] {
            near(response_db(settings, freq), 0.0, 1.0e-4);
            near(settings.response_db(freq), 0.0, 1.0e-4);
        }
        let (writer, _reader) = open();
        let stored = writer.settings(0).unwrap();
        assert!(!stored.hpf_enabled());
        assert_eq!(stored.band_db(), [0.0; EQ_BANDS]);
        assert!(writer.settings(STRIP_SLOT_COUNT).is_none());
    }

    #[test]
    fn flat_process_keeps_every_sample_bit_for_bit() {
        let (_writer, mut reader) = open();
        let mut runtime = EqRuntime::new();
        let left_in = [0.1f32, -0.25, 0.5, 1.0, -1.0, -0.0, 0.70710677, -0.003];
        let right_in = [-0.1f32, 0.25, -0.5, 0.125, 0.5, -1.0, 0.0, 0.003];
        assert_eq!(left_in[5].to_bits(), (-0.0f32).to_bits());
        let mut left = left_in;
        let mut right = right_in;
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        assert_eq!(left, left_in);
        assert_eq!(right, right_in);
        assert_eq!(left[5].to_bits(), (-0.0f32).to_bits());
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        assert_eq!(left, left_in);
        assert_eq!(left[5].to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn gains_clamp_and_a_bad_index_does_not_publish() {
        let (mut writer, _reader) = open();
        assert!(!writer.apply(
            0,
            EqUpdate::Band {
                index: 10,
                gain_db: 6.0
            }
        ));
        assert_eq!(writer.settings(0).unwrap().band_db()[0], 0.0);
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 3,
                gain_db: 40.0,
            },
        ));
        near(writer.settings(0).unwrap().band_db()[3], 12.0, 1.0e-6);
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 3,
                gain_db: f32::NAN,
            },
        ));
        assert_eq!(writer.settings(0).unwrap().band_db()[3], 0.0);
        assert!(writer.apply(0, EqUpdate::HighPassHz(f32::NEG_INFINITY)));
        near(writer.settings(0).unwrap().hpf_hz(), HPF_DEFAULT_HZ, 1.0e-4);
        assert!(writer.apply(0, EqUpdate::HighPassHz(1.0)));
        near(writer.settings(0).unwrap().hpf_hz(), HPF_MIN_HZ, 1.0e-4);
        assert!(writer.apply(0, EqUpdate::HighPassHz(10_000.0)));
        near(writer.settings(0).unwrap().hpf_hz(), HPF_MAX_HZ, 1.0e-4);
        assert!(!writer.apply(u16::MAX, EqUpdate::Reset));
        assert!(!writer.set_sample_rate(0));
        assert!(!writer.set_sample_rate(7_999));
    }

    #[test]
    fn isolated_band_center_is_the_requested_gain() {
        for (index, &freq) in EQ_BAND_HZ.iter().enumerate() {
            for gain in [EQ_GAIN_MAX_DB, EQ_GAIN_MIN_DB, 6.0] {
                let mut settings = EqSettings::flat(48_000);
                assert!(settings.set_band(index, gain));
                near(response_db(settings, freq), gain, 0.05);
                near(
                    response_db(settings, freq),
                    peaking_db_f64(48_000.0, freq as f64, gain as f64, freq as f64) as f32,
                    0.05,
                );
            }
        }
    }

    #[test]
    fn high_pass_at_the_cutoff_is_about_minus_3_db() {
        let mut settings = EqSettings::flat(48_000);
        settings.set_hpf_enabled(true);
        near(response_db(settings, HPF_DEFAULT_HZ), -3.0103, 0.05);
        assert!(response_db(settings, 20.0) < -20.0);
        assert!(response_db(settings, 1_000.0).abs() < 0.5);
    }

    #[test]
    fn a_wrong_rate_publication_is_not_adopted() {
        let (mut writer, mut reader) = open();
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 4,
                gain_db: 12.0,
            },
        ));
        let mut runtime = EqRuntime::new();
        let mut left = vec![0.2f32; BLOCK];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 44_100);
        assert!(
            runtime.strip_bypassed(0),
            "48 kHz coefficients must not run at 44.1 kHz"
        );
        assert_eq!(left[0], 0.2);
        assert!(writer.set_sample_rate(44_100));
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 44_100);
        assert!(!runtime.strip_bypassed(0));
    }

    #[test]
    fn one_block_finishes_the_ramp() {
        let (mut writer, mut reader) = flat_pair(48_000);
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 4,
                gain_db: 12.0,
            },
        ));
        let mut runtime = EqRuntime::new();
        let mut left = vec![0.0f32; BLOCK];
        let mut right = vec![0.0f32; BLOCK];
        left[0] = 1.0;
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        assert!(
            !runtime.ramping(0),
            "a block of EQ_RAMP_SAMPLES finishes the crossfade"
        );
        assert!(
            !runtime.strip_bypassed(0),
            "a +12 dB band is not a bypass after the ramp block"
        );
        let mut more_l = vec![0.0f32; BLOCK];
        let mut more_r = vec![0.0f32; BLOCK];
        runtime.process_slot(&mut reader, 0, &mut more_l, &mut more_r, 48_000);
        assert!(!runtime.strip_bypassed(0));
    }

    #[test]
    fn reset_slot_returns_to_a_flat_bypass() {
        let (mut writer, mut reader) = open();
        assert!(writer.apply(2, EqUpdate::HighPassEnabled(true)));
        assert!(writer.apply(
            2,
            EqUpdate::Band {
                index: 1,
                gain_db: -12.0,
            },
        ));
        let mut runtime = EqRuntime::new();
        let mut left = vec![0.2f32; BLOCK];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 2, &mut left, &mut right, 48_000);
        writer.reset_slot(2);
        runtime.reset_slot(2);
        let input = [0.2f32, -0.2, 0.0, 0.5];
        let mut left = input;
        let mut right = input;
        runtime.process_slot(&mut reader, 2, &mut left, &mut right, 48_000);
        assert_eq!(left, input);
        assert!(runtime.strip_bypassed(2));
        assert!(!writer.settings(2).unwrap().hpf_enabled());
        assert_eq!(writer.settings(2).unwrap().band_db(), [0.0; EQ_BANDS]);
    }

    #[test]
    fn published_blocks_do_not_tear() {
        let channel = Arc::new(EqChannel::new());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producer = Arc::clone(&channel);
        let consumer = Arc::clone(&channel);
        let done_flag = Arc::clone(&done);
        let writer = thread::spawn(move || {
            for stamp in 1..20_001u32 {
                producer.publish_block(marker(stamp));
            }
            done_flag.store(true, Ordering::Release);
        });
        let reader = thread::spawn(move || {
            let mut read_idx = INITIAL_READ;
            let mut dst = EqCoeffBlock::bypass(48_000);
            let mut saw = 0u32;
            while saw == 0 || !done.load(Ordering::Acquire) {
                if consumer.poll(&mut read_idx, &mut dst) {
                    let bits = dst.stamp_uniform().expect("torn coefficient block");
                    assert_eq!(dst.sample_rate, (bits >> 32) as u32);
                    saw = saw.saturating_add(1);
                }
            }
            if consumer.poll(&mut read_idx, &mut dst) {
                let bits = dst.stamp_uniform().expect("torn coefficient block");
                assert_eq!(dst.sample_rate, (bits >> 32) as u32);
                saw = saw.saturating_add(1);
            }
            saw
        });
        writer.join().unwrap();
        let saw = reader.join().unwrap();
        assert!(saw > 0, "the consumer never observed a publication");
    }

    #[test]
    fn sequential_swap_keeps_the_three_indices_distinct() {
        let channel = EqChannel::new();
        let mut read_idx = INITIAL_READ;
        let mut dst = EqCoeffBlock::bypass(48_000);
        for stamp in 1..500u32 {
            channel.publish_block(marker(stamp));
            assert!(channel.poll(&mut read_idx, &mut dst));
            assert_eq!(
                dst.stamp_uniform().map(|bits| (bits >> 32) as u32),
                Some(stamp)
            );
            let write_idx = channel.write_idx.load(Ordering::Relaxed);
            let spare = channel.state.load(Ordering::Relaxed) & 0b11;
            let mut indices = [write_idx, read_idx, spare];
            indices.sort_unstable();
            assert_eq!(indices, [0, 1, 2], "aliased triple-buffer indices");
        }
    }

    #[test]
    fn sine_sweep_matches_response_db_within_half_a_decibel() {
        for (index, &freq) in EQ_BAND_HZ.iter().enumerate() {
            for gain in [12.0, -12.0] {
                let mut settings = EqSettings::flat(48_000);
                assert!(settings.set_band(index, gain));
                assert_sweep(settings, &[freq]);
            }
        }

        let curve = mixed(48_000);
        let mut freqs = Vec::with_capacity(EQ_BANDS * 2 + 2);
        freqs.extend_from_slice(&EQ_BAND_HZ);
        for pair in EQ_BAND_HZ.windows(2) {
            freqs.push((pair[0] * pair[1]).sqrt());
        }
        freqs.push(20.0);
        freqs.push(12_000.0);
        assert_sweep(curve, &freqs);

        let mut hpf = EqSettings::flat(48_000);
        hpf.set_hpf_enabled(true);
        assert_sweep(hpf, &[20.0, 40.0, 80.0, 160.0, 1_000.0, 8_000.0]);

        let mut both = EqSettings::flat(48_000);
        both.set_hpf_enabled(true);
        assert!(both.set_band(4, 6.0));
        assert_sweep(both, &[80.0, 1_000.0, 4_000.0]);

        assert_sweep(mixed(44_100), &[100.0, 1_000.0, 10_000.0]);
    }

    #[test]
    fn master_sine_matches_response_db() {
        let mut settings = EqSettings::flat(48_000);
        assert!(settings.set_band(6, -6.0));
        let (mut writer, mut reader) = flat_pair(48_000);
        writer.apply_master_settings(settings);
        let freq = EQ_BAND_HZ[6];
        let rate = 48_000u32;
        let cycles = rate as f64 / freq as f64;
        let warmup = ((32.0 * cycles).round() as usize).max(4_096);
        let measure = ((16.0 * cycles).round() as usize).max(2_048);
        let total = warmup + measure;
        let amp = 0.1f32;
        let mut left = vec![0.0f32; total];
        let mut right = vec![0.0f32; total];
        for (index, sample) in left.iter_mut().enumerate() {
            let phase = freq as f64 * index as f64 / rate as f64;
            *sample = (phase * std::f64::consts::TAU).sin() as f32 * amp;
        }
        right.copy_from_slice(&left);
        let mut runtime = EqRuntime::new();
        let mut offset = 0;
        while offset < total {
            let end = (offset + BLOCK).min(total);
            runtime.process_master(
                &mut reader,
                &mut left[offset..end],
                &mut right[offset..end],
                rate,
            );
            offset = end;
        }
        let measured = correlate_db(&left[warmup..], freq, rate, warmup, amp);
        let want = response_db(settings, freq);
        assert!(
            (measured - want).abs() <= 0.5,
            "master measured {measured} dB, response_db {want} dB"
        );
        assert!(!runtime.master_bypassed());
    }

    #[test]
    fn a_band_at_the_nyquist_margin_is_disabled() {
        let at_32k = EqSettings::flat(32_000);
        assert!(at_32k.band_disabled(9), "16 kHz at 32 kHz is at Nyquist");
        assert!(!at_32k.band_disabled(8), "8 kHz at 32 kHz still runs");
        assert!(!EqSettings::flat(44_100).band_disabled(9));
        assert!(!EqSettings::flat(48_000).band_disabled(9));
        let mut boosted = at_32k;
        assert!(boosted.set_band(9, 12.0));
        assert!(boosted.coefficients().bands[9].is_identity());
        assert!(!boosted.coefficients().bands[8].is_identity() || boosted.band_db()[8] == 0.0);
        let mut eight = at_32k;
        assert!(eight.set_band(8, 12.0));
        assert!(!eight.coefficients().bands[8].is_identity());
    }

    #[test]
    fn low_band_at_192k_stays_within_half_a_decibel() {
        let mut settings = EqSettings::flat(192_000);
        assert!(settings.set_band(0, 12.0));
        assert!(!settings.band_disabled(0));
        near(response_db(settings, 31.5), 12.0, 0.05);
        assert_sweep(settings, &[31.5]);
    }

    #[test]
    fn short_blocks_ramp_for_a_fixed_count_and_the_step_stays_bounded() {
        let (mut writer, mut reader) = flat_pair(48_000);
        let mut runtime = EqRuntime::new();
        let rate = 48_000u32;
        let freq = 1_000.0f32;
        let amp = 0.2f32;
        let quantum = 32usize;
        let mut sample_index = 0usize;
        let mut produced = Vec::new();
        for _ in 0..4 {
            push_sine(
                &mut runtime,
                &mut reader,
                &mut produced,
                &mut sample_index,
                quantum,
                freq,
                amp,
                rate,
            );
        }
        assert!(runtime.strip_bypassed(0));
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 4,
                gain_db: 12.0,
            },
        ));
        let ramp_start = produced.len();
        push_sine(
            &mut runtime,
            &mut reader,
            &mut produced,
            &mut sample_index,
            quantum,
            freq,
            amp,
            rate,
        );
        assert!(
            runtime.ramping(0),
            "a 32-sample quantum must not finish the crossfade"
        );
        let rest = EQ_RAMP_SAMPLES as usize - quantum;
        push_sine(
            &mut runtime,
            &mut reader,
            &mut produced,
            &mut sample_index,
            rest,
            freq,
            amp,
            rate,
        );
        assert!(!runtime.ramping(0));
        assert!(!runtime.strip_bypassed(0));
        let ramp_end = ramp_start + EQ_RAMP_SAMPLES as usize;
        let mut max_jump = 0.0f32;
        for pair in produced[ramp_start..ramp_end].windows(2) {
            max_jump = max_jump.max((pair[1] - pair[0]).abs());
        }
        let settled = amp * 10f32.powf(12.0 / 20.0);
        let steady_step = settled * std::f32::consts::TAU * freq / rate as f32;
        assert!(
            max_jump <= steady_step * 2.0 + 0.02,
            "ramp jump {max_jump} exceeded the settled sine step {steady_step}"
        );
    }

    #[test]
    fn a_pass_through_section_clears_state_so_reenable_does_not_click() {
        let (mut writer, mut reader) = flat_pair(48_000);
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 2,
                gain_db: 6.0,
            },
        ));
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 4,
                gain_db: 12.0,
            },
        ));
        let mut runtime = EqRuntime::new();
        let mut index = 0usize;
        let mut run = |runtime: &mut EqRuntime, frames: usize, amp: f32| -> Vec<f32> {
            let mut left = vec![0.0f32; frames];
            let mut right = vec![0.0f32; frames];
            for (sample_at, sample) in left.iter_mut().enumerate() {
                let phase = 1_000.0 * (index + sample_at) as f64 / 48_000.0;
                *sample = (phase * std::f64::consts::TAU).sin() as f32 * amp;
            }
            right.copy_from_slice(&left);
            let mut offset = 0;
            while offset < frames {
                let end = (offset + 64).min(frames);
                runtime.process_slot(
                    &mut reader,
                    0,
                    &mut left[offset..end],
                    &mut right[offset..end],
                    48_000,
                );
                offset = end;
            }
            index += frames;
            left
        };
        let _ = run(&mut runtime, 4_096, 0.5);
        assert!(runtime.section_state(0, 5) > 1.0e-4);
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 4,
                gain_db: 0.0,
            },
        ));
        let _ = run(&mut runtime, EQ_RAMP_SAMPLES as usize + 64, 0.0);
        assert!(
            runtime.section_state(0, 5) < 1.0e-12,
            "band 4 state {} survived pass-through",
            runtime.section_state(0, 5)
        );
        assert!(
            runtime.section_state(0, 3) > 1.0e-8,
            "clearing band 4 must not wipe the section that is still on"
        );
        let before = run(&mut runtime, 8, 0.0);
        assert!(writer.apply(
            0,
            EqUpdate::Band {
                index: 4,
                gain_db: 12.0,
            },
        ));
        let after = run(&mut runtime, 32, 0.0);
        let last = before[before.len() - 1];
        let jump = (after[0] - last).abs();
        let later = after
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f32::max);
        assert!(
            jump < 0.02 && later < 0.02,
            "re-enable jump {jump}, later {later}"
        );
    }

    #[test]
    fn a_ramp_that_ends_mid_block_still_runs_every_section() {
        let frames = 96usize;
        let (mut writer, mut reader) = flat_pair(48_000);
        let mut runtime = EqRuntime::new();
        drive_dc(&mut runtime, &mut reader, frames, 0.5);
        assert!(!runtime.ramping(0));
        publish_every_section(&mut writer, true, 6.0);
        assert_block_runs_every_section(&mut runtime, &mut reader, frames);
        assert!(runtime.ramping(0));
        assert_block_runs_every_section(&mut runtime, &mut reader, frames);
        assert!(
            runtime.ramping(0),
            "192 samples is still inside the 256-sample ramp"
        );
        runtime.clear_memory(0);
        assert_block_runs_every_section(&mut runtime, &mut reader, frames);
        assert!(
            !runtime.ramping(0),
            "sample 256 falls inside this 96-frame block"
        );
        runtime.clear_memory(0);
        assert_block_runs_every_section(&mut runtime, &mut reader, frames);
        assert!(!runtime.ramping(0));
    }

    #[test]
    fn a_coefficient_ramp_toward_or_away_from_zero_db_runs_every_section() {
        let mut away = EqRuntime::new();
        let (mut writer, mut reader) = flat_pair(48_000);
        publish_every_section(&mut writer, true, 6.0);
        drive_dc(&mut away, &mut reader, 48, 0.5);
        assert!(away.ramping(0));
        assert_every_section_moved(&away);

        let mut toward = EqRuntime::new();
        let (mut writer, mut reader) = flat_pair(48_000);
        publish_every_section(&mut writer, true, 6.0);
        drive_dc(&mut toward, &mut reader, EQ_RAMP_SAMPLES as usize + 64, 0.5);
        assert!(!toward.ramping(0));
        publish_every_section(&mut writer, false, 0.0);
        drive_dc(&mut toward, &mut reader, 48, 0.5);
        assert!(toward.ramping(0));
        assert_every_section_moved(&toward);
    }

    fn publish_every_section(writer: &mut EqWriter, hpf: bool, gain_db: f32) {
        assert!(writer.apply(0, EqUpdate::HighPassEnabled(hpf)));
        for index in 0..EQ_BANDS {
            assert!(writer.apply(
                0,
                EqUpdate::Band {
                    index: index as u8,
                    gain_db,
                },
            ));
        }
    }

    fn drive_dc(runtime: &mut EqRuntime, reader: &mut EqReader, frames: usize, amp: f32) {
        let mut left = vec![amp; frames];
        let mut right = vec![amp; frames];
        let mut offset = 0;
        while offset < frames {
            let end = (offset + BLOCK).min(frames);
            runtime.process_slot(
                reader,
                0,
                &mut left[offset..end],
                &mut right[offset..end],
                48_000,
            );
            offset = end;
        }
    }

    fn assert_every_section_moved(runtime: &EqRuntime) {
        for section in 0..SECTIONS {
            let energy = runtime.section_state(0, section);
            assert!(
                energy > 1.0e-6,
                "section {section} did not run, state {energy}"
            );
        }
    }

    fn assert_block_runs_every_section(
        runtime: &mut EqRuntime,
        reader: &mut EqReader,
        frames: usize,
    ) {
        let before: Vec<f64> = (0..SECTIONS)
            .map(|section| runtime.section_state(0, section))
            .collect();
        drive_dc(runtime, reader, frames, 0.5);
        for (section, earlier) in before.iter().enumerate() {
            let energy = runtime.section_state(0, section);
            assert!(
                (energy - earlier).abs() > 1.0e-6,
                "section {section} did not run, state {energy}"
            );
        }
    }

    #[test]
    fn block_cost_is_measured() {
        let (mut writer, mut reader) = flat_pair(48_000);
        assert!(writer.apply(0, EqUpdate::HighPassEnabled(true)));
        for index in 0..EQ_BANDS {
            assert!(writer.apply(
                0,
                EqUpdate::Band {
                    index: index as u8,
                    gain_db: 12.0,
                },
            ));
        }
        assert!(writer.apply_master(EqUpdate::HighPassEnabled(true)));
        let mut runtime = EqRuntime::new();
        let frames = 256usize;
        let mut left = vec![0.1f32; frames];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        let reps = 1_000u32;
        let start = std::time::Instant::now();
        for _ in 0..reps {
            runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
            runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        }
        let elapsed = start.elapsed();
        let per = elapsed / reps;
        eprintln!(
            "MEASURED eq block: {per:?} for {frames} stereo frames, high-pass plus 10 peaking sections, strip and master"
        );
        std::hint::black_box((&left, &right));
        std::hint::black_box(per);
    }

    fn marker(stamp: u32) -> EqCoeffBlock {
        let sample = f64::from_bits((stamp as u64) << 32);
        let coeffs = BiquadCoeffs {
            b0: sample,
            b1: sample,
            b2: sample,
            a1: sample,
            a2: sample,
        };
        EqCoeffBlock {
            hpf: coeffs,
            bands: [coeffs; EQ_BANDS],
            sample_rate: stamp,
        }
    }

    /// Independent f64 copy of the RBJ peaking magnitude, for the center check.
    fn peaking_db_f64(rate: f64, freq: f64, gain_db: f64, at: f64) -> f64 {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = std::f64::consts::TAU * freq / rate;
        let (sin_w, cos_w) = w0.sin_cos();
        let alpha = sin_w * ((std::f64::consts::LN_2 / 2.0) * w0 / sin_w).sinh();
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha / a;
        let w = std::f64::consts::TAU * at / rate;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num_re = b0 / a0 + (b1 / a0) * c1 + (b2 / a0) * c2;
        let num_im = -((b1 / a0) * s1 + (b2 / a0) * s2);
        let den_re = 1.0 + (a1 / a0) * c1 + (a2 / a0) * c2;
        let den_im = -((a1 / a0) * s1 + (a2 / a0) * s2);
        let num = num_re * num_re + num_im * num_im;
        let den = den_re * den_re + den_im * den_im;
        10.0 * (num / den).log10()
    }
}

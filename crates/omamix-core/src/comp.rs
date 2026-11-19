//! Feed-forward compressor for each strip and the master.
//!
//! The detector is a stereo-linked peak follower. Its level is the louder
//! channel. Attack and release are one-pole time constants, the same shape
//! as the fader smoother:
//!
//! ```text
//! coeff = 1 - exp(-1 / (seconds * sample_rate))
//! envelope += coeff * (level - envelope)
//! ```
//!
//! A rising level uses the attack coefficient. A falling level uses the
//! release coefficient. One time constant reaches `1 - 1/e` of the step.
//!
//! The static curve is `static_reduction_db`. Hard knee: nothing at or below
//! the threshold, and above it the excess is reduced by `1 - 1/ratio`. Soft
//! knee of width W blends that with a quadratic through the W dB around the
//! threshold. Makeup is a constant output gain and is not part of the
//! published reduction.
//!
//! Defaults are bypassed. The stage is off, so a default strip does not
//! change samples. The follower stays live while the stage is bypassed, so
//! turning it on does not start from a stale envelope. A bypassed sample is
//! returned unchanged. The numbers stored behind that switch are generic:
//! threshold −18 dB, ratio 4:1, attack 10 ms, release 100 ms, hard knee,
//! makeup 0 dB. They are not a measured microphone and not a preset copied
//! from another app.
//!
//! The control thread builds the detector coefficients and publishes the
//! whole block by swapping an index in a triple buffer. [`CompWriter`] is
//! the only producer. It is not `Clone` and not `Sync`, and publishing takes
//! `&mut self`, so safe code cannot publish from two threads. The audio
//! callback holds [`CompReader`], which owns the consumer index. That reader
//! is not `Clone` and not `Sync`, and `process` takes `&mut CompReader`.
//! The callback only loads the block. It does not build coefficients,
//! allocate, or take a lock. A newly published block is linearly ramped
//! across [`COMP_RAMP_SAMPLES`] samples, including when the quantum is
//! shorter than that. The last sample of the ramp uses the new parameters.
//!
//! A block built at another sample rate is left unused, except a bypass
//! block. Bypass has wet 0, so the samples stay unchanged. The follower
//! still steps with that block's attack and release until the loop thread
//! republishes at the callback's rate.
//!
//! Gain reduction is one atomic per strip and one for the master, in dB.
//! It is the minimum of the curve over the block. That is 0 when every
//! sample is bypassed or under the knee, and the most negative sample when
//! the signal is reduced, so a short transient is not replaced by a later
//! sample that has already released. Makeup is not included. The callback
//! does not hold or decay that number.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::db;
use crate::params::STRIP_SLOT_COUNT;

/// Generic threshold. Not a measured level.
pub const COMP_THRESHOLD_DEFAULT_DB: f32 = -18.0;
pub const COMP_THRESHOLD_MIN_DB: f32 = -60.0;
/// Least sensitive finite threshold, 0 dB. A non-finite write is stored here
/// so a bad value does not compress more.
pub const COMP_THRESHOLD_MAX_DB: f32 = 0.0;

/// Generic ratio. Neutral, no compression, is 1:1.
pub const COMP_RATIO_DEFAULT: f32 = 4.0;
/// Neutral ratio, 1:1. A non-finite write is stored here so a bad value does
/// not start compressing.
pub const COMP_RATIO_MIN: f32 = 1.0;
pub const COMP_RATIO_MAX: f32 = 20.0;

pub const COMP_ATTACK_DEFAULT_MS: f32 = 10.0;
pub const COMP_ATTACK_MIN_MS: f32 = 0.1;
pub const COMP_ATTACK_MAX_MS: f32 = 200.0;

pub const COMP_RELEASE_DEFAULT_MS: f32 = 100.0;
pub const COMP_RELEASE_MIN_MS: f32 = 1.0;
pub const COMP_RELEASE_MAX_MS: f32 = 2_000.0;

pub const COMP_KNEE_DEFAULT_DB: f32 = 0.0;
pub const COMP_KNEE_MIN_DB: f32 = 0.0;
pub const COMP_KNEE_MAX_DB: f32 = 24.0;

pub const COMP_MAKEUP_DEFAULT_DB: f32 = 0.0;
pub const COMP_MAKEUP_MIN_DB: f32 = -24.0;
pub const COMP_MAKEUP_MAX_DB: f32 = 24.0;

/// Samples in a parameter crossfade. A shorter quantum continues on the next callback.
pub const COMP_RAMP_SAMPLES: u32 = 256;

const SAMPLE_RATE_MIN: u32 = 8_000;
const SAMPLE_RATE_MAX: u32 = 384_000;

const THRESHOLD: usize = 0;
const SLOPE: usize = 1;
const KNEE: usize = 2;
const MAKEUP: usize = 3;
const ATTACK: usize = 4;
const RELEASE: usize = 5;
const WET: usize = 6;
const PACK: usize = 7;

/// Producer's first buffer. The consumer starts on 1. The spare starts at 2.
const INITIAL_WRITE: u32 = 0;
const INITIAL_READ: u32 = 1;
const INITIAL_SPARE: u32 = 2;
const DIRTY: u32 = 0b100;

/// What the control thread asks to store. Applied outside `process`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompUpdate {
    Enabled(bool),
    ThresholdDb(f32),
    Ratio(f32),
    AttackMs(f32),
    ReleaseMs(f32),
    KneeDb(f32),
    MakeupDb(f32),
    /// Bypass, and the generic numbers above.
    Reset,
}

/// One compressor snapshot.
///
/// Fields are private so a ratio below 1:1 or a threshold above 0 dB cannot
/// be stored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompSettings {
    sample_rate: u32,
    enabled: bool,
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    knee_db: f32,
    makeup_db: f32,
}

impl CompSettings {
    /// Bypass at `sample_rate`, with the generic numbers behind the switch.
    ///
    /// An unusable rate becomes 48 kHz. Enabling this value is what makes
    /// those numbers audible. The bypass itself does not change samples.
    pub fn bypass(sample_rate: u32) -> Self {
        Self {
            sample_rate: usable_rate(sample_rate).unwrap_or(48_000),
            enabled: false,
            threshold_db: COMP_THRESHOLD_DEFAULT_DB,
            ratio: COMP_RATIO_DEFAULT,
            attack_ms: COMP_ATTACK_DEFAULT_MS,
            release_ms: COMP_RELEASE_DEFAULT_MS,
            knee_db: COMP_KNEE_DEFAULT_DB,
            makeup_db: COMP_MAKEUP_DEFAULT_DB,
        }
    }

    pub fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    pub fn threshold_db(self) -> f32 {
        self.threshold_db
    }

    pub fn ratio(self) -> f32 {
        self.ratio
    }

    pub fn attack_ms(self) -> f32 {
        self.attack_ms
    }

    pub fn release_ms(self) -> f32 {
        self.release_ms
    }

    pub fn knee_db(self) -> f32 {
        self.knee_db
    }

    pub fn makeup_db(self) -> f32 {
        self.makeup_db
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

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.threshold_db = sanitize_threshold(threshold_db);
    }

    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = sanitize_ratio(ratio);
    }

    pub fn set_attack_ms(&mut self, attack_ms: f32) {
        self.attack_ms = sanitize_ms(
            attack_ms,
            COMP_ATTACK_MIN_MS,
            COMP_ATTACK_MAX_MS,
            COMP_ATTACK_DEFAULT_MS,
        );
    }

    pub fn set_release_ms(&mut self, release_ms: f32) {
        self.release_ms = sanitize_ms(
            release_ms,
            COMP_RELEASE_MIN_MS,
            COMP_RELEASE_MAX_MS,
            COMP_RELEASE_DEFAULT_MS,
        );
    }

    pub fn set_knee_db(&mut self, knee_db: f32) {
        self.knee_db = sanitize_knee(knee_db);
    }

    pub fn set_makeup_db(&mut self, makeup_db: f32) {
        self.makeup_db = sanitize_makeup(makeup_db);
    }

    /// Reduction at `level_db`, in dB. 0 at or under the knee. Negative above it.
    ///
    /// Makeup is not included. A bypassed setting still reports the curve of
    /// the stored numbers; the audio path applies that curve only while the
    /// stage is on.
    pub fn static_reduction_db(self, level_db: f32) -> f32 {
        static_reduction_db(level_db, self.threshold_db, self.ratio, self.knee_db)
    }
}

/// Gain reduction of one static level, in dB.
///
/// `level_db` is the detector level, not a sample. The result is 0 when
/// `level_db` is under the knee, and negative when the level is reduced.
/// `ratio` is clamped to 1:1..=20:1. A non-finite level or ratio produces 0,
/// which is no reduction. Makeup is not included.
///
/// Hard knee (`knee_db` <= 0):
///
/// ```text
/// slope = 1 - 1/ratio
/// over = level_db - threshold_db
/// reduction = 0            when over <= 0
/// reduction = -over * slope
/// ```
///
/// Soft knee of width W places that hard curve outside `threshold ± W/2`
/// and a quadratic between them. The quadratic meets 0 dB at the lower
/// edge and the hard curve at the upper edge.
pub fn static_reduction_db(level_db: f32, threshold_db: f32, ratio: f32, knee_db: f32) -> f32 {
    if !level_db.is_finite() {
        return 0.0;
    }
    let ratio = sanitize_ratio(ratio);
    let threshold_db = sanitize_threshold(threshold_db);
    let knee_db = sanitize_knee(knee_db);
    let slope = 1.0 - 1.0 / ratio;
    if slope <= 0.0 {
        return 0.0;
    }
    let gr = if knee_db <= 0.0 {
        let over = level_db - threshold_db;
        if over <= 0.0 { 0.0 } else { -over * slope }
    } else {
        let half = knee_db * 0.5;
        let lower = threshold_db - half;
        if level_db <= lower {
            0.0
        } else if level_db >= threshold_db + half {
            -(level_db - threshold_db) * slope
        } else {
            let t = level_db - lower;
            -slope * t * t / (2.0 * knee_db)
        }
    };
    if gr.is_finite() { gr.min(0.0) } else { 0.0 }
}

/// The only producer. Not `Clone`. Not `Sync`.
///
/// [`open`] returns this and the [`CompReader`] the audio callback loads.
/// Publishing takes `&mut self`, so two callers cannot publish at once, and
/// the type cannot be shared across threads.
///
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::comp::CompWriter>();
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::comp::CompWriter>();
/// }
/// ```
#[derive(Debug)]
pub struct CompWriter {
    inner: Arc<CompShared>,
    _unshared: PhantomData<Cell<()>>,
}

/// Detector publications for the audio callback.
///
/// Not `Clone` and not `Sync`. This handle owns the consumer index for each
/// strip and for the master. [`CompRuntime`] polls it with `&mut self`. A
/// shared `&CompReader` cannot advance the index, and a second reader would
/// alias the triple buffer. This handle does not publish.
///
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::comp::CompReader>();
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::comp::CompReader>();
/// }
/// ```
///
/// A shared `&CompReader` cannot be passed to `process`. The signature
/// requires `&mut CompReader` because the reader owns the consumer index.
///
/// ```compile_fail,E0308
/// fn demo() {
///     let mut eq_a = omamix_core::eq::EqReader::flat();
///     let mut eq_b = omamix_core::eq::EqReader::flat();
///     let comp = omamix_core::comp::CompReader::bypassed();
///     let mut limit = omamix_core::limit::LimitReader::bypassed();
///     let mut a = omamix_core::params::StripMixer::new(48_000);
///     let mut b = omamix_core::params::StripMixer::new(48_000);
///     let bank = omamix_core::params::StripParamBank::new();
///     let meters = omamix_core::params::StripMeterBank::new();
///     let mut scopes_a = omamix_core::scope::ScopeBank::new();
///     let mut scopes_b = omamix_core::scope::ScopeBank::new();
///     let mut left_a = [0.0f32; 8];
///     let mut right_a = [0.0f32; 8];
///     let mut left_b = [0.0f32; 8];
///     let mut right_b = [0.0f32; 8];
///     let shared = &comp;
///     a.process(
///         &bank,
///         &mut eq_a,
///         shared,
///         &mut limit,
///         &meters,
///         &mut scopes_a,
///         &[],
///         &mut left_a,
///         &mut right_a,
///         48_000,
///     );
///     b.process(
///         &bank,
///         &mut eq_b,
///         shared,
///         &mut limit,
///         &meters,
///         &mut scopes_b,
///         &[],
///         &mut left_b,
///         &mut right_b,
///         48_000,
///     );
/// }
/// ```
///
/// The same two calls compile when each one receives `&mut CompReader`.
/// The mutable borrow ends when the call returns, so the next call can
/// take it.
///
/// ```
/// let mut eq_a = omamix_core::eq::EqReader::flat();
/// let mut eq_b = omamix_core::eq::EqReader::flat();
/// let mut comp = omamix_core::comp::CompReader::bypassed();
/// let mut limit = omamix_core::limit::LimitReader::bypassed();
/// let mut a = omamix_core::params::StripMixer::new(48_000);
/// let mut b = omamix_core::params::StripMixer::new(48_000);
/// let bank = omamix_core::params::StripParamBank::new();
/// let meters = omamix_core::params::StripMeterBank::new();
/// let mut scopes_a = omamix_core::scope::ScopeBank::new();
/// let mut scopes_b = omamix_core::scope::ScopeBank::new();
/// let mut left_a = [0.0f32; 8];
/// let mut right_a = [0.0f32; 8];
/// let mut left_b = [0.0f32; 8];
/// let mut right_b = [0.0f32; 8];
/// a.process(
///     &bank,
///     &mut eq_a,
///     &mut comp,
///     &mut limit,
///     &meters,
///     &mut scopes_a,
///     &[],
///     &mut left_a,
///     &mut right_a,
///     48_000,
/// );
/// b.process(
///     &bank,
///     &mut eq_b,
///     &mut comp,
///     &mut limit,
///     &meters,
///     &mut scopes_b,
///     &[],
///     &mut left_b,
///     &mut right_b,
///     48_000,
/// );
/// assert_eq!(comp.gain_reduction_db(0), 0.0);
/// ```
#[derive(Debug)]
pub struct CompReader {
    inner: Arc<CompShared>,
    strip_read: [u32; STRIP_SLOT_COUNT],
    master_read: u32,
    _unshared: PhantomData<Cell<()>>,
}

#[derive(Debug)]
struct CompShared {
    strips: [CompChannel; STRIP_SLOT_COUNT],
    master: CompChannel,
    sample_rate: AtomicU32,
}

/// One producer and one consumer. Dropping either does not free the other.
pub fn open() -> (CompWriter, CompReader) {
    let inner = Arc::new(CompShared {
        strips: std::array::from_fn(|_| CompChannel::new()),
        master: CompChannel::new(),
        sample_rate: AtomicU32::new(48_000),
    });
    (
        CompWriter {
            inner: Arc::clone(&inner),
            _unshared: PhantomData,
        },
        CompReader {
            inner,
            strip_read: [INITIAL_READ; STRIP_SLOT_COUNT],
            master_read: INITIAL_READ,
            _unshared: PhantomData,
        },
    )
}

impl CompWriter {
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

    /// Store one field and publish that strip's block.
    ///
    /// Returns false when `slot` is outside the bank. A non-finite value is
    /// stored as the neutral end of that control, not as the generic default
    /// when those two differ.
    pub fn apply(&mut self, slot: u16, update: CompUpdate) -> bool {
        let Some(channel) = self.inner.strips.get(slot as usize) else {
            return false;
        };
        store_and_publish(channel, update, self.sample_rate())
    }

    pub fn apply_master(&mut self, update: CompUpdate) -> bool {
        store_and_publish(&self.inner.master, update, self.sample_rate())
    }

    /// Replace one strip's parameters, then publish once.
    ///
    /// Coefficients use this writer's sample rate, not `settings.sample_rate`.
    pub fn apply_settings(&mut self, slot: u16, settings: CompSettings) -> bool {
        let Some(channel) = self.inner.strips.get(slot as usize) else {
            return false;
        };
        channel.store_settings(settings);
        channel.publish_at(self.sample_rate());
        true
    }

    pub fn apply_master_settings(&mut self, settings: CompSettings) {
        self.inner.master.store_settings(settings);
        self.inner.master.publish_at(self.sample_rate());
    }

    pub fn settings(&self, slot: usize) -> Option<CompSettings> {
        self.inner
            .strips
            .get(slot)
            .map(|channel| channel_settings(self.sample_rate(), channel))
    }

    pub fn master_settings(&self) -> CompSettings {
        channel_settings(self.sample_rate(), &self.inner.master)
    }

    /// Block-minimum gain reduction, in dB. 0 is no reduction.
    ///
    /// The audio callback writes this through [`CompReader`]. The loop thread
    /// reads it here. An unused slot reads 0.
    pub fn gain_reduction_db(&self, slot: usize) -> f32 {
        self.inner
            .strips
            .get(slot)
            .map(CompChannel::load_gr)
            .unwrap_or(0.0)
    }

    pub fn master_gain_reduction_db(&self) -> f32 {
        self.inner.master.load_gr()
    }

    /// Bypass, generic numbers, and a 0 dB gain-reduction publication.
    ///
    /// Call this off the audio callback. It builds the bypass block here.
    /// The callback only clears the envelope.
    pub fn reset_slot(&mut self, slot: usize) {
        if let Some(channel) = self.inner.strips.get(slot) {
            channel.store_settings(CompSettings::bypass(self.sample_rate()));
            channel.publish_at(self.sample_rate());
            channel.store_gr(0.0);
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

impl CompReader {
    /// A bypassed reader whose producer has been dropped. Nothing can publish.
    pub fn bypassed() -> Self {
        open().1
    }

    pub fn gain_reduction_db(&self, slot: usize) -> f32 {
        self.inner
            .strips
            .get(slot)
            .map(CompChannel::load_gr)
            .unwrap_or(0.0)
    }

    pub fn master_gain_reduction_db(&self) -> f32 {
        self.inner.master.load_gr()
    }

    fn poll_strip(&mut self, slot: usize, sample_rate: u32) -> Option<CompBlock> {
        let CompReader {
            inner, strip_read, ..
        } = self;
        let channel = inner.strips.get(slot)?;
        let mut loaded = bypass_block(sample_rate);
        if channel.poll(&mut strip_read[slot], &mut loaded) {
            Some(loaded)
        } else {
            None
        }
    }

    fn poll_master(&mut self, sample_rate: u32) -> Option<CompBlock> {
        let CompReader {
            inner, master_read, ..
        } = self;
        let mut loaded = bypass_block(sample_rate);
        if inner.master.poll(master_read, &mut loaded) {
            Some(loaded)
        } else {
            None
        }
    }

    fn store_strip_gr(&self, slot: usize, gr_db: f32) {
        if let Some(channel) = self.inner.strips.get(slot) {
            channel.store_gr(gr_db);
        }
    }

    fn store_master_gr(&self, gr_db: f32) {
        self.inner.master.store_gr(gr_db);
    }
}

fn store_and_publish(channel: &CompChannel, update: CompUpdate, rate: u32) -> bool {
    channel.store(update);
    channel.publish_at(rate);
    true
}

fn channel_settings(sample_rate: u32, channel: &CompChannel) -> CompSettings {
    let mut settings = CompSettings::bypass(sample_rate);
    settings.enabled = channel.enabled.load(Ordering::Acquire);
    settings.threshold_db =
        sanitize_threshold(f32::from_bits(channel.threshold_db.load(Ordering::Acquire)));
    settings.ratio = sanitize_ratio(f32::from_bits(channel.ratio.load(Ordering::Acquire)));
    settings.attack_ms = sanitize_ms(
        f32::from_bits(channel.attack_ms.load(Ordering::Acquire)),
        COMP_ATTACK_MIN_MS,
        COMP_ATTACK_MAX_MS,
        COMP_ATTACK_DEFAULT_MS,
    );
    settings.release_ms = sanitize_ms(
        f32::from_bits(channel.release_ms.load(Ordering::Acquire)),
        COMP_RELEASE_MIN_MS,
        COMP_RELEASE_MAX_MS,
        COMP_RELEASE_DEFAULT_MS,
    );
    settings.knee_db = sanitize_knee(f32::from_bits(channel.knee_db.load(Ordering::Acquire)));
    settings.makeup_db = sanitize_makeup(f32::from_bits(channel.makeup_db.load(Ordering::Acquire)));
    settings
}

/// Audio-callback state for every strip and the master.
///
/// Only the thread that calls [`crate::params::StripMixer::process`] may
/// touch this value. It polls [`CompReader`] with `&mut`.
#[derive(Debug)]
pub struct CompRuntime {
    strips: [StripCompRt; STRIP_SLOT_COUNT],
    master: StripCompRt,
}

impl CompRuntime {
    pub(crate) fn new() -> Self {
        Self {
            strips: std::array::from_fn(|_| StripCompRt::new()),
            master: StripCompRt::new(),
        }
    }

    pub(crate) fn begin(
        &mut self,
        reader: &mut CompReader,
        slots: usize,
        frames: usize,
        sample_rate: u32,
    ) {
        let slots = slots.min(STRIP_SLOT_COUNT);
        for slot in 0..slots {
            let polled = reader.poll_strip(slot, sample_rate);
            self.strips[slot].begin(polled, frames, sample_rate);
        }
        let polled = reader.poll_master(sample_rate);
        self.master.begin(polled, frames, sample_rate);
    }

    pub(crate) fn tick(&mut self, slot: usize, left: f32, right: f32) -> (f32, f32) {
        match self.strips.get_mut(slot) {
            Some(strip) => strip.tick_one(left, right),
            None => (left, right),
        }
    }

    pub(crate) fn publish_strips(&mut self, reader: &CompReader, slots: usize) {
        let slots = slots.min(STRIP_SLOT_COUNT);
        for slot in 0..slots {
            reader.store_strip_gr(slot, self.strips[slot].block_min_gr);
        }
    }

    pub(crate) fn apply_master(
        &mut self,
        reader: &CompReader,
        left: &mut [f32],
        right: &mut [f32],
    ) {
        let frames = left
            .len()
            .min(right.len())
            .min(self.master.block_frames as usize);
        for frame in 0..frames {
            let (sample_l, sample_r) = self.master.tick_one(left[frame], right[frame]);
            left[frame] = sample_l;
            right[frame] = sample_r;
        }
        if frames > 0 {
            reader.store_master_gr(self.master.block_min_gr);
        }
    }

    /// Bypass coefficients and a cleared envelope.
    ///
    /// The consumer index stays on [`CompReader`]. Resetting it would alias a
    /// buffer the producer still owns.
    pub(crate) fn reset_slot(&mut self, slot: usize) {
        if let Some(strip) = self.strips.get_mut(slot) {
            strip.reset_detector();
        }
    }

    #[cfg(test)]
    fn process_slot(
        &mut self,
        reader: &mut CompReader,
        slot: usize,
        left: &mut [f32],
        right: &mut [f32],
        sample_rate: u32,
    ) {
        let frames = left.len().min(right.len());
        if frames == 0 || slot >= STRIP_SLOT_COUNT {
            return;
        }
        let polled = reader.poll_strip(slot, sample_rate);
        self.strips[slot].begin(polled, frames, sample_rate);
        for frame in 0..frames {
            let (sample_l, sample_r) = self.strips[slot].tick_one(left[frame], right[frame]);
            left[frame] = sample_l;
            right[frame] = sample_r;
        }
        reader.store_strip_gr(slot, self.strips[slot].block_min_gr);
    }

    #[cfg(test)]
    fn process_master(
        &mut self,
        reader: &mut CompReader,
        left: &mut [f32],
        right: &mut [f32],
        sample_rate: u32,
    ) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }
        let polled = reader.poll_master(sample_rate);
        self.master.begin(polled, frames, sample_rate);
        for frame in 0..frames {
            let (sample_l, sample_r) = self.master.tick_one(left[frame], right[frame]);
            left[frame] = sample_l;
            right[frame] = sample_r;
        }
        reader.store_master_gr(self.master.block_min_gr);
    }

    #[cfg(test)]
    fn envelope(&self, slot: usize) -> f32 {
        self.strips.get(slot).map(|strip| strip.env).unwrap_or(0.0)
    }

    #[cfg(test)]
    fn strip_bypassed(&self, slot: usize) -> bool {
        self.strips.get(slot).is_none_or(|strip| strip.bypass)
    }

    #[cfg(test)]
    fn ramping(&self, slot: usize) -> bool {
        self.strips.get(slot).is_some_and(|strip| strip.ramping)
    }
}

impl Default for CompRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug)]
struct CompBlock {
    pack: [f32; PACK],
    sample_rate: u32,
}

impl CompBlock {
    fn is_bypass(self) -> bool {
        self.pack[WET] == 0.0
    }

    #[cfg(test)]
    fn stamp_uniform(&self) -> Option<u32> {
        let bits = self.pack[0].to_bits();
        if self.pack.iter().all(|sample| sample.to_bits() == bits) {
            Some(bits)
        } else {
            None
        }
    }
}

fn bypass_block(sample_rate: u32) -> CompBlock {
    let rate = usable_rate(sample_rate).unwrap_or(48_000);
    let mut pack = [0.0; PACK];
    pack[THRESHOLD] = COMP_THRESHOLD_DEFAULT_DB;
    pack[SLOPE] = 0.0;
    pack[KNEE] = COMP_KNEE_DEFAULT_DB;
    pack[MAKEUP] = 1.0;
    pack[ATTACK] = time_coeff(rate, COMP_ATTACK_DEFAULT_MS * 0.001);
    pack[RELEASE] = time_coeff(rate, COMP_RELEASE_DEFAULT_MS * 0.001);
    pack[WET] = 0.0;
    CompBlock {
        pack,
        sample_rate: rate,
    }
}

fn block_from(settings: CompSettings, sample_rate: u32) -> CompBlock {
    let rate = usable_rate(sample_rate).unwrap_or(48_000);
    let mut pack = [0.0; PACK];
    pack[THRESHOLD] = settings.threshold_db;
    pack[SLOPE] = if settings.enabled {
        1.0 - 1.0 / settings.ratio
    } else {
        0.0
    };
    pack[KNEE] = if settings.enabled {
        settings.knee_db
    } else {
        0.0
    };
    pack[MAKEUP] = if settings.enabled {
        db::db_to_linear(settings.makeup_db)
    } else {
        1.0
    };
    pack[ATTACK] = time_coeff(rate, settings.attack_ms * 0.001);
    pack[RELEASE] = time_coeff(rate, settings.release_ms * 0.001);
    pack[WET] = if settings.enabled { 1.0 } else { 0.0 };
    CompBlock {
        pack,
        sample_rate: rate,
    }
}

/// Unity gain, so the sample can be returned untouched.
///
/// A bypassed stage has wet 0. An enabled stage with ratio 1:1 and 0 dB of
/// makeup also has unity gain. Either one leaves the bits alone.
fn is_unity(pack: &[f32; PACK]) -> bool {
    pack[WET] == 0.0 || (pack[SLOPE] == 0.0 && pack[MAKEUP] == 1.0)
}

fn lerp(from: [f32; PACK], to: [f32; PACK], t: f32) -> [f32; PACK] {
    let mut out = [0.0; PACK];
    for index in 0..PACK {
        out[index] = from[index] + (to[index] - from[index]) * t;
    }
    out
}

#[derive(Debug)]
struct CompChannel {
    enabled: AtomicBool,
    threshold_db: AtomicU32,
    ratio: AtomicU32,
    attack_ms: AtomicU32,
    release_ms: AtomicU32,
    knee_db: AtomicU32,
    makeup_db: AtomicU32,
    slots: [UnsafeCell<CompBlock>; 3],
    /// Spare index in bits 0..2. Bit 2 is set when a block is waiting.
    state: AtomicU32,
    write_idx: AtomicU32,
    /// Block-minimum gain reduction, in dB. The audio thread writes this.
    gr_db: AtomicU32,
}

impl CompChannel {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            threshold_db: AtomicU32::new(COMP_THRESHOLD_DEFAULT_DB.to_bits()),
            ratio: AtomicU32::new(COMP_RATIO_DEFAULT.to_bits()),
            attack_ms: AtomicU32::new(COMP_ATTACK_DEFAULT_MS.to_bits()),
            release_ms: AtomicU32::new(COMP_RELEASE_DEFAULT_MS.to_bits()),
            knee_db: AtomicU32::new(COMP_KNEE_DEFAULT_DB.to_bits()),
            makeup_db: AtomicU32::new(COMP_MAKEUP_DEFAULT_DB.to_bits()),
            slots: std::array::from_fn(|_| UnsafeCell::new(bypass_block(48_000))),
            state: AtomicU32::new(INITIAL_SPARE),
            write_idx: AtomicU32::new(INITIAL_WRITE),
            gr_db: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    fn store(&self, update: CompUpdate) {
        match update {
            CompUpdate::Enabled(on) => self.enabled.store(on, Ordering::Release),
            CompUpdate::ThresholdDb(db) => {
                self.threshold_db
                    .store(sanitize_threshold(db).to_bits(), Ordering::Release);
            }
            CompUpdate::Ratio(ratio) => {
                self.ratio
                    .store(sanitize_ratio(ratio).to_bits(), Ordering::Release);
            }
            CompUpdate::AttackMs(ms) => {
                self.attack_ms.store(
                    sanitize_ms(
                        ms,
                        COMP_ATTACK_MIN_MS,
                        COMP_ATTACK_MAX_MS,
                        COMP_ATTACK_DEFAULT_MS,
                    )
                    .to_bits(),
                    Ordering::Release,
                );
            }
            CompUpdate::ReleaseMs(ms) => {
                self.release_ms.store(
                    sanitize_ms(
                        ms,
                        COMP_RELEASE_MIN_MS,
                        COMP_RELEASE_MAX_MS,
                        COMP_RELEASE_DEFAULT_MS,
                    )
                    .to_bits(),
                    Ordering::Release,
                );
            }
            CompUpdate::KneeDb(db) => {
                self.knee_db
                    .store(sanitize_knee(db).to_bits(), Ordering::Release);
            }
            CompUpdate::MakeupDb(db) => {
                self.makeup_db
                    .store(sanitize_makeup(db).to_bits(), Ordering::Release);
            }
            CompUpdate::Reset => self.store_settings(CompSettings::bypass(48_000)),
        }
    }

    fn store_settings(&self, settings: CompSettings) {
        self.enabled.store(settings.enabled, Ordering::Release);
        self.threshold_db
            .store(settings.threshold_db.to_bits(), Ordering::Release);
        self.ratio
            .store(settings.ratio.to_bits(), Ordering::Release);
        self.attack_ms
            .store(settings.attack_ms.to_bits(), Ordering::Release);
        self.release_ms
            .store(settings.release_ms.to_bits(), Ordering::Release);
        self.knee_db
            .store(settings.knee_db.to_bits(), Ordering::Release);
        self.makeup_db
            .store(settings.makeup_db.to_bits(), Ordering::Release);
    }

    fn publish_at(&self, sample_rate: u32) {
        let enabled = self.enabled.load(Ordering::Acquire);
        let mut settings = CompSettings::bypass(sample_rate);
        settings.enabled = enabled;
        settings.threshold_db =
            sanitize_threshold(f32::from_bits(self.threshold_db.load(Ordering::Acquire)));
        settings.ratio = sanitize_ratio(f32::from_bits(self.ratio.load(Ordering::Acquire)));
        settings.attack_ms = sanitize_ms(
            f32::from_bits(self.attack_ms.load(Ordering::Acquire)),
            COMP_ATTACK_MIN_MS,
            COMP_ATTACK_MAX_MS,
            COMP_ATTACK_DEFAULT_MS,
        );
        settings.release_ms = sanitize_ms(
            f32::from_bits(self.release_ms.load(Ordering::Acquire)),
            COMP_RELEASE_MIN_MS,
            COMP_RELEASE_MAX_MS,
            COMP_RELEASE_DEFAULT_MS,
        );
        settings.knee_db = sanitize_knee(f32::from_bits(self.knee_db.load(Ordering::Acquire)));
        settings.makeup_db =
            sanitize_makeup(f32::from_bits(self.makeup_db.load(Ordering::Acquire)));
        self.publish_block(block_from(settings, sample_rate));
    }

    /// Publish `block` and take the previous spare index as the next write.
    fn publish_block(&self, block: CompBlock) {
        let write_idx = self.write_idx.load(Ordering::Relaxed);
        if write_idx > 2 {
            return;
        }
        // SAFETY: `write_idx` is at most 2, and this producer owns that slot until the swap.
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
    fn poll(&self, read_idx: &mut u32, dst: &mut CompBlock) -> bool {
        let state = self.state.load(Ordering::Acquire);
        if !claim(self, state, read_idx, dst) {
            let state = self.state.load(Ordering::Acquire);
            return claim(self, state, read_idx, dst);
        }
        true
    }

    fn store_gr(&self, gr_db: f32) {
        let gr = if gr_db.is_finite() && gr_db < 0.0 {
            gr_db
        } else {
            0.0
        };
        self.gr_db.store(gr.to_bits(), Ordering::Release);
    }

    fn load_gr(&self) -> f32 {
        let gr = f32::from_bits(self.gr_db.load(Ordering::Acquire));
        if gr.is_finite() && gr < 0.0 { gr } else { 0.0 }
    }
}

fn claim(channel: &CompChannel, state: u32, read_idx: &mut u32, dst: &mut CompBlock) -> bool {
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
    // SAFETY: `spare` is at most 2, differs from the consumer index, and the CAS moved that slot to this consumer.
    unsafe {
        *dst = *channel.slots[spare as usize].get();
    }
    true
}

// SAFETY: [`CompWriter`] is the only producer. It is not `Clone` and not `Sync`, and publish takes `&mut self`, so safe code cannot publish from two threads.
// SAFETY: [`CompReader`] is the only consumer. It is not `Clone` and not `Sync`, and it owns `read_idx`. `poll` takes `&mut CompReader`.
// SAFETY: The producer writes a slot only while it owns `write_idx`. The consumer reads a slot only after `poll` has swapped that index out of the spare field.
// SAFETY: Those two indices are distinct, so the cells are not aliased. The parameter atomics and the gain-reduction atomic are synchronized on their own.
// SAFETY: A second producer or a second consumer would be a data race, and the handles do not allow one from safe code.
unsafe impl Sync for CompChannel {}

#[derive(Debug)]
struct StripCompRt {
    current: [f32; PACK],
    target: [f32; PACK],
    origin: [f32; PACK],
    env: f32,
    block_frames: u32,
    ramp_pos: u32,
    ramping: bool,
    bypass: bool,
    block_min_gr: f32,
}

impl StripCompRt {
    fn new() -> Self {
        let pack = bypass_block(48_000).pack;
        Self {
            current: pack,
            target: pack,
            origin: pack,
            env: 0.0,
            block_frames: 1,
            ramp_pos: 0,
            ramping: false,
            bypass: true,
            block_min_gr: 0.0,
        }
    }

    fn begin(&mut self, loaded: Option<CompBlock>, frames: usize, sample_rate: u32) {
        self.block_frames = frames.max(1) as u32;
        self.block_min_gr = 0.0;
        let previous = self.target;
        if let Some(loaded) = loaded
            && (loaded.sample_rate == sample_rate || loaded.is_bypass())
        {
            self.target = loaded.pack;
        }
        if self.current == self.target {
            self.ramping = false;
            self.bypass = is_unity(&self.current);
        } else if self.ramping && self.target == previous {
            self.bypass = false;
        } else {
            self.origin = self.current;
            self.ramp_pos = 0;
            self.ramping = true;
            self.bypass = false;
        }
    }

    fn tick_one(&mut self, left: f32, right: f32) -> (f32, f32) {
        let pack = self.pack_for();
        let left_f = if left.is_finite() { left } else { 0.0 };
        let right_f = if right.is_finite() { right } else { 0.0 };
        let level = left_f.abs().max(right_f.abs());
        self.env = follow(self.env, level, pack[ATTACK], pack[RELEASE]);
        if self.bypass || is_unity(&pack) {
            return (left, right);
        }
        let gr = curve_db(self.env, pack[THRESHOLD], pack[SLOPE], pack[KNEE]);
        let wet = pack[WET];
        let sample_gr = if wet > 0.0 { gr } else { 0.0 };
        if sample_gr < self.block_min_gr {
            self.block_min_gr = sample_gr;
        }
        let compressed = linear_from_db(gr) * pack[MAKEUP];
        let gain = 1.0 + (compressed - 1.0) * wet;
        if gain == 1.0 {
            return (left_f, right_f);
        }
        let gain = if gain.is_finite() { gain } else { 0.0 };
        let out_l = left_f * gain;
        let out_r = right_f * gain;
        (
            if out_l.is_finite() { out_l } else { 0.0 },
            if out_r.is_finite() { out_r } else { 0.0 },
        )
    }

    fn pack_for(&mut self) -> [f32; PACK] {
        if !self.ramping {
            return self.current;
        }
        let pos = self.ramp_pos.saturating_add(1);
        self.ramp_pos = pos;
        if pos >= COMP_RAMP_SAMPLES.max(1) {
            self.current = self.target;
            self.ramping = false;
            self.bypass = is_unity(&self.current);
            return self.current;
        }
        let t = pos as f32 / COMP_RAMP_SAMPLES.max(1) as f32;
        self.current = lerp(self.origin, self.target, t);
        self.bypass = false;
        self.current
    }

    fn reset_detector(&mut self) {
        let pack = bypass_block(48_000).pack;
        self.current = pack;
        self.target = pack;
        self.origin = pack;
        self.env = 0.0;
        self.ramp_pos = 0;
        self.ramping = false;
        self.bypass = true;
        self.block_min_gr = 0.0;
    }
}

fn follow(env: f32, level: f32, attack: f32, release: f32) -> f32 {
    let env = if env.is_finite() && env >= 0.0 {
        env
    } else {
        0.0
    };
    let level = if level.is_finite() && level >= 0.0 {
        level
    } else {
        0.0
    };
    let coeff = if level > env { attack } else { release };
    let coeff = if coeff.is_finite() {
        coeff.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let y = env + coeff * (level - env);
    if y.is_finite() {
        if y.abs() < 1.0e-15 { 0.0 } else { y }
    } else {
        level
    }
}

fn curve_db(level: f32, threshold_db: f32, slope: f32, knee_db: f32) -> f32 {
    if slope <= 0.0 || !level.is_finite() || level <= 0.0 {
        return 0.0;
    }
    let x = 20.0 * level.log10();
    if !x.is_finite() {
        return 0.0;
    }
    let gr = if knee_db <= 0.0 {
        let over = x - threshold_db;
        if over <= 0.0 { 0.0 } else { -over * slope }
    } else {
        let half = knee_db * 0.5;
        let lower = threshold_db - half;
        if x <= lower {
            0.0
        } else if x >= threshold_db + half {
            -(x - threshold_db) * slope
        } else {
            let t = x - lower;
            -slope * t * t / (2.0 * knee_db)
        }
    };
    if gr.is_finite() { gr.min(0.0) } else { 0.0 }
}

fn linear_from_db(db: f32) -> f32 {
    if db == 0.0 {
        return 1.0;
    }
    if !db.is_finite() {
        return 0.0;
    }
    let lin = 10f32.powf(db / 20.0);
    if lin.is_finite() { lin } else { 0.0 }
}

/// `1 - exp(-1 / (seconds * sample_rate))`.
///
/// A non-positive duration or a zero sample rate snaps (`coeff` 1). This is
/// the same formula as [`crate::params::one_pole_coeff`].
fn time_coeff(sample_rate: u32, seconds: f32) -> f32 {
    if sample_rate == 0 || !seconds.is_finite() || seconds <= 0.0 {
        return 1.0;
    }
    let denom = seconds * sample_rate as f32;
    if !denom.is_finite() || denom <= 0.0 {
        return 1.0;
    }
    let coeff = 1.0 - (-1.0 / denom).exp();
    if coeff.is_finite() {
        coeff.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn usable_rate(sample_rate: u32) -> Option<u32> {
    if (SAMPLE_RATE_MIN..SAMPLE_RATE_MAX).contains(&sample_rate) {
        Some(sample_rate)
    } else {
        None
    }
}

fn sanitize_threshold(db: f32) -> f32 {
    if db.is_finite() {
        db.clamp(COMP_THRESHOLD_MIN_DB, COMP_THRESHOLD_MAX_DB)
    } else {
        COMP_THRESHOLD_MAX_DB
    }
}

fn sanitize_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(COMP_RATIO_MIN, COMP_RATIO_MAX)
    } else {
        COMP_RATIO_MIN
    }
}

fn sanitize_ms(ms: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if ms.is_finite() {
        ms.clamp(min, max)
    } else {
        fallback
    }
}

fn sanitize_knee(db: f32) -> f32 {
    if db.is_finite() {
        db.clamp(COMP_KNEE_MIN_DB, COMP_KNEE_MAX_DB)
    } else {
        0.0
    }
}

fn sanitize_makeup(db: f32) -> f32 {
    if db.is_finite() {
        db.clamp(COMP_MAKEUP_MIN_DB, COMP_MAKEUP_MAX_DB)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn near(actual: f32, expected: f32, tol: f32) {
        assert!(
            (actual - expected).abs() <= tol,
            "{actual} != {expected} (tol {tol})"
        );
    }

    fn constant(level: f32, frames: usize) -> (Vec<f32>, Vec<f32>) {
        (vec![level; frames], vec![level; frames])
    }

    /// Silence for [`COMP_RAMP_SAMPLES`] so the parameter ramp finishes. The envelope stays 0.
    fn settle_params(runtime: &mut CompRuntime, reader: &mut CompReader, slot: usize, rate: u32) {
        let (mut left, mut right) = constant(0.0, COMP_RAMP_SAMPLES as usize);
        runtime.process_slot(reader, slot, &mut left, &mut right, rate);
        assert!(
            runtime.envelope(slot).abs() < 1.0e-8,
            "silence moved the envelope to {}",
            runtime.envelope(slot)
        );
    }

    fn engaged(rate: u32) -> CompSettings {
        let mut settings = CompSettings::bypass(rate);
        settings.set_enabled(true);
        settings.set_threshold_db(-18.0);
        settings.set_ratio(4.0);
        settings.set_attack_ms(10.0);
        settings.set_release_ms(100.0);
        settings.set_knee_db(0.0);
        settings.set_makeup_db(0.0);
        settings
    }

    #[test]
    fn defaults_are_bypassed_and_generic() {
        let settings = CompSettings::bypass(48_000);
        assert!(!settings.enabled());
        assert_eq!(settings.threshold_db(), -18.0);
        assert_eq!(settings.ratio(), 4.0);
        assert_eq!(settings.attack_ms(), 10.0);
        assert_eq!(settings.release_ms(), 100.0);
        assert_eq!(settings.knee_db(), 0.0);
        assert_eq!(settings.makeup_db(), 0.0);
        let (writer, reader) = open();
        let stored = writer.settings(0).unwrap();
        assert!(!stored.enabled());
        assert_eq!(stored.threshold_db(), COMP_THRESHOLD_DEFAULT_DB);
        assert_eq!(reader.gain_reduction_db(0), 0.0);
        assert_eq!(reader.master_gain_reduction_db(), 0.0);
        assert!(writer.settings(STRIP_SLOT_COUNT).is_none());
        assert_eq!(reader.gain_reduction_db(STRIP_SLOT_COUNT), 0.0);
        assert!(!writer.master_settings().enabled());
    }

    #[test]
    fn bypass_process_keeps_every_sample_bit_for_bit() {
        let (_writer, mut reader) = open();
        let mut runtime = CompRuntime::new();
        let left_in = [0.1f32, -0.25, 0.5, 1.0, -1.0, -0.0, 0.70710677, -0.003];
        let right_in = [-0.1f32, 0.25, -0.5, 0.125, 0.5, -1.0, 0.0, 0.003];
        assert_eq!(left_in[5].to_bits(), (-0.0f32).to_bits());
        let mut left = left_in;
        let mut right = right_in;
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        assert_eq!(left, left_in);
        assert_eq!(right, right_in);
        assert_eq!(left[5].to_bits(), (-0.0f32).to_bits());
        assert_eq!(reader.gain_reduction_db(0), 0.0);
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        assert_eq!(left, left_in);
        assert_eq!(left[5].to_bits(), (-0.0f32).to_bits());
        assert_eq!(reader.master_gain_reduction_db(), 0.0);
        assert!(runtime.strip_bypassed(0));
    }

    #[test]
    fn stored_makeup_does_not_apply_while_bypassed() {
        let (mut writer, mut reader) = open();
        assert!(writer.apply(0, CompUpdate::MakeupDb(12.0)));
        assert!(!writer.settings(0).unwrap().enabled());
        near(writer.settings(0).unwrap().makeup_db(), 12.0, 1.0e-5);
        let mut runtime = CompRuntime::new();
        let input = [0.25f32, -0.5, 0.125, -0.0];
        let mut left = input;
        let mut right = input;
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        assert_eq!(left, input);
        assert_eq!(right, input);
        assert_eq!(reader.gain_reduction_db(0), 0.0);
    }

    #[test]
    fn static_curve_matches_the_hard_and_soft_knee() {
        near(static_reduction_db(-6.0, -18.0, 4.0, 0.0), -9.0, 1.0e-4);
        near(static_reduction_db(-18.0, -18.0, 4.0, 0.0), 0.0, 1.0e-5);
        near(static_reduction_db(-30.0, -18.0, 4.0, 0.0), 0.0, 1.0e-5);
        near(static_reduction_db(0.0, -20.0, 1.0, 0.0), 0.0, 1.0e-5);
        near(static_reduction_db(0.0, -20.0, 20.0, 0.0), -19.0, 1.0e-4);
        near(static_reduction_db(-23.0, -20.0, 4.0, 6.0), 0.0, 1.0e-5);
        near(static_reduction_db(-20.0, -20.0, 4.0, 6.0), -0.5625, 1.0e-4);
        near(static_reduction_db(-17.0, -20.0, 4.0, 6.0), -2.25, 1.0e-4);
        near(static_reduction_db(-10.0, -20.0, 4.0, 6.0), -7.5, 1.0e-4);
        assert_eq!(static_reduction_db(f32::NAN, -18.0, 4.0, 0.0), 0.0);
        assert_eq!(static_reduction_db(-6.0, -18.0, f32::NAN, 0.0), 0.0);
        let settings = engaged(48_000);
        near(settings.static_reduction_db(-6.0), -9.0, 1.0e-4);
    }

    #[test]
    fn non_finite_parameters_do_not_increase_compression() {
        let (mut writer, _reader) = open();
        assert!(writer.apply(0, CompUpdate::Enabled(true)));
        assert!(writer.apply(0, CompUpdate::ThresholdDb(f32::NAN)));
        assert_eq!(writer.settings(0).unwrap().threshold_db(), 0.0);
        assert!(writer.apply(0, CompUpdate::ThresholdDb(40.0)));
        assert_eq!(writer.settings(0).unwrap().threshold_db(), 0.0);
        assert!(writer.apply(0, CompUpdate::ThresholdDb(-100.0)));
        assert_eq!(writer.settings(0).unwrap().threshold_db(), -60.0);
        assert!(writer.apply(0, CompUpdate::Ratio(f32::INFINITY)));
        assert_eq!(writer.settings(0).unwrap().ratio(), 1.0);
        assert!(writer.apply(0, CompUpdate::Ratio(0.1)));
        assert_eq!(writer.settings(0).unwrap().ratio(), 1.0);
        assert!(writer.apply(0, CompUpdate::Ratio(100.0)));
        assert_eq!(writer.settings(0).unwrap().ratio(), 20.0);
        assert!(writer.apply(0, CompUpdate::AttackMs(f32::NAN)));
        assert_eq!(writer.settings(0).unwrap().attack_ms(), 10.0);
        assert!(writer.apply(0, CompUpdate::AttackMs(0.0)));
        near(writer.settings(0).unwrap().attack_ms(), 0.1, 1.0e-6);
        assert!(writer.apply(0, CompUpdate::ReleaseMs(-5.0)));
        near(writer.settings(0).unwrap().release_ms(), 1.0, 1.0e-6);
        assert!(writer.apply(0, CompUpdate::KneeDb(f32::NAN)));
        assert_eq!(writer.settings(0).unwrap().knee_db(), 0.0);
        assert!(writer.apply(0, CompUpdate::KneeDb(40.0)));
        assert_eq!(writer.settings(0).unwrap().knee_db(), 24.0);
        assert!(writer.apply(0, CompUpdate::MakeupDb(f32::NAN)));
        assert_eq!(writer.settings(0).unwrap().makeup_db(), 0.0);
        assert!(writer.apply(0, CompUpdate::MakeupDb(40.0)));
        assert_eq!(writer.settings(0).unwrap().makeup_db(), 24.0);
        assert!(!writer.apply(u16::MAX, CompUpdate::Enabled(true)));
        assert!(!writer.set_sample_rate(0));
        assert!(!writer.set_sample_rate(7_999));
        assert!(writer.apply(0, CompUpdate::Reset));
        assert!(!writer.settings(0).unwrap().enabled());
        assert_eq!(writer.settings(0).unwrap().ratio(), COMP_RATIO_DEFAULT);
        assert_eq!(
            writer.settings(0).unwrap().threshold_db(),
            COMP_THRESHOLD_DEFAULT_DB
        );
    }

    #[test]
    fn time_coeff_matches_the_fader_one_pole() {
        for rate in [44_100_u32, 48_000] {
            for ms in [0.1_f32, 10.0, 100.0, 200.0] {
                let got = time_coeff(rate, ms * 0.001);
                let want = crate::params::one_pole_coeff(rate, ms * 0.001);
                assert_eq!(got.to_bits(), want.to_bits(), "rate {rate} ms {ms}");
            }
        }
        assert_eq!(time_coeff(0, 0.01), 1.0);
    }

    #[test]
    fn settled_constant_matches_the_static_curve() {
        for rate in [44_100_u32, 48_000] {
            let settings = engaged(rate);
            let (mut writer, mut reader) = open();
            assert!(writer.set_sample_rate(rate));
            assert!(writer.apply_settings(0, settings));
            let mut runtime = CompRuntime::new();
            settle_params(&mut runtime, &mut reader, 0, rate);
            let amp = db::db_to_linear(-6.0);
            let attack_n = (settings.attack_ms() * 0.001 * rate as f32).round() as usize;
            let frames = attack_n * 5;
            let (mut left, mut right) = constant(amp, frames);
            runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
            let env = runtime.envelope(0);
            let env_err = (env - amp).abs() / amp;
            assert!(
                env_err < 0.01,
                "envelope {env} should be within 1% of {amp} after 5 attack constants"
            );
            let gr = static_reduction_db(-6.0, -18.0, 4.0, 0.0);
            assert!((gr - (-9.0)).abs() < 1.0e-3);
            let want = amp * db::db_to_linear(gr);
            near(left[frames - 1], want, want.abs() * 0.01 + 1.0e-5);
            near(right[frames - 1], want, want.abs() * 0.01 + 1.0e-5);
            let published = reader.gain_reduction_db(0);
            near(published, gr, 0.1);
            assert!(
                published < -8.0,
                "settled GR {published} should be near −9 dB"
            );
            assert_eq!(reader.gain_reduction_db(1), 0.0);
            assert_eq!(reader.master_gain_reduction_db(), 0.0);
            assert!(!runtime.strip_bypassed(0));
        }
    }

    #[test]
    fn attack_reaches_one_time_constant() {
        for rate in [44_100_u32, 48_000] {
            let settings = engaged(rate);
            let (mut writer, mut reader) = open();
            assert!(writer.set_sample_rate(rate));
            assert!(writer.apply_settings(0, settings));
            let mut runtime = CompRuntime::new();
            settle_params(&mut runtime, &mut reader, 0, rate);
            let amp = db::db_to_linear(-6.0);
            let n = (0.010 * rate as f32).round() as usize;
            assert_eq!(n, if rate == 48_000 { 480 } else { 441 });
            let (mut left, mut right) = constant(amp, n);
            runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
            let env = runtime.envelope(0);
            let want_env = amp * (1.0 - std::f32::consts::E.recip());
            let err = (env - want_env).abs() / amp;
            assert!(
                err < 0.005,
                "rate {rate}: envelope {env} vs one time constant {want_env}"
            );
            assert!(
                (env - amp).abs() / amp > 0.2,
                "one attack constant should not already be settled"
            );
            let gr = static_reduction_db(db::linear_to_db(env), -18.0, 4.0, 0.0);
            let want = amp * db::db_to_linear(gr);
            near(left[n - 1], want, 1.0e-5);
            near(right[n - 1], left[n - 1], 1.0e-6);
            near(reader.gain_reduction_db(0), gr, 1.0e-3);
            assert!(gr < -1.0, "the envelope should already be over the knee");
            assert!(
                (reader.gain_reduction_db(0) - (-9.0)).abs() > 1.0,
                "attack GR should not already be the settled −9 dB"
            );
        }
    }

    #[test]
    fn release_reaches_one_time_constant() {
        for rate in [44_100_u32, 48_000] {
            let settings = engaged(rate);
            let (mut writer, mut reader) = open();
            assert!(writer.set_sample_rate(rate));
            assert!(writer.apply_settings(0, settings));
            let mut runtime = CompRuntime::new();
            settle_params(&mut runtime, &mut reader, 0, rate);
            let loud = db::db_to_linear(-6.0);
            let quiet = db::db_to_linear(-24.0);
            let attack_n = (0.010 * rate as f32).round() as usize;
            let (mut left, mut right) = constant(loud, attack_n * 5);
            runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
            let env0 = runtime.envelope(0);
            assert!((env0 - loud).abs() / loud < 0.01);
            let n = (0.100 * rate as f32).round() as usize;
            assert_eq!(n, if rate == 48_000 { 4_800 } else { 4_410 });
            let (mut left, mut right) = constant(quiet, n);
            runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
            let env = runtime.envelope(0);
            let want_env = quiet + (env0 - quiet) * std::f32::consts::E.recip();
            let gap = env0 - quiet;
            let err = (env - want_env).abs() / gap;
            assert!(
                err < 0.005,
                "rate {rate}: release envelope {env} vs {want_env}"
            );
            assert!(env < env0 && env > quiet);
            let gr_end = static_reduction_db(db::linear_to_db(env), -18.0, 4.0, 0.0);
            let want = quiet * db::db_to_linear(gr_end);
            near(left[n - 1], want, 1.0e-5);
            near(right[n - 1], want, 1.0e-5);
            let gr_start = static_reduction_db(db::linear_to_db(env0), -18.0, 4.0, 0.0);
            near(reader.gain_reduction_db(0), gr_start, 0.15);
            assert!(reader.gain_reduction_db(0) < gr_end);
        }
    }

    #[test]
    fn gain_reduction_is_published_for_the_strip_and_the_master() {
        let rate = 48_000;
        let mut strip = engaged(rate);
        strip.set_attack_ms(0.1);
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(writer.apply_settings(0, strip));
        let mut runtime = CompRuntime::new();
        settle_params(&mut runtime, &mut reader, 0, rate);
        let amp = db::db_to_linear(-6.0);
        let frames = 64;
        let (mut left, mut right) = constant(amp, frames);
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        let gr = reader.gain_reduction_db(0);
        near(gr, -9.0, 0.15);
        assert_eq!(reader.gain_reduction_db(1), 0.0);
        assert_eq!(reader.master_gain_reduction_db(), 0.0);

        let mut master = engaged(rate);
        master.set_attack_ms(0.1);
        master.set_threshold_db(-12.0);
        writer.apply_master_settings(master);
        let (mut left, mut right) = constant(0.0, COMP_RAMP_SAMPLES as usize);
        runtime.process_master(&mut reader, &mut left, &mut right, rate);
        let (mut left, mut right) = constant(amp, frames);
        runtime.process_master(&mut reader, &mut left, &mut right, rate);
        near(reader.master_gain_reduction_db(), -4.5, 0.15);
        near(reader.gain_reduction_db(0), gr, 1.0e-4);
        assert!(reader.master_gain_reduction_db() < -4.0);

        writer.apply_master(CompUpdate::Enabled(false));
        let (mut left, mut right) = constant(amp, COMP_RAMP_SAMPLES as usize);
        runtime.process_master(&mut reader, &mut left, &mut right, rate);
        let (mut left, mut right) = constant(amp, 8);
        let before = left.clone();
        runtime.process_master(&mut reader, &mut left, &mut right, rate);
        assert_eq!(left, before);
        assert_eq!(right, before);
        assert_eq!(
            reader.master_gain_reduction_db(),
            0.0,
            "master gain reduction returns to 0 after the disable ramp"
        );
    }

    #[test]
    fn makeup_does_not_change_published_gain_reduction() {
        let rate = 48_000;
        let mut settings = engaged(rate);
        settings.set_attack_ms(0.1);
        settings.set_makeup_db(6.0);
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(writer.apply_settings(0, settings));
        let mut runtime = CompRuntime::new();
        settle_params(&mut runtime, &mut reader, 0, rate);
        let amp = db::db_to_linear(-6.0);
        let (mut left, mut right) = constant(amp, 64);
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        near(reader.gain_reduction_db(0), -9.0, 0.15);
        let gr_only = amp * db::db_to_linear(-9.0);
        let with_makeup = gr_only * db::db_to_linear(6.0);
        near(left[63], with_makeup, with_makeup.abs() * 0.02);
        assert!(left[63] > gr_only);
    }

    #[test]
    fn a_loud_channel_reduces_both_sides() {
        let rate = 48_000;
        let mut settings = engaged(rate);
        settings.set_attack_ms(0.1);
        settings.set_threshold_db(-12.0);
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(writer.apply_settings(0, settings));
        let mut runtime = CompRuntime::new();
        settle_params(&mut runtime, &mut reader, 0, rate);
        let loud = db::db_to_linear(-6.0);
        let quiet = db::db_to_linear(-24.0);
        let mut left = vec![loud; 64];
        let mut right = vec![quiet; 64];
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        let gain = db::db_to_linear(-4.5);
        near(left[63], loud * gain, loud * gain * 0.02);
        near(right[63], quiet * gain, quiet * gain * 0.02 + 1.0e-6);
        assert!(right[63] < quiet);
        near(reader.gain_reduction_db(0), -4.5, 0.15);
    }

    #[test]
    fn below_threshold_publishes_zero_and_keeps_the_sample() {
        let rate = 48_000;
        let mut settings = engaged(rate);
        settings.set_threshold_db(0.0);
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(writer.apply_settings(0, settings));
        let mut runtime = CompRuntime::new();
        let input = [0.1f32, -0.2, 0.05, -0.0];
        let mut left = input;
        let mut right = input;
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        assert_eq!(left, input);
        assert_eq!(right, input);
        assert_eq!(left[3].to_bits(), (-0.0f32).to_bits());
        assert_eq!(reader.gain_reduction_db(0), 0.0);
    }

    #[test]
    fn a_wrong_rate_publication_is_not_adopted() {
        let (mut writer, mut reader) = open();
        assert!(writer.apply(0, CompUpdate::Enabled(true)));
        let mut runtime = CompRuntime::new();
        let mut left = vec![0.5f32; 32];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 44_100);
        assert!(
            runtime.strip_bypassed(0),
            "48 kHz coefficients must not run at 44.1 kHz"
        );
        assert_eq!(left[0], 0.5);
        assert_eq!(reader.gain_reduction_db(0), 0.0);
        assert!(writer.set_sample_rate(44_100));
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 44_100);
        assert!(!runtime.strip_bypassed(0));
        assert!(writer.set_sample_rate(48_000));
        assert!(writer.apply(0, CompUpdate::Enabled(false)));
        let mut left = vec![0.5f32; COMP_RAMP_SAMPLES as usize];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 44_100);
        assert!(
            runtime.strip_bypassed(0),
            "a bypass block is adopted when its sample rate does not match"
        );
        let mut left = vec![0.5f32; 8];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 44_100);
        assert_eq!(left, vec![0.5f32; 8]);
        assert_eq!(reader.gain_reduction_db(0), 0.0);
    }

    #[test]
    fn one_block_finishes_the_parameter_ramp() {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(48_000));
        assert!(writer.apply(0, CompUpdate::Enabled(true)));
        assert!(writer.apply(0, CompUpdate::AttackMs(0.1)));
        let mut runtime = CompRuntime::new();
        let amp = db::db_to_linear(-6.0);
        let frames = COMP_RAMP_SAMPLES as usize;
        let mut left = vec![amp; frames];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        assert!(
            !runtime.ramping(0),
            "a block of COMP_RAMP_SAMPLES finishes the crossfade"
        );
        assert!(!runtime.strip_bypassed(0));
        near(reader.gain_reduction_db(0), -9.0, 0.15);
    }

    #[test]
    fn short_quanta_ramp_across_256_samples_and_the_step_stays_bounded() {
        let rate = 48_000u32;
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        let mut runtime = CompRuntime::new();
        let amp = db::db_to_linear(-6.0);
        let warm = (0.010 * rate as f32).round() as usize * 8;
        let (mut left, mut right) = constant(amp, warm);
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        let env = runtime.envelope(0);
        assert!((env - amp).abs() / amp < 0.01);
        assert_eq!(reader.gain_reduction_db(0), 0.0);
        assert!(writer.apply(0, CompUpdate::Enabled(true)));
        assert!(writer.apply(0, CompUpdate::AttackMs(0.1)));
        let quantum = 32usize;
        let mut produced = Vec::new();
        let mut left = vec![amp; quantum];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        produced.extend_from_slice(&left);
        assert!(
            runtime.ramping(0),
            "a 32-sample quantum must not finish the gain ramp"
        );
        let mut done = quantum;
        while done < COMP_RAMP_SAMPLES as usize {
            let n = quantum.min(COMP_RAMP_SAMPLES as usize - done);
            let mut left = vec![amp; n];
            let mut right = left.clone();
            runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
            produced.extend_from_slice(&left);
            done += n;
        }
        assert_eq!(produced.len(), COMP_RAMP_SAMPLES as usize);
        assert!(!runtime.ramping(0));
        assert!(!runtime.strip_bypassed(0));
        let settled = amp * db::db_to_linear(-9.0);
        let mut max_jump = 0.0f32;
        for pair in produced.windows(2) {
            max_jump = max_jump.max((pair[1] - pair[0]).abs());
        }
        let full = (amp - settled).abs();
        assert!(
            max_jump < full / 32.0,
            "ramp jump {max_jump} is too large for a {COMP_RAMP_SAMPLES}-sample crossfade (full step {full})"
        );
        near(produced[produced.len() - 1], settled, settled.abs() * 0.02);
    }

    #[test]
    fn gain_reduction_publishes_the_block_minimum() {
        let rate = 48_000u32;
        let mut settings = engaged(rate);
        settings.set_attack_ms(0.1);
        settings.set_release_ms(1.0);
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(writer.apply_settings(0, settings));
        let mut runtime = CompRuntime::new();
        settle_params(&mut runtime, &mut reader, 0, rate);
        let loud = db::db_to_linear(-6.0);
        let quiet = db::db_to_linear(-40.0);
        let (mut left, mut right) = constant(loud, 64);
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        let hot = reader.gain_reduction_db(0);
        assert!(hot < -8.0, "settled GR {hot} should be near -9 dB");
        let frames = 480usize;
        let (mut left, mut right) = constant(quiet, frames);
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, rate);
        near(left[frames - 1], quiet, 1.0e-4);
        near(right[frames - 1], quiet, 1.0e-4);
        let published = reader.gain_reduction_db(0);
        assert!(
            published < -8.0,
            "block minimum keeps the transient, got {published}"
        );
        near(published, hot, 0.5);
    }

    #[test]
    fn reset_slot_returns_to_a_bypass_and_clears_gain_reduction() {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(48_000));
        assert!(writer.apply(2, CompUpdate::Enabled(true)));
        assert!(writer.apply(2, CompUpdate::AttackMs(0.1)));
        let mut runtime = CompRuntime::new();
        let amp = db::db_to_linear(-6.0);
        let mut left = vec![amp; 64];
        let mut right = left.clone();
        runtime.process_slot(&mut reader, 2, &mut left, &mut right, 48_000);
        assert!(reader.gain_reduction_db(2) < -1.0);
        assert!(runtime.envelope(2) > 0.1);
        writer.reset_slot(2);
        runtime.reset_slot(2);
        assert_eq!(runtime.envelope(2), 0.0);
        assert_eq!(reader.gain_reduction_db(2), 0.0);
        let input = [0.2f32, -0.2, 0.0, 0.5];
        let mut left = input;
        let mut right = input;
        runtime.process_slot(&mut reader, 2, &mut left, &mut right, 48_000);
        assert_eq!(left, input);
        assert!(runtime.strip_bypassed(2));
        assert!(runtime.envelope(2) > 0.0);
        assert_eq!(reader.gain_reduction_db(2), 0.0);
        assert!(!writer.settings(2).unwrap().enabled());
    }

    #[test]
    fn non_finite_input_does_not_poison_the_envelope() {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(48_000));
        assert!(writer.apply(0, CompUpdate::Enabled(true)));
        let mut runtime = CompRuntime::new();
        settle_params(&mut runtime, &mut reader, 0, 48_000);
        let mut left = [f32::NAN, f32::INFINITY, 0.1];
        let mut right = [0.1, f32::NEG_INFINITY, 0.1];
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        assert!(runtime.envelope(0).is_finite());
        assert!(
            left.iter()
                .chain(right.iter())
                .all(|sample| sample.is_finite())
        );
    }

    #[test]
    fn enabled_neutral_ratio_is_unity() {
        let (mut writer, mut reader) = open();
        assert!(writer.apply(0, CompUpdate::Enabled(true)));
        assert!(writer.apply(0, CompUpdate::Ratio(1.0)));
        assert!(writer.apply(0, CompUpdate::MakeupDb(0.0)));
        let mut runtime = CompRuntime::new();
        let input = [0.8f32, -0.4, 0.2, -0.0];
        let mut left = input;
        let mut right = input;
        runtime.process_slot(&mut reader, 0, &mut left, &mut right, 48_000);
        assert_eq!(left, input);
        assert_eq!(right, input);
        assert_eq!(reader.gain_reduction_db(0), 0.0);
    }

    #[test]
    fn published_blocks_do_not_tear() {
        let channel = Arc::new(CompChannel::new());
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
            let mut dst = bypass_block(48_000);
            let mut saw = 0u32;
            while saw == 0 || !done.load(Ordering::Acquire) {
                if consumer.poll(&mut read_idx, &mut dst) {
                    let bits = dst.stamp_uniform().expect("torn compressor block");
                    assert_eq!(dst.sample_rate, bits);
                    saw = saw.saturating_add(1);
                }
            }
            if consumer.poll(&mut read_idx, &mut dst) {
                let bits = dst.stamp_uniform().expect("torn compressor block");
                assert_eq!(dst.sample_rate, bits);
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
        let channel = CompChannel::new();
        let mut read_idx = INITIAL_READ;
        let mut dst = bypass_block(48_000);
        for stamp in 1..500u32 {
            channel.publish_block(marker(stamp));
            assert!(channel.poll(&mut read_idx, &mut dst));
            assert_eq!(dst.stamp_uniform(), Some(stamp));
            let write_idx = channel.write_idx.load(Ordering::Relaxed);
            let spare = channel.state.load(Ordering::Relaxed) & 0b11;
            let mut indices = [write_idx, read_idx, spare];
            indices.sort_unstable();
            assert_eq!(indices, [0, 1, 2], "aliased triple-buffer indices");
        }
    }

    #[test]
    fn block_cost_is_measured() {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(48_000));
        assert!(writer.apply(0, CompUpdate::Enabled(true)));
        assert!(writer.apply_master(CompUpdate::Enabled(true)));
        let mut runtime = CompRuntime::new();
        let frames = 256usize;
        let mut left = vec![0.5f32; frames];
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
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let machine = std::env::var("OMAMIX_MACHINE").unwrap_or_else(|_| "unset".to_string());
        eprintln!(
            "MEASURED compressor block: {per:?} for {frames} stereo frames, strip and master, profile={profile}, OMAMIX_MACHINE={machine}"
        );
        std::hint::black_box((&left, &right));
        std::hint::black_box(per);
    }

    fn marker(stamp: u32) -> CompBlock {
        let sample = f32::from_bits(stamp);
        CompBlock {
            pack: [sample; PACK],
            sample_rate: stamp,
        }
    }
}

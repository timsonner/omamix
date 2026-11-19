//! Master lookahead true-peak limiter.
//!
//! The detector is the same 4x windowed-sinc interpolator libebur128 uses
//! for true peak (49 taps, Hann window, factor 4). Stereo channels are
//! linked: the gain follows the louder channel. The audio is delayed by the
//! lookahead, and the gain for a peak is on its way down before that peak
//! leaves the delay.
//!
//! Lookahead is a parameter. The default is 5 ms. `0` ms is a clipper: the
//! delay is off, and each output sample is held to the guarded ceiling.
//! The feasible sample is a closed form over a fixed set of kernel
//! alignments. It does not search the ring. If no finite sample fits, the
//! clipper emits 0 and leaves the other history samples in place, so the
//! ring still matches what was played.
//! The stored ceiling defaults to -1 dBTP. A fixed guard
//! ([`LIMIT_GUARD_DB`]) sits under that ceiling inside the gain computer so
//! a later 4x reading stays at or under the published ceiling. Release is a
//! fixed 100 ms one-pole, the same shape as the fader smoother. It is not a
//! measured room and not another app's preset.
//!
//! The master starts on: ceiling -1 dBTP, lookahead 5 ms.
//!
//! Reported latency is the delay that is actually in the signal. A bypassed
//! master reports 0. Lookahead 0 reports 0. Otherwise it is
//! [`lookahead_samples`] at the rate the callback is running. The loop
//! thread copies that number into the filter `ProcessLatency` param as
//! samples only (`ns` is 0, because PipeWire adds the two fields). Output
//! port `Latency` adds the upstream rate to that delay. Only the rate is
//! carried: peer `ns` and quantum are not added, and the written pod sets
//! both to 0. `node.latency` stays the quantum contract.
//!
//! [`LimitWriter`] is the only producer. It is not `Clone` and not `Sync`,
//! and publishing takes `&mut self`. [`LimitReader`] owns the consumer index.
//! It is not `Clone` and not `Sync`, and `process` takes `&mut LimitReader`.
//! The callback only loads the block. It does not build the interpolator
//! or the clip plan, allocate, or take a lock. The clip plan is built in
//! [`LimitRuntime::new`], off the audio thread.
//!
//! A 0 ms request is stored as [`LIMIT_LOOKAHEAD_DEFAULT_MS`] when
//! [`zero_ms_limiter_cap`] is 0. [`LimitWriter::apply_master`] returns
//! [`LimitApply::Demoted`] for that request. The same rewrite from a rate
//! or quantum change is queued for the loop thread, which logs one line.
//! The flag stays until an explicit lookahead, a reset, or a disable. A
//! later longer quantum does not put a demoted master back on 0 ms. Wet
//! snaps when a block is adopted. Ceiling and release ramp across
//! [`LIMIT_RAMP_SAMPLES`], including when the quantum is shorter than that.
//! The last sample of the ramp uses the new values. The delay line keeps
//! running while the master is bypassed, so a later lookahead or enable
//! change crossfades to the new delay instead of clearing the line. A block
//! published at another rate is still applied: the callback recomputes the
//! delay and the release coefficient at the rate it is running.
//!
//! `callback_quantum` has one production writer: the audio callback, through
//! [`LimitReader::note_callback_quantum`]. [`LimitWriter::note_callback_quantum`]
//! exists only in tests.
//!
//! Gain reduction is one atomic, in dB, negative when the signal is reduced
//! and 0 when the master is bypassed or the envelope is under the ceiling.
//! The callback does not hold or decay it.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::params;

/// Published ceiling. The gain computer aims a little under this.
pub const LIMIT_CEILING_DEFAULT_DB: f32 = -1.0;
/// Lowest stored ceiling.
pub const LIMIT_CEILING_MIN_DB: f32 = -24.0;
/// Highest stored ceiling. 0 dBTP.
pub const LIMIT_CEILING_MAX_DB: f32 = 0.0;

/// Lookahead. `0` is off.
pub const LIMIT_LOOKAHEAD_DEFAULT_MS: f32 = 5.0;
/// Shortest lookahead. `0` is the clipper.
pub const LIMIT_LOOKAHEAD_MIN_MS: f32 = 0.0;
/// Longest lookahead.
pub const LIMIT_LOOKAHEAD_MAX_MS: f32 = 20.0;

/// Quantum the 0 ms reference was scaled from. One 256-frame block at 48 kHz.
pub const ZERO_MS_REFERENCE_FRAMES: u32 = 256;
/// Rate the 0 ms reference was scaled from.
pub const ZERO_MS_REFERENCE_RATE: u32 = 48_000;
/// Reference count for [`zero_ms_limiter_cap`].
///
/// At 48 kHz and 256 frames the scaled count is this value. The master 0 ms
/// clipper stays when the scaled count is at least 1. A shorter quantum or a
/// higher rate can scale it to 0, and that stores the request as 5 ms.
pub const ZERO_MS_CAP_48K_256: u32 = 27;

/// Scaled 0 ms room at `sample_rate` and `quantum_frames`.
///
/// [`ZERO_MS_CAP_48K_256`] is the reference. A higher rate or a quantum
/// shorter than [`ZERO_MS_REFERENCE_FRAMES`] shrinks the count. A longer
/// quantum does not raise it. `0` means a 0 ms master request is stored as
/// [`LIMIT_LOOKAHEAD_DEFAULT_MS`].
pub fn zero_ms_limiter_cap(sample_rate: u32, quantum_frames: u32) -> u32 {
    let rate = u64::from(usable_rate(sample_rate).unwrap_or(ZERO_MS_REFERENCE_RATE));
    let frames = u64::from(quantum_frames.max(1)).min(u64::from(ZERO_MS_REFERENCE_FRAMES));
    let scaled = u64::from(ZERO_MS_CAP_48K_256) * frames * u64::from(ZERO_MS_REFERENCE_RATE)
        / (u64::from(ZERO_MS_REFERENCE_FRAMES) * rate);
    u32::try_from(scaled).unwrap_or(0)
}

/// Result of storing one master limiter field.
///
/// [`LimitApply::Demoted`] means a 0 ms request was stored as
/// [`LIMIT_LOOKAHEAD_DEFAULT_MS`] because [`zero_ms_limiter_cap`] was 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitApply {
    /// The requested field was stored.
    Applied,
    /// A 0 ms request was stored as the default lookahead.
    Demoted,
}

impl LimitApply {
    /// Stored, including a cap demotion.
    pub fn accepted(self) -> bool {
        matches!(self, Self::Applied | Self::Demoted)
    }

    /// The cap rewrote 0 ms to the default lookahead.
    pub fn demoted(self) -> bool {
        matches!(self, Self::Demoted)
    }
}

/// The only producer. Not `Clone`. Not `Sync`.
///
/// [`open`] returns this and the [`LimitReader`] the audio callback loads.
/// Publishing takes `&mut self`.
///
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::limit::LimitWriter>();
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::limit::LimitWriter>();
/// }
/// ```
#[derive(Debug)]
pub struct LimitWriter {
    inner: Arc<LimitShared>,
    /// Quantum changes that rewrote 0 ms. [`Self::apply_master`] does not
    /// push here: it returns [`LimitApply::Demoted`] instead.
    demotions: Vec<Option<u16>>,
    _unshared: PhantomData<Cell<()>>,
}

/// Publications for the audio callback.
///
/// Not `Clone` and not `Sync`. This handle owns the consumer index. It does
/// not publish.
///
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::limit::LimitReader>();
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::limit::LimitReader>();
/// }
/// ```
///
/// A shared `&LimitReader` cannot be passed to `process`, which takes
/// `&mut LimitReader`.
///
/// ```compile_fail,E0308
/// fn demo() {
///     let mut reader = omamix_core::limit::LimitReader::bypassed();
///     let mut a = omamix_core::params::StripMixer::new(48_000);
///     let mut b = omamix_core::params::StripMixer::new(48_000);
///     let bank = omamix_core::params::StripParamBank::new();
///     let meters = omamix_core::params::StripMeterBank::new();
///     let mut scopes = omamix_core::scope::ScopeBank::new();
///     let mut eq = omamix_core::eq::EqReader::flat();
///     let mut comp = omamix_core::comp::CompReader::bypassed();
///     let mut left = [0.0f32; 8];
///     let mut right = [0.0f32; 8];
///     let shared = &reader;
///     a.process(
///         &bank,
///         &mut eq,
///         &mut comp,
///         shared,
///         &meters,
///         &mut scopes,
///         &[],
///         &mut left,
///         &mut right,
///         48_000,
///     );
///     b.process(
///         &bank,
///         &mut eq,
///         &mut comp,
///         shared,
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
pub struct LimitReader {
    inner: Arc<LimitShared>,
    master_read: u32,
    _unshared: PhantomData<Cell<()>>,
}

#[derive(Debug)]
struct LimitShared {
    master: LimitChannel,
    sample_rate: AtomicU32,
    /// Quantum the control thread last enforced the 0 ms cap against.
    applied_quantum: AtomicU32,
    /// Quantum the last audio block reported.
    ///
    /// The audio callback is the only production writer. The loop thread
    /// copies this into [`Self::applied_quantum`].
    callback_quantum: AtomicU32,
}

/// Master on.
pub fn open() -> (LimitWriter, LimitReader) {
    open_inner(true)
}

fn open_inner(master_on: bool) -> (LimitWriter, LimitReader) {
    let inner = Arc::new(LimitShared {
        master: LimitChannel::new(master_on),
        sample_rate: AtomicU32::new(48_000),
        applied_quantum: AtomicU32::new(ZERO_MS_REFERENCE_FRAMES),
        callback_quantum: AtomicU32::new(ZERO_MS_REFERENCE_FRAMES),
    });
    let mut writer = LimitWriter {
        inner: Arc::clone(&inner),
        demotions: Vec::new(),
        _unshared: PhantomData,
    };
    let reader = LimitReader {
        inner,
        master_read: INITIAL_READ,
        _unshared: PhantomData,
    };
    writer.republish();
    (writer, reader)
}

impl LimitShared {
    fn store_callback_quantum(&self, frames: usize) {
        let frames = u32::try_from(frames).unwrap_or(u32::MAX).max(1);
        self.callback_quantum.store(frames, Ordering::Release);
    }
}

impl LimitWriter {
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate.load(Ordering::Acquire)
    }

    /// Rebuild the publication at `sample_rate`.
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
            self.republish();
            self.enforce_zero_ms_cap();
        }
        true
    }

    /// Quantum the 0 ms cap is enforced against.
    pub fn processing_quantum(&self) -> u32 {
        let quantum = self.inner.applied_quantum.load(Ordering::Acquire);
        if quantum == 0 {
            ZERO_MS_REFERENCE_FRAMES
        } else {
            quantum
        }
    }

    /// Last quantum the audio callback reported.
    pub fn callback_quantum(&self) -> u32 {
        self.inner.callback_quantum.load(Ordering::Acquire)
    }

    /// Record a quantum from a test. Production audio uses
    /// [`LimitReader::note_callback_quantum`] instead, so this build has one
    /// writer of `callback_quantum`.
    #[cfg(test)]
    pub fn note_callback_quantum(&self, frames: usize) {
        self.inner.store_callback_quantum(frames);
    }

    /// Enforce [`zero_ms_limiter_cap`] at `frames`. Returns whether the
    /// quantum changed. A count of 0 rewrites a 0 ms master to
    /// [`LIMIT_LOOKAHEAD_DEFAULT_MS`] and queues that demotion.
    pub fn set_processing_quantum(&mut self, frames: u32) -> bool {
        let frames = frames.max(1);
        let prev = self.inner.applied_quantum.swap(frames, Ordering::AcqRel);
        if prev == frames {
            return false;
        }
        self.enforce_zero_ms_cap();
        true
    }

    /// Store one master field and publish the block.
    ///
    /// A 0 ms lookahead that [`zero_ms_limiter_cap`] does not allow is stored
    /// as [`LIMIT_LOOKAHEAD_DEFAULT_MS`] and the result is
    /// [`LimitApply::Demoted`].
    pub fn apply_master(&mut self, update: LimitUpdate) -> LimitApply {
        let (update, demoted) = self.limit_zero_ms(update);
        self.store_update(update, demoted, false);
        if demoted {
            LimitApply::Demoted
        } else {
            LimitApply::Applied
        }
    }

    /// Replace the master snapshot and publish it.
    pub fn apply_master_settings(&mut self, settings: LimitSettings) {
        let requested_zero = settings_are_zero_ms(settings);
        let settings = self.fallback_settings(settings);
        self.inner.master.store_settings(settings);
        let demoted = requested_zero && !settings_are_zero_ms(settings);
        self.inner
            .master
            .cap_demoted
            .store(demoted, Ordering::Release);
        self.inner.master.publish_at(self.sample_rate());
    }

    /// True after the cap rewrote the master from 0 ms to the default lookahead.
    pub fn master_cap_demoted(&self) -> bool {
        self.inner.master.cap_demoted.load(Ordering::Acquire)
    }

    /// Quantum-change demotions since the last drain. `None` is the master.
    pub fn drain_cap_demotions(&mut self) -> Vec<Option<u16>> {
        std::mem::take(&mut self.demotions)
    }

    pub fn master_settings(&self) -> LimitSettings {
        channel_settings(self.sample_rate(), &self.inner.master)
    }

    pub fn master_gain_reduction_db(&self) -> f32 {
        self.inner.master.load_gr()
    }

    /// Delay the master inserts right now. 0 when it is bypassed or at 0 ms.
    pub fn master_latency_samples(&self) -> u32 {
        self.master_settings().latency_samples()
    }

    /// Samples from an input tap to the master output.
    pub fn reported_latency_samples(&self) -> u32 {
        self.master_latency_samples()
    }

    fn republish(&mut self) {
        self.inner.master.publish_at(self.sample_rate());
    }

    fn store_update(&mut self, update: LimitUpdate, cap_demoted: bool, record: bool) {
        let rate = self.sample_rate();
        self.inner.master.store(update, true, cap_demoted);
        self.inner.master.publish_at(rate);
        if cap_demoted && record {
            self.demotions.push(None);
        }
    }

    fn limit_zero_ms(&mut self, update: LimitUpdate) -> (LimitUpdate, bool) {
        let current = self.master_settings();
        let (enabled, lookahead, replacing_lookahead) = match update {
            LimitUpdate::Enabled(on) => (on, current.lookahead_ms(), false),
            LimitUpdate::LookaheadMs(ms) => (current.enabled(), sanitize_lookahead(ms), true),
            _ => return (update, false),
        };
        if !(enabled && lookahead == 0.0) || settings_are_zero_ms(current) {
            return (update, false);
        }
        let cap = zero_ms_limiter_cap(self.sample_rate(), self.processing_quantum());
        if cap >= 1 {
            return (update, false);
        }
        if replacing_lookahead {
            return (LimitUpdate::LookaheadMs(LIMIT_LOOKAHEAD_DEFAULT_MS), true);
        }
        self.store_update(
            LimitUpdate::LookaheadMs(LIMIT_LOOKAHEAD_DEFAULT_MS),
            true,
            false,
        );
        (update, true)
    }

    fn fallback_settings(&self, mut settings: LimitSettings) -> LimitSettings {
        if !settings_are_zero_ms(settings) || settings_are_zero_ms(self.master_settings()) {
            return settings;
        }
        let cap = zero_ms_limiter_cap(self.sample_rate(), self.processing_quantum());
        if cap >= 1 {
            return settings;
        }
        settings.set_lookahead_ms(LIMIT_LOOKAHEAD_DEFAULT_MS);
        settings
    }

    fn enforce_zero_ms_cap(&mut self) {
        if !settings_are_zero_ms(self.master_settings()) {
            return;
        }
        let cap = zero_ms_limiter_cap(self.sample_rate(), self.processing_quantum());
        if cap >= 1 {
            return;
        }
        self.store_update(
            LimitUpdate::LookaheadMs(LIMIT_LOOKAHEAD_DEFAULT_MS),
            true,
            true,
        );
    }
}

fn settings_are_zero_ms(settings: LimitSettings) -> bool {
    settings.enabled() && settings.lookahead_ms() == 0.0
}

impl LimitReader {
    /// A reader whose master was never switched on. Nothing can publish.
    pub fn bypassed() -> Self {
        open_inner(false).1
    }

    pub fn master_gain_reduction_db(&self) -> f32 {
        self.inner.master.load_gr()
    }

    fn poll_master(&mut self, sample_rate: u32) -> Option<LimitBlock> {
        let mut loaded = bypass_block(sample_rate);
        if self.inner.master.poll(&mut self.master_read, &mut loaded) {
            Some(loaded)
        } else {
            None
        }
    }

    fn store_master_gr(&self, gr_db: f32) {
        self.inner.master.store_gr(gr_db);
    }

    /// Record the quantum this audio block is running.
    ///
    /// This is the production writer of `callback_quantum`.
    pub(crate) fn note_callback_quantum(&self, frames: usize) {
        self.inner.store_callback_quantum(frames);
    }
}

fn channel_settings(sample_rate: u32, channel: &LimitChannel) -> LimitSettings {
    let mut settings = LimitSettings::bypass(sample_rate);
    settings.enabled = channel.enabled.load(Ordering::Acquire);
    settings.ceiling_db =
        sanitize_ceiling(f32::from_bits(channel.ceiling_db.load(Ordering::Acquire)));
    settings.lookahead_ms =
        sanitize_lookahead(f32::from_bits(channel.lookahead_ms.load(Ordering::Acquire)));
    settings
}

/// Audio-callback state for the master limiter.
///
/// Only the thread that calls [`crate::params::StripMixer::process`] may
/// touch this value. The delay line is allocated in [`Self::new`], and that
/// constructor builds the clip plan. `process` does not allocate and does
/// not initialize the plan.
#[derive(Debug)]
pub struct LimitRuntime {
    tables: InterpTables,
    master: MasterLimitRt,
}

#[cfg(target_arch = "x86_64")]
unsafe fn mxcsr() -> u32 {
    let mut value = 0u32;
    // SAFETY: `value` is a local u32. `stmxcsr` writes the SSE control word there.
    unsafe {
        core::arch::asm!(
            "stmxcsr [{ptr}]",
            ptr = in(reg) &mut value,
            options(nostack, preserves_flags)
        );
    }
    value
}

#[cfg(target_arch = "x86_64")]
unsafe fn set_mxcsr(value: u32) {
    // SAFETY: `value` is a local u32 holding an MXCSR bit pattern.
    unsafe {
        core::arch::asm!(
            "ldmxcsr [{ptr}]",
            ptr = in(reg) &value,
            options(nostack, preserves_flags)
        );
    }
}

fn flush_denormals(body: impl FnOnce()) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: x86_64 has SSE. The previous MXCSR is restored when this guard drops.
        struct Restore(u32);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    set_mxcsr(self.0);
                }
            }
        }
        let old = unsafe { mxcsr() };
        let _restore = Restore(old);
        unsafe {
            set_mxcsr(old | 0x8040);
        }
        body();
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        body();
    }
}

impl LimitRuntime {
    pub(crate) fn new() -> Self {
        let tables = InterpTables::new();
        ensure_clip_plan();
        Self {
            tables,
            master: MasterLimitRt::new(),
        }
    }

    /// Load the published master block and note this block's quantum.
    ///
    /// A 0 ms publication on a quantum whose cap is 0 is read as the default
    /// lookahead before the delay is applied. A later poll of that same 0 ms
    /// block then matches the delay already running, so the crossfade is not
    /// restarted.
    pub(crate) fn begin(&mut self, reader: &mut LimitReader, frames: usize, sample_rate: u32) {
        reader.note_callback_quantum(frames);
        let mut polled = reader.poll_master(sample_rate);
        let cap = zero_ms_limiter_cap(sample_rate, frames.max(1) as u32);
        let published_rate = polled
            .as_ref()
            .map(|block| block.sample_rate)
            .unwrap_or(sample_rate);
        let fallback = default_lookahead_samples(sample_rate, published_rate);
        if cap == 0
            && let Some(block) = polled.as_mut()
            && block.pack[WET] != 0.0
            && block.lookahead_ms == 0.0
        {
            block.lookahead_ms = LIMIT_LOOKAHEAD_DEFAULT_MS;
            block.lookahead = fallback;
        }
        self.master.begin(polled, frames, sample_rate);
        if cap == 0 && self.master.runs_clipper() && fallback > 0 {
            self.master.force_delay(fallback);
        }
    }

    pub(crate) fn apply_master(
        &mut self,
        reader: &LimitReader,
        left: &mut [f32],
        right: &mut [f32],
    ) {
        let frames = left
            .len()
            .min(right.len())
            .min(self.master.block_frames as usize);
        flush_denormals(|| {
            self.master
                .render(&self.tables, &mut left[..frames], &mut right[..frames])
        });
        if frames > 0 {
            reader.store_master_gr(reduction_db(self.master.last_applied));
        }
    }

    #[cfg(test)]
    fn process_master(
        &mut self,
        reader: &mut LimitReader,
        left: &mut [f32],
        right: &mut [f32],
        sample_rate: u32,
    ) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }
        self.begin(reader, frames, sample_rate);
        flush_denormals(|| self.master.render(&self.tables, left, right));
        reader.store_master_gr(reduction_db(self.master.last_applied));
    }

    #[cfg(test)]
    fn master_bypassed(&self) -> bool {
        self.master.bypass
    }

    #[cfg(test)]
    fn master_crossfade(&self) -> bool {
        self.master.xfade_on
    }

    #[cfg(test)]
    fn master_ramping(&self) -> bool {
        self.master.ramping
    }

    #[cfg(test)]
    fn master_ceiling_linear(&self) -> f32 {
        self.master.current[CEILING]
    }

    #[cfg(test)]
    fn detector_order_fault(&self) -> bool {
        self.master.mins.order_fault()
    }

    #[cfg(test)]
    fn stick_backward_clock(&mut self) {
        self.master.mins.push(10, 1.0, 4);
        self.master.mins.push(9, 0.5, 4);
    }
}

impl Default for LimitRuntime {
    fn default() -> Self {
        Self::new()
    }
}

pub const LIMIT_RELEASE_MS: f32 = 100.0;

/// Ceiling and release ramp length. Same count as the EQ coefficient ramp.
/// It does not follow the quantum.
pub const LIMIT_RAMP_SAMPLES: u32 = 256;

/// Subtracted from the published ceiling inside the detector.
///
/// A gain step and the 4× filter can land a fraction of a dB over the
/// number the gain computer saw. This guard keeps a later true-peak read
/// at or under the published ceiling. It is not a user control.
pub const LIMIT_GUARD_DB: f32 = 0.30;

/// Oversample factor. The spec asks for 4× at every rate.
pub const LIMIT_OVERSAMPLE: usize = 4;

const SAMPLE_RATE_MIN: u32 = 8_000;
const SAMPLE_RATE_MAX: u32 = 384_000;

const INTERP_TAPS: usize = 49;
const INTERP_FACTOR: usize = LIMIT_OVERSAMPLE;
const INTERP_DELAY: usize = INTERP_TAPS.div_ceil(INTERP_FACTOR);
/// Group delay of the interpolator, in input samples. `(taps - 1) / 2 / factor`.
const DETECT_DELAY_SAMPLES: u32 = ((INTERP_TAPS - 1) / 2 / INTERP_FACTOR) as u32;
const ALMOST_ZERO: f64 = 0.000001;

/// Longest delay the callback can ask for. 20 ms at the top of the rate range.
pub const MAX_LOOKAHEAD_SAMPLES: usize = (20.0 * (SAMPLE_RATE_MAX as f32) / 1000.0) as usize;
/// Power-of-two ring that holds [`MAX_LOOKAHEAD_SAMPLES`]. The mask replaces a divide.
const DELAY_RING: usize = 8192;
const DELAY_MASK: usize = DELAY_RING - 1;
const CEILING: usize = 0;
const RELEASE: usize = 1;
const WET: usize = 2;
const PACK: usize = 3;

const INITIAL_WRITE: u32 = 0;
const INITIAL_READ: u32 = 1;
const INITIAL_SPARE: u32 = 2;
const DIRTY: u32 = 0b100;

/// What the control thread asks to store. Applied outside `process`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LimitUpdate {
    Enabled(bool),
    /// Ceiling in dBTP. The published number, before the guard.
    CeilingDb(f32),
    /// Lookahead in milliseconds. `0` turns the delay off.
    LookaheadMs(f32),
    /// Factory state. The master goes back to on, at -1 dBTP, with 5 ms of lookahead.
    Reset,
}

/// One limiter snapshot.
///
/// Fields are private so a ceiling above 0 dBTP or a lookahead past 20 ms
/// cannot be stored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimitSettings {
    sample_rate: u32,
    enabled: bool,
    ceiling_db: f32,
    lookahead_ms: f32,
}

impl LimitSettings {
    /// Bypass at `sample_rate`, with the generic ceiling and lookahead behind
    /// the switch. An unusable rate becomes 48 kHz. The bypass itself does
    /// not change samples and does not add latency.
    pub fn bypass(sample_rate: u32) -> Self {
        Self {
            sample_rate: usable_rate(sample_rate).unwrap_or(48_000),
            enabled: false,
            ceiling_db: LIMIT_CEILING_DEFAULT_DB,
            lookahead_ms: LIMIT_LOOKAHEAD_DEFAULT_MS,
        }
    }

    /// Master factory state: on, −1 dBTP, 5 ms lookahead.
    pub fn master(sample_rate: u32) -> Self {
        let mut settings = Self::bypass(sample_rate);
        settings.enabled = true;
        settings
    }

    pub fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    pub fn ceiling_db(self) -> f32 {
        self.ceiling_db
    }

    pub fn lookahead_ms(self) -> f32 {
        self.lookahead_ms
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

    pub fn set_ceiling_db(&mut self, ceiling_db: f32) {
        self.ceiling_db = sanitize_ceiling(ceiling_db);
    }

    pub fn set_lookahead_ms(&mut self, lookahead_ms: f32) {
        self.lookahead_ms = sanitize_lookahead(lookahead_ms);
    }

    /// Samples of delay this setting inserts. 0 when bypassed or when
    /// lookahead is off.
    pub fn latency_samples(self) -> u32 {
        if !self.enabled {
            0
        } else {
            lookahead_samples(self.sample_rate, self.lookahead_ms)
        }
    }
}

/// Samples of [`LIMIT_LOOKAHEAD_DEFAULT_MS`].
///
/// An unusable callback rate uses `published_rate`. The cap-0 rewrite and
/// `force_delay` both use this count.
fn default_lookahead_samples(callback_rate: u32, published_rate: u32) -> u32 {
    let rate = usable_rate(callback_rate).unwrap_or(published_rate);
    lookahead_samples(rate, LIMIT_LOOKAHEAD_DEFAULT_MS)
}

/// Samples of delay for `lookahead_ms` at `sample_rate`.
///
/// `0` ms is 0 samples. A positive length that rounds to 0 is also 0.
/// The result is capped at [`MAX_LOOKAHEAD_SAMPLES`].
pub fn lookahead_samples(sample_rate: u32, lookahead_ms: f32) -> u32 {
    if sample_rate == 0 || !lookahead_ms.is_finite() || lookahead_ms <= 0.0 {
        return 0;
    }
    let n = lookahead_ms * 0.001 * sample_rate as f32;
    if !n.is_finite() || n <= 0.0 {
        return 0;
    }
    let rounded = n.round();
    if rounded < 1.0 {
        0
    } else {
        (rounded as u32).min(MAX_LOOKAHEAD_SAMPLES as u32)
    }
}

/// Nanoseconds for `samples` at `sample_rate`. 0 when the rate is 0.
pub fn latency_ns(samples: u32, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        0
    } else {
        u64::from(samples) * 1_000_000_000 / u64::from(sample_rate)
    }
}

/// Peak absolute level of `left` and `right` after the 4× interpolator.
///
/// This is the libebur128 true-peak path for a rate under 96 kHz: the max
/// of the oversampled stream, not the raw samples. It allocates nothing
/// beyond its stack history. It is not for the audio callback; the callback
/// keeps its own history inside [`LimitRuntime`].
pub fn measure_true_peak(left: &[f32], right: &[f32]) -> f32 {
    let tables = InterpTables::new();
    let mut hist_l = InterpHist::new();
    let mut hist_r = InterpHist::new();
    let frames = left.len().max(right.len());
    let mut peak = 0.0f32;
    for index in 0..frames {
        let sample_l = if index < left.len() { left[index] } else { 0.0 };
        let sample_r = if index < right.len() {
            right[index]
        } else {
            0.0
        };
        let level = tables
            .push(&mut hist_l, sample_l)
            .max(tables.push(&mut hist_r, sample_r));
        if level > peak {
            peak = level;
        }
    }
    peak
}

/// Delay line, detector, and clipper for the master.
#[derive(Debug)]
struct MasterLimitRt {
    current: [f32; PACK],
    target: [f32; PACK],
    origin: [f32; PACK],
    ramp_goal: [f32; PACK],
    own: u32,
    out_delay: u32,
    block_frames: u32,
    ramp_pos: u32,
    ramping: bool,
    releasing: bool,
    bypass: bool,
    gain: f32,
    goal: f32,
    step: f32,
    span: f32,
    /// Linear gain of the last sample that published a reduction. The block
    /// converts it once, after the callback loop.
    last_applied: f32,
    hist_l: InterpHist,
    hist_r: InterpHist,
    det_packed: [f64; INTERP_DELAY * 4],
    left: Box<[f32]>,
    right: Box<[f32]>,
    mins: MinDeque,
    clock: u32,
    write_pos: u32,
    filled: u32,
    startup_left: u32,
    xfade_on: bool,
    xfade_from: u32,
    xfade_to: u32,
    xfade_pos: u32,
    out_hist_l: InterpHist,
    out_hist_r: InterpHist,
    recent_l: [f32; INTERP_DELAY],
    recent_r: [f32; INTERP_DELAY],
    recent_i: usize,
    out_hist_live: bool,
    catchup_left: u32,
}

impl MasterLimitRt {
    fn new() -> Self {
        let pack = bypass_block(48_000).pack;
        Self {
            current: pack,
            target: pack,
            origin: pack,
            ramp_goal: pack,
            own: 0,
            out_delay: 0,
            block_frames: 1,
            ramp_pos: 0,
            ramping: false,
            releasing: false,
            bypass: true,
            gain: 1.0,
            goal: 1.0,
            step: 0.0,
            span: 1.0,
            last_applied: 1.0,
            hist_l: InterpHist::new(),
            hist_r: InterpHist::new(),
            det_packed: [0.0; INTERP_DELAY * 4],
            left: vec![0.0; DELAY_RING].into_boxed_slice(),
            right: vec![0.0; DELAY_RING].into_boxed_slice(),
            mins: MinDeque::new(MAX_LOOKAHEAD_SAMPLES + 1),
            clock: 0,
            write_pos: 0,
            filled: 0,
            startup_left: 0,
            xfade_on: false,
            xfade_from: 0,
            xfade_to: 0,
            xfade_pos: 0,
            out_hist_l: InterpHist::new(),
            out_hist_r: InterpHist::new(),
            recent_l: [0.0; INTERP_DELAY],
            recent_r: [0.0; INTERP_DELAY],
            recent_i: 0,
            out_hist_live: false,
            catchup_left: 0,
        }
    }

    fn runs_clipper(&self) -> bool {
        !self.bypass && self.own == 0
    }

    fn detect_delay(&self) -> u32 {
        if self.xfade_on {
            self.xfade_from.max(self.xfade_to)
        } else {
            self.out_delay
        }
    }

    fn attack_delay(&self) -> u32 {
        if self.xfade_on { 0 } else { self.out_delay }
    }

    fn begin(&mut self, loaded: Option<LimitBlock>, frames: usize, sample_rate: u32) {
        self.block_frames = frames.max(1) as u32;
        self.releasing = false;
        if let Some(loaded) = loaded {
            let (pack, delay) = block_at_rate(loaded, sample_rate);
            self.target = pack;
            self.current[WET] = pack[WET];
            self.target[WET] = pack[WET];
            self.own = delay;
            self.set_out_delay(delay);
        }
        let ceiling_moved = self.current[CEILING] != self.target[CEILING]
            || self.current[RELEASE] != self.target[RELEASE];
        if ceiling_moved {
            if !self.ramping || self.ramp_goal != self.target {
                self.origin = self.current;
                self.ramp_pos = 0;
                self.ramping = true;
                self.ramp_goal = self.target;
            }
        } else {
            self.ramping = false;
            self.current[CEILING] = self.target[CEILING];
            self.current[RELEASE] = self.target[RELEASE];
        }
        self.bypass = self.current[WET] == 0.0;
    }

    fn force_delay(&mut self, delay: u32) {
        self.own = delay;
        self.set_out_delay(delay);
    }

    fn set_out_delay(&mut self, delay: u32) {
        let delay = delay.min(MAX_LOOKAHEAD_SAMPLES as u32);
        if delay == self.out_delay {
            return;
        }
        if self.filled == 0 {
            self.out_delay = delay;
            self.startup_left = delay;
            self.xfade_on = false;
            return;
        }
        self.xfade_from = if self.xfade_on {
            self.xfade_to
        } else {
            self.out_delay
        };
        self.xfade_to = delay;
        self.xfade_pos = 0;
        self.xfade_on = true;
        self.out_delay = delay;
    }

    fn settled_delay(&self) -> bool {
        !self.bypass
            && !self.ramping
            && !self.releasing
            && !self.xfade_on
            && self.current[WET] != 0.0
            && self.out_delay > 0
    }

    fn settled_clip(&self) -> bool {
        !self.bypass
            && !self.ramping
            && !self.releasing
            && !self.xfade_on
            && self.current[WET] != 0.0
            && self.out_delay == 0
    }

    fn render(&mut self, tables: &InterpTables, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        if self.settled_delay() {
            self.render_delay(tables, left, right, frames);
        } else if self.settled_clip() {
            self.render_clip(tables, left, right, frames);
        } else {
            for frame in 0..frames {
                let (sample_l, sample_r) = self.tick_one(tables, frame, left[frame], right[frame]);
                left[frame] = sample_l;
                right[frame] = sample_r;
            }
        }
    }

    fn render_delay(
        &mut self,
        tables: &InterpTables,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
    ) {
        let window = self.out_delay.saturating_add(1).max(1);
        let release = self.current[RELEASE];
        let delay = self.out_delay;
        let ceiling = applied_ceiling(self.current[CEILING]);
        for frame in 0..frames {
            let sample_l = if left[frame].is_finite() {
                left[frame]
            } else {
                0.0
            };
            let sample_r = if right[frame].is_finite() {
                right[frame]
            } else {
                0.0
            };
            self.store(sample_l, sample_r);
            let over = tables.push_pair(
                &mut self.hist_l,
                &mut self.hist_r,
                &mut self.det_packed,
                sample_l,
                sample_r,
            );
            let raw = sample_l.abs().max(sample_r.abs());
            let need = if over.max(raw) > ceiling && ceiling > 0.0 {
                (ceiling / over.max(raw)).clamp(0.0, 1.0)
            } else {
                1.0
            };
            self.mins.push(self.clock, need, window);
            let mut window_min = self.mins.min(self.clock, window);
            if !window_min.is_finite() {
                window_min = 1.0;
            }
            self.follow_gain(window_min, release, delay);
            let (mut out_l, mut out_r) = self.read_delay(delay);
            self.advance();
            if self.gain != 1.0 {
                out_l = finite_mul(out_l, self.gain);
                out_r = finite_mul(out_r, self.gain);
                self.last_applied = self.gain;
            }
            self.out_hist_live = false;
            self.remember_output(out_l, out_r);
            left[frame] = out_l;
            right[frame] = out_r;
        }
    }

    fn render_clip(
        &mut self,
        tables: &InterpTables,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
    ) {
        let window = 1u32;
        let release = self.current[RELEASE];
        let ceiling = applied_ceiling(self.current[CEILING]);
        for frame in 0..frames {
            let sample_l = if left[frame].is_finite() {
                left[frame]
            } else {
                0.0
            };
            let sample_r = if right[frame].is_finite() {
                right[frame]
            } else {
                0.0
            };
            self.store(sample_l, sample_r);
            let over = tables.push_pair(
                &mut self.hist_l,
                &mut self.hist_r,
                &mut self.det_packed,
                sample_l,
                sample_r,
            );
            let raw = sample_l.abs().max(sample_r.abs());
            let need = if over.max(raw) > ceiling && ceiling > 0.0 {
                (ceiling / over.max(raw)).clamp(0.0, 1.0)
            } else {
                1.0
            };
            self.mins.push(self.clock, need, window);
            let mut window_min = self.mins.min(self.clock, window);
            if !window_min.is_finite() {
                window_min = 1.0;
            }
            self.follow_gain(window_min, release, 0);
            let (mut out_l, mut out_r) = self.read_delay(0);
            self.advance();
            if self.gain != 1.0 {
                out_l = finite_mul(out_l, self.gain);
                out_r = finite_mul(out_r, self.gain);
                self.last_applied = self.gain;
            }
            self.seed_out_hist(tables);
            let before = out_l.abs().max(out_r.abs());
            (out_l, out_r) = clip_pair(
                tables,
                &mut self.out_hist_l,
                &mut self.out_hist_r,
                out_l,
                out_r,
                ceiling,
            );
            let after = out_l.abs().max(out_r.abs());
            if before > 0.0 && after < before {
                let reduced = (self.gain * (after / before)).clamp(0.0, 1.0);
                if reduced < self.gain {
                    self.gain = reduced;
                    self.last_applied = reduced;
                }
            }
            self.remember_output(out_l, out_r);
            left[frame] = out_l;
            right[frame] = out_r;
        }
    }

    fn tick_one(
        &mut self,
        tables: &InterpTables,
        _frame: usize,
        left: f32,
        right: f32,
    ) -> (f32, f32) {
        let pack = self.pack_for();
        let wet = pack[WET] != 0.0;
        if !wet && self.out_delay == 0 && !self.xfade_on {
            self.store(left, right);
            self.mins.push(self.clock, 1.0, 1);
            self.gain = 1.0;
            self.goal = 1.0;
            self.step = 0.0;
            self.last_applied = 1.0;
            self.out_hist_live = false;
            self.remember_output(left, right);
            self.advance();
            return (left, right);
        }
        let left_f = if left.is_finite() { left } else { 0.0 };
        let right_f = if right.is_finite() { right } else { 0.0 };
        if wet {
            self.store(left_f, right_f);
            let window = self.detect_delay().saturating_add(1).max(1);
            let over = tables.push_pair(
                &mut self.hist_l,
                &mut self.hist_r,
                &mut self.det_packed,
                left_f,
                right_f,
            );
            let raw = left_f.abs().max(right_f.abs());
            let need = self.need_for(over.max(raw));
            self.mins.push(self.clock, need, window);
            let window_min = self.window_min();
            self.follow_gain(window_min, pack[RELEASE], self.attack_delay());
        } else {
            self.store(left, right);
            self.mins.push(self.clock, 1.0, 1);
            self.gain = 1.0;
            self.goal = 1.0;
            self.step = 0.0;
            self.last_applied = 1.0;
        }
        let fading = self.xfade_on;
        let cover = self.detect_delay();
        let (mut out_l, mut out_r) = self.read_output();
        self.advance();
        if wet && (self.gain != 1.0 || self.releasing) {
            let applied = if self.releasing {
                let t = (self.ramp_pos as f32 / LIMIT_RAMP_SAMPLES.max(1) as f32).clamp(0.0, 1.0);
                self.gain + (1.0 - self.gain) * t
            } else {
                self.gain
            };
            if applied != 1.0 {
                out_l = finite_mul(out_l, applied);
                out_r = finite_mul(out_r, applied);
                self.last_applied = applied;
            }
        }
        if wet && !self.releasing && (cover == 0 || fading) {
            let ceiling = applied_ceiling(pack[CEILING]);
            self.seed_out_hist(tables);
            let before = out_l.abs().max(out_r.abs());
            (out_l, out_r) = clip_pair(
                tables,
                &mut self.out_hist_l,
                &mut self.out_hist_r,
                out_l,
                out_r,
                ceiling,
            );
            let after = out_l.abs().max(out_r.abs());
            if before > 0.0 && after < before {
                let reduced = (self.gain * (after / before)).clamp(0.0, 1.0);
                if reduced < self.gain {
                    self.gain = reduced;
                    self.last_applied = reduced;
                }
            }
        } else {
            self.out_hist_live = false;
        }
        self.remember_output(out_l, out_r);
        (out_l, out_r)
    }

    fn pack_for(&mut self) -> [f32; PACK] {
        if !self.ramping {
            return self.current;
        }
        let pos = self.ramp_pos.saturating_add(1);
        self.ramp_pos = pos;
        let span = LIMIT_RAMP_SAMPLES.max(1);
        if pos >= span {
            if self.releasing {
                self.current = self.ramp_goal;
                self.current[WET] = 0.0;
                self.releasing = false;
                self.gain = 1.0;
                self.goal = 1.0;
                self.step = 0.0;
            } else {
                self.current = self.target;
                self.current[WET] = self.target[WET];
            }
            self.ramping = false;
            self.bypass = self.current[WET] == 0.0;
            return self.current;
        }
        let t = pos as f32 / span as f32;
        let goal = if self.releasing {
            self.ramp_goal
        } else {
            self.target
        };
        let mut pack = lerp(self.origin, goal, t);
        if !self.releasing {
            pack[WET] = self.target[WET];
        }
        self.current = pack;
        self.bypass = false;
        self.current
    }

    fn store(&mut self, left: f32, right: f32) {
        let write = self.write_pos as usize & DELAY_MASK;
        self.left[write] = left;
        self.right[write] = right;
    }

    fn read_output(&mut self) -> (f32, f32) {
        if !self.xfade_on {
            return self.read_delay(self.out_delay);
        }
        let from = self.read_delay(self.xfade_from);
        let to = self.read_delay(self.xfade_to);
        self.xfade_pos = self.xfade_pos.saturating_add(1);
        if self.xfade_pos >= LIMIT_RAMP_SAMPLES.max(1) {
            self.xfade_on = false;
            return to;
        }
        let t = self.xfade_pos as f32 / LIMIT_RAMP_SAMPLES.max(1) as f32;
        (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
    }

    fn read_delay(&self, delay: u32) -> (f32, f32) {
        let write = self.write_pos as usize & DELAY_MASK;
        if delay == 0 {
            return (self.left[write], self.right[write]);
        }
        if self.startup_left > 0 && delay > self.filled {
            return (0.0, 0.0);
        }
        let use_delay = delay.min(self.filled).min(DELAY_RING as u32);
        if use_delay == 0 {
            return (self.left[write], self.right[write]);
        }
        let read = write.wrapping_sub(use_delay as usize) & DELAY_MASK;
        (self.left[read], self.right[read])
    }

    fn advance(&mut self) {
        self.clock = self.clock.wrapping_add(1);
        self.write_pos = self.write_pos.wrapping_add(1) & (DELAY_MASK as u32);
        self.filled = self.filled.saturating_add(1);
        if self.startup_left > 0 {
            self.startup_left -= 1;
        }
    }

    fn window_min(&mut self) -> f32 {
        let window = self.detect_delay().saturating_add(1).max(1);
        let mut window_min = self.mins.min(self.clock, window);
        if !window_min.is_finite() {
            window_min = 1.0;
        }
        window_min
    }

    fn follow_gain(&mut self, window_min: f32, release: f32, lookahead: u32) {
        let window_min = if window_min.is_finite() {
            window_min.clamp(0.0, 1.0)
        } else {
            1.0
        };
        if self.catchup_left > 0 {
            self.catchup_left -= 1;
            let target = self.goal.min(window_min);
            if self.gain > target {
                let step = if self.step.is_finite() && self.step > 0.0 {
                    self.step
                } else {
                    (self.gain - target).max(0.0)
                };
                self.gain = (self.gain - step).max(target);
            }
        } else if window_min < self.gain {
            let span = attack_span(lookahead);
            let harder = window_min < self.goal - 1.0e-6;
            let span_changed = (self.span - span).abs() > 0.5;
            if harder || self.step == 0.0 || span_changed {
                self.goal = window_min;
                self.span = span;
                self.step = (self.gain - window_min) / span;
            }
            if !self.step.is_finite() || self.step < 0.0 {
                self.step = self.gain - window_min;
            }
            self.gain = (self.gain - self.step).max(window_min);
        } else {
            let span = attack_span(lookahead);
            self.goal = window_min;
            self.step = 0.0;
            self.span = span;
            let coeff = if release.is_finite() {
                release.clamp(0.0, 1.0)
            } else {
                1.0
            };
            let next = self.gain + coeff * (window_min - self.gain);
            self.gain = if next.is_finite() {
                next.clamp(0.0, window_min.max(self.gain))
            } else {
                window_min
            };
            if self.gain > window_min {
                self.gain = window_min;
            }
        }
        if !self.gain.is_finite() || self.gain < 0.0 {
            self.gain = 0.0;
        } else if self.gain > 1.0 {
            self.gain = 1.0;
        } else if self.gain < 1.0e-15 {
            self.gain = 0.0;
        }
    }

    fn need_for(&self, peak: f32) -> f32 {
        let ceiling = applied_ceiling(self.current[CEILING]);
        if peak > ceiling && ceiling > 0.0 && peak.is_finite() {
            (ceiling / peak).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    fn remember_output(&mut self, left: f32, right: f32) {
        self.recent_l[self.recent_i] = left;
        self.recent_r[self.recent_i] = right;
        self.recent_i += 1;
        if self.recent_i == INTERP_DELAY {
            self.recent_i = 0;
        }
    }

    fn seed_out_hist(&mut self, tables: &InterpTables) {
        if self.out_hist_live {
            return;
        }
        self.out_hist_l = InterpHist::new();
        self.out_hist_r = InterpHist::new();
        for age in (1..=INTERP_DELAY).rev() {
            let index = (self.recent_i + INTERP_DELAY - age) % INTERP_DELAY;
            tables.push(&mut self.out_hist_l, self.recent_l[index]);
            tables.push(&mut self.out_hist_r, self.recent_r[index]);
        }
        self.out_hist_live = true;
    }
}

#[derive(Clone, Copy, Debug)]
struct LimitBlock {
    pack: [f32; PACK],
    lookahead: u32,
    lookahead_ms: f32,
    sample_rate: u32,
}

impl LimitBlock {
    fn is_bypass(self) -> bool {
        self.pack[WET] == 0.0
    }

    #[cfg(test)]
    fn stamp_uniform(&self) -> Option<u32> {
        let bits = self.pack[0].to_bits();
        if self.pack.iter().all(|sample| sample.to_bits() == bits)
            && self.lookahead == bits
            && self.lookahead_ms.to_bits() == bits
            && self.sample_rate == bits
        {
            Some(bits)
        } else {
            None
        }
    }
}

fn bypass_block(sample_rate: u32) -> LimitBlock {
    block_from(LimitSettings::bypass(sample_rate), sample_rate)
}

fn block_from(settings: LimitSettings, sample_rate: u32) -> LimitBlock {
    let rate = usable_rate(sample_rate).unwrap_or(48_000);
    let mut pack = [0.0; PACK];
    pack[CEILING] = guarded_ceiling_linear(settings.ceiling_db);
    pack[RELEASE] = params::one_pole_coeff(rate, LIMIT_RELEASE_MS * 0.001);
    pack[WET] = if settings.enabled { 1.0 } else { 0.0 };
    let lookahead = if settings.enabled {
        lookahead_samples(rate, settings.lookahead_ms)
    } else {
        0
    };
    LimitBlock {
        pack,
        lookahead,
        lookahead_ms: settings.lookahead_ms,
        sample_rate: rate,
    }
}

/// Delay and release for the rate the callback is actually running.
///
/// A publication from another rate still carries the ceiling, the wet flag,
/// and the lookahead in milliseconds. Recomputing here is what keeps the
/// master limiting before the loop thread republishes.
fn block_at_rate(block: LimitBlock, sample_rate: u32) -> ([f32; PACK], u32) {
    let Some(rate) = usable_rate(sample_rate) else {
        return (block.pack, block.lookahead);
    };
    if block.is_bypass() || block.sample_rate == rate {
        return (block.pack, block.lookahead);
    }
    let mut pack = block.pack;
    pack[RELEASE] = params::one_pole_coeff(rate, LIMIT_RELEASE_MS * 0.001);
    let delay = lookahead_samples(rate, block.lookahead_ms);
    (pack, delay)
}

fn guarded_ceiling_linear(ceiling_db: f32) -> f32 {
    let db = sanitize_ceiling(ceiling_db) - LIMIT_GUARD_DB;
    let lin = 10f32.powf(db / 20.0);
    if lin.is_finite() { lin } else { 0.0 }
}

fn lerp(from: [f32; PACK], to: [f32; PACK], t: f32) -> [f32; PACK] {
    let mut out = [0.0; PACK];
    for index in 0..PACK {
        out[index] = from[index] + (to[index] - from[index]) * t;
    }
    out
}

#[derive(Debug)]
struct LimitChannel {
    enabled: AtomicBool,
    ceiling_db: AtomicU32,
    lookahead_ms: AtomicU32,
    /// Set when the cap stores 5 ms in place of a 0 ms request.
    cap_demoted: AtomicBool,
    slots: [UnsafeCell<LimitBlock>; 3],
    state: AtomicU32,
    write_idx: AtomicU32,
    gr_db: AtomicU32,
}

impl LimitChannel {
    fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            ceiling_db: AtomicU32::new(LIMIT_CEILING_DEFAULT_DB.to_bits()),
            lookahead_ms: AtomicU32::new(LIMIT_LOOKAHEAD_DEFAULT_MS.to_bits()),
            cap_demoted: AtomicBool::new(false),
            slots: std::array::from_fn(|_| UnsafeCell::new(bypass_block(48_000))),
            state: AtomicU32::new(INITIAL_SPARE),
            write_idx: AtomicU32::new(INITIAL_WRITE),
            gr_db: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    fn store(&self, update: LimitUpdate, master: bool, cap_demoted: bool) {
        match update {
            LimitUpdate::Enabled(on) => {
                self.enabled.store(on, Ordering::Release);
                if !on {
                    self.cap_demoted.store(false, Ordering::Release);
                }
            }
            LimitUpdate::CeilingDb(db) => {
                self.ceiling_db
                    .store(sanitize_ceiling(db).to_bits(), Ordering::Release);
            }
            LimitUpdate::LookaheadMs(ms) => {
                self.lookahead_ms
                    .store(sanitize_lookahead(ms).to_bits(), Ordering::Release);
                self.cap_demoted.store(cap_demoted, Ordering::Release);
            }
            LimitUpdate::Reset => {
                let settings = if master {
                    LimitSettings::master(48_000)
                } else {
                    LimitSettings::bypass(48_000)
                };
                self.store_settings(settings);
                self.cap_demoted.store(false, Ordering::Release);
            }
        }
    }

    fn store_settings(&self, settings: LimitSettings) {
        self.enabled.store(settings.enabled, Ordering::Release);
        self.ceiling_db
            .store(settings.ceiling_db.to_bits(), Ordering::Release);
        self.lookahead_ms
            .store(settings.lookahead_ms.to_bits(), Ordering::Release);
    }

    fn publish_at(&self, sample_rate: u32) {
        let settings = channel_settings(sample_rate, self);
        self.publish_block(block_from(settings, sample_rate));
    }

    fn publish_block(&self, block: LimitBlock) {
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

    fn poll(&self, read_idx: &mut u32, dst: &mut LimitBlock) -> bool {
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

fn claim(channel: &LimitChannel, state: u32, read_idx: &mut u32, dst: &mut LimitBlock) -> bool {
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

// SAFETY: [`LimitWriter`] is the only producer. It is not `Clone` and not `Sync`, and publish takes `&mut self`, so safe code cannot publish from two threads. [`LimitReader`] is the only consumer. It is not `Clone` and not `Sync`, and it owns `read_idx`. `poll` takes `&mut LimitReader`. The producer writes a slot only while it owns `write_idx`. The consumer reads a slot only after `poll` has swapped that index out of the spare field. Those two indices are distinct, so the cells are not aliased. The parameter atomics and the gain-reduction atomic are synchronized on their own. A second producer or a second consumer would be a data race, and the handles do not allow one from safe code.
unsafe impl Sync for LimitChannel {}

/// Sliding-window minimum. The slots are allocated in [`LimitRuntime::new`].
/// `push` does not allocate. The front is the smallest value whose index is
/// still inside the window. Needs for the last `cap` clocks stay in `hist`,
/// and a new window length rebuilds the front from those needs. In test and
/// debug builds each push must use a later clock than the previous one. A
/// repeated or older clock sticks a fault. `clear` starts a new run of
/// clocks and leaves that fault set.
#[derive(Debug)]
struct MinDeque {
    idx: Box<[u32]>,
    val: Box<[f32]>,
    hist: Box<[f32]>,
    head: u32,
    len: u32,
    pushed: u32,
    built: u32,
    #[cfg(any(test, debug_assertions))]
    has_last: bool,
    #[cfg(any(test, debug_assertions))]
    last_idx: u32,
    #[cfg(any(test, debug_assertions))]
    order_fault: bool,
}

impl MinDeque {
    fn new(cap: usize) -> Self {
        let cap = cap.max(1).next_power_of_two();
        Self {
            idx: vec![0; cap].into_boxed_slice(),
            val: vec![1.0; cap].into_boxed_slice(),
            hist: vec![1.0; cap].into_boxed_slice(),
            head: 0,
            len: 0,
            pushed: 0,
            built: 0,
            #[cfg(any(test, debug_assertions))]
            has_last: false,
            #[cfg(any(test, debug_assertions))]
            last_idx: 0,
            #[cfg(any(test, debug_assertions))]
            order_fault: false,
        }
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.pushed = 0;
        self.built = 0;
        for slot in &mut self.hist[..] {
            *slot = 1.0;
        }
        #[cfg(any(test, debug_assertions))]
        {
            self.has_last = false;
            self.last_idx = 0;
        }
    }

    #[cfg(any(test, debug_assertions))]
    fn note_order(&mut self, index: u32) {
        if self.has_last {
            let forward = index.wrapping_sub(self.last_idx);
            let back = self.last_idx.wrapping_sub(index);
            if forward == 0 || back < forward {
                self.order_fault = true;
            }
        }
        self.has_last = true;
        self.last_idx = index;
    }

    #[cfg(test)]
    fn order_fault(&self) -> bool {
        self.order_fault
    }

    fn cap(&self) -> u32 {
        self.idx.len() as u32
    }

    fn mask(&self) -> u32 {
        self.cap() - 1
    }

    fn push(&mut self, index: u32, value: f32, window: u32) {
        #[cfg(any(test, debug_assertions))]
        self.note_order(index);
        let value = if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let window = window.max(1).min(self.cap());
        self.hist[(index as usize) & (self.hist.len() - 1)] = value;
        self.pushed = self.pushed.saturating_add(1);
        if window != self.built {
            self.head = 0;
            self.len = 0;
            let span = window.min(self.pushed);
            let mut clock = index.wrapping_sub(span.saturating_sub(1));
            for _ in 0..span {
                let stored = self.hist[(clock as usize) & (self.hist.len() - 1)];
                self.insert(clock, stored, window);
                clock = clock.wrapping_add(1);
            }
            self.built = window;
            return;
        }
        self.insert(index, value, window);
    }

    fn insert(&mut self, index: u32, value: f32, window: u32) {
        self.expire(index, window);
        while self.len > 0 {
            let back = (self.head + self.len - 1) & self.mask();
            if self.val[back as usize] < value {
                break;
            }
            self.len -= 1;
        }
        if self.len == self.cap() {
            self.head = (self.head + 1) & self.mask();
            self.len -= 1;
        }
        let at = (self.head + self.len) & self.mask();
        self.idx[at as usize] = index;
        self.val[at as usize] = value;
        self.len += 1;
    }

    fn min(&mut self, index: u32, window: u32) -> f32 {
        let window = window.max(1);
        self.expire(index, window);
        if self.len == 0 {
            return 1.0;
        }
        let value = self.val[self.head as usize];
        if value.is_finite() { value } else { 1.0 }
    }

    fn expire(&mut self, index: u32, window: u32) {
        while self.len > 0 {
            let front = self.idx[self.head as usize];
            if index.wrapping_sub(front) < window {
                break;
            }
            self.head = (self.head + 1) & self.mask();
            self.len -= 1;
        }
    }
}

fn applied_ceiling(ceiling: f32) -> f32 {
    if ceiling.is_finite() && ceiling > 0.0 {
        ceiling
    } else {
        0.0
    }
}

/// Future holds whose 4× output still sees the new sample in the kernel.
///
/// The center of the 49-tap window is about six samples ahead. Alignments
/// before 2 and after 8 sit on sidelobes that do not set the peak once the
/// center is constrained. Seven alignments is the whole search. It does not
/// grow with the ring and it does not retry a candidate.
const CLIP_ALIGNMENTS: [usize; 7] = [2, 3, 4, 5, 6, 7, 8];

const CLIP_ROW_STRIDE: usize = CLIP_ALIGNMENTS.len() * INTERP_FACTOR;
/// AVX2 lanes. The 28 clip rows split into seven groups of four.
const CLIP_LANE: usize = 4;
/// Groups stored in the tap-major matrix. One extra group is zeros so four
/// groups can share a tap and stay in the same add order.
const CLIP_MAJOR_GROUPS: usize = 8;
const MAJOR_LEN: usize = INTERP_DELAY * CLIP_MAJOR_GROUPS * INTERP_DELAY * CLIP_LANE;

const _: () = assert!(CLIP_ROW_STRIDE.is_multiple_of(CLIP_LANE));

const fn major_index(zi: usize, group: usize, tap: usize, lane: usize) -> usize {
    ((zi * CLIP_MAJOR_GROUPS + group) * INTERP_DELAY + tap) * CLIP_LANE + lane
}

#[derive(Clone, Copy)]
struct ClipRow {
    slope: f64,
    inv_slope: f64,
    n: u8,
    idx: [u8; INTERP_DELAY],
    w: [f64; INTERP_DELAY],
    spread: [f64; INTERP_DELAY * 2],
}

struct ClipPlan {
    rows: Box<[ClipRow]>,
    /// Tap-major weights. One group of four rows, then the tap, then the lane.
    /// A zero pad is left at 0 so it does not replace a real weight on slot 0.
    #[cfg(target_arch = "x86_64")]
    major: Box<[f64]>,
    #[cfg(target_arch = "x86_64")]
    avx2: bool,
}

fn clip_plan_cell() -> &'static std::sync::OnceLock<ClipPlan> {
    static PLAN: std::sync::OnceLock<ClipPlan> = std::sync::OnceLock::new();
    &PLAN
}

/// Build the clip plan. [`LimitRuntime::new`] calls this. The audio callback does not.
fn ensure_clip_plan() {
    let _ = clip_plan_cell().get_or_init(ClipPlan::build);
}

/// The plan, if [`ensure_clip_plan`] already ran. Does not initialize.
fn clip_plan() -> Option<&'static ClipPlan> {
    clip_plan_cell().get()
}

fn clip_rows(tables: &InterpTables, zi: usize, alignments: &[usize]) -> Vec<ClipRow> {
    let mut rows = Vec::with_capacity(alignments.len() * INTERP_FACTOR);
    for &delay in alignments {
        let mut held = [false; INTERP_DELAY];
        for step in 0..=delay {
            held[(zi + step) % INTERP_DELAY] = true;
        }
        let at = (zi + delay) % INTERP_DELAY;
        for phase in &tables.phases {
            let mut slope = 0.0f64;
            let mut weight = [0.0f64; INTERP_DELAY];
            for tap in 0..phase.count as usize {
                let mut index = at as i32 - i32::from(phase.index[tap]);
                if index < 0 {
                    index += INTERP_DELAY as i32;
                }
                let slot = index as usize;
                let coeff = phase.coeff[tap];
                if held[slot] {
                    slope += coeff;
                } else {
                    weight[slot] += coeff;
                }
            }
            let mut n = 0usize;
            let mut idx = [0u8; INTERP_DELAY];
            let mut w = [0.0f64; INTERP_DELAY];
            let mut spread = [0.0f64; INTERP_DELAY * 2];
            for (slot, coeff) in weight.iter().copied().enumerate() {
                if coeff == 0.0 {
                    continue;
                }
                idx[n] = slot as u8;
                w[n] = coeff;
                spread[n * 2] = coeff;
                spread[n * 2 + 1] = coeff;
                n += 1;
            }
            let inv_slope = if slope.abs() <= 1.0e-12 {
                0.0
            } else {
                1.0 / slope
            };
            rows.push(ClipRow {
                slope,
                inv_slope,
                n: n as u8,
                idx,
                w,
                spread,
            });
        }
    }
    rows
}

impl ClipPlan {
    fn rows_at(&self, zi: usize) -> &[ClipRow] {
        let start = zi * CLIP_ROW_STRIDE;
        &self.rows[start..start + CLIP_ROW_STRIDE]
    }

    fn build() -> Self {
        let tables = InterpTables::new();
        let blank = ClipRow {
            slope: 0.0,
            inv_slope: 0.0,
            n: 0,
            idx: [0; INTERP_DELAY],
            w: [0.0; INTERP_DELAY],
            spread: [0.0; INTERP_DELAY * 2],
        };
        let mut rows = vec![blank; INTERP_DELAY * CLIP_ROW_STRIDE].into_boxed_slice();
        for zi in 0..INTERP_DELAY {
            let built = clip_rows(&tables, zi, &CLIP_ALIGNMENTS);
            let start = zi * CLIP_ROW_STRIDE;
            for (dst, src) in rows[start..start + CLIP_ROW_STRIDE].iter_mut().zip(built) {
                *dst = src;
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            let major = fill_major(&rows);
            let avx2 = std::arch::is_x86_feature_detected!("avx2");
            Self { rows, major, avx2 }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Self { rows }
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn fill_major(rows: &[ClipRow]) -> Box<[f64]> {
    let mut major = vec![0.0f64; MAJOR_LEN].into_boxed_slice();
    for zi in 0..INTERP_DELAY {
        let start = zi * CLIP_ROW_STRIDE;
        for (row_i, row) in rows[start..start + CLIP_ROW_STRIDE].iter().enumerate() {
            let group = row_i / CLIP_LANE;
            let lane = row_i % CLIP_LANE;
            for coeff_i in 0..row.n as usize {
                let coeff = row.w[coeff_i];
                if coeff == 0.0 {
                    continue;
                }
                let tap = row.idx[coeff_i] as usize;
                major[major_index(zi, group, tap, lane)] = coeff;
            }
        }
    }
    major
}

/// Hold `sample` under `ceiling` using the precomputed alignments.
///
/// Each alignment is `offset + slope * candidate`. The offset is one dot
/// product with the samples already played. The candidate closest to
/// `sample` inside every alignment is the result. If the alignments do not
/// overlap, the result is 0. Other history slots are not cleared.
#[cfg(test)]
fn pull_under_ceiling(
    _tables: &InterpTables,
    hist: &mut InterpHist,
    sample: f32,
    ceiling: f32,
) -> f32 {
    let sample = if sample.is_finite() { sample } else { 0.0 };
    let ceiling = if ceiling.is_finite() && ceiling > 0.0 {
        ceiling
    } else {
        0.0
    };
    let chosen = if ceiling == 0.0 {
        0.0
    } else if let Some(held) = smooth_hold(hist, sample, ceiling) {
        held
    } else {
        project_held(hist, sample, ceiling)
    };
    store_played(hist, chosen);
    chosen
}

/// The last played sample is enough when the new one barely moves.
///
/// The constant 0 ms block is this path. Checking one sample keeps it next
/// to the lookahead cost. A step, noise, or a hot previous sample is outside
/// the band and goes through [`project_held`]. The band is far below a
/// 997 Hz sine step at 48 kHz, so that sine still uses the solve. A sample
/// within 0.02 of the ceiling also uses the solve, including a 96 kHz sine
/// peak. A constant the gain has already eased down still fits in the room.
fn smooth_hold(hist: &InterpHist, sample: f32, ceiling: f32) -> Option<f32> {
    const HEADROOM: f32 = 0.02;
    const BAND: f32 = 0.002;
    if !sample.is_finite() || sample.abs() > ceiling - HEADROOM {
        return None;
    }
    let prev_at = (hist.zi + INTERP_DELAY - 1) % INTERP_DELAY;
    let prev = hist.z[prev_at];
    if !prev.is_finite() || prev.abs() > ceiling || (prev - sample).abs() > BAND {
        return None;
    }
    Some(sample)
}

fn project_held(hist: &InterpHist, sample: f32, ceiling: f32) -> f32 {
    let Some(plan) = clip_plan() else {
        return 0.0;
    };
    let mut played = [0.0f64; INTERP_DELAY];
    for (slot, value) in played.iter_mut().zip(hist.z) {
        *slot = f64::from(value);
    }
    solve_interval(sample, ceiling, &played, plan.rows_at(hist.zi))
}

fn solve_interval(
    sample: f32,
    ceiling: f32,
    played: &[f64; INTERP_DELAY],
    rows: &[ClipRow],
) -> f32 {
    let mut lo = -f64::from(ceiling);
    let mut hi = f64::from(ceiling);
    let limit = f64::from(ceiling) * (1.0 - 1.0e-5);
    for row in rows {
        let off = dot_row(row, played);
        if !tighten_abs(&mut lo, &mut hi, off, row.slope, limit) {
            return 0.0;
        }
    }
    choose_interval(sample, lo, hi)
}

fn choose_interval(sample: f32, lo: f64, hi: f64) -> f32 {
    if lo > hi {
        if lo - hi < 1.0e-6 {
            return (0.5 * (lo + hi)) as f32;
        }
        return 0.0;
    }
    let sample = f64::from(sample);
    let chosen = if sample < lo {
        lo
    } else if sample > hi {
        hi
    } else {
        sample
    };
    if chosen.is_finite() {
        chosen as f32
    } else {
        0.0
    }
}

fn tighten_with_inv(
    lo: &mut f64,
    hi: &mut f64,
    off: f64,
    slope: f64,
    inv_slope: f64,
    limit: f64,
) -> bool {
    if !off.is_finite() || !slope.is_finite() || !limit.is_finite() {
        return false;
    }
    if slope.abs() <= 1.0e-12 {
        return off.abs() <= limit;
    }
    let mut lower = (-limit - off) * inv_slope;
    let mut upper = (limit - off) * inv_slope;
    if lower > upper {
        std::mem::swap(&mut lower, &mut upper);
    }
    if lower > *lo {
        *lo = lower;
    }
    if upper < *hi {
        *hi = upper;
    }
    true
}

fn played_from(hist: &InterpHist) -> [f64; INTERP_DELAY] {
    let mut played = [0.0f64; INTERP_DELAY];
    for (slot, value) in played.iter_mut().zip(hist.z) {
        *slot = f64::from(value);
    }
    played
}

/// Both channels, same rows. Each dot matches [`dot_row`].
fn project_held_pair(
    hist_l: &InterpHist,
    hist_r: &InterpHist,
    sample_l: f32,
    sample_r: f32,
    ceiling: f32,
) -> (f32, f32) {
    let Some(plan) = clip_plan() else {
        return (0.0, 0.0);
    };
    if hist_l.zi != hist_r.zi {
        return (
            project_held(hist_l, sample_l, ceiling),
            project_held(hist_r, sample_r, ceiling),
        );
    }
    let played_l = played_from(hist_l);
    let played_r = played_from(hist_r);
    #[cfg(target_arch = "x86_64")]
    if plan.avx2 {
        // SAFETY: `plan.avx2` is set only when this CPU has AVX2.
        return unsafe {
            project_held_pair_avx2(
                &plan.major,
                hist_l.zi,
                &played_l,
                &played_r,
                plan.rows_at(hist_l.zi),
                (sample_l, sample_r),
                ceiling,
            )
        };
    }
    let mut packed = [0.0f64; INTERP_DELAY * 2];
    for slot in 0..INTERP_DELAY {
        packed[slot * 2] = played_l[slot];
        packed[slot * 2 + 1] = played_r[slot];
    }
    let rows = plan.rows_at(hist_l.zi);
    let mut lo_l = -f64::from(ceiling);
    let mut hi_l = f64::from(ceiling);
    let mut lo_r = -f64::from(ceiling);
    let mut hi_r = f64::from(ceiling);
    let limit = f64::from(ceiling) * (1.0 - 1.0e-5);
    let mut fail_l = false;
    let mut fail_r = false;
    for row in rows {
        let (acc_l, acc_r) = dot_both(row, &packed);
        if !fail_l
            && !tighten_with_inv(&mut lo_l, &mut hi_l, acc_l, row.slope, row.inv_slope, limit)
        {
            fail_l = true;
        }
        if !fail_r
            && !tighten_with_inv(&mut lo_r, &mut hi_r, acc_r, row.slope, row.inv_slope, limit)
        {
            fail_r = true;
        }
        if fail_l && fail_r {
            return (0.0, 0.0);
        }
    }
    (
        if fail_l {
            0.0
        } else {
            choose_interval(sample_l, lo_l, hi_l)
        },
        if fail_r {
            0.0
        } else {
            choose_interval(sample_r, lo_r, hi_r)
        },
    )
}

fn clip_pair(
    _tables: &InterpTables,
    hist_l: &mut InterpHist,
    hist_r: &mut InterpHist,
    sample_l: f32,
    sample_r: f32,
    ceiling: f32,
) -> (f32, f32) {
    let sample_l = if sample_l.is_finite() { sample_l } else { 0.0 };
    let sample_r = if sample_r.is_finite() { sample_r } else { 0.0 };
    let ceiling = if ceiling.is_finite() && ceiling > 0.0 {
        ceiling
    } else {
        0.0
    };
    let (out_l, out_r) = if ceiling == 0.0 {
        (0.0, 0.0)
    } else {
        match (
            smooth_hold(hist_l, sample_l, ceiling),
            smooth_hold(hist_r, sample_r, ceiling),
        ) {
            (Some(left), Some(right)) => (left, right),
            (Some(left), None) => (left, project_held(hist_r, sample_r, ceiling)),
            (None, Some(right)) => (project_held(hist_l, sample_l, ceiling), right),
            (None, None) => project_held_pair(hist_l, hist_r, sample_l, sample_r, ceiling),
        }
    };
    store_played(hist_l, out_l);
    store_played(hist_r, out_r);
    (out_l, out_r)
}

#[inline(always)]
fn dot_both(row: &ClipRow, packed: &[f64; INTERP_DELAY * 2]) -> (f64, f64) {
    #[cfg(target_arch = "x86_64")]
    {
        dot_both_sse(row, packed)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut acc_l = 0.0f64;
        let mut acc_r = 0.0f64;
        for i in 0..row.n as usize {
            let at = row.idx[i] as usize * 2;
            let coeff = row.w[i];
            acc_l += coeff * packed[at];
            acc_r += coeff * packed[at + 1];
        }
        (acc_l, acc_r)
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn dot_both_sse(row: &ClipRow, packed: &[f64; INTERP_DELAY * 2]) -> (f64, f64) {
    use core::arch::x86_64::{_mm_add_pd, _mm_loadu_pd, _mm_mul_pd, _mm_setzero_pd, _mm_storeu_pd};
    let mut lanes = [0.0f64; 2];
    // SAFETY: x86_64 always has SSE2. `packed` is left, right pairs. `lanes` is two local f64 slots.
    unsafe {
        let mut acc = _mm_setzero_pd();
        let samples = packed.as_ptr();
        for i in 0..row.n as usize {
            let sample = _mm_loadu_pd(samples.add(row.idx[i] as usize * 2));
            let coeff = _mm_loadu_pd(row.spread.as_ptr().add(i * 2));
            acc = _mm_add_pd(acc, _mm_mul_pd(sample, coeff));
        }
        _mm_storeu_pd(lanes.as_mut_ptr(), acc);
    }
    (lanes[0], lanes[1])
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx_dots_only(
    major: &[f64],
    zi: usize,
    played_l: &[f64; INTERP_DELAY],
    played_r: &[f64; INTERP_DELAY],
) -> (
    [f64; CLIP_MAJOR_GROUPS * CLIP_LANE],
    [f64; CLIP_MAJOR_GROUPS * CLIP_LANE],
) {
    use core::arch::x86_64::{
        _mm256_add_pd, _mm256_loadu_pd, _mm256_mul_pd, _mm256_set1_pd, _mm256_setzero_pd,
        _mm256_storeu_pd,
    };
    let mut acc_l = [0.0f64; CLIP_MAJOR_GROUPS * CLIP_LANE];
    let mut acc_r = [0.0f64; CLIP_MAJOR_GROUPS * CLIP_LANE];
    // SAFETY: AVX2 is enabled on this function. Each tap is four contiguous f64 lanes.
    unsafe {
        let weights = major.as_ptr();
        for chunk in 0..(CLIP_MAJOR_GROUPS / 4) {
            let group0 = chunk * 4;
            let mut al0 = _mm256_setzero_pd();
            let mut ar0 = _mm256_setzero_pd();
            let mut al1 = _mm256_setzero_pd();
            let mut ar1 = _mm256_setzero_pd();
            let mut al2 = _mm256_setzero_pd();
            let mut ar2 = _mm256_setzero_pd();
            let mut al3 = _mm256_setzero_pd();
            let mut ar3 = _mm256_setzero_pd();
            for tap in 0..INTERP_DELAY {
                let left = _mm256_set1_pd(played_l[tap]);
                let right = _mm256_set1_pd(played_r[tap]);
                let w0 = _mm256_loadu_pd(weights.add(major_index(zi, group0, tap, 0)));
                let w1 = _mm256_loadu_pd(weights.add(major_index(zi, group0 + 1, tap, 0)));
                let w2 = _mm256_loadu_pd(weights.add(major_index(zi, group0 + 2, tap, 0)));
                let w3 = _mm256_loadu_pd(weights.add(major_index(zi, group0 + 3, tap, 0)));
                al0 = _mm256_add_pd(al0, _mm256_mul_pd(w0, left));
                ar0 = _mm256_add_pd(ar0, _mm256_mul_pd(w0, right));
                al1 = _mm256_add_pd(al1, _mm256_mul_pd(w1, left));
                ar1 = _mm256_add_pd(ar1, _mm256_mul_pd(w1, right));
                al2 = _mm256_add_pd(al2, _mm256_mul_pd(w2, left));
                ar2 = _mm256_add_pd(ar2, _mm256_mul_pd(w2, right));
                al3 = _mm256_add_pd(al3, _mm256_mul_pd(w3, left));
                ar3 = _mm256_add_pd(ar3, _mm256_mul_pd(w3, right));
            }
            let at = group0 * CLIP_LANE;
            _mm256_storeu_pd(acc_l.as_mut_ptr().add(at), al0);
            _mm256_storeu_pd(acc_r.as_mut_ptr().add(at), ar0);
            _mm256_storeu_pd(acc_l.as_mut_ptr().add(at + 4), al1);
            _mm256_storeu_pd(acc_r.as_mut_ptr().add(at + 4), ar1);
            _mm256_storeu_pd(acc_l.as_mut_ptr().add(at + 8), al2);
            _mm256_storeu_pd(acc_r.as_mut_ptr().add(at + 8), ar2);
            _mm256_storeu_pd(acc_l.as_mut_ptr().add(at + 12), al3);
            _mm256_storeu_pd(acc_r.as_mut_ptr().add(at + 12), ar3);
        }
    }
    (acc_l, acc_r)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn tighten_pair_sse(
    acc_l: &[f64; CLIP_MAJOR_GROUPS * CLIP_LANE],
    acc_r: &[f64; CLIP_MAJOR_GROUPS * CLIP_LANE],
    rows: &[ClipRow],
    ceiling: f32,
    limit: f64,
) -> (f64, f64, f64, f64, bool, bool) {
    use core::arch::x86_64::{
        _mm_and_pd, _mm_blendv_pd, _mm_castsi128_pd, _mm_cmpgt_pd, _mm_max_pd, _mm_min_pd,
        _mm_movemask_pd, _mm_mul_pd, _mm_set_pd, _mm_set1_epi64x, _mm_set1_pd, _mm_storeu_pd,
        _mm_sub_pd,
    };
    let mut fail_l = false;
    let mut fail_r = false;
    let mut lo_s = [0.0f64; 2];
    let mut hi_s = [0.0f64; 2];
    // SAFETY: AVX2 is enabled. Both lanes hold finite f64 values.
    unsafe {
        let mut lo = _mm_set1_pd(-f64::from(ceiling));
        let mut hi = _mm_set1_pd(f64::from(ceiling));
        let limit_v = _mm_set1_pd(limit);
        let neg_limit = _mm_set1_pd(-limit);
        let abs_mask = _mm_castsi128_pd(_mm_set1_epi64x(i64::MAX));
        for (row_i, row) in rows.iter().enumerate() {
            let off = _mm_set_pd(acc_r[row_i], acc_l[row_i]);
            if row.slope.abs() <= 1.0e-12 {
                let abs_off = _mm_and_pd(off, abs_mask);
                let over = _mm_movemask_pd(_mm_cmpgt_pd(abs_off, limit_v));
                if over & 1 != 0 {
                    fail_l = true;
                }
                if over & 2 != 0 {
                    fail_r = true;
                }
                if fail_l && fail_r {
                    return (0.0, 0.0, 0.0, 0.0, true, true);
                }
                continue;
            }
            let inv = _mm_set1_pd(row.inv_slope);
            let a = _mm_mul_pd(_mm_sub_pd(neg_limit, off), inv);
            let b = _mm_mul_pd(_mm_sub_pd(limit_v, off), inv);
            let swap = _mm_cmpgt_pd(a, b);
            let lower = _mm_blendv_pd(a, b, swap);
            let upper = _mm_blendv_pd(b, a, swap);
            lo = _mm_max_pd(lo, lower);
            hi = _mm_min_pd(hi, upper);
        }
        _mm_storeu_pd(lo_s.as_mut_ptr(), lo);
        _mm_storeu_pd(hi_s.as_mut_ptr(), hi);
    }
    (lo_s[0], hi_s[0], lo_s[1], hi_s[1], fail_l, fail_r)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn project_held_pair_avx2(
    major: &[f64],
    zi: usize,
    played_l: &[f64; INTERP_DELAY],
    played_r: &[f64; INTERP_DELAY],
    rows: &[ClipRow],
    samples: (f32, f32),
    ceiling: f32,
) -> (f32, f32) {
    let mut lo_l = -f64::from(ceiling);
    let mut hi_l = f64::from(ceiling);
    let mut lo_r = -f64::from(ceiling);
    let mut hi_r = f64::from(ceiling);
    let limit = f64::from(ceiling) * (1.0 - 1.0e-5);
    // SAFETY: this function enables AVX2, and `avx_dots_only` requires it.
    let (acc_l, acc_r) = unsafe { avx_dots_only(major, zi, played_l, played_r) };
    let finite = limit.is_finite()
        && (0..CLIP_ROW_STRIDE).all(|i| acc_l[i].is_finite() && acc_r[i].is_finite());
    if finite {
        // SAFETY: this function enables AVX2, and `tighten_pair_sse` requires it.
        let (lo_l, hi_l, lo_r, hi_r, fail_l, fail_r) =
            unsafe { tighten_pair_sse(&acc_l, &acc_r, rows, ceiling, limit) };
        return (
            if fail_l {
                0.0
            } else {
                choose_interval(samples.0, lo_l, hi_l)
            },
            if fail_r {
                0.0
            } else {
                choose_interval(samples.1, lo_r, hi_r)
            },
        );
    }
    let mut fail_l = false;
    let mut fail_r = false;
    for (row_i, row) in rows.iter().enumerate() {
        if !fail_l
            && !tighten_with_inv(
                &mut lo_l,
                &mut hi_l,
                acc_l[row_i],
                row.slope,
                row.inv_slope,
                limit,
            )
        {
            fail_l = true;
        }
        if !fail_r
            && !tighten_with_inv(
                &mut lo_r,
                &mut hi_r,
                acc_r[row_i],
                row.slope,
                row.inv_slope,
                limit,
            )
        {
            fail_r = true;
        }
        if fail_l && fail_r {
            return (0.0, 0.0);
        }
    }
    (
        if fail_l {
            0.0
        } else {
            choose_interval(samples.0, lo_l, hi_l)
        },
        if fail_r {
            0.0
        } else {
            choose_interval(samples.1, lo_r, hi_r)
        },
    )
}

fn dot_row(row: &ClipRow, played: &[f64; INTERP_DELAY]) -> f64 {
    let mut off = 0.0f64;
    for i in 0..row.n as usize {
        off += row.w[i] * played[row.idx[i] as usize];
    }
    off
}

/// Intersect `[lo, hi]` with `|off + slope * c| <= limit`.
fn tighten_abs(lo: &mut f64, hi: &mut f64, off: f64, slope: f64, limit: f64) -> bool {
    if !off.is_finite() || !slope.is_finite() || !limit.is_finite() {
        return false;
    }
    if slope.abs() <= 1.0e-12 {
        return off.abs() <= limit;
    }
    let mut lower = (-limit - off) / slope;
    let mut upper = (limit - off) / slope;
    if lower > upper {
        std::mem::swap(&mut lower, &mut upper);
    }
    if lower > *lo {
        *lo = lower;
    }
    if upper < *hi {
        *hi = upper;
    }
    true
}

/// Write the sample that was just emitted. Does not read the other slots.
fn store_played(hist: &mut InterpHist, sample: f32) {
    hist.z[hist.zi] = sample;
    hist.zi += 1;
    if hist.zi == INTERP_DELAY {
        hist.zi = 0;
    }
}

/// Half of the samples after the detector delay, and at least 1.
///
/// The ramp finishes while the opening delay is still silence, and a later
/// peak still reaches its gain before that peak leaves the delay.
fn attack_span(lookahead: u32) -> f32 {
    let ahead = lookahead.saturating_sub(DETECT_DELAY_SAMPLES);
    if ahead <= 1 {
        1.0
    } else {
        (ahead / 2).max(1) as f32
    }
}

fn reduction_db(gain: f32) -> f32 {
    if !gain.is_finite() || gain >= 1.0 || gain <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * gain.log10();
    if db.is_finite() && db < 0.0 { db } else { 0.0 }
}

fn finite_mul(sample: f32, gain: f32) -> f32 {
    let out = sample * gain;
    if out.is_finite() { out } else { 0.0 }
}

fn usable_rate(sample_rate: u32) -> Option<u32> {
    if (SAMPLE_RATE_MIN..=SAMPLE_RATE_MAX).contains(&sample_rate) {
        Some(sample_rate)
    } else {
        None
    }
}

fn sanitize_ceiling(db: f32) -> f32 {
    if db.is_finite() {
        db.clamp(LIMIT_CEILING_MIN_DB, LIMIT_CEILING_MAX_DB)
    } else {
        LIMIT_CEILING_DEFAULT_DB
    }
}

fn sanitize_lookahead(ms: f32) -> f32 {
    if ms.is_finite() {
        ms.clamp(LIMIT_LOOKAHEAD_MIN_MS, LIMIT_LOOKAHEAD_MAX_MS)
    } else {
        LIMIT_LOOKAHEAD_DEFAULT_MS
    }
}

#[derive(Clone, Copy, Debug)]
struct Phase {
    count: u8,
    index: [u8; INTERP_DELAY],
    coeff: [f64; INTERP_DELAY],
    spread: [f64; INTERP_DELAY * 2],
}

#[derive(Clone, Copy, Debug)]
struct InterpTables {
    phases: [Phase; INTERP_FACTOR],
}

impl InterpTables {
    fn new() -> Self {
        let mut phases = [Phase {
            count: 0,
            index: [0; INTERP_DELAY],
            coeff: [0.0; INTERP_DELAY],
            spread: [0.0; INTERP_DELAY * 2],
        }; INTERP_FACTOR];
        for tap in 0..INTERP_TAPS {
            let m = tap as f64 - (INTERP_TAPS - 1) as f64 / 2.0;
            let mut coeff = 1.0;
            if m.abs() > ALMOST_ZERO {
                let x = m * std::f64::consts::PI / INTERP_FACTOR as f64;
                coeff = x.sin() / x;
            }
            let window = 0.5
                * (1.0
                    - (2.0 * std::f64::consts::PI * tap as f64 / (INTERP_TAPS - 1) as f64).cos());
            coeff *= window;
            if coeff.abs() <= ALMOST_ZERO {
                continue;
            }
            let phase = tap % INTERP_FACTOR;
            let slot = phases[phase].count as usize;
            if slot >= INTERP_DELAY {
                continue;
            }
            phases[phase].index[slot] = (tap / INTERP_FACTOR) as u8;
            phases[phase].coeff[slot] = coeff;
            phases[phase].spread[slot * 2] = coeff;
            phases[phase].spread[slot * 2 + 1] = coeff;
            phases[phase].count += 1;
        }
        Self { phases }
    }

    fn push(&self, hist: &mut InterpHist, sample: f32) -> f32 {
        let x = if sample.is_finite() { sample } else { 0.0 };
        let zi = hist.zi;
        hist.store(zi, x);
        let peak = self.dot_peak(hist, hist, zi);
        hist.advance();
        peak
    }

    /// One 4× pass for both channels. The per-channel sums match [`Self::push`].
    fn push_pair(
        &self,
        left: &mut InterpHist,
        right: &mut InterpHist,
        packed: &mut [f64; INTERP_DELAY * 4],
        sample_l: f32,
        sample_r: f32,
    ) -> f32 {
        let xl = if sample_l.is_finite() { sample_l } else { 0.0 };
        let xr = if sample_r.is_finite() { sample_r } else { 0.0 };
        let zi = left.zi;
        left.store(zi, xl);
        right.store(zi, xr);
        let xl64 = f64::from(xl);
        let xr64 = f64::from(xr);
        let at = zi * 2;
        packed[at] = xl64;
        packed[at + 1] = xr64;
        let mirror = (zi + INTERP_DELAY) * 2;
        packed[mirror] = xl64;
        packed[mirror + 1] = xr64;
        let peak = self.dot_peak_packed(packed, zi);
        left.advance();
        right.advance();
        peak
    }

    fn dot_peak_packed(&self, packed: &[f64; INTERP_DELAY * 4], zi: usize) -> f32 {
        #[cfg(target_arch = "x86_64")]
        {
            self.dot_peak_packed_sse(packed, zi)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            self.dot_peak_packed_scalar(packed, zi)
        }
    }

    fn dot_peak(&self, left: &InterpHist, right: &InterpHist, zi: usize) -> f32 {
        self.dot_peak_scalar(left, right, zi)
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn dot_peak_packed_scalar(&self, packed: &[f64; INTERP_DELAY * 4], zi: usize) -> f32 {
        let base = zi + INTERP_DELAY;
        let phases = &self.phases;
        let mut al0 = 0.0f64;
        let mut ar0 = 0.0f64;
        let mut al1 = 0.0f64;
        let mut ar1 = 0.0f64;
        let mut al2 = 0.0f64;
        let mut ar2 = 0.0f64;
        let mut al3 = 0.0f64;
        let mut ar3 = 0.0f64;
        for tap in 0..INTERP_DELAY {
            let i0 = (base - phases[0].index[tap] as usize) * 2;
            let i1 = (base - phases[1].index[tap] as usize) * 2;
            let i2 = (base - phases[2].index[tap] as usize) * 2;
            let i3 = (base - phases[3].index[tap] as usize) * 2;
            let c0 = phases[0].coeff[tap];
            let c1 = phases[1].coeff[tap];
            let c2 = phases[2].coeff[tap];
            let c3 = phases[3].coeff[tap];
            al0 += packed[i0] * c0;
            ar0 += packed[i0 + 1] * c0;
            al1 += packed[i1] * c1;
            ar1 += packed[i1 + 1] * c1;
            al2 += packed[i2] * c2;
            ar2 += packed[i2 + 1] * c2;
            al3 += packed[i3] * c3;
            ar3 += packed[i3 + 1] * c3;
        }
        let mut peak = 0.0f32;
        for acc in [al0, ar0, al1, ar1, al2, ar2, al3, ar3] {
            let y = acc as f32;
            let level = if y.is_finite() { y.abs() } else { 0.0 };
            if level > peak {
                peak = level;
            }
        }
        peak
    }

    #[cfg(target_arch = "x86_64")]
    fn dot_peak_packed_sse(&self, packed: &[f64; INTERP_DELAY * 4], zi: usize) -> f32 {
        use core::arch::x86_64::{
            _mm_add_pd, _mm_loadu_pd, _mm_mul_pd, _mm_setzero_pd, _mm_storeu_pd,
        };
        let base = zi + INTERP_DELAY;
        let phases = &self.phases;
        let mut lanes = [0.0f64; 8];
        // SAFETY: x86_64 always has SSE2. `packed` holds left, right pairs. `lanes` is eight local f64 slots.
        unsafe {
            let mut acc0 = _mm_setzero_pd();
            let mut acc1 = _mm_setzero_pd();
            let mut acc2 = _mm_setzero_pd();
            let mut acc3 = _mm_setzero_pd();
            let samples = packed.as_ptr();
            for tap in 0..INTERP_DELAY {
                let i0 = (base - phases[0].index[tap] as usize) * 2;
                let i1 = (base - phases[1].index[tap] as usize) * 2;
                let i2 = (base - phases[2].index[tap] as usize) * 2;
                let i3 = (base - phases[3].index[tap] as usize) * 2;
                let s0 = _mm_loadu_pd(samples.add(i0));
                let s1 = _mm_loadu_pd(samples.add(i1));
                let s2 = _mm_loadu_pd(samples.add(i2));
                let s3 = _mm_loadu_pd(samples.add(i3));
                let c0 = _mm_loadu_pd(phases[0].spread.as_ptr().add(tap * 2));
                let c1 = _mm_loadu_pd(phases[1].spread.as_ptr().add(tap * 2));
                let c2 = _mm_loadu_pd(phases[2].spread.as_ptr().add(tap * 2));
                let c3 = _mm_loadu_pd(phases[3].spread.as_ptr().add(tap * 2));
                acc0 = _mm_add_pd(acc0, _mm_mul_pd(s0, c0));
                acc1 = _mm_add_pd(acc1, _mm_mul_pd(s1, c1));
                acc2 = _mm_add_pd(acc2, _mm_mul_pd(s2, c2));
                acc3 = _mm_add_pd(acc3, _mm_mul_pd(s3, c3));
            }
            _mm_storeu_pd(lanes.as_mut_ptr(), acc0);
            _mm_storeu_pd(lanes.as_mut_ptr().add(2), acc1);
            _mm_storeu_pd(lanes.as_mut_ptr().add(4), acc2);
            _mm_storeu_pd(lanes.as_mut_ptr().add(6), acc3);
        }
        let mut peak = 0.0f32;
        for acc in lanes {
            let y = acc as f32;
            let level = if y.is_finite() { y.abs() } else { 0.0 };
            if level > peak {
                peak = level;
            }
        }
        peak
    }

    fn dot_peak_scalar(&self, left: &InterpHist, right: &InterpHist, zi: usize) -> f32 {
        let base = zi + INTERP_DELAY;
        let phases = &self.phases;
        let mut al0 = 0.0f64;
        let mut ar0 = 0.0f64;
        let mut al1 = 0.0f64;
        let mut ar1 = 0.0f64;
        let mut al2 = 0.0f64;
        let mut ar2 = 0.0f64;
        let mut al3 = 0.0f64;
        let mut ar3 = 0.0f64;
        for tap in 0..INTERP_DELAY {
            let i0 = base - phases[0].index[tap] as usize;
            let i1 = base - phases[1].index[tap] as usize;
            let i2 = base - phases[2].index[tap] as usize;
            let i3 = base - phases[3].index[tap] as usize;
            let c0 = phases[0].coeff[tap];
            let c1 = phases[1].coeff[tap];
            let c2 = phases[2].coeff[tap];
            let c3 = phases[3].coeff[tap];
            al0 += f64::from(left.z[i0]) * c0;
            ar0 += f64::from(right.z[i0]) * c0;
            al1 += f64::from(left.z[i1]) * c1;
            ar1 += f64::from(right.z[i1]) * c1;
            al2 += f64::from(left.z[i2]) * c2;
            ar2 += f64::from(right.z[i2]) * c2;
            al3 += f64::from(left.z[i3]) * c3;
            ar3 += f64::from(right.z[i3]) * c3;
        }
        let mut peak = 0.0f32;
        for acc in [al0, ar0, al1, ar1, al2, ar2, al3, ar3] {
            let y = acc as f32;
            let level = if y.is_finite() { y.abs() } else { 0.0 };
            if level > peak {
                peak = level;
            }
        }
        peak
    }
}

#[derive(Clone, Copy, Debug)]
struct InterpHist {
    z: [f32; INTERP_DELAY * 2],
    zi: usize,
}

impl InterpHist {
    fn new() -> Self {
        Self {
            z: [0.0; INTERP_DELAY * 2],
            zi: 0,
        }
    }

    fn store(&mut self, zi: usize, sample: f32) {
        self.z[zi] = sample;
        self.z[zi + INTERP_DELAY] = sample;
    }

    fn advance(&mut self) {
        self.zi += 1;
        if self.zi == INTERP_DELAY {
            self.zi = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::sync::Arc;
    use std::thread;

    fn render_master(left: &mut [f32], right: &mut [f32], rate: u32) -> (LimitWriter, LimitReader) {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate) || rate == 48_000);
        let mut runtime = LimitRuntime::new();
        let mut offset = 0;
        while offset < left.len() {
            let end = (offset + 256).min(left.len());
            runtime.process_master(
                &mut reader,
                &mut left[offset..end],
                &mut right[offset..end],
                rate,
            );
            offset = end;
        }
        (writer, reader)
    }
    #[test]
    fn a_bypassed_reader_does_not_limit_the_master() {
        let (_writer, mut reader) = (open_inner(false).0, LimitReader::bypassed());
        let _ = _writer;
        let mut runtime = LimitRuntime::new();
        let input = [1.0f32, -1.0, 0.8, -0.2];
        let mut left = input;
        let mut right = input;
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        assert_eq!(left, input);
        assert_eq!(right, input);
        assert!(runtime.master_bypassed());
        assert_eq!(reader.master_gain_reduction_db(), 0.0);
    }
    #[test]
    fn quiet_audio_is_delayed_by_the_lookahead_and_not_reduced() {
        let rate = 48_000u32;
        let delay = lookahead_samples(rate, 5.0) as usize;
        assert_eq!(delay, 240);
        let frames = delay + 32;
        let mut left: Vec<f32> = (0..frames)
            .map(|index| if index % 2 == 0 { 0.05 } else { -0.04 })
            .collect();
        let mut right = left.clone();
        right[3] = -0.0;
        let original_l = left.clone();
        let original_r = right.clone();
        let (_writer, reader) = render_master(&mut left, &mut right, rate);
        assert_eq!(left[..delay], vec![0.0; delay]);
        assert_eq!(left[delay..], original_l[..frames - delay]);
        assert_eq!(right[delay..], original_r[..frames - delay]);
        assert_eq!(right[delay + 3].to_bits(), (-0.0f32).to_bits());
        assert_eq!(reader.master_gain_reduction_db(), 0.0);
    }
    #[test]
    fn constant_true_peak_of_a_full_scale_sine_is_near_unity() {
        let rate = 48_000usize;
        let mut left = vec![0.0f32; rate];
        for (index, sample) in left.iter_mut().enumerate() {
            let phase = 1000.0 * index as f32 / rate as f32;
            *sample = (phase * std::f32::consts::TAU).sin();
        }
        let peak = measure_true_peak(&left, &left);
        let db = db::linear_to_db(peak);
        assert!(
            (db - 0.0).abs() < 0.2,
            "a 0 dBFS sine measured {db} dBTP (linear {peak})"
        );
    }
    #[test]
    fn lookahead_off_still_limits_a_hot_constant() {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(48_000));
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(0.0))
                .accepted()
        );
        assert_eq!(writer.master_latency_samples(), 0);
        let mut runtime = LimitRuntime::new();
        let mut left = vec![2.0f32; 4_096];
        let mut right = left.clone();
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        let peak = measure_true_peak(&left, &right);
        let db = db::linear_to_db(peak);
        assert!(db <= -1.0 + 1.0e-3, "lookahead off true peak {db} dBTP");
        assert!(reader.master_gain_reduction_db() < 0.0);
        assert_eq!(writer.reported_latency_samples(), 0);
    }
    #[test]
    fn a_different_rate_still_limits_before_the_loop_republishes() {
        let (writer, mut reader) = open();
        assert_eq!(writer.sample_rate(), 48_000);
        let rate = 44_100u32;
        let delay = lookahead_samples(rate, 5.0) as usize;
        assert_eq!(delay, 221);
        let frames = delay + rate as usize / 2;
        let mut left = vec![2.0f32; frames];
        let mut right = left.clone();
        let mut runtime = LimitRuntime::new();
        let mut offset = 0;
        while offset < frames {
            let end = (offset + 256).min(frames);
            runtime.process_master(
                &mut reader,
                &mut left[offset..end],
                &mut right[offset..end],
                rate,
            );
            offset = end;
        }
        assert!(
            !runtime.master_bypassed(),
            "the master stays on when the callback rate is not 48 kHz yet"
        );
        assert!(
            left[..delay].iter().all(|sample| *sample == 0.0),
            "the opening lookahead is silence, not the unlimited input"
        );
        assert_ne!(left[delay], 2.0);
        let peak = measure_true_peak(&left, &right);
        let db = db::linear_to_db(peak);
        assert!(
            db <= -1.0 + 1.0e-3,
            "44.1 kHz startup true peak {db} dBTP is above -1"
        );
    }
    #[test]
    fn detector_window_and_interpolator_per_sample_cost_is_measured() {
        if cfg!(debug_assertions) {
            crate::measured(
                "MEASURED detector query: debug profile, not compared with a settled block",
            );
            return;
        }
        let window = 1921u32;
        let n = 100_000u32;
        let mut mins = MinDeque::new(MAX_LOOKAHEAD_SAMPLES + 1);
        for i in 0..window {
            mins.push(i, 0.5, window);
        }
        let mut acc = 0.0f32;
        let start = std::time::Instant::now();
        for i in 0..n {
            let clock = window.wrapping_add(i);
            mins.push(clock, 0.5, window);
            acc += mins.min(clock, window);
        }
        let one = start.elapsed();
        let start = std::time::Instant::now();
        for i in 0..n {
            let clock = window.wrapping_add(i);
            mins.push(clock, 0.5, window);
            acc += mins.min(clock, window);
            acc += mins.min(clock, window);
        }
        let two = start.elapsed();
        std::hint::black_box(acc);
        let tables = InterpTables::new();
        let mut hist = InterpHist::new();
        let mut live = InterpHist::new();
        let mut peak = 0.0f32;
        let start = std::time::Instant::now();
        for i in 0..n {
            let sample = if i % 2 == 0 { 0.85 } else { -0.85 };
            peak = peak.max(tables.push(&mut hist, sample));
        }
        let push_one = start.elapsed();
        let start = std::time::Instant::now();
        for i in 0..n {
            let sample = if i % 2 == 0 { 0.85 } else { -0.85 };
            peak = peak.max(
                tables
                    .push(&mut hist, sample)
                    .max(tables.push(&mut live, sample)),
            );
        }
        let push_two = start.elapsed();
        std::hint::black_box(peak);
        let ns = |elapsed: std::time::Duration| elapsed.as_secs_f64() * 1.0e9 / f64::from(n);
        let one_ns = ns(one);
        let two_ns = ns(two);
        let push_one_ns = ns(push_one);
        let push_two_ns = ns(push_two);
        crate::measured(&format!(
            "MEASURED detector query: one window {one_ns:.1} ns/sample, two windows {two_ns:.1} ns/sample, second query {diff_q:.1} ns/sample, one 49-tap {push_one_ns:.1} ns/sample, two 49-tap pushes {push_two_ns:.1} ns/sample, extra push {diff_p:.1} ns/sample",
            diff_q = two_ns - one_ns,
            diff_p = push_two_ns - push_one_ns,
        ));
    }
    #[test]
    fn hot_signal_stays_at_or_under_minus_1_dbtp() {
        let rate = 48_000u32;
        let frames = rate as usize;
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        for index in 0..frames {
            let t = index as f32 / rate as f32;
            let sine = (t * 997.0 * std::f32::consts::TAU).sin();
            let sine_r = ((t * 997.0 + 0.17) * std::f32::consts::TAU).sin();
            let body = if index < frames / 2 {
                sine
            } else {
                sine.signum()
            };
            let body_r = if index < frames / 2 {
                sine_r
            } else {
                sine_r.signum()
            };
            left[index] = 2.0 * body;
            right[index] = 2.0 * body_r;
            if index == frames / 4 || index == (frames * 3) / 4 {
                left[index] = 4.0;
                right[index] = -4.0;
            }
        }
        let (_writer, reader) = render_master(&mut left, &mut right, rate);
        let peak = measure_true_peak(&left, &right);
        let db = db::linear_to_db(peak);
        let delay = lookahead_samples(rate, 5.0) as usize;
        let skip = delay + rate as usize / 20;
        let body = measure_true_peak(&left[skip..], &right[skip..]);
        let body_db = db::linear_to_db(body);
        crate::measured(&format!(
            "MEASURED limiter true peak: {db} dBTP, steady {body_db} dBTP, guard {} dB",
            LIMIT_GUARD_DB
        ));
        assert!(
            db <= -1.0 + 1.0e-3,
            "hot signal true peak {db} dBTP is above -1"
        );
        assert!(body_db > -4.0, "steady true peak {body_db} dBTP collapsed");
        assert!(reader.master_gain_reduction_db() < -0.5);
        if let Ok(path) = std::env::var("OMAMIX_LIMIT_WAV") {
            write_f32_wav(std::path::Path::new(&path), rate, &left, &right);
        }
    }
    #[test]
    fn non_finite_input_does_not_poison_an_enabled_master() {
        let (writer, mut reader) = open();
        assert!(writer.master_settings().enabled());
        let mut runtime = LimitRuntime::new();
        let mut left = [f32::NAN, f32::INFINITY, 0.2, -0.2];
        let mut right = [0.2, f32::NEG_INFINITY, f32::NAN, 0.1];
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        assert!(
            left.iter()
                .chain(right.iter())
                .all(|sample| sample.is_finite())
        );
    }
    #[test]
    fn published_blocks_do_not_tear() {
        let channel = Arc::new(LimitChannel::new(false));
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
                    let bits = dst.stamp_uniform().expect("torn limiter block");
                    assert_eq!(dst.sample_rate, bits);
                    saw = saw.saturating_add(1);
                }
            }
            if consumer.poll(&mut read_idx, &mut dst) {
                let bits = dst.stamp_uniform().expect("torn limiter block");
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
        let channel = LimitChannel::new(false);
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
    fn fill_hot_noise(left: &mut [f32], right: &mut [f32], state: &mut u32) {
        for index in 0..left.len().min(right.len()) {
            *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let l = (*state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let r = (*state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            left[index] = l * 2.0;
            right[index] = r * 2.0;
        }
    }
    #[test]
    fn clip_plan_is_built_off_the_audio_thread() {
        let before = clip_plan();
        let _runtime = LimitRuntime::new();
        let after = clip_plan();
        assert!(
            after.is_some(),
            "LimitRuntime::new left the clip plan unbuilt"
        );
        if let Some(plan) = before {
            assert!(std::ptr::eq(plan, after.expect("plan")));
        }
    }
    #[test]
    fn non_finite_input_at_zero_lookahead_stays_finite() {
        let (mut writer, mut reader) = open();
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(0.0))
                .accepted()
        );
        assert_eq!(writer.master_latency_samples(), 0);
        let mut runtime = LimitRuntime::new();
        let mut left = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25];
        let mut right = [1.0, f32::NAN, -0.5, f32::INFINITY];
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        assert!(
            left.iter()
                .chain(right.iter())
                .all(|sample| sample.is_finite()),
            "left {left:?} right {right:?}"
        );
    }
    #[test]
    fn guard_and_scale_hold_sine_square_bursts_and_noise() {
        for rate in [48_000u32, 96_000] {
            for ms in [0.0f32, 5.0] {
                for kind in ["sine", "square", "noise"] {
                    let frames = rate as usize;
                    let mut left = vec![0.0f32; frames];
                    let mut right = vec![0.0f32; frames];
                    let mut state = 0x1234_5678u32;
                    for index in 0..frames {
                        let t = index as f32 / rate as f32;
                        let (sample_l, sample_r) = match kind {
                            "sine" => {
                                let l = (t * 997.0 * std::f32::consts::TAU).sin();
                                let r = ((t * 997.0 + 0.17) * std::f32::consts::TAU).sin();
                                (l, r)
                            }
                            "square" => {
                                let on = (index / 64) % 2 == 0;
                                if on { (1.0, -1.0) } else { (0.0, 0.0) }
                            }
                            _ => {
                                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                                let l = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                                let r = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                                (l, r)
                            }
                        };
                        left[index] = 2.0 * sample_l;
                        right[index] = 2.0 * sample_r;
                    }
                    let (mut writer, mut reader) = open();
                    assert!(writer.set_sample_rate(rate));
                    assert!(writer.apply_master(LimitUpdate::LookaheadMs(ms)).accepted());
                    let mut runtime = LimitRuntime::new();
                    let mut offset = 0;
                    while offset < frames {
                        let end = (offset + 64).min(frames);
                        runtime.process_master(
                            &mut reader,
                            &mut left[offset..end],
                            &mut right[offset..end],
                            rate,
                        );
                        offset = end;
                    }
                    let peak = measure_true_peak(&left, &right);
                    let db = db::linear_to_db(peak);
                    let skip = lookahead_samples(rate, ms) as usize + rate as usize / 20;
                    let steady = measure_true_peak(&left[skip..], &right[skip..]);
                    let steady_db = db::linear_to_db(steady);
                    crate::measured(&format!(
                        "MEASURED limiter {kind} at {rate} Hz, {ms} ms: {db} dBTP, steady {steady_db} dBTP"
                    ));
                    assert!(
                        db <= -1.0 + 1.0e-3,
                        "{kind} at {rate} Hz, {ms} ms measured {db} dBTP"
                    );
                    if ms == 0.0 {
                        assert!(
                            db > -1.5,
                            "{kind} at {rate} Hz, 0 ms measured {db} dBTP, more than 0.5 dB under the ceiling"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn ceiling_ramps_across_256_samples_not_the_quantum() {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(48_000));
        let mut runtime = LimitRuntime::new();
        let mut quiet = vec![0.05f32; 64];
        let mut quiet_r = quiet.clone();
        runtime.process_master(&mut reader, &mut quiet, &mut quiet_r, 48_000);
        let before = runtime.master_ceiling_linear();
        assert!(!runtime.master_ramping());
        assert!(
            writer
                .apply_master(LimitUpdate::CeilingDb(-12.0))
                .accepted()
        );
        let target = guarded_ceiling_linear(-12.0);
        assert!((target - before).abs() > 1.0e-4);
        runtime.process_master(&mut reader, &mut quiet, &mut quiet_r, 48_000);
        assert!(
            runtime.master_ramping(),
            "a 64-sample block must not finish the 256-sample ramp"
        );
        let mid = runtime.master_ceiling_linear();
        assert!(
            (mid - target).abs() > 1.0e-4,
            "ceiling reached the target inside one short quantum"
        );
        assert!((mid - before).abs() > 1.0e-6);
        for _ in 0..3 {
            runtime.process_master(&mut reader, &mut quiet, &mut quiet_r, 48_000);
        }
        assert!(!runtime.master_ramping());
        let done = runtime.master_ceiling_linear();
        assert!(
            (done - target).abs() < 1.0e-5,
            "settled {done} target {target}"
        );
    }
    #[test]
    fn lookahead_change_does_not_insert_silence() {
        let rate = 48_000u32;
        let frames = rate as usize;
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        for index in 0..frames {
            let t = index as f32 / rate as f32;
            left[index] = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
            right[index] = left[index];
        }
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        let mut runtime = LimitRuntime::new();
        let mut offset = 0;
        while offset < frames {
            let end = (offset + 256).min(frames);
            runtime.process_master(
                &mut reader,
                &mut left[offset..end],
                &mut right[offset..end],
                rate,
            );
            offset = end;
        }
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(20.0))
                .accepted()
        );
        let mut more_l = left.clone();
        let mut more_r = right.clone();
        runtime.process_master(&mut reader, &mut more_l[..256], &mut more_r[..256], rate);
        let energy = more_l[..256].iter().map(|sample| sample.abs()).sum::<f32>();
        assert!(
            energy > 10.0,
            "lookahead change inserted silence, energy {energy}"
        );
    }
    #[test]
    fn steady_fast_path_drift_stays_bounded() {
        ensure_clip_plan();
        let tables = InterpTables::new();
        let mut hist = InterpHist::new();
        let ceiling = db::db_to_linear(-1.0 - LIMIT_GUARD_DB);
        let level = ceiling - 0.05;
        let mut tail = Vec::new();
        for index in 0..512 {
            let out = pull_under_ceiling(&tables, &mut hist, level, ceiling);
            if index >= 64 {
                tail.push(out);
            }
        }
        let min = tail.iter().copied().fold(f32::INFINITY, f32::min);
        let max = tail.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let drift_db = db::linear_to_db(max) - db::linear_to_db(min);
        crate::measured(&format!(
            "MEASURED steady fast-path drift: {drift_db} dB from {min} to {max}"
        ));
        assert!(
            drift_db < 0.01,
            "smooth_hold drifted {drift_db} dB on a constant under the headroom"
        );
        assert!(
            (min - level).abs() < 1.0e-4 && (max - level).abs() < 1.0e-4,
            "fast path left {level}, range {min}..{max}"
        );
    }
    #[test]
    fn closed_form_noise_sits_near_the_thirteen_alignment_preview() {
        ensure_clip_plan();
        let tables = InterpTables::new();
        let ceiling = db::db_to_linear(-1.0 - LIMIT_GUARD_DB);
        let rate = 48_000u32;
        let frames = rate as usize;
        let mut noise = 0x1234_5678u32;
        let mut hot_l = vec![0.0f32; frames];
        let mut hot_r = vec![0.0f32; frames];
        fill_hot_noise(&mut hot_l, &mut hot_r, &mut noise);
        let mut seven_l = hot_l.clone();
        let mut seven_r = hot_r.clone();
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(0.0))
                .accepted()
        );
        let mut runtime = LimitRuntime::new();
        let mut offset = 0;
        while offset < frames {
            let end = (offset + 64).min(frames);
            runtime.process_master(
                &mut reader,
                &mut seven_l[offset..end],
                &mut seven_r[offset..end],
                rate,
            );
            offset = end;
        }
        let mut thirteen_l = hot_l.clone();
        let mut thirteen_r = hot_r.clone();
        let mut wide_l = InterpHist::new();
        let mut wide_r = InterpHist::new();
        for index in 0..frames {
            thirteen_l[index] = preview_thirteen(&tables, &mut wide_l, hot_l[index], ceiling);
            thirteen_r[index] = preview_thirteen(&tables, &mut wide_r, hot_r[index], ceiling);
        }
        let seven_db = db::linear_to_db(measure_true_peak(&seven_l, &seven_r));
        let thirteen_db = db::linear_to_db(measure_true_peak(&thirteen_l, &thirteen_r));
        let over = seven_db - thirteen_db;
        crate::measured(&format!(
            "MEASURED 7-alignment noise {seven_db} dBTP, 13-alignment preview {thirteen_db} dBTP, over {over} dB"
        ));
        assert!(
            seven_db <= -1.0 + 1.0e-3,
            "7-alignment noise {seven_db} dBTP"
        );
        assert!(
            seven_db > -1.5,
            "7-alignment noise {seven_db} dBTP collapsed"
        );
        assert!(
            (thirteen_db - (-1.0 - LIMIT_GUARD_DB)).abs() < 0.02,
            "13-alignment preview {thirteen_db} dBTP left the guard"
        );
        assert!(
            (over - 0.09).abs() < 0.03,
            "noise is {over} dB over the 13-alignment preview, not about 0.09 dB"
        );
    }
    #[test]
    fn hot_program_audio_counts_zeroed_samples() {
        let rate = 48_000u32;
        let frames = rate as usize;
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        for index in 0..frames {
            let t = index as f32 / rate as f32;
            let sine = (t * 997.0 * std::f32::consts::TAU).sin();
            let sine_r = ((t * 997.0 + 0.17) * std::f32::consts::TAU).sin();
            let body = if index < frames / 2 {
                sine
            } else {
                sine.signum()
            };
            let body_r = if index < frames / 2 {
                sine_r
            } else {
                sine_r.signum()
            };
            left[index] = 2.0 * body;
            right[index] = 2.0 * body_r;
            if index == frames / 4 || index == (frames * 3) / 4 {
                left[index] = 4.0;
                right[index] = -4.0;
            }
        }
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(0.0))
                .accepted()
        );
        let mut runtime = LimitRuntime::new();
        let mut offset = 0;
        while offset < frames {
            let end = (offset + 256).min(frames);
            runtime.process_master(
                &mut reader,
                &mut left[offset..end],
                &mut right[offset..end],
                rate,
            );
            offset = end;
        }
        let zeros = left
            .iter()
            .chain(right.iter())
            .filter(|sample| **sample == 0.0)
            .count();
        let total = left.len() + right.len();
        let db = db::linear_to_db(measure_true_peak(&left, &right));
        crate::measured(&format!(
            "MEASURED hot program zeroed samples: {zeros} of {total} at 0 ms, true peak {db} dBTP"
        ));
        assert!(db <= -1.0 + 1.0e-3, "hot program true peak {db} dBTP");
        assert!(
            zeros * 2 < total,
            "hot program audio zeroed {zeros} of {total} samples"
        );
    }
    #[test]
    fn constant_block_stays_on_the_flat_hold() {
        ensure_clip_plan();
        let tables = InterpTables::new();
        let mut hist = InterpHist::new();
        let ceiling = db::db_to_linear(-1.0 - LIMIT_GUARD_DB);
        let level = ceiling - 0.05;
        let mut tail_misses = 0u32;
        for index in 0..80 {
            let missed = smooth_hold(&hist, level, ceiling).is_none();
            let _ = pull_under_ceiling(&tables, &mut hist, level, ceiling);
            if index >= 64 && missed {
                tail_misses += 1;
            }
        }
        assert_eq!(tail_misses, 0, "smooth hold missed on a settled constant");
        assert!(
            smooth_hold(&hist, ceiling, ceiling).is_none(),
            "a sample on the ceiling must use the solve"
        );
    }
    #[test]
    fn clipper_emits_zero_without_clearing_history() {
        ensure_clip_plan();
        let tables = InterpTables::new();
        let mut hist = InterpHist::new();
        let ceiling = db::db_to_linear(-1.0 - LIMIT_GUARD_DB);
        assert!(ceiling > 0.0);
        for index in 0..INTERP_DELAY {
            let sample = if index % 2 == 0 { 2.0 } else { -2.0 };
            store_played(&mut hist, sample);
        }
        let before = hist.z;
        let desired = 1.5f32;
        assert!(smooth_hold(&hist, desired, ceiling).is_none());
        assert_eq!(
            project_held(&hist, desired, ceiling),
            0.0,
            "hot history fit an alignment"
        );
        let out = pull_under_ceiling(&tables, &mut hist, desired, ceiling);
        assert_eq!(out, 0.0);
        let wrote = (hist.zi + INTERP_DELAY - 1) % INTERP_DELAY;
        assert_eq!(hist.z[wrote], 0.0);
        for (index, (slot, previous)) in hist.z.iter().zip(before).enumerate() {
            if index == wrote {
                continue;
            }
            assert_eq!(*slot, previous, "slot {index} was cleared");
        }
        assert!(hist.z.contains(&2.0), "hot history left the ring");
        assert!(hist.z.contains(&-2.0), "hot history left the ring");
    }
    fn preview_thirteen(
        tables: &InterpTables,
        hist: &mut InterpHist,
        sample: f32,
        ceiling: f32,
    ) -> f32 {
        let alignments: [usize; INTERP_DELAY] = std::array::from_fn(|index| index);
        let rows = clip_rows(tables, hist.zi, &alignments);
        let mut played = [0.0f64; INTERP_DELAY];
        for (slot, value) in played.iter_mut().zip(hist.z) {
            *slot = f64::from(value);
        }
        let finite = if sample.is_finite() { sample } else { 0.0 };
        let ceiling = if ceiling.is_finite() && ceiling > 0.0 {
            ceiling
        } else {
            0.0
        };
        let chosen = if ceiling == 0.0 {
            0.0
        } else {
            solve_interval(finite, ceiling, &played, &rows)
        };
        store_played(hist, chosen);
        chosen
    }
    #[test]
    fn sliding_min_matches_a_brute_force_window() {
        let mut deque = MinDeque::new(64);
        let mut values = Vec::new();
        let window = 17u32;
        let mut state = 1u32;
        for index in 0..200u32 {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let value = (state % 1000) as f32 / 1000.0;
            values.push(value);
            deque.push(index, value, window);
            let start = index.saturating_sub(window - 1) as usize;
            let brute = values[start..=index as usize]
                .iter()
                .copied()
                .fold(1.0f32, f32::min);
            let got = deque.min(index, window);
            assert!(
                (got - brute).abs() < 1.0e-6,
                "index {index} got {got} brute {brute}"
            );
        }
        deque.push(200, 0.5, 4);
        let got = deque.min(200, 4);
        let brute = values[197..200].iter().copied().fold(0.5f32, f32::min);
        assert!((got - brute).abs() < 1.0e-6, "shrunk window {got} {brute}");
    }
    #[test]
    fn sliding_min_matches_across_the_clock_wrap() {
        let mut deque = MinDeque::new(MAX_LOOKAHEAD_SAMPLES + 1);
        let window = 240u32;
        let start = u32::MAX - 80;
        let mut values = Vec::new();
        let mut state = 7u32;
        for step in 0..160u32 {
            let index = start.wrapping_add(step);
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let value = (state % 1000) as f32 / 1000.0;
            values.push(value);
            deque.push(index, value, window);
            let mut brute = 1.0f32;
            for (age, stored) in values.iter().rev().enumerate() {
                if age as u32 >= window {
                    break;
                }
                brute = brute.min(*stored);
            }
            let got = deque.min(index, window);
            assert!(
                (got - brute).abs() < 1.0e-6,
                "wrap index {index} got {got} brute {brute}"
            );
        }
    }
    #[test]
    fn delay_line_does_not_glitch_when_the_clock_wraps() {
        let rate = 48_000u32;
        let delay = lookahead_samples(rate, 5.0) as usize;
        assert_eq!(delay, 240);
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(5.0))
                .accepted()
        );
        let mut runtime = LimitRuntime::new();
        let prime = delay + 32;
        let mut input = Vec::with_capacity(prime + 40);
        for index in 0..prime {
            input.push((index as f32 + 1.0) * 1.0e-4);
        }
        let mut left = input.clone();
        let mut right = input.clone();
        runtime.process_master(&mut reader, &mut left, &mut right, rate);
        for index in delay..prime {
            let expect = input[index - delay];
            assert!(
                (left[index] - expect).abs() < 1.0e-6,
                "prime {index} got {} expect {expect}",
                left[index]
            );
        }
        runtime.master.clock = u32::MAX - 5;
        runtime.master.mins.clear();
        let mut more = Vec::new();
        for index in 0..30 {
            more.push((prime + index) as f32 * 1.0e-4 + 0.002);
        }
        input.extend_from_slice(&more);
        let mut left = more.clone();
        let mut right = more.clone();
        runtime.process_master(&mut reader, &mut left, &mut right, rate);
        let base = prime;
        for index in 0..30 {
            let expect = input[base + index - delay];
            assert!(
                (left[index] - expect).abs() < 1.0e-6,
                "across wrap {index} got {} expect {expect}",
                left[index]
            );
        }
    }
    /// True peak of `left[start..]` / `right[start..]`, with the interpolator
    /// warmed by the samples before `start`.
    fn true_peak_from(left: &[f32], right: &[f32], start: usize) -> f32 {
        let tables = InterpTables::new();
        let mut hist_l = InterpHist::new();
        let mut hist_r = InterpHist::new();
        let mut peak = 0.0f32;
        let frames = left.len().min(right.len());
        for index in 0..frames {
            let level = tables
                .push(&mut hist_l, left[index])
                .max(tables.push(&mut hist_r, right[index]));
            if index >= start && level > peak {
                peak = level;
            }
        }
        peak
    }
    #[test]
    fn master_lookahead_change_keeps_the_gain_aligned() {
        let rate = 48_000u32;
        for (from_ms, to_ms) in [(5.0f32, 20.0f32), (20.0, 5.0), (20.0, 0.0)] {
            let frames = rate as usize / 2;
            let mut left = vec![0.0f32; frames];
            let mut right = vec![0.0f32; frames];
            for index in 0..frames {
                let t = index as f32 / rate as f32;
                left[index] = (t * 997.0 * std::f32::consts::TAU).sin() * 2.0;
                right[index] = ((t * 997.0 + 0.17) * std::f32::consts::TAU).sin() * 2.0;
            }
            let (mut writer, mut reader) = open();
            assert!(writer.set_sample_rate(rate));
            assert!(
                writer
                    .apply_master(LimitUpdate::LookaheadMs(from_ms))
                    .accepted()
            );
            let mut runtime = LimitRuntime::new();
            let mut offset = 0;
            while offset < frames {
                let end = (offset + 256).min(frames);
                runtime.process_master(
                    &mut reader,
                    &mut left[offset..end],
                    &mut right[offset..end],
                    rate,
                );
                offset = end;
            }
            assert!(
                writer
                    .apply_master(LimitUpdate::LookaheadMs(to_ms))
                    .accepted()
            );
            let mut more_l = left.clone();
            let mut more_r = right.clone();
            runtime.process_master(&mut reader, &mut more_l[..256], &mut more_r[..256], rate);
            let energy: f32 = more_l[..256].iter().map(|sample| sample.abs()).sum();
            assert!(
                energy > 10.0,
                "lookahead {from_ms} to {to_ms} dropped the block, energy {energy}"
            );
            let mut whole_l = left;
            let mut whole_r = right;
            let start = whole_l.len();
            whole_l.extend_from_slice(&more_l[..256]);
            whole_r.extend_from_slice(&more_r[..256]);
            let db = db::linear_to_db(true_peak_from(&whole_l, &whole_r, start));
            crate::measured(&format!(
                "MEASURED lookahead {from_ms} ms to {to_ms} ms: {db} dBTP across the 256-sample crossfade"
            ));
            assert!(
                db <= -1.0 + 1.0e-3,
                "lookahead {from_ms} to {to_ms} measured {db} dBTP during the transition"
            );
        }

        let hot = lookahead_samples(rate, 20.0) as usize + rate as usize / 20;
        let quiet = lookahead_samples(rate, 5.0) as usize + 60;
        let frames = hot + quiet;
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        for index in 0..hot {
            let t = index as f32 / rate as f32;
            left[index] = (t * 997.0 * std::f32::consts::TAU).sin() * 2.0;
            right[index] = ((t * 997.0 + 0.17) * std::f32::consts::TAU).sin() * 2.0;
        }
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(5.0))
                .accepted()
        );
        let mut runtime = LimitRuntime::new();
        let mut offset = 0;
        while offset < frames {
            let end = (offset + 256).min(frames);
            runtime.process_master(
                &mut reader,
                &mut left[offset..end],
                &mut right[offset..end],
                rate,
            );
            offset = end;
        }
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(20.0))
                .accepted()
        );
        let mut more_l = vec![0.0f32; 256];
        let mut more_r = vec![0.0f32; 256];
        runtime.process_master(&mut reader, &mut more_l, &mut more_r, rate);
        let mut after_l = vec![0.0f32; 256];
        let mut after_r = vec![0.0f32; 256];
        runtime.process_master(&mut reader, &mut after_l, &mut after_r, rate);
        let energy: f32 = after_l.iter().map(|sample| sample.abs()).sum();
        assert!(
            energy > 10.0,
            "5 ms to 20 ms after a release dropped the burst, energy {energy}"
        );
        let start = left.len() + more_l.len();
        left.extend_from_slice(&more_l);
        right.extend_from_slice(&more_r);
        left.extend_from_slice(&after_l);
        right.extend_from_slice(&after_r);
        let db = db::linear_to_db(true_peak_from(&left, &right, start));
        crate::measured(&format!(
            "MEASURED lookahead 5 ms to 20 ms after release: {db} dBTP on the block after the crossfade"
        ));
        assert!(
            db <= -1.0 + 1.0e-3,
            "5 ms to 20 ms after release measured {db} dBTP once the new lookahead had settled"
        );
    }
    fn marker(stamp: u32) -> LimitBlock {
        let sample = f32::from_bits(stamp);
        LimitBlock {
            pack: [sample; PACK],
            lookahead: stamp,
            lookahead_ms: sample,
            sample_rate: stamp,
        }
    }
    fn write_f32_wav(path: &std::path::Path, rate: u32, left: &[f32], right: &[f32]) {
        let frames = left.len().min(right.len());
        let data_bytes = frames * 8;
        let mut bytes = Vec::with_capacity(44 + data_bytes);
        bytes.extend(b"RIFF");
        bytes.extend(&((36 + data_bytes) as u32).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(&16u32.to_le_bytes());
        bytes.extend(&3u16.to_le_bytes());
        bytes.extend(&2u16.to_le_bytes());
        bytes.extend(&rate.to_le_bytes());
        bytes.extend(&(rate * 8).to_le_bytes());
        bytes.extend(&8u16.to_le_bytes());
        bytes.extend(&32u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(&(data_bytes as u32).to_le_bytes());
        for index in 0..frames {
            bytes.extend(&left[index].to_le_bytes());
            bytes.extend(&right[index].to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn master_defaults_on_at_minus_1_dbtp_and_5_ms() {
        let (writer, reader) = open();
        let master = writer.master_settings();
        assert!(master.enabled());
        assert_eq!(master.ceiling_db(), -1.0);
        assert_eq!(master.lookahead_ms(), 5.0);
        assert_eq!(master.latency_samples(), 240);
        assert_eq!(lookahead_samples(48_000, 5.0), 240);
        assert_eq!(lookahead_samples(44_100, 5.0), 221);
        assert_eq!(latency_ns(240, 48_000), 5_000_000);
        assert_eq!(writer.master_latency_samples(), 240);
        assert_eq!(writer.reported_latency_samples(), 240);
        assert_eq!(reader.master_gain_reduction_db(), 0.0);
        assert_eq!(lookahead_samples(48_000, 0.0), 0);
        assert_eq!(lookahead_samples(48_000, -3.0), 0);
        assert_eq!(zero_ms_limiter_cap(48_000, 256), ZERO_MS_CAP_48K_256);
        assert_eq!(zero_ms_limiter_cap(48_000, 1), 0);
    }

    #[test]
    fn master_zero_ms_is_stored_as_5_ms_when_the_cap_is_zero() {
        let (mut writer, _reader) = open();
        assert!(writer.set_sample_rate(48_000));
        assert!(!writer.set_processing_quantum(256));
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(0.0))
                .accepted()
        );
        assert!(!writer.master_cap_demoted());
        assert_eq!(writer.master_settings().lookahead_ms(), 0.0);
        assert!(writer.drain_cap_demotions().is_empty());
        assert!(writer.set_processing_quantum(1));
        assert_eq!(zero_ms_limiter_cap(48_000, 1), 0);
        assert!(writer.master_cap_demoted());
        assert_eq!(
            writer.master_settings().lookahead_ms(),
            LIMIT_LOOKAHEAD_DEFAULT_MS
        );
        assert_eq!(writer.drain_cap_demotions(), vec![None]);
        assert!(writer.set_processing_quantum(256));
        assert_eq!(
            writer.master_settings().lookahead_ms(),
            LIMIT_LOOKAHEAD_DEFAULT_MS
        );
        assert!(writer.master_cap_demoted());
        assert!(writer.drain_cap_demotions().is_empty());
        assert!(writer.set_processing_quantum(1));
        assert!(writer.apply_master(LimitUpdate::LookaheadMs(0.0)).demoted());
        assert_eq!(
            writer.master_settings().lookahead_ms(),
            LIMIT_LOOKAHEAD_DEFAULT_MS
        );
        assert!(writer.apply_master(LimitUpdate::Enabled(false)).accepted());
        assert!(!writer.master_cap_demoted());
    }

    #[test]
    fn a_short_quantum_promotes_zero_ms_without_restarting_the_crossfade() {
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(48_000));
        let _ = writer.set_processing_quantum(256);
        assert!(writer.apply_master(LimitUpdate::Enabled(true)).accepted());
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(0.0))
                .accepted()
        );
        assert!(!writer.master_cap_demoted());
        let mut runtime = LimitRuntime::new();
        let mut cursor = 0u32;
        let mut next_sample = || {
            let phase = (cursor % 480) as f32;
            cursor += 1;
            (phase / 480.0) * 0.4 - 0.2
        };
        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        for _ in 0..8 {
            for sample in &mut left {
                *sample = next_sample();
            }
            right.copy_from_slice(&left);
            runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        }
        assert!(!runtime.master_crossfade());
        let mut one_l = [0.0f32; 1];
        let mut one_r = [0.0f32; 1];
        one_l[0] = next_sample();
        one_r[0] = one_l[0];
        runtime.process_master(&mut reader, &mut one_l, &mut one_r, 48_000);
        assert!(runtime.master_crossfade());
        let mut left_the_undelayed = false;
        for _ in 0..(LIMIT_RAMP_SAMPLES + 8) {
            assert!(
                writer
                    .apply_master(LimitUpdate::LookaheadMs(0.0))
                    .accepted()
            );
            assert!(!writer.master_cap_demoted());
            let undelayed = next_sample();
            one_l[0] = undelayed;
            one_r[0] = undelayed;
            runtime.process_master(&mut reader, &mut one_l, &mut one_r, 48_000);
            let gap = (one_l[0] - undelayed)
                .abs()
                .max((one_r[0] - undelayed).abs());
            if left_the_undelayed {
                assert!(
                    gap > 0.01,
                    "output stepped back to the undelayed signal, gap {gap}"
                );
            } else if gap > 0.02 {
                left_the_undelayed = true;
            }
        }
        assert!(left_the_undelayed, "output never left the undelayed signal");
        assert!(!runtime.master_crossfade());
        assert_eq!(writer.master_settings().lookahead_ms(), 0.0);
    }

    #[test]
    fn a_backward_detector_clock_sticks_a_fault_and_does_not_panic() {
        let mut runtime = LimitRuntime::new();
        runtime.stick_backward_clock();
        assert!(runtime.detector_order_fault());
    }

    #[test]
    fn the_audio_block_records_callback_quantum() {
        let (writer, mut reader) = open();
        assert_eq!(writer.callback_quantum(), ZERO_MS_REFERENCE_FRAMES);
        let mut runtime = LimitRuntime::new();
        let mut left = [0.1f32; 64];
        let mut right = [0.1f32; 64];
        runtime.process_master(&mut reader, &mut left, &mut right, 48_000);
        assert_eq!(writer.callback_quantum(), 64);
    }

    #[test]
    fn master_limiter_block_cost_at_0_5_and_20_ms_is_measured() {
        let frames = 256usize;
        let reps = 100u32;
        for (rate, ms) in [
            (48_000u32, 0.0f32),
            (48_000, 5.0),
            (48_000, 20.0),
            (96_000, 0.0),
            (96_000, 5.0),
            (96_000, 20.0),
        ] {
            let (mut writer, mut reader) = open();
            assert!(writer.set_sample_rate(rate));
            let _ = writer.set_processing_quantum(frames as u32);
            assert_eq!(writer.processing_quantum(), frames as u32);
            assert!(writer.apply_master(LimitUpdate::Enabled(true)).accepted());
            let applied = writer.apply_master(LimitUpdate::LookaheadMs(ms));
            assert!(applied.accepted());
            if ms == 0.0 {
                assert!(!applied.demoted());
            }
            let mut runtime = LimitRuntime::new();
            let mut left = vec![1.0f32; frames];
            let mut right = left.clone();
            runtime.process_master(&mut reader, &mut left, &mut right, rate);
            let start = std::time::Instant::now();
            for _ in 0..reps {
                runtime.process_master(&mut reader, &mut left, &mut right, rate);
            }
            let per = start.elapsed() / reps;
            crate::measured(&format!(
                "MEASURED master limiter block: {per:?} for {frames} stereo frames at {rate} Hz, lookahead {ms} ms"
            ));
            std::hint::black_box((&left, &right, per));
        }
    }

    #[test]
    fn zero_ms_hot_block_matches_the_saved_clipper() {
        let rate = 96_000u32;
        let frames = 1024usize;
        let mut noise = 0x1234_5678u32;
        let mut hot_l = vec![0.0f32; frames];
        let mut hot_r = vec![0.0f32; frames];
        for (left, right) in hot_l.iter_mut().zip(hot_r.iter_mut()) {
            noise = noise.wrapping_mul(1664525).wrapping_add(1013904223);
            *left = ((noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 2.0;
            noise = noise.wrapping_mul(1664525).wrapping_add(1013904223);
            *right = ((noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 2.0;
        }
        let (mut writer, mut reader) = open();
        assert!(writer.set_sample_rate(rate));
        let _ = writer.set_processing_quantum(frames as u32);
        assert!(writer.apply_master(LimitUpdate::Enabled(true)).accepted());
        assert!(
            writer
                .apply_master(LimitUpdate::LookaheadMs(0.0))
                .accepted()
        );
        let mut runtime = LimitRuntime::new();
        let mut left = hot_l.clone();
        let mut right = hot_r.clone();
        for _ in 0..54 {
            left.copy_from_slice(&hot_l);
            right.copy_from_slice(&hot_r);
            runtime.process_master(&mut reader, &mut left, &mut right, rate);
        }
        let mut hash = 0u64;
        for sample in left.iter().chain(right.iter()) {
            hash = hash
                .wrapping_mul(0x100000001b3)
                .wrapping_add(sample.to_bits() as u64);
        }
        assert_eq!(hash, 0x582f0a2efe00324b);

        let ceiling = applied_ceiling(runtime.master.current[CEILING]);
        let tables = InterpTables::new();
        let mut pull_l = InterpHist::new();
        let mut pull_r = InterpHist::new();
        let mut pair_l = InterpHist::new();
        let mut pair_r = InterpHist::new();
        for (&sample_l, &sample_r) in hot_l.iter().zip(hot_r.iter()) {
            let one = pull_under_ceiling(&tables, &mut pull_l, sample_l, ceiling);
            let other = pull_under_ceiling(&tables, &mut pull_r, sample_r, ceiling);
            let both = clip_pair(
                &tables,
                &mut pair_l,
                &mut pair_r,
                sample_l,
                sample_r,
                ceiling,
            );
            assert_eq!(both, (one, other));
        }
    }

    #[test]
    fn lookahead_hot_blocks_match_the_saved_limiter() {
        for (ms, expect) in [(5.0f32, 0x436a16b283fc3ce7u64), (20.0, 0xdceedc6cdfba63d3)] {
            let rate = 96_000u32;
            let frames = 1024usize;
            let mut noise = 0x1234_5678u32;
            let mut hot_l = vec![0.0f32; frames];
            let mut hot_r = vec![0.0f32; frames];
            for (left, right) in hot_l.iter_mut().zip(hot_r.iter_mut()) {
                noise = noise.wrapping_mul(1664525).wrapping_add(1013904223);
                *left = ((noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 2.0;
                noise = noise.wrapping_mul(1664525).wrapping_add(1013904223);
                *right = ((noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 2.0;
            }
            let (mut writer, mut reader) = open();
            assert!(writer.set_sample_rate(rate));
            let _ = writer.set_processing_quantum(frames as u32);
            assert!(writer.apply_master(LimitUpdate::Enabled(true)).accepted());
            assert!(writer.apply_master(LimitUpdate::LookaheadMs(ms)).accepted());
            let mut runtime = LimitRuntime::new();
            let mut left = hot_l.clone();
            let mut right = hot_r.clone();
            for _ in 0..54 {
                left.copy_from_slice(&hot_l);
                right.copy_from_slice(&hot_r);
                runtime.process_master(&mut reader, &mut left, &mut right, rate);
            }
            let mut hash = 0u64;
            for sample in left.iter().chain(right.iter()) {
                hash = hash
                    .wrapping_mul(0x100000001b3)
                    .wrapping_add(sample.to_bits() as u64);
            }
            assert_eq!(hash, expect);
        }
    }
}

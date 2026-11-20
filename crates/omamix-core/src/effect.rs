//! Master effect chain.
//!
//! Blocks are plain slots with a kind tag. The audio callback matches that
//! tag and calls the compressor, the limiter, or a gain multiply. It does
//! not allocate, lock, or make a system call. A trait object would be a heap
//! pointer, and the publish path copies a fixed snapshot the way the EQ, the
//! compressor, and the limiter already do. A later block is a new kind and a
//! new arm.
//!
//! The chain keeps one limiter, and it keeps that limiter last. Reported
//! latency is the sum of the blocks. The compressor and the gain contribute
//! 0. The limiter contributes the delay the callback applies, unless that
//! block is bypassed, in which case it contributes 0 and the callback skips it.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

use crate::comp::{CompReader, CompRuntime, CompSettings};
use crate::db::{self, BelowMin};
use crate::limit::{self, LimitReader, LimitRuntime, LimitSettings, LimitWriter};

/// Stable id for the master compressor block.
pub const KIND_COMPRESSOR: &str = "compressor";
/// Stable id for the master limiter block.
pub const KIND_LIMITER: &str = "limiter";
/// Stable id for a dB gain block. The default chain does not include one.
pub const KIND_GAIN: &str = "gain";

/// Slots in one master chain, including the limiter.
pub const MASTER_CHAIN_CAP: usize = 8;

const SNAP_EMPTY: u8 = 0;
const INITIAL_WRITE: u32 = 0;
const INITIAL_READ: u32 = 1;
const INITIAL_SPARE: u32 = 2;
const DIRTY: u32 = 0b100;

fn default_rate() -> u32 {
    48_000
}

fn default_true() -> bool {
    true
}

fn default_threshold() -> f32 {
    crate::comp::COMP_THRESHOLD_DEFAULT_DB
}

fn default_ratio() -> f32 {
    crate::comp::COMP_RATIO_DEFAULT
}

fn default_attack() -> f32 {
    crate::comp::COMP_ATTACK_DEFAULT_MS
}

fn default_release() -> f32 {
    crate::comp::COMP_RELEASE_DEFAULT_MS
}

fn default_knee() -> f32 {
    crate::comp::COMP_KNEE_DEFAULT_DB
}

fn default_makeup() -> f32 {
    crate::comp::COMP_MAKEUP_DEFAULT_DB
}

fn default_ceiling() -> f32 {
    limit::LIMIT_CEILING_DEFAULT_DB
}

fn default_lookahead() -> f32 {
    limit::LIMIT_LOOKAHEAD_DEFAULT_MS
}

fn sanitize_gain(gain_db: f32) -> f32 {
    db::sanitize_db(
        gain_db,
        -db::MAX_BOOST_DB,
        db::MAX_BOOST_DB,
        BelowMin::Clamp,
    )
}

/// Kind of one chain block. The tag is what the callback matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKind {
    /// Feed-forward compressor. Latency 0.
    Compressor,
    /// Lookahead limiter. The chain keeps this last.
    Limiter,
    /// Constant dB gain. Latency 0.
    Gain,
}

impl EffectKind {
    /// Stable id stored in a scene file.
    pub fn id(self) -> &'static str {
        match self {
            Self::Compressor => KIND_COMPRESSOR,
            Self::Limiter => KIND_LIMITER,
            Self::Gain => KIND_GAIN,
        }
    }

    /// Parse a scene kind id. Unknown strings are `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            KIND_COMPRESSOR => Some(Self::Compressor),
            KIND_LIMITER => Some(Self::Limiter),
            KIND_GAIN => Some(Self::Gain),
            _ => None,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Compressor => 1,
            Self::Limiter => 2,
            Self::Gain => 3,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Compressor),
            2 => Some(Self::Limiter),
            3 => Some(Self::Gain),
            _ => None,
        }
    }
}

/// Serde form of one block's parameters.
///
/// The tag is the kind id. Scene files store these fields under `params`
/// and keep the kind on the block, so an unknown kind can be skipped before
/// this enum is built.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectParams {
    /// Compressor controls. `enabled` false is the effect's own bypass.
    Compressor {
        #[serde(default = "default_rate")]
        sample_rate: u32,
        #[serde(default)]
        enabled: bool,
        #[serde(default = "default_threshold")]
        threshold_db: f32,
        #[serde(default = "default_ratio")]
        ratio: f32,
        #[serde(default = "default_attack")]
        attack_ms: f32,
        #[serde(default = "default_release")]
        release_ms: f32,
        #[serde(default = "default_knee")]
        knee_db: f32,
        #[serde(default = "default_makeup")]
        makeup_db: f32,
    },
    /// Limiter controls. `enabled` defaults on, matching the master limiter.
    Limiter {
        #[serde(default = "default_rate")]
        sample_rate: u32,
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_ceiling")]
        ceiling_db: f32,
        #[serde(default = "default_lookahead")]
        lookahead_ms: f32,
    },
    /// Gain in dB, clamped to +/-24.
    Gain {
        #[serde(default)]
        gain_db: f32,
    },
}

impl EffectParams {
    /// Kind this parameter set belongs to.
    pub fn kind(self) -> EffectKind {
        match self {
            Self::Compressor { .. } => EffectKind::Compressor,
            Self::Limiter { .. } => EffectKind::Limiter,
            Self::Gain { .. } => EffectKind::Gain,
        }
    }

    fn to_comp(self) -> CompSettings {
        match self {
            Self::Compressor {
                sample_rate,
                enabled,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                knee_db,
                makeup_db,
            } => {
                let mut settings = CompSettings::bypass(sample_rate);
                settings.set_enabled(enabled);
                settings.set_threshold_db(threshold_db);
                settings.set_ratio(ratio);
                settings.set_attack_ms(attack_ms);
                settings.set_release_ms(release_ms);
                settings.set_knee_db(knee_db);
                settings.set_makeup_db(makeup_db);
                settings
            }
            _ => CompSettings::bypass(48_000),
        }
    }

    fn to_limit(self) -> LimitSettings {
        match self {
            Self::Limiter {
                sample_rate,
                enabled,
                ceiling_db,
                lookahead_ms,
            } => {
                let mut settings = LimitSettings::bypass(sample_rate);
                settings.set_enabled(enabled);
                settings.set_ceiling_db(ceiling_db);
                settings.set_lookahead_ms(lookahead_ms);
                settings
            }
            _ => LimitSettings::master(48_000),
        }
    }

    fn gain_db(self) -> f32 {
        match self {
            Self::Gain { gain_db } => sanitize_gain(gain_db),
            _ => 0.0,
        }
    }
}

/// One block in the master chain.
///
/// `process` is the audio-callback entry. Bypass returns without touching
/// the buffers. The compressor and the limiter then call the same runtimes
/// the mixer already uses, so a default chain is that call sequence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectBlock {
    kind: EffectKind,
    bypass: bool,
    params: EffectParams,
}

impl EffectBlock {
    /// Compressor block. The chain bypass starts off.
    ///
    /// The effect's own `enabled` flag is inside `settings`. Off still runs
    /// the follower when the chain itself is not bypassed.
    pub fn compressor(settings: CompSettings) -> Self {
        Self {
            kind: EffectKind::Compressor,
            bypass: false,
            params: EffectParams::Compressor {
                sample_rate: settings.sample_rate(),
                enabled: settings.enabled(),
                threshold_db: settings.threshold_db(),
                ratio: settings.ratio(),
                attack_ms: settings.attack_ms(),
                release_ms: settings.release_ms(),
                knee_db: settings.knee_db(),
                makeup_db: settings.makeup_db(),
            },
        }
    }

    /// Limiter block. The chain bypass starts off. The chain places it last.
    pub fn limiter(settings: LimitSettings) -> Self {
        Self {
            kind: EffectKind::Limiter,
            bypass: false,
            params: EffectParams::Limiter {
                sample_rate: settings.sample_rate(),
                enabled: settings.enabled(),
                ceiling_db: settings.ceiling_db(),
                lookahead_ms: settings.lookahead_ms(),
            },
        }
    }

    /// Gain block. `gain_db` is clamped to +/-24. NaN becomes mute.
    pub fn gain(gain_db: f32) -> Self {
        Self {
            kind: EffectKind::Gain,
            bypass: false,
            params: EffectParams::Gain {
                gain_db: sanitize_gain(gain_db),
            },
        }
    }

    fn idle() -> Self {
        let mut block = Self::gain(0.0);
        block.bypass = true;
        block
    }

    /// Stable kind id.
    pub fn kind_id(&self) -> &'static str {
        self.kind.id()
    }

    /// Kind tag.
    pub fn kind(&self) -> EffectKind {
        self.kind
    }

    /// Chain bypass. This is separate from the effect's own enabled flag.
    pub fn bypassed(&self) -> bool {
        self.bypass
    }

    /// Set the chain bypass. A bypassed block is unity and reports 0 latency.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }

    /// Parameters currently stored on this block.
    pub fn params(&self) -> EffectParams {
        self.params
    }

    /// Replace parameters. Returns false when `params` is a different kind.
    pub fn replace_params(&mut self, params: EffectParams) -> bool {
        if params.kind() != self.kind {
            return false;
        }
        let bypass = self.bypass;
        *self = match self.kind {
            EffectKind::Compressor => Self::compressor(params.to_comp()),
            EffectKind::Limiter => Self::limiter(params.to_limit()),
            EffectKind::Gain => Self::gain(params.gain_db()),
        };
        self.bypass = bypass;
        true
    }

    /// Factory parameters for this kind, and the chain bypass cleared.
    ///
    /// The compressor returns to its bypassed settings. The limiter returns
    /// to on, at -1 dBTP, with 5 ms of lookahead. Gain returns to 0 dB.
    pub fn reset(&mut self) {
        *self = match self.kind {
            EffectKind::Compressor => Self::compressor(CompSettings::bypass(48_000)),
            EffectKind::Limiter => Self::limiter(LimitSettings::master(48_000)),
            EffectKind::Gain => Self::gain(0.0),
        };
    }

    /// Samples of delay at `sample_rate` and `quantum_frames`.
    ///
    /// The compressor and the gain are 0. A bypassed block is 0. The limiter
    /// uses the same count the callback applies, including the 0 ms cap.
    pub fn latency_samples(&self, sample_rate: u32, quantum_frames: u32) -> u32 {
        if self.bypass {
            return 0;
        }
        match self.params {
            EffectParams::Compressor { .. } | EffectParams::Gain { .. } => 0,
            EffectParams::Limiter {
                enabled,
                lookahead_ms,
                ..
            } => limit::signal_delay_samples(enabled, lookahead_ms, sample_rate, quantum_frames),
        }
    }

    /// In-place stereo process for this block.
    ///
    /// A bypassed block returns immediately. Otherwise the kind is matched.
    /// The compressor and the limiter use the runtimes in `cx`.
    pub fn process(&self, left: &mut [f32], right: &mut [f32], cx: &mut BlockCx<'_>) {
        process_slot(
            self.kind,
            self.bypass,
            self.params.gain_db(),
            left,
            right,
            cx,
        );
    }

    fn gain_db(&self) -> f32 {
        self.params.gain_db()
    }

    pub(crate) fn comp_settings(&self) -> Option<CompSettings> {
        if self.kind == EffectKind::Compressor {
            Some(self.params.to_comp())
        } else {
            None
        }
    }

    pub(crate) fn limit_settings(&self) -> Option<LimitSettings> {
        if self.kind == EffectKind::Limiter {
            Some(self.params.to_limit())
        } else {
            None
        }
    }
}

/// Runtimes `process` needs. The callback already owns them.
///
/// The gain block does not touch these fields.
pub struct BlockCx<'a> {
    /// Compressor envelope and ramp.
    pub comp: &'a mut CompRuntime,
    /// Published compressor block.
    pub comp_reader: &'a CompReader,
    /// Limiter delay line.
    pub limit: &'a mut LimitRuntime,
    /// Published limiter block.
    pub limit_reader: &'a LimitReader,
}

fn process_slot(
    kind: EffectKind,
    bypass: bool,
    gain_db: f32,
    left: &mut [f32],
    right: &mut [f32],
    cx: &mut BlockCx<'_>,
) {
    if bypass {
        return;
    }
    match kind {
        EffectKind::Compressor => cx.comp.apply_master(cx.comp_reader, left, right),
        EffectKind::Limiter => cx.limit.apply_master(cx.limit_reader, left, right),
        EffectKind::Gain => apply_gain(left, right, gain_db),
    }
}

fn apply_gain(left: &mut [f32], right: &mut [f32], gain_db: f32) {
    let gain = db::db_to_linear(gain_db);
    if gain == 1.0 {
        return;
    }
    let frames = left.len().min(right.len());
    for index in 0..frames {
        left[index] *= gain;
        right[index] *= gain;
    }
}

/// Ordered master chain. A limiter in the list is kept last.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasterChain {
    slots: [EffectBlock; MASTER_CHAIN_CAP],
    len: u8,
}

impl MasterChain {
    /// Compressor, then limiter. Both chain bypasses are off.
    pub fn from_settings(comp: CompSettings, limit: LimitSettings) -> Self {
        let (chain, warnings) =
            Self::from_blocks(&[EffectBlock::compressor(comp), EffectBlock::limiter(limit)]);
        debug_assert!(warnings.is_empty());
        chain
    }

    /// Today's master: bypassed compressor settings, limiter on at 5 ms.
    pub fn standard() -> Self {
        Self::from_settings(CompSettings::bypass(48_000), LimitSettings::master(48_000))
    }

    /// Build a chain, enforcing one limiter at the end and one compressor.
    ///
    /// Warnings are short reasons. The caller prints them. Blocks past
    /// [`MASTER_CHAIN_CAP`] are dropped, and the limiter stays in the last
    /// kept slot when one was present.
    pub fn from_blocks(blocks: &[EffectBlock]) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut kept = Vec::new();
        let mut limiter = None;
        let mut compressors = 0usize;
        let mut limiters = 0usize;
        for block in blocks {
            match block.kind {
                EffectKind::Limiter => {
                    limiters += 1;
                    if limiters > 1 {
                        warnings.push("skipped a second limiter".to_string());
                    }
                    limiter = Some(*block);
                }
                EffectKind::Compressor => {
                    compressors += 1;
                    if compressors > 1 {
                        warnings.push("skipped a second compressor".to_string());
                        continue;
                    }
                    kept.push(*block);
                }
                EffectKind::Gain => kept.push(*block),
            }
        }
        let already_last = limiters == 1
            && blocks
                .last()
                .is_some_and(|block| block.kind == EffectKind::Limiter);
        if limiters > 0 && !already_last {
            warnings.push("kept the limiter last".to_string());
        }
        if let Some(block) = limiter {
            kept.push(block);
        }
        if kept.len() > MASTER_CHAIN_CAP {
            warnings.push(format!("kept the first {MASTER_CHAIN_CAP} blocks"));
            if limiter.is_some() {
                let last = kept.pop().unwrap_or_else(EffectBlock::idle);
                kept.truncate(MASTER_CHAIN_CAP - 1);
                kept.push(last);
            } else {
                kept.truncate(MASTER_CHAIN_CAP);
            }
        }
        (Self::pack(&kept), warnings)
    }

    fn pack(kept: &[EffectBlock]) -> Self {
        let mut slots = [EffectBlock::idle(); MASTER_CHAIN_CAP];
        let count = kept.len().min(MASTER_CHAIN_CAP);
        slots[..count].copy_from_slice(&kept[..count]);
        Self {
            slots,
            len: count as u8,
        }
    }

    /// Blocks in order.
    pub fn blocks(&self) -> &[EffectBlock] {
        &self.slots[..self.len as usize]
    }

    /// Kind ids in order.
    pub fn kind_ids(&self) -> Vec<&'static str> {
        self.blocks().iter().map(EffectBlock::kind_id).collect()
    }

    /// How many blocks are stored.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// True when the chain has no blocks.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Chain bypass of one block. Returns false when `index` is past the end.
    pub fn set_bypass(&mut self, index: usize, bypass: bool) -> bool {
        if index >= self.len as usize {
            return false;
        }
        self.slots[index].set_bypass(bypass);
        true
    }

    /// Sum of each block's latency at `sample_rate` and `quantum_frames`.
    pub fn latency_samples(&self, sample_rate: u32, quantum_frames: u32) -> u32 {
        self.blocks().iter().fold(0u32, |sum, block| {
            sum.saturating_add(block.latency_samples(sample_rate, quantum_frames))
        })
    }

    /// Copy compressor and limiter parameters onto the matching blocks.
    ///
    /// Order, bypass, and gain blocks stay put. Used when the legacy master
    /// fields are the ones a caller just edited.
    pub fn mirror_settings(&mut self, comp: CompSettings, limit: LimitSettings) {
        for slot in &mut self.slots[..self.len as usize] {
            let bypass = slot.bypass;
            match slot.kind {
                EffectKind::Compressor => *slot = EffectBlock::compressor(comp),
                EffectKind::Limiter => *slot = EffectBlock::limiter(limit),
                EffectKind::Gain => {}
            }
            if slot.kind != EffectKind::Gain {
                slot.bypass = bypass;
            }
        }
    }

    /// First compressor's settings.
    pub fn compressor_settings(&self) -> Option<CompSettings> {
        self.blocks().iter().find_map(EffectBlock::comp_settings)
    }

    /// The limiter's settings, when the chain has one.
    pub fn limiter_settings(&self) -> Option<LimitSettings> {
        self.blocks().iter().find_map(EffectBlock::limit_settings)
    }

    /// Reset every block to its factory parameters.
    pub fn reset(&mut self) {
        for slot in &mut self.slots[..self.len as usize] {
            slot.reset();
        }
    }

    pub(crate) fn snapshot(&self) -> ChainSnap {
        let mut snap = ChainSnap {
            len: self.len,
            slots: [ChainSlot {
                kind: SNAP_EMPTY,
                bypass: 1,
                gain_db: 0.0,
            }; MASTER_CHAIN_CAP],
        };
        for (index, block) in self.blocks().iter().enumerate() {
            snap.slots[index] = ChainSlot {
                kind: block.kind.tag(),
                bypass: u8::from(block.bypass),
                gain_db: block.gain_db(),
            };
        }
        snap
    }
}

impl Default for MasterChain {
    fn default() -> Self {
        Self::standard()
    }
}

/// Copied into the audio callback. No heap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChainSnap {
    len: u8,
    slots: [ChainSlot; MASTER_CHAIN_CAP],
}

#[derive(Clone, Copy, Debug)]
struct ChainSlot {
    kind: u8,
    bypass: u8,
    gain_db: f32,
}

impl ChainSnap {
    pub(crate) fn standard() -> Self {
        MasterChain::standard().snapshot()
    }

    fn iter(&self) -> impl Iterator<Item = ChainSlot> {
        let count = self.len as usize;
        self.slots.into_iter().take(count.min(MASTER_CHAIN_CAP))
    }
}

/// Walk `snap` in order. The first compressor and the first limiter run.
///
/// Later copies of those two kinds are ignored. Gain blocks all run.
pub(crate) fn apply_snap(
    snap: ChainSnap,
    left: &mut [f32],
    right: &mut [f32],
    cx: &mut BlockCx<'_>,
) {
    let mut saw_comp = false;
    let mut saw_limit = false;
    for slot in snap.iter() {
        let Some(kind) = EffectKind::from_tag(slot.kind) else {
            continue;
        };
        if kind == EffectKind::Compressor {
            if saw_comp {
                continue;
            }
            saw_comp = true;
        }
        if kind == EffectKind::Limiter {
            if saw_limit {
                continue;
            }
            saw_limit = true;
        }
        process_slot(kind, slot.bypass != 0, slot.gain_db, left, right, cx);
    }
}

/// The only chain producer. Not `Clone`. Not `Sync`.
///
/// Publishing takes `&mut self`. The callback holds [`ChainReader`].
///
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::effect::ChainWriter>();
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::effect::ChainWriter>();
/// }
/// ```
#[derive(Debug)]
pub struct ChainWriter {
    inner: Arc<ChainShared>,
    snap: ChainSnap,
    _unshared: PhantomData<Cell<()>>,
}

/// Chain snapshot for the audio callback. Not `Clone`. Not `Sync`.
///
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// fn demo() {
///     assert_sync::<omamix_core::effect::ChainReader>();
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn assert_clone<T: Clone>() {}
/// fn demo() {
///     assert_clone::<omamix_core::effect::ChainReader>();
/// }
/// ```
#[derive(Debug)]
pub struct ChainReader {
    inner: Arc<ChainShared>,
    read_idx: u32,
    _unshared: PhantomData<Cell<()>>,
}

#[derive(Debug)]
struct ChainShared {
    slots: [UnsafeCell<ChainSnap>; 3],
    state: AtomicU32,
    write_idx: AtomicU32,
}

/// One producer and one consumer. The first snapshot is the standard chain.
pub fn open() -> (ChainWriter, ChainReader) {
    let snap = ChainSnap::standard();
    let inner = Arc::new(ChainShared {
        slots: [
            UnsafeCell::new(snap),
            UnsafeCell::new(snap),
            UnsafeCell::new(snap),
        ],
        state: AtomicU32::new(INITIAL_SPARE),
        write_idx: AtomicU32::new(INITIAL_WRITE),
    });
    (
        ChainWriter {
            inner: Arc::clone(&inner),
            snap,
            _unshared: PhantomData,
        },
        ChainReader {
            inner,
            read_idx: INITIAL_READ,
            _unshared: PhantomData,
        },
    )
}

impl ChainWriter {
    /// Publish `chain` for the callback. The loop thread calls this.
    pub fn publish(&mut self, chain: &MasterChain) {
        let snap = chain.snapshot();
        self.snap = snap;
        self.inner.publish(snap);
    }

    /// Sum of the published blocks.
    ///
    /// A limiter that is not chain-bypassed contributes
    /// [`LimitWriter::reported_latency_samples`]. Every other block contributes 0.
    /// On the default chain this is the limiter delay the callback applies.
    pub fn reported_with_limiter(&self, limit: &LimitWriter) -> u32 {
        let mut sum = 0u32;
        for slot in self.snap.iter() {
            let part = if slot.bypass != 0 {
                0
            } else if slot.kind == EffectKind::Limiter.tag() {
                limit.reported_latency_samples()
            } else {
                0
            };
            sum = sum.saturating_add(part);
        }
        sum
    }
}

impl ChainReader {
    pub(crate) fn poll(&mut self) -> Option<ChainSnap> {
        self.inner.poll(&mut self.read_idx)
    }
}

impl ChainShared {
    fn publish(&self, snap: ChainSnap) {
        let write_idx = self.write_idx.load(Ordering::Relaxed);
        if write_idx > 2 {
            return;
        }
        // SAFETY: `write_idx` is at most 2, and this producer owns that slot until the swap.
        unsafe {
            *self.slots[write_idx as usize].get() = snap;
        }
        let prev = self.state.swap(write_idx | DIRTY, Ordering::AcqRel);
        self.write_idx.store(prev & 0b11, Ordering::Relaxed);
    }

    fn poll(&self, read_idx: &mut u32) -> Option<ChainSnap> {
        let state = self.state.load(Ordering::Acquire);
        if let Some(snap) = claim(self, state, read_idx) {
            return Some(snap);
        }
        let state = self.state.load(Ordering::Acquire);
        claim(self, state, read_idx)
    }
}

fn claim(shared: &ChainShared, state: u32, read_idx: &mut u32) -> Option<ChainSnap> {
    if state & DIRTY == 0 {
        return None;
    }
    let spare = state & 0b11;
    if spare > 2 || spare == *read_idx {
        return None;
    }
    if shared
        .state
        .compare_exchange(state, *read_idx, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    *read_idx = spare;
    // SAFETY: `spare` is at most 2, differs from the consumer index, and the CAS moved that slot to this consumer.
    Some(unsafe { *shared.slots[spare as usize].get() })
}

// SAFETY: [`ChainWriter`] is the only producer. It is not `Clone` and not `Sync`, and publish takes `&mut self`, so safe code cannot publish from two threads.
// SAFETY: [`ChainReader`] is the only consumer. It is not `Clone` and not `Sync`, and it owns `read_idx`. `poll` takes `&mut ChainReader`.
// SAFETY: The producer writes a slot only while it owns `write_idx`. The consumer reads a slot only after `poll` has swapped that index out of the spare field.
// SAFETY: Those two indices are distinct, so the cells are not aliased. A second producer or a second consumer would be a data race, and the handles do not allow one from safe code.
unsafe impl Sync for ChainShared {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comp::{CompUpdate, open as open_comp};
    use crate::eq::{EqUpdate, open as open_eq};
    use crate::limit::{lookahead_samples, open as open_limit};
    use crate::params::{
        STRIP_SLOT_COUNT, StripBlockInput, StripMeterBank, StripMixer, StripParamBank,
    };
    use crate::scope::ScopeBank;

    fn run_mix(
        mixer: &mut StripMixer,
        chain: &mut ChainReader,
        eq: &mut crate::eq::EqReader,
        comp: &mut CompReader,
        limit: &mut LimitReader,
        input: &[f32],
        rate: u32,
    ) -> (Vec<f32>, Vec<f32>) {
        mixer.load_chain(chain);
        let frames = input.len();
        let inputs = [Some(StripBlockInput {
            left: input,
            right: input,
        })];
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        let meters = StripMeterBank::new();
        let mut scopes = ScopeBank::with_capacity(4);
        mixer.process(
            &StripParamBank::new(),
            eq,
            comp,
            limit,
            &meters,
            &mut scopes,
            &inputs,
            &mut left,
            &mut right,
            rate,
        );
        (left, right)
    }

    #[test]
    fn standard_chain_is_compressor_then_limiter() {
        let chain = MasterChain::standard();
        assert_eq!(chain.kind_ids(), vec![KIND_COMPRESSOR, KIND_LIMITER]);
        assert!(chain.blocks().iter().all(|block| !block.bypassed()));
        assert_eq!(
            chain.latency_samples(48_000, 256),
            lookahead_samples(48_000, 5.0)
        );
        let (chain, warnings) = MasterChain::from_blocks(&[
            EffectBlock::limiter(LimitSettings::master(48_000)),
            EffectBlock::gain(6.0),
            EffectBlock::compressor(CompSettings::bypass(48_000)),
        ]);
        assert_eq!(
            chain.kind_ids(),
            vec![KIND_GAIN, KIND_COMPRESSOR, KIND_LIMITER]
        );
        assert!(warnings.iter().any(|line| line.contains("limiter last")));
    }

    #[test]
    fn latency_is_the_sum_and_matches_the_applied_delay() {
        let chain =
            MasterChain::from_settings(CompSettings::bypass(48_000), LimitSettings::master(48_000));
        let rate = 48_000u32;
        let quantum = 64u32;
        let sum: u32 = chain
            .blocks()
            .iter()
            .map(|block| block.latency_samples(rate, quantum))
            .sum();
        assert_eq!(chain.latency_samples(rate, quantum), sum);
        assert_eq!(sum, lookahead_samples(rate, 5.0));
        assert_eq!(chain.blocks()[0].latency_samples(rate, quantum), 0);

        let (mut eq_w, mut eq_r) = open_eq();
        let (mut comp_w, mut comp_r) = open_comp();
        let (mut limit_w, mut limit_r) = open_limit();
        assert!(eq_w.set_sample_rate(rate));
        assert!(comp_w.set_sample_rate(rate));
        assert!(limit_w.set_sample_rate(rate));
        let (mut chain_w, mut chain_r) = open();
        chain_w.publish(&chain);
        let mut mixer = StripMixer::new(rate);
        let frames = lookahead_samples(rate, 5.0) as usize + quantum as usize;
        let mut input = vec![0.0f32; frames];
        input[0] = 0.05;
        let mut cursor = 0usize;
        let mut output = Vec::with_capacity(frames);
        while cursor < frames {
            let end = (cursor + quantum as usize).min(frames);
            let (left, _) = run_mix(
                &mut mixer,
                &mut chain_r,
                &mut eq_r,
                &mut comp_r,
                &mut limit_r,
                &input[cursor..end],
                rate,
            );
            output.extend_from_slice(&left);
            cursor = end;
        }
        let applied = output
            .iter()
            .position(|sample| sample.abs() > 0.02)
            .expect("impulse left the limiter") as u32;
        assert_eq!(applied, chain.latency_samples(rate, quantum));
        assert_eq!(limit_w.reported_latency_samples(), applied);
        assert_eq!(chain_w.reported_with_limiter(&limit_w), applied);
    }

    #[test]
    fn bypass_of_every_block_is_unity_and_the_limiter_is_not() {
        let rate = 48_000u32;
        let frames = 64usize;
        let input = vec![0.25f32; frames];
        let (mut eq_w, mut eq_r) = open_eq();
        let (_comp_w, mut comp_r) = open_comp();
        let (_limit_w, mut limit_r) = open_limit();
        assert!(eq_w.set_sample_rate(rate));
        let mut chain = MasterChain::standard();
        assert!(chain.set_bypass(0, true));
        assert!(chain.set_bypass(1, true));
        let (mut chain_w, mut chain_r) = open();
        chain_w.publish(&chain);
        let mut mixer = StripMixer::new(rate);
        let (left, right) = run_mix(
            &mut mixer,
            &mut chain_r,
            &mut eq_r,
            &mut comp_r,
            &mut limit_r,
            &input,
            rate,
        );
        assert_eq!(left, input);
        assert_eq!(right, input);
        assert_eq!(chain.latency_samples(rate, frames as u32), 0);

        assert!(chain.set_bypass(1, false));
        chain_w.publish(&chain);
        let (delayed, _) = run_mix(
            &mut mixer,
            &mut chain_r,
            &mut eq_r,
            &mut comp_r,
            &mut limit_r,
            &input,
            rate,
        );
        assert_ne!(delayed, input);
        assert!(delayed.iter().all(|sample| sample.abs() < 1.0e-6));
    }

    #[test]
    fn reordering_gain_and_compressor_changes_the_output() {
        let rate = 48_000u32;
        let frames = 256usize;
        let input = vec![0.5f32; frames];
        let mut comp = CompSettings::bypass(rate);
        comp.set_enabled(true);
        comp.set_threshold_db(-40.0);
        comp.set_ratio(20.0);
        comp.set_attack_ms(0.1);
        let mut limit = LimitSettings::master(rate);
        limit.set_enabled(false);
        let gain = EffectBlock::gain(12.0);
        let compressor = EffectBlock::compressor(comp);
        let limiter = EffectBlock::limiter(limit);
        let (first, warnings) = MasterChain::from_blocks(&[gain, compressor, limiter]);
        assert!(warnings.is_empty());
        let (second, warnings) = MasterChain::from_blocks(&[compressor, gain, limiter]);
        assert!(warnings.is_empty());
        assert_eq!(
            first.kind_ids(),
            vec![KIND_GAIN, KIND_COMPRESSOR, KIND_LIMITER]
        );
        assert_eq!(
            second.kind_ids(),
            vec![KIND_COMPRESSOR, KIND_GAIN, KIND_LIMITER]
        );

        let render = |chain: &MasterChain| {
            let (mut eq_w, mut eq_r) = open_eq();
            let (mut comp_w, mut comp_r) = open_comp();
            let (mut limit_w, mut limit_r) = open_limit();
            assert!(eq_w.set_sample_rate(rate));
            assert!(comp_w.set_sample_rate(rate));
            assert!(limit_w.set_sample_rate(rate));
            comp_w.apply_master_settings(comp);
            limit_w.apply_master_settings(limit);
            let (mut chain_w, mut chain_r) = open();
            chain_w.publish(chain);
            let mut mixer = StripMixer::new(rate);
            for _ in 0..8 {
                let _ = run_mix(
                    &mut mixer,
                    &mut chain_r,
                    &mut eq_r,
                    &mut comp_r,
                    &mut limit_r,
                    &input,
                    rate,
                );
            }
            let meters = StripMeterBank::new();
            let mut scopes = ScopeBank::with_capacity(4);
            let mut left = vec![0.0f32; frames];
            let mut right = vec![0.0f32; frames];
            let inputs = [Some(StripBlockInput {
                left: input.as_slice(),
                right: input.as_slice(),
            })];
            let bank = StripParamBank::new();
            chain_w.publish(chain);
            crate::test_alloc::arm();
            mixer.load_chain(&mut chain_r);
            mixer.process(
                &bank,
                &mut eq_r,
                &mut comp_r,
                &mut limit_r,
                &meters,
                &mut scopes,
                &inputs,
                &mut left,
                &mut right,
                rate,
            );
            let allocs = crate::test_alloc::disarm();
            assert_eq!(allocs, 0, "chain process allocated");
            left
        };
        let ahead = render(&first);
        let behind = render(&second);
        assert_ne!(ahead, behind);
    }

    #[test]
    fn default_chain_matches_the_reference_mix() {
        let rate = 48_000u32;
        let frames = 64usize;
        let warmup = 4096usize.div_ceil(frames).max(8);
        let blocks = warmup + 32;
        let len = blocks * frames;
        let waves: Vec<Vec<f32>> = (0..STRIP_SLOT_COUNT)
            .map(|slot| crate::scene_check::strip_wave(slot, len, rate))
            .collect();
        let (mut eq_w, mut eq_r) = open_eq();
        let (mut comp_w, mut comp_r) = open_comp();
        let mut limit_r = LimitReader::bypassed();
        assert!(eq_w.set_sample_rate(rate));
        assert!(comp_w.set_sample_rate(rate));
        for slot in 0..STRIP_SLOT_COUNT as u16 {
            assert!(eq_w.apply(slot, EqUpdate::HighPassEnabled(true)));
            assert!(eq_w.apply(
                slot,
                EqUpdate::Band {
                    index: 4,
                    gain_db: 6.0,
                },
            ));
            assert!(comp_w.apply(slot, CompUpdate::Enabled(true)));
        }
        assert!(eq_w.apply_master(EqUpdate::HighPassEnabled(true)));
        assert!(eq_w.apply_master(EqUpdate::Band {
            index: 4,
            gain_db: 6.0,
        }));
        assert!(comp_w.apply_master(CompUpdate::Enabled(true)));
        let mut mixer = StripMixer::new(rate);
        let bank = StripParamBank::new();
        let meters = StripMeterBank::new();
        let mut scopes = ScopeBank::with_capacity(frames);
        let mut left_out = Vec::with_capacity(len);
        let mut right_out = Vec::with_capacity(len);
        let mut gr = Vec::with_capacity(blocks);
        let mut cursor = 0usize;
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        for _ in 0..blocks {
            let inputs: Vec<Option<StripBlockInput<'_>>> = waves
                .iter()
                .map(|wave| {
                    Some(StripBlockInput {
                        left: &wave[cursor..cursor + frames],
                        right: &wave[cursor..cursor + frames],
                    })
                })
                .collect();
            mixer.process(
                &bank,
                &mut eq_r,
                &mut comp_r,
                &mut limit_r,
                &meters,
                &mut scopes,
                &inputs,
                &mut left,
                &mut right,
                rate,
            );
            left_out.extend_from_slice(&left);
            right_out.extend_from_slice(&right);
            gr.push(comp_r.master_gain_reduction_db());
            cursor += frames;
        }
        let reference = crate::scene_check::render_settled(rate, frames);
        crate::scene_check::assert_near(
            "default chain settled 48000 Hz 64 frames",
            (&left_out, &right_out, &gr),
            &reference,
            (warmup * frames, frames, f64::from(rate)),
            5.0e-6,
            1.0e-4,
        );
    }

    #[test]
    fn params_round_trip_and_reject_a_different_kind() {
        let mut block = EffectBlock::compressor(CompSettings::bypass(48_000));
        let mut params = block.params();
        match &mut params {
            EffectParams::Compressor { threshold_db, .. } => *threshold_db = -30.0,
            other => panic!("compressor params became {other:?}"),
        }
        assert!(block.replace_params(params));
        let text = toml::to_string(&block.params()).unwrap();
        let loaded: EffectParams = toml::from_str(&text).unwrap();
        assert_eq!(loaded, block.params());
        assert!(!block.replace_params(EffectParams::Gain { gain_db: 3.0 }));
        block.reset();
        assert!(!block.bypassed());
        match block.params() {
            EffectParams::Compressor {
                enabled,
                threshold_db,
                ..
            } => {
                assert!(!enabled);
                assert_eq!(threshold_db, default_threshold());
            }
            other => panic!("reset became {other:?}"),
        }
    }

    #[test]
    fn unknown_kind_text_does_not_parse() {
        assert!(EffectKind::parse("plate").is_none());
        assert_eq!(EffectKind::parse(KIND_GAIN), Some(EffectKind::Gain));
    }
}

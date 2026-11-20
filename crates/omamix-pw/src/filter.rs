//! Internal `pw_filter` for the mix engine.
//!
//! pipewire-rs 0.10 does not wrap `pw_filter`. These declarations match
//! `pipewire/filter.h` on the 1.6 headers. The public Mix Mic node is an
//! adapter. This filter is `omamix.mix-engine`: one `process` callback.
//! Strip taps are input ports on it. Output ports feed the adapter.

use std::cell::UnsafeCell;
use std::ffi::{CString, c_char, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use omamix_core::comp::{CompReader, CompSettings, CompUpdate, CompWriter, open as open_comp};
use omamix_core::effect::{ChainReader, ChainWriter, MasterChain, open as open_chain};
use omamix_core::eq::{EqReader, EqSettings, EqUpdate, EqWriter, open};
use omamix_core::limit::{
    LIMIT_LOOKAHEAD_DEFAULT_MS, LimitReader, LimitSettings, LimitUpdate, LimitWriter,
    open as open_limit,
};
use omamix_core::params::{
    STRIP_SLOT_COUNT, StripBlockInput, StripMeterBank, StripMixer, StripParamBank,
};
use omamix_core::scope::{
    SCOPE_FRAMES, ScopeBank, ScopeConsumer, ScopeProducer, ScopeReadBank, ScopeWatch, split,
};
use pipewire as pw;
use pw::properties::PropertiesBox;
use pw::sys as pw_sys;

use crate::rt::{AtomicMeter, AtomicParams, MIX_SLOT_CAP, MixSlot};

const PW_FILTER_FLAG_RT_PROCESS: u32 = 1 << 2;
const PW_FILTER_FLAG_CUSTOM_LATENCY: u32 = 1 << 3;
const PW_FILTER_PORT_FLAG_MAP_BUFFERS: u32 = 1 << 0;
const PW_VERSION_FILTER_EVENTS: u32 = 1;

/// Prefix of `pw_loop`. Only `loop` is read, to reach `spa_loop_invoke`.
#[repr(C)]
struct PwLoopHead {
    _system: *mut c_void,
    spa_loop: *mut SpaLoopHead,
}

/// Prefix of `spa_loop` / `spa_interface`.
#[repr(C)]
struct SpaLoopHead {
    _type: *const c_char,
    _version: u32,
    funcs: *const SpaLoopMethods,
    data: *mut c_void,
}

type InvokeFn = unsafe extern "C" fn(
    *mut c_void,
    Option<
        unsafe extern "C" fn(*mut SpaLoopHead, bool, u32, *const c_void, usize, *mut c_void) -> i32,
    >,
    u32,
    *const c_void,
    usize,
    bool,
    *mut c_void,
) -> i32;

/// `spa_loop_methods`. `invoke` is the fourth method after `version`.
#[repr(C)]
struct SpaLoopMethods {
    version: u32,
    _add_source: *const c_void,
    _update_source: *const c_void,
    _remove_source: *const c_void,
    invoke: Option<InvokeFn>,
}

struct ClearJob {
    shared: *const ProcessShared,
    slot: usize,
    fl: *mut c_void,
    fr: *mut c_void,
}

pub const MAX_TAP_PORTS: usize = omamix_core::params::STRIP_SLOT_COUNT;

#[repr(C)]
struct SpaFraction {
    num: u32,
    denom: u32,
}

/// Prefix of `spa_io_clock`. `duration` is the quantum, in samples.
#[repr(C)]
struct SpaIoClock {
    flags: u32,
    id: u32,
    name: [u8; 64],
    nsec: u64,
    rate: SpaFraction,
    position: u64,
    duration: u64,
}

#[repr(C)]
struct SpaIoPosition {
    clock: SpaIoClock,
}

#[repr(C)]
struct PwFilterEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, i32, i32, *const c_char)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void)>,
    process: Option<unsafe extern "C" fn(*mut c_void, *mut SpaIoPosition)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
}

#[link(name = "pipewire-0.3")]
unsafe extern "C" {
    fn pw_filter_new(
        core: *mut pw_sys::pw_core,
        name: *const c_char,
        props: *mut pw_sys::pw_properties,
    ) -> *mut c_void;
    fn pw_filter_destroy(filter: *mut c_void);
    fn pw_filter_add_listener(
        filter: *mut c_void,
        listener: *mut pw::spa::sys::spa_hook,
        events: *const PwFilterEvents,
        data: *mut c_void,
    );
    fn pw_filter_connect(
        filter: *mut c_void,
        flags: u32,
        params: *const *const c_void,
        n_params: u32,
    ) -> i32;
    fn pw_filter_get_node_id(filter: *mut c_void) -> u32;
    fn pw_filter_add_port(
        filter: *mut c_void,
        direction: u32,
        flags: u32,
        port_data_size: usize,
        props: *mut pw_sys::pw_properties,
        params: *const *const c_void,
        n_params: u32,
    ) -> *mut c_void;
    fn pw_filter_get_dsp_buffer(port_data: *mut c_void, n_samples: u32) -> *mut c_void;
    fn pw_filter_remove_port(port_data: *mut c_void) -> i32;
    fn pw_filter_disconnect(filter: *mut c_void) -> i32;
    fn pw_filter_get_data_loop(filter: *mut c_void) -> *mut PwLoopHead;
    fn pw_filter_update_params(
        filter: *mut c_void,
        port_data: *mut c_void,
        params: *const *const c_void,
        n_params: u32,
    ) -> i32;
}

/// Smoother state. Only the data loop (`process` and `clear_tap_slot`) mutates it.
struct RtMixer(UnsafeCell<StripMixer>);

impl RtMixer {
    fn new(sample_rate: u32) -> Self {
        Self(UnsafeCell::new(StripMixer::new(sample_rate)))
    }

    fn get(&self) -> *mut StripMixer {
        self.0.get()
    }
}

// SAFETY: `StripMixer` is mutated only on the PipeWire data loop, from
// `process` and from `clear_tap_slot` (a blocking invoke on that same loop).
// The main loop writes atomics and port pointers and does not read the mixer.
// One data-loop mutator and no concurrent reader makes sharing the cell sound.
unsafe impl Sync for RtMixer {}

/// Scope rings shared with `process()`.
///
/// Each ring is one producer and one consumer. Those handles are `Send` and
/// not shared, so they sit in separate cells. The data loop is the only
/// mutator of the producer cells. The reader is the only mutator of the
/// consumer cells. A discard and the dropped-frame count go through
/// [`ScopeWatch`], which is an atomic and may be used from either thread.
struct RtScopes {
    writers: UnsafeCell<ScopeBank>,
    /// Kept so the strip reader is not dropped. The UI pop is not wired yet.
    #[allow(dead_code)]
    readers: UnsafeCell<ScopeReadBank>,
    master_writer: UnsafeCell<ScopeProducer>,
    /// Kept so the master reader is not dropped. The UI pop is not wired yet.
    #[allow(dead_code)]
    master_reader: UnsafeCell<ScopeConsumer>,
    watches: [ScopeWatch; STRIP_SLOT_COUNT],
    /// Dropped-frame count for the UI. Nothing reads it on the audio thread.
    #[allow(dead_code)]
    master_watch: ScopeWatch,
}

impl RtScopes {
    fn new() -> Self {
        let (writers, readers, watches) = ScopeBank::open(SCOPE_FRAMES);
        let (master_writer, master_reader, master_watch) = split(SCOPE_FRAMES);
        Self {
            writers: UnsafeCell::new(writers),
            readers: UnsafeCell::new(readers),
            master_writer: UnsafeCell::new(master_writer),
            master_reader: UnsafeCell::new(master_reader),
            watches,
            master_watch,
        }
    }

    fn request_discard(&self, slot: usize) {
        if let Some(watch) = self.watches.get(slot) {
            watch.request_discard();
        }
    }

    fn writers(&self) -> *mut ScopeBank {
        self.writers.get()
    }

    #[cfg(test)]
    fn push_strip(&self, slot: usize, left: &[f32], right: &[f32]) -> usize {
        // Safety: `RtScopes`'s Sync bound is one data-loop writer. This call
        // is that writer, or a test that does not share the cell.
        unsafe { (*self.writers.get()).push(slot, left, right) }
    }

    #[cfg(test)]
    fn pop_strip(&self, slot: usize, left: &mut [f32], right: &mut [f32]) -> usize {
        // Safety: one reader pops a strip ring. It does not alias the writer cell.
        unsafe { (*self.readers.get()).pop(slot, left, right) }
    }

    fn push_master(&self, left: &[f32], right: &[f32]) -> usize {
        // Safety: same data-loop writer as `push_strip`.
        unsafe { (*self.master_writer.get()).push(left, right) }
    }

    #[cfg(test)]
    fn pop_master(&self, left: &mut [f32], right: &mut [f32]) -> usize {
        // Safety: one reader pops the master ring. It does not alias the writer cell.
        unsafe { (*self.master_reader.get()).pop(left, right) }
    }

    #[cfg(test)]
    fn master_dropped_frames(&self) -> u64 {
        self.master_watch.dropped_frames()
    }
}

// SAFETY: producer cells are mutated only on the data loop (`process`).
// Consumer cells are mutated only by the thread that pops them. The two
// cells do not alias, and a producer is not shared, so two threads cannot
// both call `push`. `prepare_tap_slot` stores the discard flag on the watch
// and does not touch either cell.
unsafe impl Sync for RtScopes {}

/// EQ publications for the data loop. The producer is [`EqWriter`], which
/// is not stored here and is not `Sync`.
struct RtEq {
    reader: UnsafeCell<EqReader>,
}

impl RtEq {
    /// Pointer to the reader cell. The caller turns it into one `&mut EqReader`.
    ///
    /// A reference tied to `&self` would borrow all of `ProcessShared` and
    /// could not sit next to `&shared.strips` in `process`.
    fn reader_ptr(&self) -> *mut EqReader {
        self.reader.get()
    }
}

// SAFETY: `reader` is polled only on the data loop, through one `&mut EqReader`
// per callback. `EqReader` is not `Sync` and not `Clone`. This wrapper is what
// lets `ProcessShared` be shared. The producer is a separate `EqWriter` owned
// by the loop thread, not a field of this struct.
unsafe impl Sync for RtEq {}

/// Compressor publications for the data loop. The producer is [`CompWriter`],
/// which is not stored here and is not `Sync`.
struct RtComp {
    reader: UnsafeCell<CompReader>,
}

impl RtComp {
    /// Pointer to the reader cell. The caller turns it into one `&mut CompReader`.
    ///
    /// A reference tied to `&self` would borrow all of `ProcessShared` and
    /// could not sit next to `&shared.strips` in `process`.
    fn reader_ptr(&self) -> *mut CompReader {
        self.reader.get()
    }
}

// SAFETY: `reader` is polled only on the data loop, through one `&mut CompReader` per callback. `CompReader` is not `Sync` and not `Clone`.
// SAFETY: This wrapper is what lets `ProcessShared` be shared. The producer is a separate `CompWriter` owned by the loop thread, not a field of this struct.
unsafe impl Sync for RtComp {}

/// Limiter publications for the data loop. The producer is [`LimitWriter`],
/// which is not stored here and is not `Sync`.
struct RtLimit {
    reader: UnsafeCell<LimitReader>,
}

impl RtLimit {
    fn reader_ptr(&self) -> *mut LimitReader {
        self.reader.get()
    }
}

// SAFETY: `reader` is polled only on the data loop, through one `&mut LimitReader` per callback. `LimitReader` is not `Sync` and not `Clone`. This wrapper is what lets `ProcessShared` be shared. The producer is a separate `LimitWriter` owned by the loop thread, not a field of this struct.
unsafe impl Sync for RtLimit {}

/// Chain publications for the data loop. The producer is [`ChainWriter`],
/// which is not stored here and is not `Sync`.
struct RtChain {
    reader: UnsafeCell<ChainReader>,
}

impl RtChain {
    fn reader_ptr(&self) -> *mut ChainReader {
        self.reader.get()
    }
}

// SAFETY: `reader` is polled only on the data loop, through one `&mut ChainReader` per callback. `ChainReader` is not `Sync` and not `Clone`. This wrapper is what lets `ProcessShared` be shared. The producer is a separate `ChainWriter` owned by the loop thread, not a field of this struct.
unsafe impl Sync for RtChain {}

/// Shared with `process()`. The loop thread writes params and port pointers.
/// `process()` writes the meters, the gain-reduction atomics, the scope rings,
/// and the monitor slot. No channel receive. EQ coefficients and compressor
/// blocks are not published from this struct.
pub struct ProcessShared {
    pub params: AtomicParams,
    /// Per-strip trim, fader, pan, mute, solo, and enable. The loop thread writes.
    pub strips: StripParamBank,
    /// High-pass and graphic EQ publications. `process` only loads them.
    eq: RtEq,
    /// Compressor publications. `process` loads them through `&mut CompReader`
    /// and writes gain reduction back.
    comp: RtComp,
    /// Master limiter publications. `process` loads them through `&mut LimitReader`.
    limit: RtLimit,
    /// Master chain order. `process` loads it through `&mut ChainReader`.
    chain: RtChain,
    /// Sample rate from the last `process`, so the loop thread can rebuild.
    /// Zero until the first callback.
    pub observed_rate: AtomicU32,
    /// Smoother state. Only the data loop (`process` and `clear_tap_slot`) touches it.
    mixer: RtMixer,
    /// Master block peak and RMS, after the master fader.
    pub meter: AtomicMeter,
    /// Per-strip block peak and RMS, post-fader, before the master fader.
    pub strip_meters: StripMeterBank,
    /// Post-fader strip rings and the master ring. The writer drops when full.
    scopes: RtScopes,
    pub slot: MixSlot,
    pub filter: AtomicPtr<c_void>,
    pub node_id: AtomicU32,
    pub state: AtomicI32,
    pub out_fl: AtomicPtr<c_void>,
    pub out_fr: AtomicPtr<c_void>,
    pub inputs_fl: [AtomicPtr<c_void>; MAX_TAP_PORTS],
    pub inputs_fr: [AtomicPtr<c_void>; MAX_TAP_PORTS],
    pub input_count: AtomicUsize,
    /// Opt-in 440 Hz sine. The loop thread sets it before connect.
    pub tone: AtomicBool,
    /// Oscillator phase in cycles, `0.0..1.0`, stored as bits.
    pub tone_phase: AtomicU32,
    /// Master lookahead in samples. The loop thread publishes it.
    own_latency: AtomicU32,
    upstream_fl: [AtomicU32; MAX_TAP_PORTS],
    upstream_fr: [AtomicU32; MAX_TAP_PORTS],
    /// Max upstream peer rate. The filter main loop stores it from
    /// `on_param_changed` and reads it in `write_known_port_latency`.
    /// The audio callback does not read it.
    upstream_rate: AtomicU32,
}

impl ProcessShared {
    #[cfg(test)]
    fn new() -> Self {
        Self::pair().0
    }

    /// Readers stay in the shared state. The writers are the loop thread's.
    fn pair() -> (Self, EqWriter, CompWriter, LimitWriter, ChainWriter) {
        let (eq_writer, eq_reader) = open();
        let (comp_writer, comp_reader) = open_comp();
        let (limit_writer, limit_reader) = open_limit();
        let (chain_writer, chain_reader) = open_chain();
        let own = chain_writer.reported_with_limiter(&limit_writer);
        (
            Self {
                params: AtomicParams::new(),
                strips: StripParamBank::new(),
                eq: RtEq {
                    reader: UnsafeCell::new(eq_reader),
                },
                comp: RtComp {
                    reader: UnsafeCell::new(comp_reader),
                },
                limit: RtLimit {
                    reader: UnsafeCell::new(limit_reader),
                },
                chain: RtChain {
                    reader: UnsafeCell::new(chain_reader),
                },
                observed_rate: AtomicU32::new(0),
                mixer: RtMixer::new(48_000),
                meter: AtomicMeter::new(),
                strip_meters: StripMeterBank::new(),
                scopes: RtScopes::new(),
                slot: MixSlot::new(),
                filter: AtomicPtr::new(ptr::null_mut()),
                node_id: AtomicU32::new(u32::MAX),
                state: AtomicI32::new(0),
                out_fl: AtomicPtr::new(ptr::null_mut()),
                out_fr: AtomicPtr::new(ptr::null_mut()),
                inputs_fl: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
                inputs_fr: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
                input_count: AtomicUsize::new(0),
                tone: AtomicBool::new(false),
                tone_phase: AtomicU32::new(0),
                own_latency: AtomicU32::new(own),
                upstream_fl: std::array::from_fn(|_| AtomicU32::new(0)),
                upstream_fr: std::array::from_fn(|_| AtomicU32::new(0)),
                upstream_rate: AtomicU32::new(0),
            },
            eq_writer,
            comp_writer,
            limit_writer,
            chain_writer,
        )
    }

    fn recompute_upstream(&self) -> u32 {
        let mut max_rate = 0u32;
        for slot in 0..MAX_TAP_PORTS {
            max_rate = max_rate.max(self.upstream_fl[slot].load(Ordering::Acquire));
            max_rate = max_rate.max(self.upstream_fr[slot].load(Ordering::Acquire));
        }
        self.upstream_rate.store(max_rate, Ordering::Release);
        max_rate
    }

    fn clear_upstream_slot(&self, slot: usize) {
        if slot >= MAX_TAP_PORTS {
            return;
        }
        self.upstream_fl[slot].store(0, Ordering::Release);
        self.upstream_fr[slot].store(0, Ordering::Release);
        self.recompute_upstream();
    }

    /// Store one input port's peer rate and return the output-direction sum.
    ///
    /// `port` is the filter port userdata. An unknown port is ignored so an
    /// output-port event cannot wipe the upstream rate.
    fn note_peer_latency(&self, port: *mut c_void, rate: u32) -> Option<u32> {
        if port.is_null() {
            return None;
        }
        let mut found = false;
        for slot in 0..MAX_TAP_PORTS {
            if self.inputs_fl[slot].load(Ordering::Acquire) == port {
                self.upstream_fl[slot].store(rate, Ordering::Release);
                found = true;
                break;
            }
            if self.inputs_fr[slot].load(Ordering::Acquire) == port {
                self.upstream_fr[slot].store(rate, Ordering::Release);
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
        let upstream = self.recompute_upstream();
        let own = self.own_latency.load(Ordering::Acquire);
        let output = output_direction_latency(upstream, own);
        self.rewrite_output_ports(output);
        Some(output)
    }

    fn rewrite_output_ports(&self, samples: u32) {
        let filter = self.filter.load(Ordering::Acquire);
        if filter.is_null() {
            return;
        }
        let bytes = port_latency_pod(samples, pw::spa::sys::SPA_DIRECTION_OUTPUT);
        let Some(pod) = pw::spa::pod::Pod::from_bytes(&bytes) else {
            return;
        };
        let params = [pod.as_raw_ptr() as *const c_void];
        for port in [
            self.out_fl.load(Ordering::Acquire),
            self.out_fr.load(Ordering::Acquire),
        ] {
            if port.is_null() {
                continue;
            }
            // SAFETY: `filter` is the pw_filter stored on this ProcessShared and is non-null. `port` is one of its output ports and is non-null. `params` holds one spa_pod built from `bytes`, which stays alive for this call. PipeWire copies the pod before returning. `on_param_changed` runs this on the filter main loop, not from `process`.
            unsafe {
                pw_filter_update_params(filter, port, params.as_ptr(), 1);
            }
        }
    }
}

/// Output-direction latency is the upstream peer's max rate plus this node's lookahead.
///
/// Only that rate is carried. Peer `ns` and quantum are not added, and the
/// pod this node writes sets both to 0. Input-direction latency stays the
/// lookahead alone.
fn output_direction_latency(upstream: u32, own: u32) -> u32 {
    upstream.saturating_add(own)
}

fn limiter_request_matches(writer: &LimitWriter, settings: LimitSettings) -> bool {
    let current = writer.master_settings();
    if settings.enabled() != current.enabled() || settings.ceiling_db() != current.ceiling_db() {
        return false;
    }
    if settings.lookahead_ms() == current.lookahead_ms() {
        return true;
    }
    settings.lookahead_ms() == 0.0
        && current.lookahead_ms() == LIMIT_LOOKAHEAD_DEFAULT_MS
        && writer.master_cap_demoted()
}

pub struct MixFilter {
    raw: NonNull<c_void>,
    events: Box<PwFilterEvents>,
    hook: Box<pw::spa::sys::spa_hook>,
    shared: Arc<ProcessShared>,
    /// The only EQ producer. Not shared with `process`.
    eq: EqWriter,
    /// The only compressor producer. Not shared with `process`.
    comp: CompWriter,
    /// The only limiter producer. Not shared with `process`.
    limit: LimitWriter,
    /// The only chain producer. Not shared with `process`.
    chain: ChainWriter,
    destroyed: bool,
    /// True after `pw_filter_connect`. Param updates stay off the unit-test
    /// filter, which has no PipeWire object.
    live: bool,
    /// Last `(samples, rate)` passed to `pw_filter_update_params` for `ProcessLatency`.
    latency_sent: Option<(u32, u32)>,
    /// Last `(input samples, output samples)` written to port `Latency`.
    ports_latency_sent: Option<(u32, u32)>,
    /// Last `(samples, rate)` the loop decided to publish. Set even when the
    /// filter is not live, so a unit test can see a reset without PipeWire.
    #[cfg(test)]
    latency_queued: Option<(u32, u32)>,
    /// Output-direction samples the last port write used. Set even when the
    /// filter is not live, so a unit test can see upstream plus lookahead.
    #[cfg(test)]
    output_latency_written: Option<u32>,
    /// Input-direction samples the last port write used. This stays the
    /// lookahead and does not include the upstream rate.
    #[cfg(test)]
    input_latency_written: Option<u32>,
    /// How many times the port sample counts actually changed.
    #[cfg(test)]
    port_latency_writes: u32,
    /// Loop-thread lines for a master cap demotion. `process` does not push.
    #[cfg(test)]
    demotion_lines: Vec<String>,
    /// Unit tests set this so [`Self::add_tap_ports`] can run without a filter.
    /// A real connect leaves it false and adds the port.
    #[cfg(test)]
    reject_ports: bool,
}

impl MixFilter {
    pub fn create(
        core: &pw::core::Core,
        props: PropertiesBox,
        tone: bool,
    ) -> Result<Self, pw::Error> {
        let name = CString::new("omamix mix engine").expect("name");
        let (shared, eq, comp, limit, chain) = ProcessShared::pair();
        let shared = Arc::new(shared);
        shared.tone.store(tone, Ordering::Release);
        let raw = unsafe { pw_filter_new(core.as_raw_ptr(), name.as_ptr(), props.into_raw()) };
        let raw = NonNull::new(raw).ok_or(pw::Error::CreationFailed)?;
        shared.filter.store(raw.as_ptr(), Ordering::Release);

        let events = Box::new(PwFilterEvents {
            version: PW_VERSION_FILTER_EVENTS,
            destroy: None,
            state_changed: Some(on_state),
            io_changed: None,
            param_changed: Some(on_param_changed),
            add_buffer: None,
            remove_buffer: None,
            process: Some(on_process),
            drained: None,
            command: None,
        });
        let mut hook: Box<pw::spa::sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
        unsafe {
            pw_filter_add_listener(
                raw.as_ptr(),
                hook.as_mut(),
                events.as_ref(),
                Arc::as_ptr(&shared) as *mut c_void,
            );
        }

        let mut filter = Self {
            raw,
            events,
            hook,
            shared,
            eq,
            comp,
            limit,
            chain,
            destroyed: false,
            live: false,
            latency_sent: None,
            ports_latency_sent: None,
            #[cfg(test)]
            latency_queued: None,
            #[cfg(test)]
            output_latency_written: None,
            #[cfg(test)]
            input_latency_written: None,
            #[cfg(test)]
            port_latency_writes: 0,
            #[cfg(test)]
            demotion_lines: Vec::new(),
            #[cfg(test)]
            reject_ports: false,
        };
        filter.add_output_ports()?;
        let samples = filter.reported_latency_samples();
        let rate = filter.limit.sample_rate();
        let props_bytes = unity_props_pod();
        let latency_bytes = process_latency_pod(samples, rate);
        let props_pod =
            pw::spa::pod::Pod::from_bytes(&props_bytes).ok_or(pw::Error::CreationFailed)?;
        let latency_pod =
            pw::spa::pod::Pod::from_bytes(&latency_bytes).ok_or(pw::Error::CreationFailed)?;
        let params = [
            props_pod.as_raw_ptr() as *const c_void,
            latency_pod.as_raw_ptr() as *const c_void,
        ];
        let rc = unsafe {
            pw_filter_connect(
                filter.raw.as_ptr(),
                PW_FILTER_FLAG_RT_PROCESS | PW_FILTER_FLAG_CUSTOM_LATENCY,
                params.as_ptr(),
                params.len() as u32,
            )
        };
        if rc < 0 {
            return Err(pw::Error::CreationFailed);
        }
        filter.live = true;
        filter.latency_sent = Some((samples, rate));
        filter.write_known_port_latency(samples);
        Ok(filter)
    }

    pub fn shared(&self) -> Arc<ProcessShared> {
        Arc::clone(&self.shared)
    }

    pub(crate) fn apply_strip_eq(&mut self, slot: u16, update: EqUpdate) -> bool {
        self.eq.apply(slot, update)
    }

    pub(crate) fn apply_master_eq(&mut self, update: EqUpdate) -> bool {
        self.eq.apply_master(update)
    }

    pub(crate) fn set_eq_rate(&mut self, rate: u32) -> bool {
        self.eq.set_sample_rate(rate)
    }

    pub(crate) fn apply_strip_comp(&mut self, slot: u16, update: CompUpdate) -> bool {
        self.comp.apply(slot, update)
    }

    pub(crate) fn apply_master_comp(&mut self, update: CompUpdate) -> bool {
        self.comp.apply_master(update)
    }

    pub(crate) fn set_comp_rate(&mut self, rate: u32) -> bool {
        self.comp.set_sample_rate(rate)
    }

    pub(crate) fn apply_master_limit(&mut self, update: LimitUpdate) -> bool {
        let applied = self.limit.apply_master(update);
        if applied.demoted() {
            self.log_cap_demotion();
        }
        self.note_process_latency();
        applied.accepted()
    }

    pub(crate) fn apply_saved_master_limit(&mut self, settings: omamix_core::limit::LimitSettings) {
        self.limit.apply_master_settings(settings);
        self.log_recorded_demotions();
        self.note_process_latency();
    }

    pub(crate) fn sync_strip_eq(&mut self, slot: u16, enabled: bool, settings: EqSettings) {
        let Some(current) = self.eq.settings(slot as usize) else {
            return;
        };
        let Some(next) = omamix_core::eq::audible_if_changed(current, enabled, settings) else {
            return;
        };
        let _ = self.eq.apply_settings(slot, next);
    }

    pub(crate) fn sync_master_eq(&mut self, enabled: bool, settings: EqSettings) {
        let Some(next) =
            omamix_core::eq::audible_if_changed(self.eq.master_settings(), enabled, settings)
        else {
            return;
        };
        self.eq.apply_master_settings(next);
    }

    pub(crate) fn sync_strip_comp(&mut self, slot: u16, settings: CompSettings) {
        let Some(current) = self.comp.settings(slot as usize) else {
            return;
        };
        if omamix_core::comp::same_controls(current, settings) {
            return;
        }
        let _ = self.comp.apply_settings(slot, settings);
    }

    pub(crate) fn sync_master_comp(&mut self, settings: CompSettings) {
        if omamix_core::comp::same_controls(self.comp.master_settings(), settings) {
            return;
        }
        self.comp.apply_master_settings(settings);
    }

    pub(crate) fn strip_reduction(&self, slot: usize) -> f32 {
        self.comp.gain_reduction_db(slot)
    }

    pub(crate) fn master_reduction(&self) -> f32 {
        self.comp.master_gain_reduction_db()
    }

    pub(crate) fn graph_quantum(&self) -> u32 {
        self.limit.callback_quantum()
    }

    /// Publish chain order, bypass, and gain. Does not rewrite compressor
    /// or limiter settings. The loop thread calls this.
    pub(crate) fn publish_chain(&mut self, chain: &MasterChain) {
        self.chain.publish(chain);
        self.note_process_latency();
    }

    /// Publish `chain` and, when a block's settings differ, the compressor
    /// or limiter block those settings name.
    ///
    /// A 0 ms lookahead the cap already rewrote to 5 ms is left as it is, so
    /// a second copy of the same request does not publish the limiter again.
    pub(crate) fn apply_chain(&mut self, chain: &MasterChain) {
        if let Some(mut settings) = chain.compressor_settings() {
            settings.set_sample_rate(self.comp.sample_rate());
            if settings != self.comp.master_settings() {
                self.comp.apply_master_settings(settings);
            }
        }
        if let Some(mut settings) = chain.limiter_settings() {
            settings.set_sample_rate(self.limit.sample_rate());
            if !limiter_request_matches(&self.limit, settings) {
                self.limit.apply_master_settings(settings);
                self.log_recorded_demotions();
            }
        }
        self.publish_chain(chain);
    }

    pub(crate) fn set_limit_rate(&mut self, rate: u32) -> bool {
        let applied = self.limit.set_sample_rate(rate);
        self.log_recorded_demotions();
        self.note_process_latency();
        applied
    }

    /// Copy the callback quantum into the 0 ms cap and publish the delay
    /// the callback is applying.
    pub(crate) fn sync_limit_quantum(&mut self) {
        let frames = self.limit.callback_quantum();
        if frames == 0 {
            return;
        }
        if self.limit.set_processing_quantum(frames) {
            self.log_recorded_demotions();
        }
        self.note_process_latency();
    }

    fn log_recorded_demotions(&mut self) {
        for _slot in self.limit.drain_cap_demotions() {
            self.log_cap_demotion();
        }
    }

    fn log_cap_demotion(&mut self) {
        let line = "omamix: limiter master stored 5 ms lookahead; the 0 ms clipper is off and the delay is 5 ms";
        eprintln!("{line}");
        #[cfg(test)]
        self.demotion_lines.push(line.to_string());
    }

    /// Publish `ProcessLatency` on the loop thread.
    ///
    /// `process` does not call this. Port latency is rewritten when the
    /// lookahead or the upstream rate changes. Storing a port pointer clears
    /// the cached sample counts, so a newly added strip is written even when
    /// those counts match the previous pair.
    fn note_process_latency(&mut self) {
        let samples = self.reported_latency_samples();
        let rate = self.limit.sample_rate();
        self.write_known_port_latency(samples);
        if self.latency_sent == Some((samples, rate)) {
            return;
        }
        #[cfg(test)]
        {
            self.latency_queued = Some((samples, rate));
        }
        if !self.live {
            return;
        }
        let bytes = process_latency_pod(samples, rate);
        let Some(pod) = pw::spa::pod::Pod::from_bytes(&bytes) else {
            return;
        };
        let params = [pod.as_raw_ptr() as *const c_void];
        // SAFETY: `self.raw` is the live pw_filter owned by this MixFilter. A null port updates the filter object. `params` holds one spa_pod built from `bytes`, alive for this call, and PipeWire copies it before returning. Called on the loop thread. `process` does not call this.
        let rc = unsafe {
            pw_filter_update_params(self.raw.as_ptr(), ptr::null_mut(), params.as_ptr(), 1)
        };
        if rc == 0 {
            self.latency_sent = Some((samples, rate));
        }
    }

    /// Input ports carry `samples`. Output ports carry upstream + `samples`.
    fn write_known_port_latency(&mut self, samples: u32) {
        self.shared.own_latency.store(samples, Ordering::Release);
        let upstream = self.shared.upstream_rate.load(Ordering::Acquire);
        let output_samples = output_direction_latency(upstream, samples);
        if self.ports_latency_sent == Some((samples, output_samples)) {
            return;
        }
        self.ports_latency_sent = Some((samples, output_samples));
        #[cfg(test)]
        {
            self.output_latency_written = Some(output_samples);
            self.input_latency_written = Some(samples);
            self.port_latency_writes = self.port_latency_writes.saturating_add(1);
        }
        if !self.live {
            return;
        }
        let output = pw::spa::sys::SPA_DIRECTION_OUTPUT;
        let input = pw::spa::sys::SPA_DIRECTION_INPUT;
        self.write_port_latency(
            self.shared.out_fl.load(Ordering::Acquire),
            output_samples,
            output,
        );
        self.write_port_latency(
            self.shared.out_fr.load(Ordering::Acquire),
            output_samples,
            output,
        );
        for slot in 0..MAX_TAP_PORTS {
            self.write_port_latency(
                self.shared.inputs_fl[slot].load(Ordering::Acquire),
                samples,
                input,
            );
            self.write_port_latency(
                self.shared.inputs_fr[slot].load(Ordering::Acquire),
                samples,
                input,
            );
        }
    }

    fn write_port_latency(&self, port: *mut c_void, samples: u32, direction: u32) {
        if !self.live || port.is_null() {
            return;
        }
        let bytes = port_latency_pod(samples, direction);
        let Some(pod) = pw::spa::pod::Pod::from_bytes(&bytes) else {
            return;
        };
        let params = [pod.as_raw_ptr() as *const c_void];
        // SAFETY: `self.raw` is the live pw_filter. `port` is a non-null port of that filter. `params` holds one spa_pod built from `bytes`, alive for this call. PipeWire copies the pod before returning. Called on the loop thread from `note_process_latency`.
        unsafe {
            pw_filter_update_params(self.raw.as_ptr(), port, params.as_ptr(), 1);
        }
    }

    /// Samples of delay in the mix at the callback rate and quantum.
    ///
    /// This is the sum of the published chain. The default chain is the
    /// limiter delay. A chain-bypassed limiter counts as 0.
    pub fn reported_latency_samples(&self) -> u32 {
        self.chain.reported_with_limiter(&self.limit)
    }

    pub fn node_id(&self) -> Option<u32> {
        let id = unsafe { pw_filter_get_node_id(self.raw.as_ptr()) };
        self.shared.node_id.store(id, Ordering::Release);
        if id == 0 || id == u32::MAX {
            None
        } else {
            Some(id)
        }
    }

    /// One stereo tap. Reuses the first empty slot. Returns that slot.
    pub fn add_tap_ports(&mut self, peer: u32) -> Result<usize, ()> {
        let slot = (0..MAX_TAP_PORTS)
            .find(|index| {
                self.shared.inputs_fl[*index]
                    .load(Ordering::Acquire)
                    .is_null()
                    && self.shared.inputs_fr[*index]
                        .load(Ordering::Acquire)
                        .is_null()
            })
            .ok_or(())?;
        // Neutral before either new port pointer is stored. The coefficient
        // publish is here, on the loop thread. `clear_tap_slot` only clears
        // filter memory on the data loop.
        prepare_tap_slot(&self.shared, &mut self.eq, &mut self.comp, slot);
        let fl = self.add_port(
            pw::spa::utils::Direction::Input,
            &format!("in-{peer}-FL"),
            "FL",
        )?;
        self.store_input_port(slot, true, fl);
        let fr = match self.add_port(
            pw::spa::utils::Direction::Input,
            &format!("in-{peer}-FR"),
            "FR",
        ) {
            Ok(fr) => fr,
            Err(()) => {
                // FL is already on the filter. Clear it on the data loop, then
                // free it. A failed clear leaves the pointer in the slot until
                // destroy, and that slot stays occupied.
                self.remove_tap_ports(slot);
                return Err(());
            }
        };
        self.store_input_port(slot, false, fr);
        let count = self.shared.input_count.load(Ordering::Acquire);
        if slot >= count {
            self.shared.input_count.store(slot + 1, Ordering::Release);
        }
        self.note_process_latency();
        Ok(slot)
    }

    /// Drop one tap's ports so a later tap can reuse the slot.
    ///
    /// `process` runs on the data loop (`PW_FILTER_FLAG_RT_PROCESS`) and may
    /// already hold a port pointer inside `pw_filter_get_dsp_buffer`.
    /// `pw_filter_remove_port` frees that port on the main loop. The slot
    /// pointers are cleared with a blocking invoke on the data loop first, so
    /// the free happens only after `process` has dropped the pointer. If that
    /// invoke is unavailable the ports stay until [`Self::destroy`].
    ///
    /// Stress: add and drop taps while `--tone` is running on a private
    /// PipeWire. A unit test cannot schedule the realtime thread against this
    /// free. The invoke-then-free order is what keeps the pointer valid.
    pub fn remove_tap_ports(&mut self, slot: usize) {
        if slot >= MAX_TAP_PORTS {
            return;
        }
        // Flat coefficients are built here, off the data loop. The invoke
        // below only clears filter memory and the port pointers.
        self.eq.reset_slot(slot);
        self.comp.reset_slot(slot);
        self.shared.clear_upstream_slot(slot);
        self.note_process_latency();
        if !self.live {
            return;
        }
        let mut job = ClearJob {
            shared: Arc::as_ptr(&self.shared),
            slot,
            fl: ptr::null_mut(),
            fr: ptr::null_mut(),
        };
        let cleared = self.clear_slot_on_data_loop(&mut job);
        if !cleared {
            eprintln!(
                "omamix: tap slot {slot} stays until the filter is destroyed; the data loop did not clear it"
            );
            return;
        }
        unsafe {
            if !job.fl.is_null() {
                let _ = pw_filter_remove_port(job.fl);
            }
            if !job.fr.is_null() {
                let _ = pw_filter_remove_port(job.fr);
            }
        }
    }

    /// Returns true only after the data loop has published null slot pointers.
    fn clear_slot_on_data_loop(&self, job: &mut ClearJob) -> bool {
        let data_loop = unsafe { pw_filter_get_data_loop(self.raw.as_ptr()) };
        if data_loop.is_null() {
            return false;
        }
        let spa_loop = unsafe { (*data_loop).spa_loop };
        if spa_loop.is_null() {
            return false;
        }
        let funcs = unsafe { (*spa_loop).funcs };
        if funcs.is_null() {
            return false;
        }
        let Some(invoke) = (unsafe { (*funcs).invoke }) else {
            return false;
        };
        let data = unsafe { (*spa_loop).data };
        let rc = unsafe {
            invoke(
                data,
                Some(clear_tap_slot),
                0,
                ptr::null(),
                0,
                true,
                job as *mut ClearJob as *mut c_void,
            )
        };
        rc == 0
    }

    fn add_output_ports(&mut self) -> Result<(), pw::Error> {
        let fl = self
            .add_port(pw::spa::utils::Direction::Output, "capture_FL", "FL")
            .map_err(|_| pw::Error::CreationFailed)?;
        let fr = self
            .add_port(pw::spa::utils::Direction::Output, "capture_FR", "FR")
            .map_err(|_| pw::Error::CreationFailed)?;
        self.store_output_port(true, fl);
        self.store_output_port(false, fr);
        Ok(())
    }

    /// Store one tap pointer and drop the cached port-latency pair.
    ///
    /// A later [`Self::note_process_latency`] writes the new port even when
    /// the sample counts are unchanged.
    fn store_input_port(&mut self, slot: usize, left: bool, port: *mut c_void) {
        if slot >= MAX_TAP_PORTS {
            return;
        }
        if left {
            self.shared.inputs_fl[slot].store(port, Ordering::Release);
        } else {
            self.shared.inputs_fr[slot].store(port, Ordering::Release);
        }
        self.ports_latency_sent = None;
    }

    fn store_output_port(&mut self, left: bool, port: *mut c_void) {
        if left {
            self.shared.out_fl.store(port, Ordering::Release);
        } else {
            self.shared.out_fr.store(port, Ordering::Release);
        }
        self.ports_latency_sent = None;
    }

    /// No PipeWire filter. `add_tap_ports` resets the claimed slot, then the
    /// port add fails before any C call. Drop does not touch the daemon:
    /// `destroyed` is already set.
    #[cfg(test)]
    fn without_filter() -> Self {
        let (shared, eq, comp, limit, chain) = ProcessShared::pair();
        Self {
            raw: NonNull::dangling(),
            events: Box::new(PwFilterEvents {
                version: 0,
                destroy: None,
                state_changed: None,
                io_changed: None,
                param_changed: None,
                add_buffer: None,
                remove_buffer: None,
                process: None,
                drained: None,
                command: None,
            }),
            // SAFETY: `spa_hook` is a C struct of pointers. Zero is an empty
            // hook. This value is never registered, and Drop returns before
            // any PipeWire call because `destroyed` is already true.
            hook: Box::new(unsafe { std::mem::zeroed() }),
            shared: Arc::new(shared),
            eq,
            comp,
            limit,
            chain,
            destroyed: true,
            live: false,
            latency_sent: None,
            ports_latency_sent: None,
            latency_queued: None,
            output_latency_written: None,
            input_latency_written: None,
            port_latency_writes: 0,
            demotion_lines: Vec::new(),
            reject_ports: true,
        }
    }

    fn add_port(
        &mut self,
        direction: pw::spa::utils::Direction,
        name: &str,
        channel: &str,
    ) -> Result<*mut c_void, ()> {
        #[cfg(test)]
        if self.reject_ports {
            return Err(());
        }
        let mut props = PropertiesBox::new();
        props.insert("port.name", name);
        props.insert("audio.channel", channel);
        props.insert("format.dsp", "32 bit float mono audio");
        // Non-zero so the returned userdata pointer is a real port handle.
        // A size of 0 can come back NULL even when the port was added.
        let port = unsafe {
            pw_filter_add_port(
                self.raw.as_ptr(),
                direction.as_raw(),
                PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                std::mem::size_of::<u64>(),
                props.into_raw(),
                ptr::null(),
                0,
            )
        };
        if port.is_null() { Err(()) } else { Ok(port) }
    }

    pub fn destroy(&mut self) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;
        unsafe {
            // Disconnect before the hook is removed so an in-flight process
            // is not still using the filter when the node goes away.
            let _ = pw_filter_disconnect(self.raw.as_ptr());
            pw::spa::utils::hook::remove(*self.hook);
            pw_filter_destroy(self.raw.as_ptr());
        }
    }
}

impl Drop for MixFilter {
    fn drop(&mut self) {
        self.destroy();
    }
}

unsafe extern "C" fn clear_tap_slot(
    _loop: *mut SpaLoopHead,
    _async: bool,
    _seq: u32,
    _data: *const c_void,
    _size: usize,
    user: *mut c_void,
) -> i32 {
    if user.is_null() {
        return -1;
    }
    let job = unsafe { &mut *(user as *mut ClearJob) };
    if job.shared.is_null() || job.slot >= MAX_TAP_PORTS {
        return -1;
    }
    let shared = unsafe { &*job.shared };
    let slot = job.slot;
    job.fl = shared.inputs_fl[slot].swap(ptr::null_mut(), Ordering::AcqRel);
    job.fr = shared.inputs_fr[slot].swap(ptr::null_mut(), Ordering::AcqRel);
    let mut count = shared.input_count.load(Ordering::Acquire);
    while count > 0
        && shared.inputs_fl[count - 1]
            .load(Ordering::Acquire)
            .is_null()
        && shared.inputs_fr[count - 1]
            .load(Ordering::Acquire)
            .is_null()
    {
        count -= 1;
    }
    shared.input_count.store(count, Ordering::Release);
    // Same data loop as `process`. Put the slot back to the neutral smoother
    // and the neutral parameters so the next tap does not inherit a mute.
    reset_slot_atomics(shared, slot);
    unsafe { (*shared.mixer.get()).reset_slot(slot) };
    0
}

/// Neutral strip parameters and a silent meter for a slot that is about to
/// be claimed.
///
/// The EQ publication is built here, on the loop thread. The smoother reset
/// and the filter-memory clear stay in `clear_tap_slot`, which runs on the
/// data loop and does not build coefficients.
fn prepare_tap_slot(shared: &ProcessShared, eq: &mut EqWriter, comp: &mut CompWriter, slot: usize) {
    reset_slot_atomics(shared, slot);
    eq.reset_slot(slot);
    comp.reset_slot(slot);
}

/// Atomics only. Safe on the data loop: no coefficient design.
fn reset_slot_atomics(shared: &ProcessShared, slot: usize) {
    shared.strips.reset_slot(slot);
    shared.strip_meters.reset_slot(slot);
    shared.scopes.request_discard(slot);
}

unsafe extern "C" fn on_param_changed(
    data: *mut c_void,
    port_data: *mut c_void,
    id: u32,
    param: *const c_void,
) {
    if data.is_null() || param.is_null() || id != pw::spa::sys::SPA_PARAM_Latency {
        return;
    }
    let Some(bytes) = pod_bytes(param) else {
        return;
    };
    let Some((direction, rate)) = latency_direction_and_rate(&bytes) else {
        return;
    };
    if direction != pw::spa::sys::SPA_DIRECTION_OUTPUT {
        return;
    }
    // SAFETY: `data` is the ProcessShared pointer passed to `pw_filter_add_listener`. MixFilter keeps that Arc alive until the filter is destroyed. PipeWire runs this callback on the filter main loop, the thread that drops the filter, so the pointer stays valid for the call.
    let shared = unsafe { &*(data as *const ProcessShared) };
    let _ = shared.note_peer_latency(port_data, rate);
}

/// Copy a spa_pod, header included. `size` is the body size.
fn pod_bytes(param: *const c_void) -> Option<Vec<u8>> {
    // SAFETY: `param` is non-null. PipeWire passes a complete spa_pod. The first u32 is the body size, and the header may be unaligned, so the size word is read unaligned.
    let size = unsafe { (param as *const u32).read_unaligned() } as usize;
    let total = size.checked_add(8)?;
    if total > 8192 {
        return None;
    }
    // SAFETY: PipeWire's spa_pod is complete, so the object is the declared body size plus the 8-byte header. The 8192 bound refuses a larger declared total. Those bytes stay readable for this callback, and the copy ends the borrow.
    let bytes = unsafe { std::slice::from_raw_parts(param as *const u8, total) };
    Some(bytes.to_vec())
}

unsafe extern "C" fn on_state(data: *mut c_void, _old: i32, state: i32, _error: *const c_char) {
    if data.is_null() {
        return;
    }
    // `data` is the ProcessShared arc held by MixFilter for the filter's life.
    let shared = unsafe { &*(data as *const ProcessShared) };
    shared.state.store(state, Ordering::Relaxed);
    let filter = shared.filter.load(Ordering::Acquire);
    if !filter.is_null() {
        let id = unsafe { pw_filter_get_node_id(filter) };
        shared.node_id.store(id, Ordering::Release);
    }
}

unsafe extern "C" fn on_process(data: *mut c_void, position: *mut SpaIoPosition) {
    if data.is_null() || position.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Same ProcessShared pointer pw_filter_add_listener received.
        let shared = unsafe { &*(data as *const ProcessShared) };
        let position = unsafe { &*position };
        render(shared, position);
    }));
}

fn render(shared: &ProcessShared, position: &SpaIoPosition) {
    let n = position.clock.duration;
    if n == 0 || n > 16_384 {
        return;
    }
    let n = n as u32;
    let fl = shared.out_fl.load(Ordering::Acquire);
    let fr = shared.out_fr.load(Ordering::Acquire);
    if fl.is_null() || fr.is_null() {
        return;
    }
    let out_l = dsp_samples(fl, n);
    let out_r = dsp_samples(fr, n);
    let (Some(out_l), Some(out_r)) = (out_l, out_r) else {
        return;
    };
    let taps = shared
        .input_count
        .load(Ordering::Acquire)
        .min(MAX_TAP_PORTS);
    // Copy the port addresses first. The `&mut` from `dsp_samples` must not
    // still be live when the mixer borrows `out_l` / `out_r`.
    let mut ptr_l = [std::ptr::null::<f32>(); MAX_TAP_PORTS];
    let mut ptr_r = [std::ptr::null::<f32>(); MAX_TAP_PORTS];
    for index in 0..taps {
        let in_l = shared.inputs_fl[index].load(Ordering::Acquire);
        let in_r = shared.inputs_fr[index].load(Ordering::Acquire);
        ptr_l[index] = port_samples(in_l, n);
        ptr_r[index] = port_samples(in_r, n);
    }
    let mut inputs = [None; MAX_TAP_PORTS];
    for index in 0..taps {
        inputs[index] = Some(StripBlockInput {
            left: input_view(ptr_l[index], n as usize),
            right: input_view(ptr_r[index], n as usize),
        });
    }
    mix_resolved_strips(
        shared,
        &inputs[..taps],
        out_l,
        out_r,
        n,
        position.clock.rate.num,
        position.clock.rate.denom,
    );

    let frames = (n as usize).min(MIX_SLOT_CAP);
    shared.slot.write(&out_l[..frames], &out_r[..frames]);
}

/// Strip mix, master chain, optional tone, then the master fader, then the master meter.
///
/// `render` calls this after the port pointers are resolved. The default
/// chain is the compressor and then the limiter. The limiter is on at 5 ms,
/// so a short block comes out as the delay line, not the current input. Tone
/// and the master fader run after that chain. The mixer cell is the data
/// loop's, or a single-threaded test's.
fn mix_resolved_strips(
    shared: &ProcessShared,
    inputs: &[Option<StripBlockInput<'_>>],
    out_l: &mut [f32],
    out_r: &mut [f32],
    n: u32,
    num: u32,
    denom: u32,
) {
    let rate = negotiated_rate(num, denom);
    shared.observed_rate.store(rate, Ordering::Relaxed);
    // Safety: `RtMixer`'s Sync bound is the data-loop mutator. This call is
    // that mutator, or a test that does not share the cell.
    let mixer = unsafe { &mut *shared.mixer.get() };
    // Safety: same data-loop mutator as the mixer cell.
    let scopes = unsafe { &mut *shared.scopes.writers() };
    // SAFETY: one `&mut EqReader`, one `&mut CompReader`, one `&mut LimitReader`, and one `&mut ChainReader` for this callback. The cells are not borrowed again until `process` returns. They do not alias `strips` or the meters.
    let eq = unsafe { &mut *shared.eq.reader_ptr() };
    let comp = unsafe { &mut *shared.comp.reader_ptr() };
    let limit = unsafe { &mut *shared.limit.reader_ptr() };
    let chain = unsafe { &mut *shared.chain.reader_ptr() };
    mixer.load_chain(chain);
    mixer.process(
        &shared.strips,
        eq,
        comp,
        limit,
        &shared.strip_meters,
        scopes,
        inputs,
        out_l,
        out_r,
        rate,
    );

    if shared.tone.load(Ordering::Acquire) {
        write_tone(shared, n, num, denom, out_l, out_r);
    }

    // Unity unless the loop thread published a fader. No hidden mic boost.
    let gain = shared.params.master_linear();
    if gain != 1.0 {
        for sample in out_l.iter_mut().chain(out_r.iter_mut()) {
            *sample *= gain;
        }
    }

    shared.scopes.push_master(out_l, out_r);
    let (peak, rms) = omamix_core::meter::block_peak_rms(out_l, out_r);
    shared.meter.store(peak, rms);
}

/// One 440 Hz sample at amplitude 0.2, and the next phase in `0.0..1.0`.
///
/// Phase advances by one cycle increment per sample. It does not use the
/// graph clock position: that counter does not fit in `f32`, and casting it
/// turns the sine into a different pitch.
fn advance_tone(phase: f32, rate: u32) -> (f32, f32) {
    if !audio_rate(rate) {
        return (0.0, phase);
    }
    let sample = (phase * std::f32::consts::TAU).sin() * 0.2;
    let mut next = phase + (440.0 / rate as f32);
    if next >= 1.0 {
        next -= 1.0;
    }
    (sample, next)
}

fn audio_rate(rate: u32) -> bool {
    (8_000..384_000).contains(&rate)
}

pub(crate) fn negotiated_rate(num: u32, denom: u32) -> u32 {
    let num_ok = audio_rate(num);
    let denom_ok = audio_rate(denom);
    if num == 1 && denom_ok {
        denom
    } else if num_ok && (denom == 1 || num == denom) {
        num
    } else if denom_ok && !num_ok {
        denom
    } else if num_ok && !denom_ok {
        num
    } else {
        0
    }
}

fn write_tone(
    shared: &ProcessShared,
    n: u32,
    num: u32,
    denom: u32,
    out_l: &mut [f32],
    out_r: &mut [f32],
) {
    let rate = negotiated_rate(num, denom);
    let mut phase = f32::from_bits(shared.tone_phase.load(Ordering::Relaxed));
    if !phase.is_finite() || !(0.0..1.0).contains(&phase) {
        phase = 0.0;
    }
    for index in 0..n as usize {
        let (sample, next) = advance_tone(phase, rate);
        out_l[index] += sample;
        out_r[index] += sample;
        phase = next;
    }
    shared.tone_phase.store(phase.to_bits(), Ordering::Relaxed);
}

fn port_samples(port: *mut c_void, n: u32) -> *const f32 {
    match dsp_samples(port, n) {
        Some(samples) => samples.as_ptr(),
        None => std::ptr::null(),
    }
}

/// View a DSP buffer for this `process` call.
///
/// The lifetime is chosen by the caller. `ptr` stays valid until `process` returns.
fn input_view<'a>(ptr: *const f32, frames: usize) -> &'a [f32] {
    if ptr.is_null() || frames == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, frames) }
    }
}

fn dsp_samples<'a>(port: *mut c_void, n: u32) -> Option<&'a mut [f32]> {
    if port.is_null() || n == 0 {
        return None;
    }
    let ptr = unsafe { pw_filter_get_dsp_buffer(port, n) };
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts_mut(ptr as *mut f32, n as usize) })
    }
}

impl MixFilter {
    pub fn events_version(&self) -> u32 {
        self.events.version
    }
}

/// SPA Props for Mix Mic: linear 1.0 on both channels, unmuted.
///
/// The filter publishes this on connect. `process` does not apply it again.
fn unity_props_pod() -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::pod::{Object, Property, PropertyFlags, Value, ValueArray};
    use pw::spa::utils::Id;

    let object = Value::Object(Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: pw::spa::sys::SPA_PARAM_Props,
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PROP_volume,
                flags: PropertyFlags::empty(),
                value: Value::Float(1.0),
            },
            Property {
                key: pw::spa::sys::SPA_PROP_mute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(false),
            },
            Property {
                key: pw::spa::sys::SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(vec![1.0, 1.0])),
            },
            Property {
                key: pw::spa::sys::SPA_PROP_channelMap,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Id(vec![
                    Id(pw::spa::sys::SPA_AUDIO_CHANNEL_FL),
                    Id(pw::spa::sys::SPA_AUDIO_CHANNEL_FR),
                ])),
            },
            Property {
                key: pw::spa::sys::SPA_PROP_softMute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(false),
            },
            Property {
                key: pw::spa::sys::SPA_PROP_softVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(vec![1.0, 1.0])),
            },
        ],
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .expect("Mix Mic unity Props pod")
        .0
        .into_inner()
}

/// `SPA_PARAM_ProcessLatency` for the mix filter.
///
/// `rate` is the delay in samples. `ns` is 0. `spa_process_latency_info_add`
/// sums those fields, so a matching `ns` would make the graph count the
/// lookahead twice. `quantum` stays 0. `sample_rate` is the rate the graph
/// would use to convert `ns` into samples; it is not written into the pod.
/// The loop thread passes this pod to `pw_filter_update_params`.
pub(crate) fn process_latency_pod(samples: u32, sample_rate: u32) -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::pod::{Object, Property, PropertyFlags, Value};

    debug_assert!(sample_rate > 0 || samples == 0);
    let object = Value::Object(Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_ParamProcessLatency,
        id: pw::spa::sys::SPA_PARAM_ProcessLatency,
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PARAM_PROCESS_LATENCY_quantum,
                flags: PropertyFlags::empty(),
                value: Value::Float(0.0),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_PROCESS_LATENCY_rate,
                flags: PropertyFlags::empty(),
                value: Value::Int(i32::try_from(samples).unwrap_or(i32::MAX)),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_PROCESS_LATENCY_ns,
                flags: PropertyFlags::empty(),
                value: Value::Long(0),
            },
        ],
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .expect("ProcessLatency pod")
        .0
        .into_inner()
}

/// `SPA_PARAM_Latency` for one filter port.
///
/// `direction` is `SPA_DIRECTION_OUTPUT` or `SPA_DIRECTION_INPUT`. `rate` is
/// the lookahead in samples on both min and max. `ns` is 0, same as
/// [`process_latency_pod`], so the link does not add the delay twice.
pub(crate) fn port_latency_pod(samples: u32, direction: u32) -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::pod::{Object, Property, PropertyFlags, Value};
    use pw::spa::utils::Id;

    let rate = i32::try_from(samples).unwrap_or(i32::MAX);
    let object = Value::Object(Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_ParamLatency,
        id: pw::spa::sys::SPA_PARAM_Latency,
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_direction,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(direction)),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_minQuantum,
                flags: PropertyFlags::empty(),
                value: Value::Float(0.0),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_maxQuantum,
                flags: PropertyFlags::empty(),
                value: Value::Float(0.0),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_minRate,
                flags: PropertyFlags::empty(),
                value: Value::Int(rate),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_maxRate,
                flags: PropertyFlags::empty(),
                value: Value::Int(rate),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_minNs,
                flags: PropertyFlags::empty(),
                value: Value::Long(0),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_maxNs,
                flags: PropertyFlags::empty(),
                value: Value::Long(0),
            },
        ],
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .expect("Latency pod")
        .0
        .into_inner()
}

/// Direction and max rate from a `SPA_PARAM_Latency` pod.
///
/// Peer `ns` and quantum are ignored. Only the upstream rate is stored.
fn latency_direction_and_rate(bytes: &[u8]) -> Option<(u32, u32)> {
    use pw::spa::pod::Value;
    use pw::spa::pod::deserialize::PodDeserializer;

    let (_, value) = PodDeserializer::deserialize_any_from(bytes).ok()?;
    let Value::Object(object) = value else {
        return None;
    };
    if object.id != pw::spa::sys::SPA_PARAM_Latency {
        return None;
    }
    let mut direction = None;
    let mut max_rate = None;
    let mut min_rate = None;
    for prop in &object.properties {
        match prop.key {
            pw::spa::sys::SPA_PARAM_LATENCY_direction => {
                if let Value::Id(id) = &prop.value {
                    direction = Some(id.0);
                }
            }
            pw::spa::sys::SPA_PARAM_LATENCY_maxRate => {
                if let Value::Int(rate) = prop.value {
                    max_rate = Some(u32::try_from(rate).unwrap_or(0));
                }
            }
            pw::spa::sys::SPA_PARAM_LATENCY_minRate => {
                if let Value::Int(rate) = prop.value {
                    min_rate = Some(u32::try_from(rate).unwrap_or(0));
                }
            }
            _ => {}
        }
    }
    Some((direction?, max_rate.or(min_rate)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_props_advertise_full_scale_and_unmuted() {
        let bytes = unity_props_pod();
        let (_, value) =
            pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(&bytes).unwrap();
        let pw::spa::pod::Value::Object(object) = value else {
            panic!("Props pod is an object");
        };
        assert_eq!(object.id, pw::spa::sys::SPA_PARAM_Props);
        let mut volumes = false;
        let mut mute = false;
        for prop in &object.properties {
            if prop.key == pw::spa::sys::SPA_PROP_channelVolumes {
                match &prop.value {
                    pw::spa::pod::Value::ValueArray(pw::spa::pod::ValueArray::Float(samples)) => {
                        assert_eq!(samples.len(), 2);
                        assert!(samples.iter().all(|sample| (*sample - 1.0).abs() < 1e-6));
                    }
                    other => panic!("channelVolumes {other:?}"),
                }
                volumes = true;
            }
            if prop.key == pw::spa::sys::SPA_PROP_mute {
                assert_eq!(prop.value, pw::spa::pod::Value::Bool(false));
                mute = true;
            }
        }
        assert!(volumes && mute);
    }

    #[test]
    fn tone_phase_returns_to_zero_after_one_second_at_48k() {
        let mut phase = 0.0f32;
        let mut prev = 0.0f32;
        let mut rises = 0u32;
        for _ in 0..48_000 {
            let (sample, next) = super::advance_tone(phase, 48_000);
            if prev < 0.0 && sample >= 0.0 {
                rises += 1;
            }
            prev = sample;
            phase = next;
        }
        let away_from_zero = phase.min(1.0 - phase);
        assert!(away_from_zero < 0.02, "phase {phase}");
        // The rise at t=0 is the starting sample, and the 440th rise is the
        // first sample of the next second, outside this loop.
        assert_eq!(rises, 439);
    }

    #[test]
    fn a_failed_fr_add_clears_fl_before_the_port_is_freed() {
        let shared = ProcessShared::new();
        let fl = std::ptr::without_provenance_mut::<c_void>(0x1111);
        shared.inputs_fl[0].store(fl, Ordering::Release);
        shared.input_count.store(1, Ordering::Release);
        let mut job = ClearJob {
            shared: &shared,
            slot: 0,
            fl: ptr::null_mut(),
            fr: ptr::null_mut(),
        };
        let rc = unsafe {
            clear_tap_slot(
                ptr::null_mut(),
                false,
                0,
                ptr::null(),
                0,
                &mut job as *mut ClearJob as *mut c_void,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(job.fl, fl);
        assert!(job.fr.is_null());
        assert!(shared.inputs_fl[0].load(Ordering::Acquire).is_null());
        assert_eq!(shared.input_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn clear_tap_slot_nulls_pointers_before_the_port_is_freed() {
        let shared = ProcessShared::new();
        let keep = std::ptr::without_provenance_mut::<c_void>(0x10);
        shared.inputs_fl[0].store(keep, Ordering::Release);
        shared.inputs_fr[0].store(keep, Ordering::Release);
        shared.inputs_fl[1].store(keep, Ordering::Release);
        shared.inputs_fr[1].store(keep, Ordering::Release);
        let fl = std::ptr::without_provenance_mut::<c_void>(0x1000);
        let fr = std::ptr::without_provenance_mut::<c_void>(0x2000);
        shared.inputs_fl[2].store(fl, Ordering::Release);
        shared.inputs_fr[2].store(fr, Ordering::Release);
        shared.input_count.store(3, Ordering::Release);
        let mut job = ClearJob {
            shared: &shared,
            slot: 2,
            fl: ptr::null_mut(),
            fr: ptr::null_mut(),
        };
        let rc = unsafe {
            clear_tap_slot(
                ptr::null_mut(),
                false,
                0,
                ptr::null(),
                0,
                (&mut job as *mut ClearJob).cast(),
            )
        };
        assert_eq!(rc, 0);
        assert!(shared.inputs_fl[2].load(Ordering::Acquire).is_null());
        assert!(shared.inputs_fr[2].load(Ordering::Acquire).is_null());
        assert_eq!(job.fl, fl);
        assert_eq!(job.fr, fr);
        assert_eq!(shared.inputs_fl[0].load(Ordering::Acquire), keep);
        assert_eq!(shared.inputs_fr[1].load(Ordering::Acquire), keep);
        assert_eq!(shared.input_count.load(Ordering::Acquire), 2);
    }

    #[test]
    fn default_strip_mix_still_delays_when_the_publication_rate_differs() {
        let (shared, _eq, _comp, limit, _chain) = ProcessShared::pair();
        assert!(!shared.tone.load(Ordering::Acquire));
        assert_eq!(shared.params.master_linear(), 1.0);
        assert!(limit.master_settings().enabled());
        assert_eq!(limit.sample_rate(), 48_000);
        let samples_l = [0.1f32, -0.25, 0.5, 1.0, -1.0, -0.0, 0.70710677, -0.003];
        let samples_r = [-0.1f32, 0.25, -0.5, 0.125, 0.5, -1.0, 0.0, 0.003];
        let inputs = [Some(StripBlockInput {
            left: &samples_l,
            right: &samples_r,
        })];
        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        mix_resolved_strips(&shared, &inputs, &mut left, &mut right, 8, 44_100, 1);
        assert!(left.iter().chain(right.iter()).all(|sample| *sample == 0.0));
    }

    #[test]
    fn default_strip_mix_delays_when_the_rate_matches_the_master_limiter() {
        let (shared, _eq, _comp, limit, _chain) = ProcessShared::pair();
        let delay = limit.master_latency_samples() as usize;
        assert_eq!(
            delay,
            omamix_core::limit::lookahead_samples(48_000, 5.0) as usize
        );
        let quiet_l = [0.1f32, -0.2, 0.05, 0.15, -0.15, -0.0, 0.08, -0.003];
        let quiet_r = [-0.1f32, 0.2, -0.05, 0.12, 0.05, -0.08, 0.0, 0.003];
        assert_eq!(quiet_l[5].to_bits(), (-0.0f32).to_bits());
        let inputs = [Some(StripBlockInput {
            left: &quiet_l,
            right: &quiet_r,
        })];
        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        mix_resolved_strips(&shared, &inputs, &mut left, &mut right, 8, 48_000, 1);
        assert!(left.iter().chain(right.iter()).all(|sample| *sample == 0.0));
    }

    #[test]
    fn master_lookahead_returns_the_quiet_block_after_the_delay() {
        let (shared, _eq, _comp, limit, _chain) = ProcessShared::pair();
        let delay = limit.master_latency_samples() as usize;
        let quiet_l = [0.1f32, -0.2, 0.05, 0.15, -0.15, -0.0, 0.08, -0.003];
        let quiet_r = [-0.1f32, 0.2, -0.05, 0.12, 0.05, -0.08, 0.0, 0.003];
        let mut expect_l = quiet_l;
        expect_l[5] = 0.0;
        let frames = delay + quiet_l.len();
        let mut src_l = vec![0.0f32; frames];
        let mut src_r = vec![0.0f32; frames];
        src_l[..quiet_l.len()].copy_from_slice(&quiet_l);
        src_r[..quiet_r.len()].copy_from_slice(&quiet_r);
        let long = [Some(StripBlockInput {
            left: src_l.as_slice(),
            right: src_r.as_slice(),
        })];
        let mut out_l = vec![0.0f32; frames];
        let mut out_r = vec![0.0f32; frames];
        mix_resolved_strips(
            &shared,
            &long,
            &mut out_l,
            &mut out_r,
            frames as u32,
            48_000,
            1,
        );
        assert!(out_l[..delay].iter().all(|sample| *sample == 0.0));
        assert!(out_r[..delay].iter().all(|sample| *sample == 0.0));
        assert_eq!(&out_l[delay..delay + quiet_l.len()], &expect_l);
        assert_eq!(&out_r[delay..delay + quiet_r.len()], &quiet_r);
        assert_eq!(out_l[delay + 5].to_bits(), 0.0f32.to_bits());
    }

    #[test]
    fn claiming_a_tap_slot_resets_inherited_strip_params() {
        use omamix_core::params::StripParamUpdate;
        let (shared, mut eq, mut comp, _limit, _chain) = ProcessShared::pair();
        assert!(shared.strips.apply(3, StripParamUpdate::Mute(true)));
        assert!(shared.strips.apply(3, StripParamUpdate::Solo(true)));
        assert!(shared.strips.apply(3, StripParamUpdate::Enabled(false)));
        assert!(shared.strips.apply(3, StripParamUpdate::Pan(0.4)));
        assert!(shared.strips.apply(3, StripParamUpdate::FaderDb(-12.0)));
        assert!(shared.strips.apply(3, StripParamUpdate::TrimDb(6.0)));
        assert!(eq.apply(
            3,
            EqUpdate::Band {
                index: 4,
                gain_db: 6.0,
            },
        ));
        assert!(comp.apply(3, CompUpdate::Enabled(true)));
        assert!(comp.apply(3, CompUpdate::MakeupDb(6.0)));
        prepare_tap_slot(&shared, &mut eq, &mut comp, 3);
        assert!(!shared.strips.mute(3));
        assert!(!shared.strips.solo(3));
        assert!(shared.strips.enabled(3));
        assert_eq!(shared.strips.pan(3), 0.0);
        assert_eq!(shared.strips.fader_db(3), 0.0);
        assert_eq!(shared.strips.trim_db(3), 0.0);
        assert!((shared.strips.target_gain(3, false) - 1.0).abs() < 1.0e-6);
        let stored = eq.settings(3).expect("slot 3");
        assert!(!stored.hpf_enabled());
        assert_eq!(stored.band_db(), [0.0; omamix_core::eq::EQ_BANDS]);
        let stored_comp = comp.settings(3).expect("slot 3");
        assert!(!stored_comp.enabled());
        assert_eq!(stored_comp.makeup_db(), 0.0);
        assert_eq!(comp.gain_reduction_db(3), 0.0);
    }

    #[test]
    fn clear_tap_slot_resets_strip_params_with_the_pointers() {
        use omamix_core::params::StripParamUpdate;
        let shared = ProcessShared::new();
        let fl = std::ptr::without_provenance_mut::<c_void>(0x31);
        let fr = std::ptr::without_provenance_mut::<c_void>(0x32);
        shared.inputs_fl[1].store(fl, Ordering::Release);
        shared.inputs_fr[1].store(fr, Ordering::Release);
        shared.input_count.store(2, Ordering::Release);
        assert!(shared.strips.apply(1, StripParamUpdate::Mute(true)));
        assert!(shared.strips.apply(1, StripParamUpdate::Pan(-0.2)));
        shared.strip_meters.store(1, -6.0, -12.0);
        let mut job = ClearJob {
            shared: &shared,
            slot: 1,
            fl: ptr::null_mut(),
            fr: ptr::null_mut(),
        };
        let rc = unsafe {
            clear_tap_slot(
                ptr::null_mut(),
                false,
                0,
                ptr::null(),
                0,
                (&mut job as *mut ClearJob).cast(),
            )
        };
        assert_eq!(rc, 0);
        assert!(shared.inputs_fl[1].load(Ordering::Acquire).is_null());
        assert!(!shared.strips.mute(1));
        assert_eq!(shared.strips.pan(1), 0.0);
        assert!(shared.strips.enabled(1));
        let (peak, rms) = shared.strip_meters.load(1);
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert!(rms.is_infinite() && rms.is_sign_negative());
    }

    #[test]
    fn add_tap_ports_resets_the_claimed_slot_to_neutral() {
        use omamix_core::params::StripParamUpdate;
        let mut filter = MixFilter::without_filter();
        let occupied = std::ptr::without_provenance_mut::<c_void>(0x51);
        filter.shared.inputs_fl[0].store(occupied, Ordering::Release);
        filter.shared.inputs_fr[0].store(occupied, Ordering::Release);
        assert!(filter.shared.strips.apply(0, StripParamUpdate::Mute(true)));
        filter.shared.strip_meters.store(0, -6.0, -12.0);
        assert!(filter.shared.strips.apply(1, StripParamUpdate::Mute(true)));
        assert!(filter.shared.strips.apply(1, StripParamUpdate::Solo(true)));
        assert!(
            filter
                .shared
                .strips
                .apply(1, StripParamUpdate::Enabled(false))
        );
        assert!(filter.shared.strips.apply(1, StripParamUpdate::Pan(0.4)));
        assert!(
            filter
                .shared
                .strips
                .apply(1, StripParamUpdate::FaderDb(-12.0))
        );
        assert!(filter.shared.strips.apply(1, StripParamUpdate::TrimDb(6.0)));
        filter.shared.strip_meters.store(1, -6.0, -12.0);
        let baked = [0.5f32, -0.25, 0.125];
        assert_eq!(filter.shared.scopes.push_strip(1, &baked, &baked), 0);

        assert!(filter.add_tap_ports(9).is_err());

        assert_eq!(filter.shared.inputs_fl[0].load(Ordering::Acquire), occupied);
        assert!(filter.shared.strips.mute(0));
        let (kept_peak, _) = filter.shared.strip_meters.load(0);
        assert!((kept_peak - (-6.0)).abs() < 1.0e-4);
        assert!(filter.shared.inputs_fl[1].load(Ordering::Acquire).is_null());
        assert!(filter.shared.inputs_fr[1].load(Ordering::Acquire).is_null());
        assert!(!filter.shared.strips.mute(1));
        assert!(!filter.shared.strips.solo(1));
        assert!(filter.shared.strips.enabled(1));
        assert_eq!(filter.shared.strips.pan(1), 0.0);
        assert_eq!(filter.shared.strips.fader_db(1), 0.0);
        assert_eq!(filter.shared.strips.trim_db(1), 0.0);
        let (peak, rms) = filter.shared.strip_meters.load(1);
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert!(rms.is_infinite() && rms.is_sign_negative());
        let mut scope_l = [1.0f32; 3];
        let mut scope_r = [1.0f32; 3];
        assert_eq!(
            filter
                .shared
                .scopes
                .pop_strip(1, &mut scope_l, &mut scope_r),
            0
        );
        assert_eq!(scope_l, [1.0; 3]);
        assert_eq!(scope_r, [1.0; 3]);
    }

    fn assert_minus_20_sine(peak: f32, rms: f32) {
        assert!((peak - (-20.0)).abs() <= 0.1, "peak {peak}");
        let analytic = -20.0 - 10.0 * 2.0f32.log10();
        assert!(
            (rms - analytic).abs() <= 0.1,
            "rms {rms} analytic {analytic}"
        );
        assert!((rms - (-23.0)).abs() <= 0.1, "rms {rms}");
    }

    fn minus_20_sine(frames: usize) -> Vec<f32> {
        let amp = omamix_core::db_to_linear(-20.0);
        (0..frames)
            .map(|index| {
                let phase = 10.0 * index as f32 / frames as f32;
                (phase * std::f32::consts::TAU).sin() * amp
            })
            .collect()
    }

    #[test]
    fn master_and_strip_meters_read_a_minus_20_dbfs_sine() {
        let shared = ProcessShared::new();
        assert!(!shared.tone.load(Ordering::Acquire));
        assert_eq!(shared.params.master_linear(), 1.0);
        let frames = 480;
        let sine = minus_20_sine(frames);
        let inputs = [Some(StripBlockInput {
            left: sine.as_slice(),
            right: sine.as_slice(),
        })];
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        mix_resolved_strips(
            &shared,
            &inputs,
            &mut left,
            &mut right,
            frames as u32,
            48_000,
            1,
        );
        let mut drain_l = vec![0.0f32; frames];
        let mut drain_r = vec![0.0f32; frames];
        assert_eq!(
            shared.scopes.pop_strip(0, &mut drain_l, &mut drain_r),
            frames
        );
        assert_eq!(shared.scopes.pop_master(&mut drain_l, &mut drain_r), frames);
        mix_resolved_strips(
            &shared,
            &inputs,
            &mut left,
            &mut right,
            frames as u32,
            48_000,
            1,
        );
        let (strip_peak, strip_rms) = shared.strip_meters.load(0);
        assert_minus_20_sine(strip_peak, strip_rms);
        let (peak, rms) = shared.meter.load();
        assert_minus_20_sine(peak, rms);
        let mut scope_l = vec![0.0f32; frames];
        let mut scope_r = vec![0.0f32; frames];
        assert_eq!(
            shared.scopes.pop_strip(0, &mut scope_l, &mut scope_r),
            frames
        );
        assert_eq!(scope_l, sine);
        assert_eq!(shared.scopes.pop_master(&mut scope_l, &mut scope_r), frames);
        assert_eq!(shared.scopes.master_dropped_frames(), 0);
        for (got, want) in scope_l.iter().zip(sine.iter()) {
            assert!((got - want).abs() <= 1.0e-5, "master {got} sine {want}");
        }

        shared.params.set_master_db(-6.0);
        mix_resolved_strips(
            &shared,
            &inputs,
            &mut left,
            &mut right,
            frames as u32,
            48_000,
            1,
        );
        let (strip_peak, strip_rms) = shared.strip_meters.load(0);
        assert_minus_20_sine(strip_peak, strip_rms);
        let (peak, rms) = shared.meter.load();
        assert!((peak - (-26.0)).abs() <= 0.1, "master peak {peak}");
        assert!((rms - (-29.0)).abs() <= 0.1, "master rms {rms}");
    }

    #[test]
    fn tone_is_on_the_master_meter_and_not_on_the_strip() {
        let shared = ProcessShared::new();
        shared.tone.store(true, Ordering::Release);
        let frames = 2048;
        let zeros = vec![0.0f32; frames];
        let inputs = [Some(StripBlockInput {
            left: zeros.as_slice(),
            right: zeros.as_slice(),
        })];
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        mix_resolved_strips(
            &shared,
            &inputs,
            &mut left,
            &mut right,
            frames as u32,
            48_000,
            1,
        );
        let (strip_peak, strip_rms) = shared.strip_meters.load(0);
        assert!(strip_peak.is_infinite() && strip_peak.is_sign_negative());
        assert!(strip_rms.is_infinite() && strip_rms.is_sign_negative());
        let mut scope_l = vec![0.0f32; frames];
        let mut scope_r = vec![0.0f32; frames];
        assert_eq!(
            shared.scopes.pop_strip(0, &mut scope_l, &mut scope_r),
            frames
        );
        assert!(scope_l.iter().all(|sample| *sample == 0.0));
        assert_eq!(shared.scopes.pop_master(&mut scope_l, &mut scope_r), frames);
        assert!(scope_l.iter().any(|sample| sample.abs() > 0.1));
        let (peak, rms) = shared.meter.load();
        let tone_peak = omamix_core::linear_to_db(0.2);
        assert!(
            (peak - tone_peak).abs() <= 0.1,
            "tone peak {peak} expected {tone_peak}"
        );
        assert!(
            (rms - (tone_peak - 10.0 * 2.0f32.log10())).abs() <= 0.1,
            "tone rms {rms}"
        );
    }

    #[test]
    fn negotiated_rate_follows_44100_48000_and_96000() {
        for rate in [44_100, 48_000, 96_000] {
            assert_eq!(negotiated_rate(1, rate), rate);
            assert_eq!(negotiated_rate(rate, 1), rate);
            assert_eq!(negotiated_rate(rate, rate), rate);
        }
        assert_eq!(negotiated_rate(1, 1), 0);
        assert_eq!(negotiated_rate(0, 0), 0);
        assert_eq!(negotiated_rate(44_100, 48_000), 0);
        assert_eq!(negotiated_rate(96_000, 48_000), 0);
        let (sample, phase) = advance_tone(0.25, 0);
        assert_eq!(sample, 0.0);
        assert_eq!(phase, 0.25);
        let (sample, phase) = advance_tone(0.0, 44_100);
        assert!(sample.abs() < 1.0e-6);
        assert!((phase - (440.0 / 44_100.0)).abs() < 1.0e-6);
    }

    fn latency_range_pod(direction: u32, min_rate: Option<i32>, max_rate: Option<i32>) -> Vec<u8> {
        use pw::spa::pod::serialize::PodSerializer;
        use pw::spa::pod::{Object, Property, PropertyFlags, Value};
        use pw::spa::utils::Id;

        let mut properties = vec![Property {
            key: pw::spa::sys::SPA_PARAM_LATENCY_direction,
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(direction)),
        }];
        if let Some(rate) = min_rate {
            properties.push(Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_minRate,
                flags: PropertyFlags::empty(),
                value: Value::Int(rate),
            });
        }
        if let Some(rate) = max_rate {
            properties.push(Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_maxRate,
                flags: PropertyFlags::empty(),
                value: Value::Int(rate),
            });
        }
        let object = Value::Object(Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_ParamLatency,
            id: pw::spa::sys::SPA_PARAM_Latency,
            properties,
        });
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
            .expect("latency range pod")
            .0
            .into_inner()
    }

    #[test]
    fn latency_pairing_keeps_sample_counts_at_44100_48000_and_96000() {
        for rate in [44_100u32, 48_000, 96_000] {
            let samples = omamix_core::limit::lookahead_samples(rate, 5.0);
            assert!(samples > 0);
            let both = port_latency_pod(samples, pw::spa::sys::SPA_DIRECTION_OUTPUT);
            let (direction, got) = latency_direction_and_rate(&both).unwrap();
            assert_eq!(direction, pw::spa::sys::SPA_DIRECTION_OUTPUT);
            assert_eq!(got, samples);
            let min_only = latency_range_pod(
                pw::spa::sys::SPA_DIRECTION_OUTPUT,
                Some(i32::try_from(samples).unwrap()),
                None,
            );
            assert_eq!(latency_direction_and_rate(&min_only).unwrap().1, samples);
            let max_wins = latency_range_pod(
                pw::spa::sys::SPA_DIRECTION_OUTPUT,
                Some(1),
                Some(i32::try_from(samples).unwrap()),
            );
            assert_eq!(latency_direction_and_rate(&max_wins).unwrap().1, samples);
            assert_eq!(output_direction_latency(32, samples), 32 + samples);
            let missing = latency_range_pod(pw::spa::sys::SPA_DIRECTION_OUTPUT, None, None);
            assert!(latency_direction_and_rate(&missing).is_none());
        }
    }

    #[test]
    fn reported_latency_is_the_master_lookahead() {
        let filter = MixFilter::without_filter();
        assert_eq!(
            filter.reported_latency_samples(),
            omamix_core::limit::lookahead_samples(48_000, 5.0)
        );
    }

    #[test]
    fn process_latency_pod_is_the_lookahead() {
        let samples = omamix_core::limit::lookahead_samples(48_000, 5.0);
        assert_eq!(samples, 240);
        let bytes = process_latency_pod(samples, 48_000);
        let (_, value) =
            pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(&bytes).unwrap();
        let pw::spa::pod::Value::Object(object) = value else {
            panic!("ProcessLatency pod is an object");
        };
        assert_eq!(
            object.type_,
            pw::spa::sys::SPA_TYPE_OBJECT_ParamProcessLatency
        );
        assert_eq!(object.id, pw::spa::sys::SPA_PARAM_ProcessLatency);
        let mut rate = false;
        let mut ns = false;
        let mut quantum = false;
        for prop in &object.properties {
            match prop.key {
                pw::spa::sys::SPA_PARAM_PROCESS_LATENCY_rate => {
                    assert_eq!(prop.value, pw::spa::pod::Value::Int(240));
                    rate = true;
                }
                pw::spa::sys::SPA_PARAM_PROCESS_LATENCY_ns => {
                    assert_eq!(prop.value, pw::spa::pod::Value::Long(0));
                    ns = true;
                }
                pw::spa::sys::SPA_PARAM_PROCESS_LATENCY_quantum => {
                    assert_eq!(prop.value, pw::spa::pod::Value::Float(0.0));
                    quantum = true;
                }
                _ => {}
            }
        }
        assert!(rate && ns && quantum);
        assert_eq!(
            summed_process_latency_samples(0.0, 240, 0, 48_000),
            i64::from(samples)
        );
        let off = process_latency_pod(0, 48_000);
        let (_, off_value) =
            pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(&off).unwrap();
        let pw::spa::pod::Value::Object(off_object) = off_value else {
            panic!("zero ProcessLatency pod is an object");
        };
        let zero = off_object.properties.iter().any(|prop| {
            prop.key == pw::spa::sys::SPA_PARAM_PROCESS_LATENCY_rate
                && prop.value == pw::spa::pod::Value::Int(0)
        });
        assert!(zero);
        assert_eq!(summed_process_latency_samples(0.0, 0, 0, 48_000), 0);

        let port = port_latency_pod(samples, pw::spa::sys::SPA_DIRECTION_OUTPUT);
        let (_, port_value) =
            pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(&port).unwrap();
        let pw::spa::pod::Value::Object(port_object) = port_value else {
            panic!("port Latency pod is an object");
        };
        assert_eq!(
            port_object.type_,
            pw::spa::sys::SPA_TYPE_OBJECT_ParamLatency
        );
        assert_eq!(port_object.id, pw::spa::sys::SPA_PARAM_Latency);
        let mut saw_direction = false;
        let mut saw_rate = false;
        let mut saw_ns = false;
        for prop in &port_object.properties {
            match prop.key {
                pw::spa::sys::SPA_PARAM_LATENCY_direction => {
                    assert_eq!(
                        prop.value,
                        pw::spa::pod::Value::Id(pw::spa::utils::Id(
                            pw::spa::sys::SPA_DIRECTION_OUTPUT
                        ))
                    );
                    saw_direction = true;
                }
                pw::spa::sys::SPA_PARAM_LATENCY_minRate
                | pw::spa::sys::SPA_PARAM_LATENCY_maxRate => {
                    assert_eq!(prop.value, pw::spa::pod::Value::Int(240));
                    saw_rate = true;
                }
                pw::spa::sys::SPA_PARAM_LATENCY_minNs | pw::spa::sys::SPA_PARAM_LATENCY_maxNs => {
                    assert_eq!(prop.value, pw::spa::pod::Value::Long(0));
                    saw_ns = true;
                }
                pw::spa::sys::SPA_PARAM_LATENCY_minQuantum
                | pw::spa::sys::SPA_PARAM_LATENCY_maxQuantum => {
                    assert_eq!(prop.value, pw::spa::pod::Value::Float(0.0));
                }
                _ => {}
            }
        }
        assert!(saw_direction && saw_rate && saw_ns);
    }

    /// Samples after `spa_process_latency_info_add`.
    ///
    /// `rate` is already samples. `ns` converts at `sample_rate`. `quantum`
    /// is a fraction of the buffer; 0 contributes no samples.
    fn summed_process_latency_samples(quantum: f32, rate: i32, ns: i64, sample_rate: u32) -> i64 {
        assert_eq!(quantum, 0.0);
        let from_ns = i128::from(ns) * i128::from(sample_rate) / 1_000_000_000;
        i64::from(rate) + i64::try_from(from_ns).unwrap_or(i64::MAX)
    }

    #[test]
    fn lookahead_change_while_linked_keeps_upstream_plus_own() {
        let mut filter = MixFilter::without_filter();
        let port = std::ptr::without_provenance_mut::<c_void>(0x1264);
        let other = std::ptr::without_provenance_mut::<c_void>(0x1265);
        filter.shared.inputs_fl[0].store(port, Ordering::Release);
        filter.shared.inputs_fr[0].store(other, Ordering::Release);
        filter.shared.own_latency.store(240, Ordering::Release);
        let peer = port_latency_pod(1024, pw::spa::sys::SPA_DIRECTION_OUTPUT);
        // SAFETY: `data` is the live ProcessShared in `filter.shared`. `peer` is a complete latency pod that outlives the call. The callback only reads that pod and stores an atomic rate.
        unsafe {
            on_param_changed(
                Arc::as_ptr(&filter.shared) as *mut c_void,
                port,
                pw::spa::sys::SPA_PARAM_Latency,
                peer.as_ptr() as *const c_void,
            );
        }
        assert_eq!(filter.shared.upstream_rate.load(Ordering::Acquire), 1024);
        assert_eq!(output_direction_latency(1024, 240), 1264);
        let quieter = port_latency_pod(100, pw::spa::sys::SPA_DIRECTION_OUTPUT);
        // SAFETY: `data` is the live ProcessShared in `filter.shared`. `quieter` is a complete latency pod that outlives the call. The callback only reads that pod and stores an atomic rate.
        unsafe {
            on_param_changed(
                Arc::as_ptr(&filter.shared) as *mut c_void,
                other,
                pw::spa::sys::SPA_PARAM_Latency,
                quieter.as_ptr() as *const c_void,
            );
        }
        assert_eq!(filter.shared.upstream_rate.load(Ordering::Acquire), 1024);
        let inbound = port_latency_pod(240, pw::spa::sys::SPA_DIRECTION_INPUT);
        // SAFETY: `data` is the live ProcessShared in `filter.shared`. `inbound` is a complete latency pod that outlives the call. The callback only reads that pod and stores an atomic rate.
        unsafe {
            on_param_changed(
                Arc::as_ptr(&filter.shared) as *mut c_void,
                port,
                pw::spa::sys::SPA_PARAM_Latency,
                inbound.as_ptr() as *const c_void,
            );
        }
        assert_eq!(filter.shared.upstream_rate.load(Ordering::Acquire), 1024);
        assert!(filter.apply_master_limit(omamix_core::limit::LimitUpdate::LookaheadMs(20.0)));
        assert_eq!(filter.reported_latency_samples(), 960);
        assert_eq!(filter.output_latency_written, Some(1024 + 960));
        assert_eq!(filter.input_latency_written, Some(960));
        assert!(filter.apply_master_limit(omamix_core::limit::LimitUpdate::LookaheadMs(5.0)));
        assert_eq!(filter.reported_latency_samples(), 240);
        assert_eq!(filter.output_latency_written, Some(1264));
        assert_eq!(filter.input_latency_written, Some(240));
        assert_eq!(output_direction_latency(1024, 0), 1024);
        assert_eq!(output_direction_latency(1024, 480), 1504);
    }

    #[test]
    fn removing_a_disabled_tap_clears_its_upstream_latency() {
        let mut filter = MixFilter::without_filter();
        let port = std::ptr::without_provenance_mut::<c_void>(0x2401);
        filter.shared.inputs_fl[1].store(port, Ordering::Release);
        filter.shared.upstream_fl[1].store(1024, Ordering::Release);
        filter.shared.recompute_upstream();
        assert_eq!(filter.shared.upstream_rate.load(Ordering::Acquire), 1024);
        let own = filter.reported_latency_samples();
        assert_eq!(own, 240);
        filter.note_process_latency();
        assert_eq!(filter.output_latency_written, Some(1024 + own));
        assert_eq!(filter.input_latency_written, Some(own));
        filter.remove_tap_ports(1);
        assert_eq!(filter.shared.upstream_rate.load(Ordering::Acquire), 0);
        assert_eq!(filter.reported_latency_samples(), own);
        assert_eq!(filter.output_latency_written, Some(own));
        assert_eq!(filter.input_latency_written, Some(own));
    }

    #[test]
    fn unchanged_port_latency_is_not_rewritten() {
        let mut filter = MixFilter::without_filter();
        filter.note_process_latency();
        let writes = filter.port_latency_writes;
        assert_eq!(writes, 1);
        filter.note_process_latency();
        assert_eq!(filter.port_latency_writes, writes);
        let port = std::ptr::without_provenance_mut::<c_void>(0x2402);
        filter.shared.inputs_fl[0].store(port, Ordering::Release);
        filter.shared.upstream_fl[0].store(1024, Ordering::Release);
        filter.shared.recompute_upstream();
        filter.note_process_latency();
        assert!(filter.port_latency_writes > writes);
        assert_eq!(filter.output_latency_written, Some(1024 + 240));
    }

    #[test]
    fn adding_a_strip_writes_port_latency_when_the_counts_stay_put() {
        let mut filter = MixFilter::without_filter();
        filter.note_process_latency();
        let writes = filter.port_latency_writes;
        assert_eq!(writes, 1);
        let own = filter.reported_latency_samples();
        let fl = std::ptr::without_provenance_mut::<c_void>(0x2403);
        let fr = std::ptr::without_provenance_mut::<c_void>(0x2404);
        filter.store_input_port(0, true, fl);
        filter.store_input_port(0, false, fr);
        filter.note_process_latency();
        assert!(filter.port_latency_writes > writes);
        assert_eq!(filter.input_latency_written, Some(own));
        assert_eq!(filter.output_latency_written, Some(own));
    }

    #[test]
    fn master_cap_demotion_is_logged_on_the_loop_thread() {
        let mut filter = MixFilter::without_filter();
        assert!(filter.limit.set_processing_quantum(1));
        assert!(filter.demotion_lines.is_empty());
        assert!(filter.apply_master_limit(omamix_core::limit::LimitUpdate::LookaheadMs(0.0)));
        assert!(filter.demotion_lines.iter().any(|line| {
            line.contains("limiter master") && line.contains("stored 5 ms lookahead")
        }));
        assert_eq!(filter.limit.master_settings().lookahead_ms(), 5.0);
        assert!(filter.limit.master_cap_demoted());
    }

    fn five_ms_samples(rate: u32) -> u32 {
        match rate {
            44_100 => 221,
            48_000 => 240,
            96_000 => 480,
            _ => panic!("unexpected rate {rate}"),
        }
    }

    fn expected_applied_delay(rate: u32, quantum: u32, ms: f32) -> u32 {
        let five_ms = five_ms_samples(rate);
        assert_eq!(
            omamix_core::limit::lookahead_samples(
                rate,
                omamix_core::limit::LIMIT_LOOKAHEAD_DEFAULT_MS
            ),
            five_ms
        );
        if ms == 0.0 && omamix_core::limit::zero_ms_limiter_cap(rate, quantum) == 0 {
            five_ms
        } else if ms == 0.0 {
            0
        } else {
            five_ms
        }
    }

    fn impulse_delay(shared: &ProcessShared, rate: u32, quantum: u32) -> u32 {
        let quantum = quantum.max(1) as usize;
        let room = omamix_core::limit::lookahead_samples(
            rate,
            omamix_core::limit::LIMIT_LOOKAHEAD_DEFAULT_MS,
        ) as usize
            + 1;
        let frames = room.div_ceil(quantum) * quantum;
        let mut src_l = vec![0.0f32; frames];
        let mut src_r = vec![0.0f32; frames];
        src_l[0] = 0.05;
        src_r[0] = 0.05;
        let mut out_l = vec![0.0f32; frames];
        let mut out_r = vec![0.0f32; frames];
        let mut offset = 0;
        while offset < frames {
            let end = offset + quantum;
            let inputs = [Some(StripBlockInput {
                left: &src_l[offset..end],
                right: &src_r[offset..end],
            })];
            mix_resolved_strips(
                shared,
                &inputs,
                &mut out_l[offset..end],
                &mut out_r[offset..end],
                (end - offset) as u32,
                rate,
                1,
            );
            offset = end;
        }
        out_l
            .iter()
            .position(|sample| sample.abs() > 0.02)
            .unwrap_or(frames) as u32
    }

    #[test]
    fn published_process_and_port_latency_match_the_applied_delay() {
        for rate in [44_100u32, 48_000, 96_000] {
            for quantum in [1u32, 64, 256, 1024] {
                for ms in [0.0f32, omamix_core::limit::LIMIT_LOOKAHEAD_DEFAULT_MS] {
                    let mut filter = MixFilter::without_filter();
                    assert!(
                        filter.apply_master_limit(omamix_core::limit::LimitUpdate::LookaheadMs(ms))
                    );
                    let applied = impulse_delay(&filter.shared(), rate, quantum);
                    let expect = expected_applied_delay(rate, quantum, ms);
                    assert_eq!(
                        applied, expect,
                        "applied delay at {rate} Hz, quantum {quantum}, lookahead {ms} ms"
                    );
                    assert_eq!(filter.reported_latency_samples(), applied);
                    assert!(filter.set_limit_rate(rate));
                    filter.sync_limit_quantum();
                    assert_eq!(filter.reported_latency_samples(), applied);
                    assert_eq!(filter.input_latency_written, Some(applied));
                    assert_eq!(filter.output_latency_written, Some(applied));
                    assert_eq!(
                        filter.latency_queued.map(|(samples, _)| samples),
                        Some(applied)
                    );
                }
            }
        }
    }
}

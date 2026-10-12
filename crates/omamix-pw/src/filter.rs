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

use omamix_core::params::{StripBlockInput, StripMeterBank, StripMixer, StripParamBank};
use pipewire as pw;
use pw::properties::PropertiesBox;
use pw::sys as pw_sys;

use crate::rt::{AtomicMeter, AtomicParams, MIX_SLOT_CAP, MixSlot};

const PW_FILTER_FLAG_RT_PROCESS: u32 = 1 << 2;
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

/// Shared with `process()`. The loop thread writes params and port pointers.
/// `process()` writes the meters and the monitor slot. No channel receive.
pub struct ProcessShared {
    pub params: AtomicParams,
    /// Per-strip trim, fader, pan, mute, solo, and enable. The loop thread writes.
    pub strips: StripParamBank,
    /// Smoother state. Only the data loop (`process` and `clear_tap_slot`) touches it.
    mixer: RtMixer,
    /// Master block peak and RMS, after the master fader.
    pub meter: AtomicMeter,
    /// Per-strip block peak and RMS, post-fader, before the master fader.
    pub strip_meters: StripMeterBank,
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
}

impl ProcessShared {
    fn new() -> Self {
        Self {
            params: AtomicParams::new(),
            strips: StripParamBank::new(),
            mixer: RtMixer::new(48_000),
            meter: AtomicMeter::new(),
            strip_meters: StripMeterBank::new(),
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
        }
    }
}

pub struct MixFilter {
    raw: NonNull<c_void>,
    events: Box<PwFilterEvents>,
    hook: Box<pw::spa::sys::spa_hook>,
    shared: Arc<ProcessShared>,
    destroyed: bool,
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
        let shared = Arc::new(ProcessShared::new());
        shared.tone.store(tone, Ordering::Release);
        let raw = unsafe { pw_filter_new(core.as_raw_ptr(), name.as_ptr(), props.into_raw()) };
        let raw = NonNull::new(raw).ok_or(pw::Error::CreationFailed)?;
        shared.filter.store(raw.as_ptr(), Ordering::Release);

        let events = Box::new(PwFilterEvents {
            version: PW_VERSION_FILTER_EVENTS,
            destroy: None,
            state_changed: Some(on_state),
            io_changed: None,
            param_changed: None,
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
            destroyed: false,
            #[cfg(test)]
            reject_ports: false,
        };
        filter.add_output_ports()?;
        // Advertised Props. `process` writes the samples directly and does not
        // multiply by this gain; 1.0 is what `wpctl` reads for the node.
        // `pw_filter_connect` copies the pod.
        let props_bytes = unity_props_pod();
        let props_pod =
            pw::spa::pod::Pod::from_bytes(&props_bytes).ok_or(pw::Error::CreationFailed)?;
        let params = [props_pod.as_raw_ptr() as *const c_void];
        let rc = unsafe {
            pw_filter_connect(
                filter.raw.as_ptr(),
                PW_FILTER_FLAG_RT_PROCESS,
                params.as_ptr(),
                1,
            )
        };
        if rc < 0 {
            return Err(pw::Error::CreationFailed);
        }
        Ok(filter)
    }

    pub fn shared(&self) -> Arc<ProcessShared> {
        Arc::clone(&self.shared)
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
        // Neutral before either new port pointer is stored. `clear_tap_slot`
        // already resets on free; a control-thread write after that free must
        // not ride into the next input.
        prepare_tap_slot(&self.shared, slot);
        let fl = self.add_port(
            pw::spa::utils::Direction::Input,
            &format!("in-{peer}-FL"),
            "FL",
        )?;
        self.shared.inputs_fl[slot].store(fl, Ordering::Release);
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
        self.shared.inputs_fr[slot].store(fr, Ordering::Release);
        let count = self.shared.input_count.load(Ordering::Acquire);
        if slot >= count {
            self.shared.input_count.store(slot + 1, Ordering::Release);
        }
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
        self.shared.out_fl.store(fl, Ordering::Release);
        self.shared.out_fr.store(fr, Ordering::Release);
        Ok(())
    }

    /// No PipeWire filter. `add_tap_ports` resets the claimed slot, then the
    /// port add fails before any C call. Drop does not touch the daemon:
    /// `destroyed` is already set.
    #[cfg(test)]
    fn without_filter() -> Self {
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
            shared: Arc::new(ProcessShared::new()),
            destroyed: true,
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
    prepare_tap_slot(shared, slot);
    unsafe { (*shared.mixer.get()).reset_slot(slot) };
    0
}

/// Neutral strip parameters and a silent meter for a slot that is about to
/// be claimed or was just freed.
///
/// Atomics only. The smoother reset stays in `clear_tap_slot`, which runs on
/// the data loop. `add_tap_ports` calls this before either port pointer is stored.
fn prepare_tap_slot(shared: &ProcessShared, slot: usize) {
    shared.strips.reset_slot(slot);
    shared.strip_meters.reset_slot(slot);
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

/// Strip mix, optional tone, then the master fader, then the master meter.
///
/// `render` calls this after the port pointers are resolved. A default bank
/// (tone off, master gain 1, neutral strips) leaves a finite stereo block
/// unchanged, except a -0.0 sample, which is written as +0.0 because the sum
/// starts at +0.0. The mixer cell is the data loop's, or a single-threaded test's.
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
    // Safety: `RtMixer`'s Sync bound is the data-loop mutator. This call is
    // that mutator, or a test that does not share the cell.
    let mixer = unsafe { &mut *shared.mixer.get() };
    mixer.process(
        &shared.strips,
        &shared.strip_meters,
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

    let (peak, rms) = omamix_core::meter::block_peak_rms(out_l, out_r);
    shared.meter.store(peak, rms);
}

/// One 440 Hz sample at amplitude 0.2, and the next phase in `0.0..1.0`.
///
/// Phase advances by one cycle increment per sample. It does not use the
/// graph clock position: that counter does not fit in `f32`, and casting it
/// turns the sine into a different pitch.
fn advance_tone(phase: f32, rate: u32) -> (f32, f32) {
    let rate = if (8_000..384_000).contains(&rate) {
        rate
    } else {
        48_000
    };
    let sample = (phase * std::f32::consts::TAU).sin() * 0.2;
    let mut next = phase + (440.0 / rate as f32);
    if next >= 1.0 {
        next -= 1.0;
    }
    (sample, next)
}

/// `spa_io_clock.rate` is `1/samplerate` when PipeWire fills it that way
/// (`num` 1, `denom` 48000). Prefer a denominator in the audio range, then
/// the numerator, then 48 kHz.
pub(crate) fn negotiated_rate(num: u32, denom: u32) -> u32 {
    if (8_000..384_000).contains(&denom) {
        denom
    } else if (8_000..384_000).contains(&num) {
        num
    } else {
        48_000
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
    fn default_strip_mix_leaves_a_stereo_block_unchanged() {
        let shared = ProcessShared::new();
        assert!(!shared.tone.load(Ordering::Acquire));
        assert_eq!(shared.params.master_linear(), 1.0);
        // Index 5 is -0.0. The sum starts at +0.0, so it is stored as +0.0.
        let samples_l = [0.1f32, -0.25, 0.5, 1.0, -1.0, -0.0, 0.70710677, -0.003];
        let samples_r = [-0.1f32, 0.25, -0.5, 0.125, 0.5, -1.0, 0.0, 0.003];
        assert_eq!(samples_l[5].to_bits(), (-0.0f32).to_bits());
        let inputs = [Some(StripBlockInput {
            left: &samples_l,
            right: &samples_r,
        })];
        for rate in [44_100, 48_000] {
            let mut left = [0.0f32; 8];
            let mut right = [0.0f32; 8];
            mix_resolved_strips(&shared, &inputs, &mut left, &mut right, 8, rate, 1);
            assert_eq!(left, samples_l);
            assert_eq!(right, samples_r);
            assert_eq!(left[5].to_bits(), 0.0f32.to_bits());
            mix_resolved_strips(&shared, &inputs, &mut left, &mut right, 8, rate, 1);
            assert_eq!(left, samples_l);
            assert_eq!(right, samples_r);
            assert_eq!(left[5].to_bits(), 0.0f32.to_bits());
        }
    }

    #[test]
    fn claiming_a_tap_slot_resets_inherited_strip_params() {
        use omamix_core::params::StripParamUpdate;
        let shared = ProcessShared::new();
        assert!(shared.strips.apply(3, StripParamUpdate::Mute(true)));
        assert!(shared.strips.apply(3, StripParamUpdate::Solo(true)));
        assert!(shared.strips.apply(3, StripParamUpdate::Enabled(false)));
        assert!(shared.strips.apply(3, StripParamUpdate::Pan(0.4)));
        assert!(shared.strips.apply(3, StripParamUpdate::FaderDb(-12.0)));
        assert!(shared.strips.apply(3, StripParamUpdate::TrimDb(6.0)));
        prepare_tap_slot(&shared, 3);
        assert!(!shared.strips.mute(3));
        assert!(!shared.strips.solo(3));
        assert!(shared.strips.enabled(3));
        assert_eq!(shared.strips.pan(3), 0.0);
        assert_eq!(shared.strips.fader_db(3), 0.0);
        assert_eq!(shared.strips.trim_db(3), 0.0);
        assert!((shared.strips.target_gain(3, false) - 1.0).abs() < 1.0e-6);
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
        let (strip_peak, strip_rms) = shared.strip_meters.load(0);
        assert_minus_20_sine(strip_peak, strip_rms);
        let (peak, rms) = shared.meter.load();
        assert_minus_20_sine(peak, rms);

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
    fn negotiated_rate_uses_the_denominator_of_a_one_over_rate_clock() {
        assert_eq!(negotiated_rate(1, 48_000), 48_000);
        assert_eq!(negotiated_rate(48_000, 1), 48_000);
        assert_eq!(negotiated_rate(1, 1), 48_000);
    }
}

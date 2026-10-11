//! Internal `pw_filter` for the mix engine.
//!
//! pipewire-rs 0.10 does not wrap `pw_filter`. These declarations match
//! `pipewire/filter.h` on the 1.6 headers. The public Mix Mic node is an
//! adapter. This filter is `omamix.mix-engine`: one `process` callback.
//! Strip taps are input ports on it. Output ports feed the adapter.

use std::ffi::{CString, c_char, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

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

pub const MAX_TAP_PORTS: usize = 64;

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

/// Shared with `process()`. The loop thread writes params and port pointers.
/// `process()` writes the meter and the monitor slot. No channel receive.
pub struct ProcessShared {
    pub params: AtomicParams,
    pub meter: AtomicMeter,
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
            meter: AtomicMeter::new(),
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
        let fl = self.add_port(
            pw::spa::utils::Direction::Input,
            &format!("in-{peer}-FL"),
            "FL",
        )?;
        let fr = self.add_port(
            pw::spa::utils::Direction::Input,
            &format!("in-{peer}-FR"),
            "FR",
        )?;
        self.shared.inputs_fl[slot].store(fl, Ordering::Release);
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

    fn add_port(
        &mut self,
        direction: pw::spa::utils::Direction,
        name: &str,
        channel: &str,
    ) -> Result<*mut c_void, ()> {
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
    0
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
    out_l.fill(0.0);
    out_r.fill(0.0);

    let taps = shared
        .input_count
        .load(Ordering::Acquire)
        .min(MAX_TAP_PORTS);
    for index in 0..taps {
        let in_l = shared.inputs_fl[index].load(Ordering::Acquire);
        let in_r = shared.inputs_fr[index].load(Ordering::Acquire);
        if let Some(samples) = dsp_samples(in_l, n) {
            for (dst, src) in out_l.iter_mut().zip(samples.iter()) {
                *dst += *src;
            }
        }
        if let Some(samples) = dsp_samples(in_r, n) {
            for (dst, src) in out_r.iter_mut().zip(samples.iter()) {
                *dst += *src;
            }
        }
    }

    if shared.tone.load(Ordering::Acquire) {
        write_tone(
            shared,
            n,
            position.clock.rate.num,
            position.clock.rate.denom,
            out_l,
            out_r,
        );
    }

    // Unity unless the loop thread published a fader. No hidden mic boost.
    let gain = shared.params.master_linear();
    if gain != 1.0 {
        for sample in out_l.iter_mut().chain(out_r.iter_mut()) {
            *sample *= gain;
        }
    }

    let frames = (n as usize).min(MIX_SLOT_CAP);
    shared.slot.write(&out_l[..frames], &out_r[..frames]);

    let (peak, rms) = peak_rms(out_l, out_r);
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

fn peak_rms(left: &[f32], right: &[f32]) -> (f32, f32) {
    let mut peak = 0.0f32;
    let mut sum = 0.0f32;
    let mut count = 0.0f32;
    for sample in left.iter().chain(right.iter()) {
        if !sample.is_finite() {
            continue;
        }
        let abs = sample.abs();
        if abs > peak {
            peak = abs;
        }
        sum += sample * sample;
        count += 1.0;
    }
    let rms = if count == 0.0 {
        0.0
    } else {
        (sum / count).sqrt()
    };
    (
        omamix_core::linear_to_db(peak),
        omamix_core::linear_to_db(rms),
    )
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
    fn negotiated_rate_uses_the_denominator_of_a_one_over_rate_clock() {
        assert_eq!(negotiated_rate(1, 48_000), 48_000);
        assert_eq!(negotiated_rate(48_000, 1), 48_000);
        assert_eq!(negotiated_rate(1, 1), 48_000);
    }
}

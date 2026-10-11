//! `pw_filter` for Mix Mic.
//!
//! pipewire-rs 0.10 does not wrap `pw_filter`. These declarations match
//! `pipewire/filter.h` on the 1.6 headers. The filter node is Mix Mic: one
//! node, one `process` callback. Strip taps are input ports on it.

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
    fn pw_filter_disconnect(filter: *mut c_void) -> i32;
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
        let name = CString::new("Mix Mic").expect("name");
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

    /// One stereo tap. Returns the port userdata pointers `(fl, fr)`.
    pub fn add_tap_ports(&mut self, peer: u32) -> Result<(*mut c_void, *mut c_void), ()> {
        let count = self.shared.input_count.load(Ordering::Acquire);
        if count >= MAX_TAP_PORTS {
            return Err(());
        }
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
        self.shared.inputs_fl[count].store(fl, Ordering::Release);
        self.shared.inputs_fr[count].store(fr, Ordering::Release);
        self.shared.input_count.store(count + 1, Ordering::Release);
        Ok((fl, fr))
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
        let rate = position.clock.rate.num.max(1);
        let origin = position.clock.position;
        for index in 0..n as usize {
            let t = (origin + index as u64) as f32 / rate as f32;
            let sample = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
            out_l[index] += sample;
            out_r[index] += sample;
        }
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
}

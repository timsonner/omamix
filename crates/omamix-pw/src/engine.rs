//! PipeWire main loop on its own thread.
//!
//! Every PipeWire object lives on that thread. The control thread sends
//! [`LoopMsg`] through [`pipewire::channel`], which wakes the loop. A timer
//! on the same thread drains [`crate::rt::CommandQueue`]. `process()` does
//! neither: it writes the master [`crate::rt::AtomicMeter`], the per-strip
//! meters, and the scope rings, reads [`crate::rt::AtomicParams`], the per-strip
//! [`omamix_core::params::StripParamBank`], the published EQ coefficients, and
//! the published compressor block, writes each gain-reduction atomic, and
//! publishes the master quantum in [`crate::rt::MixSlot`].
//!
//! Mix Mic is an adapter node (`media.class` Audio/Source/Virtual,
//! `node.name` omamix.mix-mic) created with `support.null-audio-sink`. The
//! `pw_filter` is a separate internal node, `omamix.mix-engine`. Strip taps
//! are input ports on the filter. The filter's output ports link into the
//! adapter. Headphone sends and the monitor are separate `Stream/Output/Audio`
//! streams. They are not a second mixer. The monitor's `process` copies
//! [`crate::rt::MixSlot`], which is the master mix the filter just wrote. A
//! headphone send must copy that strip's own tap, not the master mix, because
//! the master includes the mic. This backend creates the monitor stream when
//! the scene's `monitor_mix` flag is set (`--monitor-mix`). It does not create
//! headphone streams: there is no per-strip slot yet, and playing MixSlot into
//! a headphone send would put the mic in the headphones. Neither stream sets
//! `target.object` or `AUTOCONNECT`, so neither attaches itself to a sink.
//!
//! The loop thread binds the `default` metadata object and listens to
//! `property` events. It does not write metadata. Mix Mic is created
//! only after `default.audio.source` has been read. If that read never
//! arrives, Mix Mic is not published. If the default source is Mix Mic, the
//! loop quits, which destroys the Mix Mic node and leaves the session default
//! where the session manager put it.
//!
//! Unit tests do not call [`run_headless`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omamix_core::comp::CompUpdate;
use omamix_core::effect::{EffectKind, MasterChain};
use omamix_core::eq::EqUpdate;
use omamix_core::graph::{MasterBus, Scene, Strip};
use omamix_core::limit::{
    LIMIT_CEILING_DEFAULT_DB, LIMIT_LOOKAHEAD_DEFAULT_MS, LIMIT_LOOKAHEAD_MAX_MS,
    LIMIT_LOOKAHEAD_MIN_MS, LimitSettings, LimitUpdate,
};
use omamix_core::link::{self, LinkPhase, PortEndpoint};
use omamix_core::metadata::{self, DefaultDeviceObservation};
use omamix_core::params::StripParamUpdate;
use omamix_core::route::{
    self, LinkEdge, LoopGraph, MIX_MIC_NODE_NAME, NodeIdentity, NodeRef, OwnClient, PlaybackStream,
    SourceKind,
};
use pipewire as pw;
use pw::properties::PropertiesBox;
use pw::registry::GlobalObject;
use pw::spa::pod::Pod;

use crate::EngineError;
use crate::contract;
use crate::filter::{MixFilter, ProcessShared};
use crate::rt::{CommandQueue, EngineCommand, MIX_SLOT_CAP};

const INSTANCE_KEY: &str = "omamix.instance";

static STOP: AtomicBool = AtomicBool::new(false);

/// What `--headless` asked for. `publish_mix_mic` false does not open a client.
pub struct HeadlessOptions {
    pub scene: Scene,
    pub duration: Option<Duration>,
    pub publish_mix_mic: bool,
    /// 440 Hz sine written by Mix Mic's `process`. Opt-in, so a recorder can
    /// measure this node. Off leaves the mix as the sum of the taps.
    pub tone: bool,
    pub fallback_missing_devices: bool,
}

/// Identifiers printed once the filter node is in the graph.
pub struct HeadlessReport {
    pub node_id: u32,
    pub client_id: u32,
}

enum LoopMsg {
    Shutdown,
    SetMasterFaderDb(f32),
    SetMonitorMix(bool),
    SetStripParam { slot: u16, update: StripParamUpdate },
    SetStripEq { slot: u16, update: EqUpdate },
    SetMasterEq(EqUpdate),
    SetStripComp { slot: u16, update: CompUpdate },
    SetMasterComp(CompUpdate),
    SetMasterLimit(LimitUpdate),
    SetMasterChain(Box<MasterChain>),
    BypassLimiter(bool),
}

fn engine_command_msg(command: EngineCommand) -> Option<LoopMsg> {
    Some(match command {
        EngineCommand::Shutdown => LoopMsg::Shutdown,
        EngineCommand::SetMasterFaderDb(db) => LoopMsg::SetMasterFaderDb(db),
        EngineCommand::SetMonitorMix(on) => LoopMsg::SetMonitorMix(on),
        EngineCommand::SetStripParam { slot, update } => LoopMsg::SetStripParam { slot, update },
        EngineCommand::SetStripEq { slot, update } => LoopMsg::SetStripEq { slot, update },
        EngineCommand::SetMasterEq(update) => LoopMsg::SetMasterEq(update),
        EngineCommand::SetStripComp { slot, update } => LoopMsg::SetStripComp { slot, update },
        EngineCommand::SetMasterComp(update) => LoopMsg::SetMasterComp(update),
        EngineCommand::SetMasterLimit(update) => LoopMsg::SetMasterLimit(update),
        EngineCommand::SetMasterChain(chain) => LoopMsg::SetMasterChain(chain),
        EngineCommand::ReplaceScene(_) => return None,
    })
}

struct Ready {
    node_id: u32,
    client_id: u32,
}

enum Boot {
    Ready(Ready),
    Failed(String),
    Usage(String),
}

pub struct WindowSnapshot {
    pub catalog: Vec<route::ListedNode>,
    pub default_source: Option<String>,
    pub catalog_ready: bool,
    pub published: bool,
    pub failure: Option<String>,
    pub master_peak: f32,
    pub master_rms: f32,
    pub strips: Vec<(u64, f32, f32)>,
    /// Block-minimum gain reduction per strip id, in dB. 0 is no reduction.
    pub strip_gr: Vec<(u64, f32)>,
    /// Master compressor gain reduction, in dB.
    pub master_gr: f32,
    /// Callback rate. 0 until the first process.
    pub sample_rate: u32,
    /// Callback quantum. 0 until the first process.
    pub quantum: u32,
    /// Tap slot for a strip id, when the filter has claimed one.
    pub strip_slots: Vec<(u64, u16)>,
}

struct WindowView {
    catalog: Mutex<Vec<route::ListedNode>>,
    default_source: Mutex<Option<String>>,
    catalog_ready: AtomicBool,
    published: AtomicBool,
    failure: Mutex<Option<String>>,
    master_peak: Mutex<f32>,
    master_rms: Mutex<f32>,
    strips: Mutex<Vec<(u64, f32, f32)>>,
    strip_gr: Mutex<Vec<(u64, f32)>>,
    master_gr: Mutex<f32>,
    sample_rate: AtomicU32,
    quantum: AtomicU32,
    strip_slots: Mutex<Vec<(u64, u16)>>,
}

impl WindowView {
    fn new() -> Self {
        Self {
            catalog: Mutex::new(Vec::new()),
            default_source: Mutex::new(None),
            catalog_ready: AtomicBool::new(false),
            published: AtomicBool::new(false),
            failure: Mutex::new(None),
            master_peak: Mutex::new(f32::NEG_INFINITY),
            master_rms: Mutex::new(f32::NEG_INFINITY),
            strips: Mutex::new(Vec::new()),
            strip_gr: Mutex::new(Vec::new()),
            master_gr: Mutex::new(0.0),
            sample_rate: AtomicU32::new(0),
            quantum: AtomicU32::new(0),
            strip_slots: Mutex::new(Vec::new()),
        }
    }

    fn note_failure(&self, message: &str) {
        let mut slot = lock_mutex(&self.failure);
        if slot.is_none() {
            *slot = Some(message.to_string());
        }
    }

    fn publish(&self, snap: WindowSnapshot) {
        *lock_mutex(&self.catalog) = snap.catalog;
        *lock_mutex(&self.default_source) = snap.default_source;
        self.catalog_ready
            .store(snap.catalog_ready, Ordering::Relaxed);
        self.published.store(snap.published, Ordering::Relaxed);
        *lock_mutex(&self.master_peak) = snap.master_peak;
        *lock_mutex(&self.master_rms) = snap.master_rms;
        *lock_mutex(&self.strips) = snap.strips;
        *lock_mutex(&self.strip_gr) = snap.strip_gr;
        *lock_mutex(&self.master_gr) = snap.master_gr;
        self.sample_rate.store(snap.sample_rate, Ordering::Relaxed);
        self.quantum.store(snap.quantum, Ordering::Relaxed);
        *lock_mutex(&self.strip_slots) = snap.strip_slots;
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

pub struct WindowEngine {
    commands: crate::rt::CommandSender,
    pw_tx: pw::channel::Sender<LoopMsg>,
    thread: Option<std::thread::JoinHandle<()>>,
    view: Arc<WindowView>,
}

impl WindowEngine {
    pub fn snapshot(&self) -> WindowSnapshot {
        WindowSnapshot {
            catalog: lock_mutex(&self.view.catalog).clone(),
            default_source: lock_mutex(&self.view.default_source).clone(),
            catalog_ready: self.view.catalog_ready.load(Ordering::Relaxed),
            published: self.view.published.load(Ordering::Relaxed),
            failure: lock_mutex(&self.view.failure).clone(),
            master_peak: *lock_mutex(&self.view.master_peak),
            master_rms: *lock_mutex(&self.view.master_rms),
            strips: lock_mutex(&self.view.strips).clone(),
            strip_gr: lock_mutex(&self.view.strip_gr).clone(),
            master_gr: *lock_mutex(&self.view.master_gr),
            sample_rate: self.view.sample_rate.load(Ordering::Relaxed),
            quantum: self.view.quantum.load(Ordering::Relaxed),
            strip_slots: lock_mutex(&self.view.strip_slots).clone(),
        }
    }

    pub fn published(&self) -> bool {
        self.view.published.load(Ordering::Relaxed)
    }

    pub fn failure(&self) -> Option<String> {
        lock_mutex(&self.view.failure).clone()
    }

    pub fn submit_scene(&self, scene: &Scene) {
        self.commands
            .submit(EngineCommand::ReplaceScene(Box::new(scene.clone())));
    }

    pub fn shutdown(&mut self) {
        let _ = self.pw_tx.send(LoopMsg::Shutdown);
        self.commands.submit(EngineCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WindowEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

const WINDOW_ENGINE_NAME: &str = concat!("pipe", "wire");

impl crate::AudioEngine for WindowEngine {
    fn name(&self) -> &'static str {
        WINDOW_ENGINE_NAME
    }

    fn command_sender(&self) -> crate::rt::CommandSender {
        self.commands.clone()
    }
}

pub fn open_window_engine(scene: Scene) -> Result<WindowEngine, EngineError> {
    let queue = CommandQueue::new();
    let commands = queue.sender();
    let view = Arc::new(WindowView::new());
    let (ready_tx, _ready_rx) = std::sync::mpsc::channel::<Boot>();
    let (pw_tx, pw_rx) = pw::channel::channel::<LoopMsg>();
    let options = HeadlessOptions {
        scene,
        duration: None,
        publish_mix_mic: true,
        tone: false,
        fallback_missing_devices: true,
    };
    let view_thread = Arc::clone(&view);
    let thread = std::thread::Builder::new()
        .name("omamix-pw".into())
        .spawn(move || {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                pw_thread(options, None, pw_rx, ready_tx, queue, Some(view_thread));
            }));
        })
        .map_err(|err| EngineError::PipeWire(err.to_string()))?;
    Ok(WindowEngine {
        commands,
        pw_tx,
        thread: Some(thread),
        view,
    })
}

/// Open a client only when `publish_mix_mic` is set. Blocks until shutdown.
pub fn run_headless(options: HeadlessOptions) -> Result<HeadlessReport, EngineError> {
    if !options.publish_mix_mic {
        println!("omamix: headless, Mix Mic not requested");
        return Ok(HeadlessReport {
            node_id: 0,
            client_id: 0,
        });
    }

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Boot>();
    let (pw_tx, pw_rx) = pw::channel::channel::<LoopMsg>();
    let queue = CommandQueue::new();
    arm_signals();

    let duration = options.duration;
    let tone = options.tone;
    let thread = std::thread::Builder::new()
        .name("omamix-pw".into())
        .spawn(move || {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                pw_thread(options, None, pw_rx, ready_tx, queue, None);
            }));
        })
        .map_err(|err| EngineError::PipeWire(err.to_string()))?;

    let ready = match wait_until_ready(&thread, &ready_rx, &pw_tx) {
        Ok(ready) => ready,
        Err(err) => {
            let _ = pw_tx.send(LoopMsg::Shutdown);
            let _ = thread.join();
            return Err(err);
        }
    };
    println!(
        "omamix: Mix Mic node id {} client id {} priority.session=1 priority.driver=1",
        ready.node_id, ready.client_id
    );
    if tone {
        println!("omamix: Mix Mic tone is 440 Hz at amplitude 0.2");
    }

    let deadline = duration.map(|span| Instant::now() + span);
    let (look_tx, look_rx) = std::sync::mpsc::channel::<StdinCmd>();
    let _reader = std::thread::Builder::new()
        .name("omamix-lookahead".into())
        .spawn(move || read_lookahead_lines(look_tx));
    loop {
        if STOP.swap(false, Ordering::SeqCst) {
            break;
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            break;
        }
        if thread.is_finished() {
            break;
        }
        match look_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(StdinCmd::Lookahead(ms)) => {
                let _ = pw_tx.send(LoopMsg::SetMasterLimit(LimitUpdate::LookaheadMs(ms)));
            }
            Ok(StdinCmd::BypassLimiter(on)) => {
                let _ = pw_tx.send(LoopMsg::BypassLimiter(on));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    let _ = pw_tx.send(LoopMsg::Shutdown);
    let _ = thread.join();

    Ok(HeadlessReport {
        node_id: ready.node_id,
        client_id: ready.client_id,
    })
}

/// Parse one lookahead token from a `lookahead <ms>` line.
///
/// Unparsable text, NaN, and infinities are ignored. A finite value is
/// clamped to [`LIMIT_LOOKAHEAD_MIN_MS`] and [`LIMIT_LOOKAHEAD_MAX_MS`].
fn clamp_lookahead_token(text: &str) -> Option<f32> {
    let ms = text.parse::<f32>().ok()?;
    if !ms.is_finite() {
        return None;
    }
    Some(ms.clamp(LIMIT_LOOKAHEAD_MIN_MS, LIMIT_LOOKAHEAD_MAX_MS))
}

/// One control line from stdin.
enum StdinCmd {
    Lookahead(f32),
    BypassLimiter(bool),
}

/// Parse `lookahead <ms>` or `bypass limiter` / `bypass limiter off`.
///
/// Other lines are ignored. Lookahead tokens that are not finite numbers
/// are ignored. A finite lookahead is clamped to [`LIMIT_LOOKAHEAD_MIN_MS`]
/// and [`LIMIT_LOOKAHEAD_MAX_MS`].
fn parse_stdin_line(line: &str) -> Option<StdinCmd> {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("lookahead") => {
            let text = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            Some(StdinCmd::Lookahead(clamp_lookahead_token(text)?))
        }
        Some("bypass") => {
            if parts.next() != Some("limiter") {
                return None;
            }
            let on = match parts.next() {
                None | Some("on") => true,
                Some("off") => false,
                Some(_) => return None,
            };
            if parts.next().is_some() {
                return None;
            }
            Some(StdinCmd::BypassLimiter(on))
        }
        _ => None,
    }
}

/// Chain-bypass the limiter block. Other blocks stay as they are.
fn bypass_limiter_block(chain: &mut MasterChain, bypass: bool) -> bool {
    let Some(index) = chain
        .blocks()
        .iter()
        .position(|block| block.kind() == EffectKind::Limiter)
    else {
        return false;
    };
    chain.set_bypass(index, bypass)
}

/// Read lookahead and limiter-bypass lines and forward them.
///
/// Other lines are ignored. This does not touch the audio callback. The
/// caller waits on the channel. A closed stdin ends this task, and the
/// caller sleeps 50 ms when the sender is gone so it does not spin.
fn read_lookahead_lines(tx: std::sync::mpsc::Sender<StdinCmd>) {
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Some(cmd) = parse_stdin_line(&line) else {
            continue;
        };
        if tx.send(cmd).is_err() {
            break;
        }
    }
}

fn wait_until_ready(
    thread: &std::thread::JoinHandle<()>,
    ready_rx: &std::sync::mpsc::Receiver<Boot>,
    pw_tx: &pw::channel::Sender<LoopMsg>,
) -> Result<Ready, EngineError> {
    let started = Instant::now();
    loop {
        if thread.is_finished() {
            return Err(EngineError::PipeWire(
                "PipeWire thread stopped before Mix Mic was ready".into(),
            ));
        }
        match ready_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Boot::Ready(ready)) => return Ok(ready),
            Ok(Boot::Failed(message)) => {
                let _ = pw_tx.send(LoopMsg::Shutdown);
                return Err(EngineError::PipeWire(message));
            }
            Ok(Boot::Usage(message)) => {
                let _ = pw_tx.send(LoopMsg::Shutdown);
                return Err(EngineError::Usage(message));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if STOP.load(Ordering::SeqCst) {
                    return Err(EngineError::PipeWire(
                        "stopped before Mix Mic was ready".into(),
                    ));
                }
                if started.elapsed() >= Duration::from_secs(5) {
                    return Err(EngineError::PipeWire(
                        "Mix Mic was not ready within 5s".into(),
                    ));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(EngineError::PipeWire(
                    "PipeWire thread stopped before Mix Mic was ready".into(),
                ));
            }
        }
    }
}

unsafe extern "C" fn on_stop(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn arm_signals() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_stop as *const () as usize;
        let _ = libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        let _ = libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }
}

pub fn run_list() -> Result<String, EngineError> {
    let (list_tx, list_rx) = std::sync::mpsc::channel();
    let (ready_tx, _ready_rx) = std::sync::mpsc::channel::<Boot>();
    let (pw_tx, pw_rx) = pw::channel::channel::<LoopMsg>();
    let queue = CommandQueue::new();
    arm_signals();
    let options = HeadlessOptions {
        scene: Scene::empty("list"),
        duration: None,
        publish_mix_mic: false,
        tone: false,
        fallback_missing_devices: false,
    };
    let thread = std::thread::Builder::new()
        .name("omamix-pw".into())
        .spawn(move || {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                pw_thread(options, Some(list_tx), pw_rx, ready_tx, queue, None);
            }));
        })
        .map_err(|err| EngineError::PipeWire(err.to_string()))?;
    let started = Instant::now();
    loop {
        if STOP.swap(false, Ordering::SeqCst) {
            let _ = pw_tx.send(LoopMsg::Shutdown);
            let _ = thread.join();
            return Err(EngineError::PipeWire(
                "stopped before the device list was ready".into(),
            ));
        }
        match list_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(nodes) => {
                let _ = pw_tx.send(LoopMsg::Shutdown);
                let _ = thread.join();
                return Ok(route::format_listed_nodes(&nodes));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if thread.is_finished() {
                    let _ = thread.join();
                    return Err(EngineError::PipeWire(
                        "PipeWire thread stopped before the device list was ready".into(),
                    ));
                }
                if started.elapsed() >= Duration::from_secs(5) {
                    let _ = pw_tx.send(LoopMsg::Shutdown);
                    let _ = thread.join();
                    return Err(EngineError::PipeWire(
                        "device list was not ready within 5s".into(),
                    ));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                return Err(EngineError::PipeWire(
                    "PipeWire thread stopped before the device list was ready".into(),
                ));
            }
        }
    }
}

fn pw_thread(
    options: HeadlessOptions,
    list_tx: Option<std::sync::mpsc::Sender<Vec<route::ListedNode>>>,
    pw_rx: pw::channel::Receiver<LoopMsg>,
    ready_tx: std::sync::mpsc::Sender<Boot>,
    queue: CommandQueue,
    view: Option<Arc<WindowView>>,
) {
    let report = view.clone();
    let fail = move |message: &str| {
        if let Some(view) = &report {
            view.note_failure(message);
        }
    };
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(loop_) => loop_,
        Err(err) => {
            eprintln!("omamix: main loop: {err}");
            fail("PipeWire main loop did not start");
            return;
        }
    };
    let context = match pw::context::ContextBox::new(mainloop.loop_(), None) {
        Ok(context) => context,
        Err(err) => {
            eprintln!("omamix: context: {err}");
            fail("PipeWire context did not start");
            return;
        }
    };

    let instance = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut client_props = PropertiesBox::new();
    client_props.insert("application.name", "omamix");
    client_props.insert("application.process.binary", "omamix");
    client_props.insert("application.process.id", std::process::id().to_string());
    client_props.insert(INSTANCE_KEY, instance.as_str());

    let core = match context.connect(Some(client_props)) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("omamix: connect: {err}");
            fail("PipeWire client did not connect");
            return;
        }
    };
    let registry = match core.get_registry() {
        Ok(registry) => registry,
        Err(err) => {
            eprintln!("omamix: registry: {err}");
            fail("PipeWire registry did not open");
            return;
        }
    };

    let filter: Rc<RefCell<Option<MixFilter>>> = Rc::new(RefCell::new(None));
    let monitor_keep: Rc<RefCell<Option<MonitorKeep>>> = Rc::new(RefCell::new(None));

    let strips_checked = !scene_needs_device_check(&options.scene);
    let shared_loop = Rc::new(LoopShared {
        pending: Mutex::new(Vec::new()),
        view,
        inner: RefCell::new(LoopInner {
            instance,
            scene: options.scene,
            tone: options.tone,
            fallback_missing_devices: options.fallback_missing_devices,
            queue,
            ready_tx: Some(ready_tx),
            ready_sent: false,
            own: None,
            mix_mic_node: None,
            filter_node: None,
            source_seen: false,
            sink_seen: false,
            default_source: None,
            default_sink: None,
            configured_source: None,
            configured_seen: false,
            settle_round: 0,
            settle_pending: None,
            saw_settle_done: false,
            strips_checked,
            started: Instant::now(),
            list_tx,
            published: false,
            engine_linked: false,
            engine_saw_edges: false,
            engine_link_keys: Vec::new(),
            engine_up: [false, false],
            engine_tries: 0,
            engine_wait: 0,
            retired_slots: Vec::new(),
            quit: false,
            nodes: HashMap::new(),
            ports: HashMap::new(),
            edges: HashMap::new(),
            taps: HashMap::new(),
            refused: HashMap::new(),
            unlinked_clients: HashSet::new(),
            clients: HashMap::new(),
            bind_queue: Vec::new(),
            bindings: Vec::new(),
            links: HashMap::new(),
            next_link_key: 1,
            adapter: None,
            core: core.as_raw_ptr(),
            registry: registry.as_raw_ptr(),
            stats_logged: None,
        }),
    });

    let listener = {
        let on_global = Rc::clone(&shared_loop);
        let on_remove = Rc::clone(&shared_loop);
        registry
            .add_listener_local()
            .global(move |global| {
                let owned = global.to_owned();
                if let Ok(mut pending) = on_global.pending.lock() {
                    pending.push(Pending::Global(owned));
                }
            })
            .global_remove(move |id| {
                if let Ok(mut pending) = on_remove.pending.lock() {
                    pending.push(Pending::Remove(id));
                }
            })
            .register()
    };
    let _core_listener = {
        let on_done = Rc::clone(&shared_loop);
        core.add_listener_local()
            .done(move |id, seq| {
                if let Ok(mut pending) = on_done.pending.lock() {
                    pending.push(Pending::CoreDone { id, seq });
                }
            })
            .register()
    };
    match core.sync(0) {
        Ok(seq) => {
            let mut inner = shared_loop.inner.borrow_mut();
            inner.settle_round = 1;
            inner.settle_pending = Some(seq);
        }
        Err(_) => {
            shared_loop.inner.borrow_mut().settle_round = 3;
        }
    }

    let monitor_on = Arc::new(AtomicBool::new(
        shared_loop.inner.borrow().scene.master.monitor_mix,
    ));

    let _attached = pw_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let filter = Rc::clone(&filter);
        let monitor_on = Arc::clone(&monitor_on);
        let shared_loop = Rc::clone(&shared_loop);
        move |msg| {
            if apply_loop_msg(&msg, &shared_loop, &filter, &monitor_on) {
                mainloop.quit();
            }
        }
    });

    let timer = mainloop.loop_().add_timer({
        let shared_loop = Rc::clone(&shared_loop);
        let filter = Rc::clone(&filter);
        let monitor_keep = Rc::clone(&monitor_keep);
        let mainloop = mainloop.clone();
        let monitor_on = Arc::clone(&monitor_on);
        move |_| {
            if tick(&shared_loop, &filter, &monitor_keep, &monitor_on) {
                mainloop.quit();
            }
        }
    });
    let _ = timer.update_timer(
        Some(Duration::from_millis(1)),
        Some(Duration::from_millis(50)),
    );

    mainloop.run();
    log_callback_stats(&shared_loop, &filter, true);

    drop(timer);
    drop(_attached);
    drop(listener);
    drop(_core_listener);
    drop(monitor_keep);
    drop(shared_loop);
    drop(filter);
    drop(registry);
    drop(core);
    drop(context);
    drop(mainloop);
}

fn apply_loop_msg(
    msg: &LoopMsg,
    shared: &LoopShared,
    filter: &RefCell<Option<MixFilter>>,
    monitor_on: &AtomicBool,
) -> bool {
    match msg {
        LoopMsg::Shutdown => true,
        LoopMsg::SetMasterFaderDb(db) => {
            if let Some(filter) = filter.borrow().as_ref() {
                filter.shared().params.set_master_db(*db);
            }
            false
        }
        LoopMsg::SetMonitorMix(on) => {
            monitor_on.store(*on, Ordering::Relaxed);
            false
        }
        LoopMsg::SetStripParam { slot, update } => {
            if let Some(filter) = filter.borrow().as_ref() {
                filter.shared().strips.apply(*slot, *update);
            }
            false
        }
        LoopMsg::SetStripEq { slot, update } => {
            if let Some(filter) = filter.borrow_mut().as_mut() {
                filter.apply_strip_eq(*slot, *update);
            }
            false
        }
        LoopMsg::SetMasterEq(update) => {
            if let Some(filter) = filter.borrow_mut().as_mut() {
                filter.apply_master_eq(*update);
            }
            false
        }
        LoopMsg::SetStripComp { slot, update } => {
            if let Some(filter) = filter.borrow_mut().as_mut() {
                filter.apply_strip_comp(*slot, *update);
            }
            false
        }
        LoopMsg::SetMasterComp(update) => {
            if let Some(filter) = filter.borrow_mut().as_mut() {
                filter.apply_master_comp(*update);
            }
            false
        }
        LoopMsg::SetMasterLimit(update) => {
            if let Some(filter) = filter.borrow_mut().as_mut() {
                filter.apply_master_limit(*update);
            }
            false
        }
        LoopMsg::SetMasterChain(chain) => {
            if let Some(filter) = filter.borrow_mut().as_mut() {
                filter.apply_chain(chain);
            }
            false
        }
        LoopMsg::BypassLimiter(on) => {
            let chain = {
                let mut inner = shared.inner.borrow_mut();
                if !bypass_limiter_block(&mut inner.scene.master.chain, *on) {
                    return false;
                }
                inner.scene.master.chain
            };
            if let Some(filter) = filter.borrow_mut().as_mut() {
                filter.apply_chain(&chain);
            }
            false
        }
    }
}

fn connect_monitor(
    core: &pw::core::Core,
    shared: Arc<ProcessShared>,
    monitor_on: Arc<AtomicBool>,
) -> Result<
    (
        pw::stream::StreamBox<'_>,
        pw::stream::StreamListener<MonitorData>,
    ),
    pw::Error,
> {
    let mut props = PropertiesBox::new();
    for (key, value) in contract::monitor_mix_properties("omamix.monitor") {
        props.insert(key, value);
    }
    let stream = pw::stream::StreamBox::new(core, "omamix monitor", props)?;
    let data = MonitorData {
        slot: shared,
        on: monitor_on,
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .process(|stream, data| {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| fill_monitor(stream, data)));
        })
        .register()?;
    let bytes = crate::backend::stereo_f32_format(48_000);
    let pod = Pod::from_bytes(&bytes).expect("stereo f32 format pod");
    let mut params = [pod];
    stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    Ok((stream, listener))
}

struct MonitorData {
    slot: Arc<ProcessShared>,
    on: Arc<AtomicBool>,
}

fn fill_monitor(stream: &pw::stream::Stream, data: &MonitorData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let mut left = [0.0f32; MIX_SLOT_CAP];
    let mut right = [0.0f32; MIX_SLOT_CAP];
    let copied = if data.on.load(Ordering::Relaxed) {
        data.slot.slot.read(&mut left, &mut right)
    } else {
        0
    };
    if datas.len() >= 2 {
        write_planar(&mut datas[0], &left, copied);
        write_planar(&mut datas[1], &right, copied);
    } else {
        write_interleaved(&mut datas[0], &left, &right, copied);
    }
}

fn write_planar(data: &mut pw::spa::buffer::Data, samples: &[f32], copied: usize) {
    let Some((ptr, n)) = planar_dest(data, copied) else {
        return;
    };
    if n == 0 {
        *data.chunk_mut().size_mut() = 0;
        return;
    }
    let dst = unsafe { std::slice::from_raw_parts_mut(ptr, n) };
    dst.copy_from_slice(&samples[..n]);
    *data.chunk_mut().offset_mut() = 0;
    *data.chunk_mut().stride_mut() = 4;
    *data.chunk_mut().size_mut() = (n * 4) as u32;
}

fn planar_dest(data: &mut pw::spa::buffer::Data, copied: usize) -> Option<(*mut f32, usize)> {
    let bytes = data.data()?;
    let n = copied.min(bytes.len() / 4).min(MIX_SLOT_CAP);
    Some((bytes.as_mut_ptr() as *mut f32, n))
}

fn write_interleaved(data: &mut pw::spa::buffer::Data, left: &[f32], right: &[f32], copied: usize) {
    let Some((ptr, n)) = interleaved_dest(data, copied) else {
        return;
    };
    *data.chunk_mut().offset_mut() = 0;
    *data.chunk_mut().stride_mut() = 8;
    *data.chunk_mut().size_mut() = (n * 8) as u32;
    if n == 0 {
        return;
    }
    let dst = unsafe { std::slice::from_raw_parts_mut(ptr, n * 2) };
    for i in 0..n {
        dst[i * 2] = left[i];
        dst[i * 2 + 1] = right[i];
    }
}

fn interleaved_dest(data: &mut pw::spa::buffer::Data, copied: usize) -> Option<(*mut f32, usize)> {
    let bytes = data.data()?;
    let n = copied.min(bytes.len() / 8).min(MIX_SLOT_CAP);
    Some((bytes.as_mut_ptr() as *mut f32, n))
}

struct LoopShared {
    pending: Mutex<Vec<Pending>>,
    view: Option<Arc<WindowView>>,
    inner: RefCell<LoopInner>,
}

enum Pending {
    Global(GlobalObject<PropertiesBox>),
    Remove(u32),
    Info {
        id: u32,
        props: Vec<(String, String)>,
    },
    LinkState {
        key: u64,
        active: bool,
        error: bool,
    },
    Metadata {
        key: String,
        value: Option<String>,
    },
    CoreDone {
        id: u32,
        seq: pw::spa::utils::result::AsyncSeq,
    },
}

struct LoopInner {
    instance: String,
    scene: Scene,
    tone: bool,
    fallback_missing_devices: bool,
    queue: CommandQueue,
    ready_tx: Option<std::sync::mpsc::Sender<Boot>>,
    ready_sent: bool,
    own: Option<OwnClient>,
    mix_mic_node: Option<u32>,
    filter_node: Option<u32>,
    source_seen: bool,
    sink_seen: bool,
    default_source: Option<String>,
    default_sink: Option<String>,
    configured_source: Option<String>,
    configured_seen: bool,
    settle_round: u8,
    settle_pending: Option<pw::spa::utils::result::AsyncSeq>,
    saw_settle_done: bool,
    strips_checked: bool,
    started: Instant,
    list_tx: Option<std::sync::mpsc::Sender<Vec<route::ListedNode>>>,
    published: bool,
    engine_linked: bool,
    engine_saw_edges: bool,
    engine_link_keys: Vec<u64>,
    engine_up: [bool; 2],
    engine_tries: u8,
    engine_wait: u8,
    retired_slots: Vec<usize>,
    quit: bool,
    nodes: HashMap<u32, NodeRec>,
    ports: HashMap<u32, PortRec>,
    edges: HashMap<u32, LinkEdge>,
    taps: HashMap<u32, Tap>,
    refused: HashMap<u32, bool>,
    unlinked_clients: HashSet<u32>,
    clients: HashMap<u32, ClientRec>,
    bind_queue: Vec<GlobalObject<PropertiesBox>>,
    bindings: Vec<Binding>,
    links: HashMap<u64, LiveLink>,
    next_link_key: u64,
    adapter: Option<pw::node::Node>,
    core: *mut pw::sys::pw_core,
    registry: *mut pw::sys::pw_registry,
    stats_logged: Option<Instant>,
}

#[derive(Default)]
struct ClientRec {
    binary: String,
    application_name: String,
}

struct NodeRec {
    identity: NodeIdentity,
    bound: bool,
}

struct PortRec {
    node_id: u32,
    /// `port.id` on the node. Link factories use this, not the global id.
    port_id: Option<u32>,
    direction: String,
    name: String,
    channel: String,
}

struct Tap {
    phase: LinkPhase,
    slot: Option<usize>,
    linked: bool,
    saw_edges: bool,
    link_keys: Vec<u64>,
    tries: u8,
    wait: u8,
    revivals: u8,
    relinks: u8,
    stop_logged: bool,
}

struct LiveLink {
    _listener: pw::link::LinkListener,
    _link: pw::link::Link,
}

struct Binding {
    id: u32,
    _listener: ProxyListener,
    _proxy: ProxyKeep,
}

#[allow(dead_code)]
enum ProxyKeep {
    Node(pw::node::Node),
    Client(pw::client::Client),
    Port(pw::port::Port),
    Link(pw::link::Link),
    Metadata(pw::metadata::Metadata),
}

#[allow(dead_code)]
enum ProxyListener {
    Node(pw::node::NodeListener),
    Client(pw::client::ClientListener),
    Port(pw::port::PortListener),
    Link(pw::link::LinkListener),
    Metadata(pw::metadata::MetadataListener),
}

/// Monitor stream stored with a `'static` lifetime. `pw_thread` drops this
/// before the core it was created from.
struct MonitorKeep {
    _listener: pw::stream::StreamListener<MonitorData>,
    _stream: pw::stream::StreamBox<'static>,
}

fn tick(
    shared: &Rc<LoopShared>,
    filter: &Rc<RefCell<Option<MixFilter>>>,
    monitor_keep: &Rc<RefCell<Option<MonitorKeep>>>,
    monitor_on: &Arc<AtomicBool>,
) -> bool {
    let quit = drain_commands(shared, filter, monitor_on);
    sync_eq_rate(filter);
    sync_comp_rate(filter);
    sync_limit_rate(filter);
    sync_limit_quantum(filter);
    refresh_live_cap(filter);
    for _ in 0..8 {
        let batch = match shared.pending.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(_) => break,
        };
        if batch.is_empty() {
            break;
        }
        for event in batch {
            apply_event(shared, event);
        }
        bind_outstanding(shared);
    }
    retire_ports(shared, filter);
    if shared.inner.borrow().quit {
        return true;
    }
    advance_settle(shared);
    if list_if_ready(shared) {
        return true;
    }
    if reject_devices_if_ready(shared) {
        return true;
    }
    if shared.inner.borrow().quit {
        return true;
    }
    ensure_published(shared, filter, monitor_keep, monitor_on);
    if shared.inner.borrow().quit {
        return true;
    }
    link_engine(shared);
    reconcile(shared, filter);
    log_callback_stats(shared, filter, false);
    publish_ready(shared);
    publish_view(shared, filter);
    quit || shared.inner.borrow().quit
}

fn publish_view(shared: &LoopShared, filter: &RefCell<Option<MixFilter>>) {
    let Some(view) = &shared.view else {
        return;
    };
    let inner = shared.inner.borrow();
    let catalog = listed_nodes(&inner);
    let default_source = resolved_default_name(&inner);
    let ready = inner.settle_round >= 3;
    let published = inner.published;
    let (master_peak, master_rms, strips, strip_gr, master_gr, sample_rate, quantum, strip_slots) =
        if let Some(engine) = filter.borrow().as_ref() {
            let audio = engine.shared();
            let mut best: HashMap<u64, (f32, f32)> = HashMap::new();
            let mut reductions: HashMap<u64, f32> = HashMap::new();
            let mut slots: HashMap<u64, u16> = HashMap::new();
            for (peer, tap) in &inner.taps {
                let Some(slot) = tap.slot else {
                    continue;
                };
                let Some(strip) = strip_for_peer(&inner, *peer) else {
                    continue;
                };
                let (peak, rms) = audio.strip_meters.load(slot);
                let entry = best
                    .entry(strip.id.0)
                    .or_insert((f32::NEG_INFINITY, f32::NEG_INFINITY));
                if peak > entry.0 {
                    entry.0 = peak;
                }
                if rms > entry.1 {
                    entry.1 = rms;
                }
                let reduction = engine.strip_reduction(slot);
                let gr = reductions.entry(strip.id.0).or_insert(0.0);
                if reduction < *gr {
                    *gr = reduction;
                }
                if let Ok(slot) = u16::try_from(slot) {
                    slots.entry(strip.id.0).or_insert(slot);
                }
            }
            let mut strips = Vec::new();
            for (id, (peak, rms)) in best {
                strips.push((id, peak, rms));
            }
            let mut strip_gr = Vec::new();
            for (id, gr) in reductions {
                strip_gr.push((id, gr));
            }
            let mut strip_slots = Vec::new();
            for (id, slot) in slots {
                strip_slots.push((id, slot));
            }
            let (master_peak, master_rms) = audio.meter.load();
            (
                master_peak,
                master_rms,
                strips,
                strip_gr,
                engine.master_reduction(),
                audio.observed_rate.load(Ordering::Relaxed),
                engine.graph_quantum(),
                strip_slots,
            )
        } else {
            (
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
                Vec::new(),
                Vec::new(),
                0.0,
                0,
                0,
                Vec::new(),
            )
        };
    drop(inner);
    view.publish(WindowSnapshot {
        catalog,
        default_source,
        catalog_ready: ready,
        published,
        failure: None,
        master_peak,
        master_rms,
        strips,
        strip_gr,
        master_gr,
        sample_rate,
        quantum,
        strip_slots,
    });
}

/// Rebuild EQ coefficients when the callback's rate is not the published one.
///
/// The audio callback only stores the rate. This runs on the loop thread.
fn sync_eq_rate(filter: &RefCell<Option<MixFilter>>) {
    let mut borrowed = filter.borrow_mut();
    let Some(filter) = borrowed.as_mut() else {
        return;
    };
    let rate = filter.shared().observed_rate.load(Ordering::Relaxed);
    if rate != 0 {
        filter.set_eq_rate(rate);
    }
}

/// Rebuild compressor coefficients when the callback's rate is not the published one.
///
/// The audio callback only stores the rate. This runs on the loop thread.
fn sync_comp_rate(filter: &RefCell<Option<MixFilter>>) {
    let mut borrowed = filter.borrow_mut();
    let Some(filter) = borrowed.as_mut() else {
        return;
    };
    let rate = filter.shared().observed_rate.load(Ordering::Relaxed);
    if rate != 0 {
        filter.set_comp_rate(rate);
    }
}

/// Rebuild limiter coefficients when the callback's rate is not the published one.
///
/// The audio callback only stores the rate. This runs on the loop thread.
/// Until it runs, the callback still limits: it recomputes the delay at the
/// rate it is processing. `set_limit_rate` also publishes `ProcessLatency`.
fn sync_limit_rate(filter: &RefCell<Option<MixFilter>>) {
    let mut borrowed = filter.borrow_mut();
    let Some(filter) = borrowed.as_mut() else {
        return;
    };
    let rate = filter.shared().observed_rate.load(Ordering::Relaxed);
    if rate != 0 {
        filter.set_limit_rate(rate);
    }
}

/// Apply the callback quantum to the master 0 ms cap.
///
/// The audio callback stores the frame count and the rate. This runs on the
/// loop thread from the 50 ms engine timer and publishes ProcessLatency and
/// port Latency for the delay that callback is applying.
fn sync_limit_quantum(filter: &RefCell<Option<MixFilter>>) {
    let mut borrowed = filter.borrow_mut();
    let Some(filter) = borrowed.as_mut() else {
        return;
    };
    filter.sync_limit_quantum();
}

fn refresh_live_cap(filter: &RefCell<Option<MixFilter>>) {
    let (rate, quantum, enabled, lookahead) = {
        let borrowed = filter.borrow();
        let Some(engine) = borrowed.as_ref() else {
            return;
        };
        let rate = engine.shared().observed_rate.load(Ordering::Relaxed);
        let quantum = engine.graph_quantum();
        if rate == 0 || quantum == 0 {
            return;
        }
        let settings = engine.master_limit_settings();
        if engine.cap_matches(rate, quantum, settings.enabled(), settings.lookahead_ms()) {
            drop(borrowed);
            if let Some(engine) = filter.borrow_mut().as_mut() {
                engine.note_cap_stable();
            }
            return;
        }
        (rate, quantum, settings.enabled(), settings.lookahead_ms())
    };
    let report = omamix_core::live_cap::calibrate_live_cap(rate, quantum, enabled, lookahead);
    let mut borrowed = filter.borrow_mut();
    let Some(engine) = borrowed.as_mut() else {
        return;
    };
    let rate_now = engine.shared().observed_rate.load(Ordering::Relaxed);
    let quantum_now = engine.graph_quantum();
    if rate_now != rate || quantum_now != quantum {
        return;
    }
    engine.store_live_cap(rate, quantum, enabled, lookahead, report.cap);
    let budget_us = report.budget.as_micros();
    eprintln!(
        "omamix: live cap {} at {rate} Hz quantum {quantum} budget_us {budget_us} master_us {} all_live_us {}",
        report.cap,
        join_us(&report.master_tries),
        join_us(&report.all_live_tries)
    );
}

fn join_us(tries: &[Duration]) -> String {
    let mut out = String::new();
    for (index, sample) in tries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&sample.as_micros().to_string());
    }
    out
}

fn log_callback_stats(shared: &LoopShared, filter: &RefCell<Option<MixFilter>>, force: bool) {
    let due = {
        let inner = shared.inner.borrow();
        force
            || inner
                .stats_logged
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(2))
    };
    if !due {
        return;
    }
    let summary = filter
        .borrow()
        .as_ref()
        .map(|engine| engine.shared().callback_summary());
    let Some((p50, p99, max, n, over)) = summary else {
        return;
    };
    if n == 0 && !force {
        return;
    }
    shared.inner.borrow_mut().stats_logged = Some(Instant::now());
    eprintln!("omamix: callback us p50 {p50} p99 {p99} max {max} n {n} over {over}");
}

fn drain_commands(
    shared: &LoopShared,
    filter: &RefCell<Option<MixFilter>>,
    monitor_on: &AtomicBool,
) -> bool {
    let mut commands = Vec::new();
    {
        let inner = shared.inner.borrow();
        while let Some(command) = inner.queue.try_recv() {
            commands.push(command);
        }
    }
    let mut quit = false;
    for command in commands {
        match command {
            EngineCommand::ReplaceScene(scene) => {
                let master = scene.master.clone();
                shared.inner.borrow_mut().scene = *scene;
                reapply_strip_controls(shared, filter);
                if let Some(filter) = filter.borrow_mut().as_mut() {
                    apply_saved_master(filter, &master);
                }
            }
            other => {
                if let Some(msg) = engine_command_msg(other)
                    && apply_loop_msg(&msg, shared, filter, monitor_on)
                {
                    quit = true;
                }
            }
        }
    }
    quit
}

fn reapply_strip_controls(shared: &LoopShared, filter: &RefCell<Option<MixFilter>>) {
    let (master_db, pairs) = {
        let inner = shared.inner.borrow();
        let mut pairs = Vec::new();
        for (peer, tap) in &inner.taps {
            let Some(slot) = tap.slot else {
                continue;
            };
            let Some(strip) = strip_for_peer(&inner, *peer) else {
                continue;
            };
            pairs.push((strip, slot));
        }
        (inner.scene.master.fader_db(), pairs)
    };
    let mut borrowed = filter.borrow_mut();
    let Some(engine) = borrowed.as_mut() else {
        return;
    };
    engine.shared().params.set_master_db(master_db);
    for (strip, slot) in pairs {
        apply_live_strip(engine, &strip, slot);
    }
}

fn apply_live_strip(engine: &mut MixFilter, strip: &Strip, slot: usize) {
    let Ok(slot) = u16::try_from(slot) else {
        return;
    };
    let bank = engine.shared();
    let _ = bank
        .strips
        .apply(slot, StripParamUpdate::FaderDb(strip.fader_db));
    let _ = bank
        .strips
        .apply(slot, StripParamUpdate::TrimDb(strip.input_gain_db));
    let _ = bank.strips.apply(slot, StripParamUpdate::Mute(strip.mute));
    let _ = bank.strips.apply(slot, StripParamUpdate::Solo(strip.solo));
    let _ = bank.strips.apply(slot, StripParamUpdate::Pan(strip.pan));
    let _ = bank
        .strips
        .apply(slot, StripParamUpdate::Enabled(strip.enabled));
    engine.sync_strip_eq(slot, strip.eq_enabled, strip.eq);
    engine.sync_strip_comp(slot, strip.comp);
}

fn apply_event(shared: &Rc<LoopShared>, event: Pending) {
    match event {
        Pending::Global(global) => note_global(shared, global),
        Pending::Remove(id) => note_remove(shared, id),
        Pending::Info { id, props } => note_info(shared, id, &props),
        Pending::LinkState { key, active, error } => note_link_state(shared, key, active, error),
        Pending::Metadata { key, value } => note_metadata(shared, &key, value.as_deref()),
        Pending::CoreDone { id, seq } => note_core_done(shared, id, seq),
    }
}

fn note_global(shared: &Rc<LoopShared>, global: GlobalObject<PropertiesBox>) {
    let props = global
        .props
        .as_ref()
        .map(|bag| dict_pairs(bag.dict()))
        .unwrap_or_default();
    let mut inner = shared.inner.borrow_mut();
    match global.type_ {
        pw::types::ObjectType::Node => {
            if !interesting_node(&props) {
                return;
            }
            let identity =
                NodeIdentity::from_props(props.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            inner.nodes.insert(
                global.id,
                NodeRec {
                    identity,
                    bound: false,
                },
            );
            if let Some(event) = link::registry_budget_event(link::RegistryKind::Node) {
                note_budget_event(&mut inner, event);
            }
            note_owned_ids(&mut inner);
        }
        pw::types::ObjectType::Port => {
            let port = port_from_props(global.id, &props);
            let node_id = port.node_id;
            inner.ports.insert(global.id, port);
            revive_peer(&mut inner, node_id);
        }
        pw::types::ObjectType::Link => {
            if let Some(edge) = edge_from_props(&props) {
                inner.edges.insert(global.id, edge);
                if let Some(event) = link::registry_budget_event(link::RegistryKind::Link) {
                    note_budget_event(&mut inner, event);
                }
            }
        }
        pw::types::ObjectType::Client => {
            remember_client(&mut inner, global.id, &props);
            if inner.own.is_none() && marks_our_client(&props, &inner.instance) {
                inner.own = Some(OwnClient {
                    client_id: global.id,
                    pid: std::process::id(),
                });
                note_owned_ids(&mut inner);
            }
        }
        pw::types::ObjectType::Metadata if prop(&props, "metadata.name") == Some("default") => {}
        _ => {}
    }
    let should_bind = match global.type_ {
        pw::types::ObjectType::Node => inner.nodes.contains_key(&global.id),
        pw::types::ObjectType::Client => true,
        pw::types::ObjectType::Metadata => prop(&props, "metadata.name") == Some("default"),
        pw::types::ObjectType::Port => inner.ports.get(&global.id).is_some_and(|port| {
            port.port_id.is_none() || (port.node_id == 0 && port.direction.is_empty())
        }),
        pw::types::ObjectType::Link => !inner.edges.contains_key(&global.id),
        _ => false,
    };
    if should_bind {
        inner.bind_queue.push(global);
    }
}

fn note_remove(shared: &LoopShared, id: u32) {
    let mut inner = shared.inner.borrow_mut();
    let removed_port_nodes: Vec<u32> = inner
        .ports
        .iter()
        .filter(|(gid, port)| **gid == id || port.node_id == id)
        .map(|(_, port)| port.node_id)
        .collect();
    let removed_edges: Vec<LinkEdge> = inner
        .edges
        .iter()
        .filter(|(gid, edge)| **gid == id || edge.output_node == id || edge.input_node == id)
        .map(|(_, edge)| *edge)
        .collect();
    let had_node = inner.nodes.remove(&id).is_some();
    inner
        .ports
        .retain(|gid, port| *gid != id && port.node_id != id);
    inner
        .edges
        .retain(|gid, edge| *gid != id && edge.output_node != id && edge.input_node != id);
    if had_node && let Some(event) = link::registry_budget_event(link::RegistryKind::Node) {
        note_budget_event(&mut inner, event);
    }
    if !removed_edges.is_empty()
        && let Some(event) = link::registry_budget_event(link::RegistryKind::Link)
    {
        note_budget_event(&mut inner, event);
    }
    let filter = inner.filter_node;
    if let Some(filter) = filter {
        let peers: Vec<u32> = removed_edges
            .iter()
            .filter(|edge| edge.input_node == filter)
            .map(|edge| edge.output_node)
            .collect();
        for peer in peers {
            silent_unlink_peer(&mut inner, peer);
        }
    }
    for node_id in removed_port_nodes {
        revive_peer(&mut inner, node_id);
    }
    if inner.mix_mic_node == Some(id) {
        inner.mix_mic_node = None;
    }
    if inner.filter_node == Some(id) {
        inner.filter_node = None;
    }
    if inner.clients.remove(&id).is_some() {
        route::forget_client(&mut inner.unlinked_clients, id);
    }
    if let Some(tap) = inner.taps.remove(&id) {
        if let Some(slot) = tap.slot {
            inner.retired_slots.push(slot);
        }
        for key in tap.link_keys {
            inner.links.remove(&key);
        }
    }
    inner.bindings.retain(|binding| binding.id != id);
}

fn note_info(shared: &LoopShared, id: u32, props: &[(String, String)]) {
    let mut inner = shared.inner.borrow_mut();
    let mut peer_port = None;
    if let Some(node) = inner.nodes.get_mut(&id) {
        let bound = NodeIdentity::from_props(props.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        node.identity.merge_bind(&bound);
        node.bound = true;
    }
    note_owned_ids(&mut inner);
    if inner.clients.contains_key(&id) {
        remember_client(&mut inner, id, props);
        let instance = inner.instance.clone();
        if inner.own.is_none() && marks_our_client(props, &instance) {
            inner.own = Some(OwnClient {
                client_id: id,
                pid: std::process::id(),
            });
            note_owned_ids(&mut inner);
        }
    }
    if let Some(port) = inner.ports.get_mut(&id) {
        let parsed = port_from_props(id, props);
        let mut changed = false;
        if port.node_id == 0 {
            port.node_id = parsed.node_id;
            if parsed.node_id != 0 {
                changed = true;
            }
        }
        if port.port_id.is_none() {
            port.port_id = parsed.port_id;
            if parsed.port_id.is_some() {
                changed = true;
            }
        }
        if port.direction.is_empty() {
            port.direction = parsed.direction;
            if !port.direction.is_empty() {
                changed = true;
            }
        }
        if port.name.is_empty() {
            port.name = parsed.name;
        }
        if port.channel.is_empty() {
            port.channel = parsed.channel;
            if !port.channel.is_empty() {
                changed = true;
            }
        }
        if changed {
            peer_port = Some(port.node_id);
        }
    }
    if let Some(node_id) = peer_port {
        revive_peer(&mut inner, node_id);
    }
    if let Some(edge) = edge_from_props(props)
        && let std::collections::hash_map::Entry::Vacant(slot) = inner.edges.entry(id)
    {
        slot.insert(edge);
        if let Some(event) = link::registry_budget_event(link::RegistryKind::Link) {
            note_budget_event(&mut inner, event);
        }
    }
}

fn fail_tap(tap: &mut Tap, peer: u32) {
    if link::fail_budget(
        &mut tap.phase,
        &mut tap.linked,
        &mut tap.saw_edges,
        &mut tap.tries,
        &mut tap.wait,
    ) && link::claim_stop_log(&mut tap.stop_logged)
    {
        eprintln!("omamix: tap for node {peer} stopped retrying");
    }
}

fn fail_engine(inner: &mut LoopInner) {
    let keys = std::mem::take(&mut inner.engine_link_keys);
    for key in keys {
        inner.links.remove(&key);
    }
    inner.engine_up = [false, false];
    inner.engine_linked = false;
    inner.engine_saw_edges = false;
    let before = inner.engine_tries;
    if before < link::RETRY_LIMIT {
        link::arm_retry(&mut inner.engine_tries, &mut inner.engine_wait);
    }
    if before < link::RETRY_LIMIT && inner.engine_tries >= link::RETRY_LIMIT {
        eprintln!("omamix: Mix Mic engine link stopped retrying");
    } else if before < link::RETRY_LIMIT {
        eprintln!("omamix: Mix Mic engine link failed; linking it again later");
    }
}

fn note_link_state(shared: &LoopShared, key: u64, active: bool, error: bool) {
    let mut inner = shared.inner.borrow_mut();
    if !inner.links.contains_key(&key) {
        return;
    }
    if let Some(index) = inner.engine_link_keys.iter().position(|item| *item == key) {
        let mut sides = link::EngineSides {
            up: inner.engine_up,
            proxies: if inner.engine_link_keys.len() >= 2 {
                2
            } else {
                inner.engine_link_keys.len() as u8
            },
        };
        let drop_proxies = link::note_engine_link(&mut sides, index, active, error);
        if drop_proxies {
            fail_engine(&mut inner);
            return;
        }
        inner.engine_up = sides.up;
        inner.engine_linked = sides.ready();
        if inner.engine_linked {
            inner.engine_tries = 0;
            inner.engine_wait = 0;
        }
        return;
    }
    let peer = inner
        .taps
        .iter()
        .find_map(|(peer, tap)| tap.link_keys.contains(&key).then_some(*peer));
    let Some(peer) = peer else {
        return;
    };
    let keys = {
        let Some(tap) = inner.taps.get_mut(&peer) else {
            return;
        };
        if error {
            fail_tap(tap, peer);
            std::mem::take(&mut tap.link_keys)
        } else if active && tap.phase == LinkPhase::Linking {
            tap.phase = link::advance_link(LinkPhase::Linking, true);
            link::note_tap_success(
                &mut tap.tries,
                &mut tap.wait,
                &mut tap.revivals,
                &mut tap.relinks,
            );
            Vec::new()
        } else {
            Vec::new()
        }
    };
    for key in keys {
        inner.links.remove(&key);
    }
}

fn bind_outstanding(shared: &Rc<LoopShared>) {
    let batch = std::mem::take(&mut shared.inner.borrow_mut().bind_queue);
    for global in batch {
        bind_one(shared, &global);
    }
}

fn bind_one(shared: &Rc<LoopShared>, global: &GlobalObject<PropertiesBox>) {
    {
        let inner = shared.inner.borrow();
        if inner.nodes.get(&global.id).is_some_and(|node| node.bound)
            && matches!(global.type_, pw::types::ObjectType::Node)
        {
            return;
        }
    }
    let registry_ptr = shared.inner.borrow().registry;
    let registry = unsafe { &*registry_ptr.cast::<pw::registry::Registry>() };
    let id = global.id;
    let shared_loop = Rc::clone(shared);
    let push_info = move |props: Vec<(String, String)>| {
        if let Ok(mut pending) = shared_loop.pending.lock() {
            pending.push(Pending::Info { id, props });
        }
    };
    let binding = match global.type_ {
        pw::types::ObjectType::Node => {
            let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                return;
            };
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    if let Some(props) = info.props() {
                        push_info(dict_pairs(props));
                    }
                })
                .register();
            Binding {
                id,
                _listener: ProxyListener::Node(listener),
                _proxy: ProxyKeep::Node(node),
            }
        }
        pw::types::ObjectType::Client => {
            let Ok(client) = registry.bind::<pw::client::Client, _>(global) else {
                return;
            };
            let listener = client
                .add_listener_local()
                .info(move |info| {
                    if let Some(props) = info.props() {
                        push_info(dict_pairs(props));
                    }
                })
                .register();
            Binding {
                id,
                _listener: ProxyListener::Client(listener),
                _proxy: ProxyKeep::Client(client),
            }
        }
        pw::types::ObjectType::Port => {
            let Ok(port) = registry.bind::<pw::port::Port, _>(global) else {
                return;
            };
            let listener = port
                .add_listener_local()
                .info(move |info| {
                    let mut props = info.props().map(dict_pairs).unwrap_or_default();
                    props.push(("port.id".into(), info.id().to_string()));
                    let direction = match info.direction() {
                        pw::spa::utils::Direction::Input => "in",
                        pw::spa::utils::Direction::Output => "out",
                        _ => "",
                    };
                    if !direction.is_empty() {
                        props.push(("port.direction".into(), direction.into()));
                    }
                    push_info(props);
                })
                .register();
            Binding {
                id,
                _listener: ProxyListener::Port(listener),
                _proxy: ProxyKeep::Port(port),
            }
        }
        pw::types::ObjectType::Metadata => {
            let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) else {
                return;
            };
            let shared_loop = Rc::clone(shared);
            let listener = metadata
                .add_listener_local()
                .property(move |_subject, key, _type, value| {
                    let Some(key) = key else {
                        return 0;
                    };
                    if let Ok(mut pending) = shared_loop.pending.lock() {
                        pending.push(Pending::Metadata {
                            key: key.to_string(),
                            value: value.map(str::to_string),
                        });
                    }
                    0
                })
                .register();
            Binding {
                id,
                _listener: ProxyListener::Metadata(listener),
                _proxy: ProxyKeep::Metadata(metadata),
            }
        }
        pw::types::ObjectType::Link => {
            let Ok(link) = registry.bind::<pw::link::Link, _>(global) else {
                return;
            };
            let listener = link
                .add_listener_local()
                .info(move |info| {
                    let props = vec![
                        ("link.output.node".into(), info.output_node_id().to_string()),
                        ("link.input.node".into(), info.input_node_id().to_string()),
                        ("link.output.port".into(), info.output_port_id().to_string()),
                        ("link.input.port".into(), info.input_port_id().to_string()),
                    ];
                    push_info(props);
                })
                .register();
            Binding {
                id,
                _listener: ProxyListener::Link(listener),
                _proxy: ProxyKeep::Link(link),
            }
        }
        _ => return,
    };
    shared.inner.borrow_mut().bindings.push(binding);
    if let Some(node) = shared.inner.borrow_mut().nodes.get_mut(&id) {
        node.bound = true;
    }
}

fn reconcile(shared: &Rc<LoopShared>, filter: &Rc<RefCell<Option<MixFilter>>>) {
    let peers = {
        let inner = shared.inner.borrow();
        if inner.own.is_none() || inner.mix_mic_node.is_none() || inner.filter_node.is_none() {
            return;
        }
        wanted_peers(&inner)
    };
    let stale: Vec<u32> = {
        let inner = shared.inner.borrow();
        inner
            .taps
            .keys()
            .copied()
            .filter(|peer| !peers.contains(peer))
            .collect()
    };
    if !stale.is_empty() {
        let mut inner = shared.inner.borrow_mut();
        for peer in stale {
            let mix = inner.mix_mic_node.unwrap_or(0);
            let node_refs = node_refs(&inner);
            let edges: Vec<LinkEdge> = inner.edges.values().copied().collect();
            let graph = LoopGraph {
                mix_mic_node: mix,
                nodes: &node_refs,
                links: &edges,
            };
            let looped = mix != 0 && route::upstream_includes_mix_mic(peer, &graph);
            let client_id = inner
                .nodes
                .get(&peer)
                .and_then(|node| node.identity.client_id);
            let own_id = inner.own.map(|own| own.client_id);
            let had_tap = inner.taps.contains_key(&peer);
            let shared_client = had_tap
                && mix != 0
                && client_id.is_some_and(|id| {
                    Some(id) != own_id && route::client_captures_mix_mic(id, &graph)
                });
            if shared_client && let Some(id) = client_id {
                if route::claim_client_once(&mut inner.unlinked_clients, id) {
                    eprintln!(
                        "omamix: unlinking client {id}: a stream of that client captures Mix Mic"
                    );
                }
            } else if looped && inner.refused.insert(peer, true).is_none() {
                let name = inner
                    .nodes
                    .get(&peer)
                    .map(|node| node.identity.node_name.clone())
                    .unwrap_or_default();
                eprintln!("omamix: unlinking node {peer} ({name}): upstream includes Mix Mic");
            }
            drop_tap(&mut inner, peer);
        }
    }
    for peer in peers {
        ensure_tap(shared, filter, peer);
    }
}

fn wanted_peers(inner: &LoopInner) -> Vec<u32> {
    let Some(mix) = inner.mix_mic_node else {
        return Vec::new();
    };
    let Some(own) = inner.own else {
        return Vec::new();
    };
    let node_refs: Vec<NodeRef> = inner
        .nodes
        .iter()
        .map(|(id, node)| NodeRef {
            id: *id,
            client_id: node.identity.client_id,
        })
        .collect();
    let edges: Vec<LinkEdge> = inner.edges.values().copied().collect();
    let graph = LoopGraph {
        mix_mic_node: mix,
        nodes: &node_refs,
        links: &edges,
    };
    let mut peers = Vec::new();
    let default_name = resolved_default_name(inner);
    for strip in &inner.scene.strips {
        match &strip.source {
            SourceKind::CaptureDevice { node_name } => {
                push_named_device(&mut peers, inner, &graph, mix, node_name);
            }
            SourceKind::DefaultSource => {
                if let Some(name) = default_name.as_deref() {
                    push_named_device(&mut peers, inner, &graph, mix, name);
                }
            }
            SourceKind::Application(_) => {
                for id in inner.nodes.keys().copied().collect::<Vec<_>>() {
                    if id == mix {
                        continue;
                    }
                    let stream = playback_of(inner, id);
                    if route::strip_should_attach(strip, &stream, Some(&own), Some(&graph)) {
                        peers.push(id);
                    }
                }
            }
            SourceKind::OtherDesktopAudio => {}
        }
    }
    if inner
        .scene
        .strips
        .iter()
        .any(|strip| matches!(strip.source, SourceKind::OtherDesktopAudio))
    {
        let streams: Vec<PlaybackStream> = inner
            .nodes
            .keys()
            .map(|id| playback_of(inner, *id))
            .collect();
        for stream in inner
            .scene
            .other_desktop_streams(&streams, Some(&own), Some(&graph))
        {
            if let Some(id) = stream.node_id {
                peers.push(id);
            }
        }
    }
    peers.sort_unstable();
    peers.dedup();
    peers
}

fn push_named_device(
    peers: &mut Vec<u32>,
    inner: &LoopInner,
    graph: &LoopGraph<'_>,
    mix: u32,
    node_name: &str,
) {
    for (id, node) in &inner.nodes {
        if *id == mix || node.identity.node_name != node_name {
            continue;
        }
        if !route::is_capture_device_class(&node.identity.media_class) {
            continue;
        }
        if node.identity.omamix_owned || node.identity.node_name == MIX_MIC_NODE_NAME {
            continue;
        }
        if inner.filter_node == Some(*id) {
            continue;
        }
        if route::upstream_includes_mix_mic(*id, graph) {
            continue;
        }
        peers.push(*id);
    }
}

fn ensure_tap(shared: &Rc<LoopShared>, filter: &Rc<RefCell<Option<MixFilter>>>, peer: u32) {
    let open = filter
        .borrow()
        .as_ref()
        .is_some_and(|engine| engine.cap_settled());
    if !open {
        return;
    }
    relink_if_edges_gone(shared, peer);
    let refuse = {
        let inner = shared.inner.borrow();
        let mix = inner.mix_mic_node.unwrap_or(0);
        let refs = node_refs(&inner);
        let edges: Vec<LinkEdge> = inner.edges.values().copied().collect();
        let graph = LoopGraph {
            mix_mic_node: mix,
            nodes: &refs,
            links: &edges,
        };
        let name = inner
            .nodes
            .get(&peer)
            .map(|node| node.identity.node_name.clone())
            .unwrap_or_default();
        if mix != 0 && route::upstream_includes_mix_mic(peer, &graph) {
            Some(name)
        } else {
            None
        }
    };
    if let Some(name) = refuse {
        let mut inner = shared.inner.borrow_mut();
        if inner.refused.insert(peer, true).is_none() {
            eprintln!("omamix: not tapping node {peer} ({name}): upstream includes Mix Mic");
        }
        drop_tap(&mut inner, peer);
        return;
    }

    let (phase, slotted, peer_ports, our_ports) = {
        let mut inner = shared.inner.borrow_mut();
        if let Some(tap) = inner.taps.get_mut(&peer)
            && !link::allow_attempt(
                &mut tap.phase,
                &mut tap.linked,
                &mut tap.saw_edges,
                tap.tries,
                &mut tap.wait,
            )
        {
            return;
        }
        let slotted = inner.taps.get(&peer).is_some_and(|tap| tap.slot.is_some());
        let phase = inner
            .taps
            .get(&peer)
            .map(|tap| tap.phase)
            .unwrap_or(link::start_link(false));
        let endpoints = port_endpoints(&inner);
        let peer_ports = link::stereo_output_ports(&endpoints, peer);
        let our_ports = our_input_ports(&inner, peer);
        (phase, slotted, peer_ports, our_ports)
    };

    if !slotted && peer_ports.is_some() {
        let result = filter
            .borrow_mut()
            .as_mut()
            .ok_or(())
            .and_then(|engine| engine.add_tap_ports(peer));
        let mut inner = shared.inner.borrow_mut();
        let tap = inner.taps.entry(peer).or_insert(Tap {
            phase: link::start_link(false),
            slot: None,
            linked: false,
            saw_edges: false,
            link_keys: Vec::new(),
            tries: 0,
            wait: 0,
            revivals: 0,
            relinks: 0,
            stop_logged: false,
        });
        match result {
            Ok(slot) => tap.slot = Some(slot),
            Err(()) => {
                fail_tap(tap, peer);
                return;
            }
        }
        let saved = strip_for_peer(&inner, peer);
        let slot = inner.taps.get(&peer).and_then(|tap| tap.slot);
        drop(inner);
        if let Some(slot) = slot
            && let Some(strip) = saved
            && let Some(engine) = filter.borrow_mut().as_mut()
        {
            apply_saved_strip(engine, &strip, slot);
        }
    }

    let ports_ready = peer_ports.is_some() && our_ports.is_some();
    let mut phase = phase;
    if phase == LinkPhase::WaitingForPorts {
        phase = link::advance_link(phase, ports_ready);
    } else if phase == LinkPhase::Linking && !ports_ready {
        phase = link::advance_link(phase, false);
    }
    {
        let mut inner = shared.inner.borrow_mut();
        let tap = inner.taps.entry(peer).or_insert(Tap {
            phase,
            slot: None,
            linked: false,
            saw_edges: false,
            link_keys: Vec::new(),
            tries: 0,
            wait: 0,
            revivals: 0,
            relinks: 0,
            stop_logged: false,
        });
        tap.phase = phase;
        if tap.linked || tap.phase != LinkPhase::Linking {
            return;
        }
        if peer_ports.is_none() || our_ports.is_none() {
            return;
        }
    }
    let (Some((out_fl, out_fr)), Some((in_fl, in_fr))) = (peer_ports, our_ports) else {
        return;
    };
    let filter_node = shared.inner.borrow().filter_node;
    let Some(filter_node) = filter_node else {
        return;
    };
    let pairs = [(out_fl, in_fl), (out_fr, in_fr)];
    let mut created = Vec::new();
    for (output_port, input_port) in pairs {
        match create_link(shared, peer, filter_node, output_port, input_port) {
            Ok(key) => created.push(key),
            Err(()) => {
                let mut inner = shared.inner.borrow_mut();
                if let Some(tap) = inner.taps.get_mut(&peer) {
                    fail_tap(tap, peer);
                    let keys = std::mem::take(&mut tap.link_keys);
                    for key in keys.into_iter().chain(created.drain(..)) {
                        inner.links.remove(&key);
                    }
                }
                return;
            }
        }
    }
    let mut inner = shared.inner.borrow_mut();
    if let Some(tap) = inner.taps.get_mut(&peer) {
        tap.linked = true;
        tap.link_keys.extend(created);
    }
}

fn create_link(
    shared: &Rc<LoopShared>,
    peer: u32,
    mix: u32,
    output_port: u32,
    input_port: u32,
) -> Result<u64, ()> {
    let core_ptr = shared.inner.borrow().core;
    let core = unsafe { &*core_ptr.cast::<pw::core::Core>() };
    let mut props = PropertiesBox::new();
    props.insert("link.output.node", peer.to_string());
    props.insert("link.output.port", output_port.to_string());
    props.insert("link.input.node", mix.to_string());
    props.insert("link.input.port", input_port.to_string());
    for (key, value) in link::link_properties() {
        props.insert(key, value);
    }
    let link = core
        .create_object::<pw::link::Link>("link-factory", &props)
        .map_err(|_| ())?;
    let key = {
        let mut inner = shared.inner.borrow_mut();
        let key = inner.next_link_key;
        inner.next_link_key = inner.next_link_key.wrapping_add(1);
        key
    };
    let shared_loop = Rc::clone(shared);
    let listener = link
        .add_listener_local()
        .info(move |info| {
            let error = matches!(info.state(), pw::link::LinkState::Error(_));
            let active = matches!(
                info.state(),
                pw::link::LinkState::Active | pw::link::LinkState::Paused
            );
            if let Ok(mut pending) = shared_loop.pending.lock() {
                pending.push(Pending::LinkState { key, active, error });
            }
        })
        .register();
    shared.inner.borrow_mut().links.insert(
        key,
        LiveLink {
            _listener: listener,
            _link: link,
        },
    );
    let _ = peer;
    Ok(key)
}

fn publish_ready(shared: &LoopShared) {
    let mut inner = shared.inner.borrow_mut();
    if inner.ready_sent || !inner.engine_linked {
        return;
    }
    let (Some(node_id), Some(own)) = (inner.mix_mic_node, inner.own) else {
        return;
    };
    if node_id == 0 {
        return;
    }
    inner.ready_sent = true;
    if let Some(tx) = inner.ready_tx.take() {
        let _ = tx.send(Boot::Ready(Ready {
            node_id,
            client_id: own.client_id,
        }));
    }
}

fn port_endpoints(inner: &LoopInner) -> Vec<PortEndpoint> {
    inner
        .ports
        .values()
        .filter_map(|port| {
            let id = port.port_id?;
            Some(PortEndpoint {
                id,
                node_id: port.node_id,
                direction: port.direction.clone(),
                channel: channel_label(&port.channel, &port.name),
            })
        })
        .collect()
}

fn our_input_ports(inner: &LoopInner, peer: u32) -> Option<(u32, u32)> {
    let mix = inner.filter_node?;
    let fl_name = format!("in-{peer}-FL");
    let fr_name = format!("in-{peer}-FR");
    let find = |name: &str| {
        inner.ports.values().find_map(|port| {
            if port.node_id == mix && port.name == name {
                port.port_id
            } else {
                None
            }
        })
    };
    Some((find(&fl_name)?, find(&fr_name)?))
}

fn port_from_props(global_id: u32, props: &[(String, String)]) -> PortRec {
    let _ = global_id;
    PortRec {
        node_id: prop(props, "node.id")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        port_id: prop(props, "port.id").and_then(|v| v.parse().ok()),
        direction: prop(props, "port.direction").unwrap_or("").to_string(),
        name: prop(props, "port.name").unwrap_or("").to_string(),
        channel: prop(props, "audio.channel").unwrap_or("").to_string(),
    }
}

fn edge_from_props(props: &[(String, String)]) -> Option<LinkEdge> {
    Some(LinkEdge {
        output_node: prop(props, "link.output.node")?.parse().ok()?,
        input_node: prop(props, "link.input.node")?.parse().ok()?,
    })
}

/// Mix Mic and the mix engine are ours only when the node carries
/// `omamix.owned` and its `client.id` is the client this process opened.
/// A foreign node that reuses the name is ignored.
fn note_owned_ids(inner: &mut LoopInner) {
    let Some(own_id) = inner.own.map(|own| own.client_id) else {
        return;
    };
    inner.mix_mic_node =
        select_owned_id(&inner.nodes, inner.mix_mic_node, own_id, MIX_MIC_NODE_NAME);
    inner.filter_node = select_owned_id(
        &inner.nodes,
        inner.filter_node,
        own_id,
        contract::FILTER_NODE_NAME,
    );
}

/// Keep an id from `pw_filter_get_node_id` until the registry describes that
/// node. A foreign node that reuses the name does not replace it. An unbound
/// node that still carries our name waits for bind before it can be dropped.
fn select_owned_id(
    nodes: &HashMap<u32, NodeRec>,
    recorded: Option<u32>,
    own_id: u32,
    name: &str,
) -> Option<u32> {
    if let Some(id) = nodes
        .iter()
        .find_map(|(id, node)| owned_name(node, own_id, name).then_some(*id))
    {
        return Some(id);
    }
    let id = recorded?;
    let Some(node) = nodes.get(&id) else {
        return Some(id);
    };
    if !node.bound
        && node.identity.node_name == name
        && node.identity.client_id.unwrap_or(own_id) == own_id
    {
        return Some(id);
    }
    None
}

fn owned_name(node: &NodeRec, own_id: u32, name: &str) -> bool {
    node.identity.omamix_owned
        && node.identity.client_id == Some(own_id)
        && node.identity.node_name == name
}

fn interesting_node(props: &[(String, String)]) -> bool {
    match prop(props, "media.class") {
        Some(class) if class.starts_with("Video") || class.starts_with("Midi") => false,
        Some(class) if class.contains("Audio") || class.contains("Stream") => true,
        None => true,
        Some(_) => false,
    }
}

fn marks_our_client(props: &[(String, String)], instance: &str) -> bool {
    if prop(props, INSTANCE_KEY) == Some(instance) {
        return true;
    }
    let pid = std::process::id().to_string();
    prop(props, "application.name") == Some("omamix")
        && prop(props, "application.process.id") == Some(pid.as_str())
}

fn prop<'a>(props: &'a [(String, String)], key: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn dict_pairs(dict: &pw::spa::utils::dict::DictRef) -> Vec<(String, String)> {
    dict.iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn channel_label(channel: &str, name: &str) -> String {
    if !channel.is_empty() {
        return channel.to_string();
    }
    for label in ["FL", "FR", "MONO", "FC", "AUX"] {
        if name == label
            || name.ends_with(&format!("_{label}"))
            || name.ends_with(&format!("-{label}"))
        {
            return label.to_string();
        }
    }
    String::new()
}

fn node_refs(inner: &LoopInner) -> Vec<NodeRef> {
    inner
        .nodes
        .iter()
        .map(|(id, node)| NodeRef {
            id: *id,
            client_id: node.identity.client_id,
        })
        .collect()
}

fn playback_of(inner: &LoopInner, id: u32) -> PlaybackStream {
    let Some(node) = inner.nodes.get(&id) else {
        let mut missing = PlaybackStream::app("", "");
        missing.media_class.clear();
        missing.binary.clear();
        return missing;
    };
    let mut stream = node.identity.playback_stream(id);
    if stream.binary.is_empty() {
        let client_binary = stream
            .client_id
            .and_then(|client_id| inner.clients.get(&client_id))
            .map(|client| client.binary.as_str())
            .unwrap_or("");
        stream.binary = route::resolve_binary(&stream.binary, client_binary);
    }
    stream
}

fn remember_client(inner: &mut LoopInner, id: u32, props: &[(String, String)]) {
    let rec = inner.clients.entry(id).or_default();
    if let Some(binary) = prop(props, "application.process.binary")
        && !binary.is_empty()
        && rec.binary.is_empty()
    {
        rec.binary = binary.to_string();
    }
    if let Some(name) = prop(props, "application.name")
        && !name.is_empty()
        && rec.application_name.is_empty()
    {
        rec.application_name = name.to_string();
    }
}

fn drop_tap(inner: &mut LoopInner, peer: u32) {
    let Some(tap) = inner.taps.remove(&peer) else {
        return;
    };
    if let Some(slot) = tap.slot {
        inner.retired_slots.push(slot);
    }
    for key in tap.link_keys {
        inner.links.remove(&key);
    }
}

fn retire_ports(shared: &LoopShared, filter: &RefCell<Option<MixFilter>>) {
    let slots = std::mem::take(&mut shared.inner.borrow_mut().retired_slots);
    if slots.is_empty() {
        return;
    }
    {
        let mut filter = filter.borrow_mut();
        let Some(engine) = filter.as_mut() else {
            return;
        };
        for slot in slots {
            engine.remove_tap_ports(slot);
        }
    }
    note_budget_event(&mut shared.inner.borrow_mut(), link::BudgetEvent::SlotFreed);
}

fn note_budget_event(inner: &mut LoopInner, event: link::BudgetEvent) {
    for tap in inner.taps.values_mut() {
        let _ = link::apply_budget_event(
            event,
            tap.phase,
            &mut tap.tries,
            &mut tap.wait,
            &mut tap.saw_edges,
            &mut tap.revivals,
        );
    }
}

fn revive_peer(inner: &mut LoopInner, node_id: u32) {
    let Some(event) = link::port_budget_event(node_id, inner.filter_node, inner.mix_mic_node)
    else {
        return;
    };
    if let Some(tap) = inner.taps.get_mut(&node_id) {
        let _ = link::apply_budget_event(
            event,
            tap.phase,
            &mut tap.tries,
            &mut tap.wait,
            &mut tap.saw_edges,
            &mut tap.revivals,
        );
    }
}

fn silent_unlink_peer(inner: &mut LoopInner, peer: u32) {
    let keys = {
        let Some(tap) = inner.taps.get_mut(&peer) else {
            return;
        };
        if !link::note_silent_unlink(
            &mut tap.phase,
            &mut tap.linked,
            &mut tap.saw_edges,
            &mut tap.tries,
            &mut tap.wait,
            &mut tap.relinks,
        ) {
            return;
        }
        if tap.tries >= link::RETRY_LIMIT && link::claim_stop_log(&mut tap.stop_logged) {
            eprintln!("omamix: tap for node {peer} stopped retrying");
        }
        std::mem::take(&mut tap.link_keys)
    };
    for key in keys {
        inner.links.remove(&key);
    }
}

fn note_metadata(shared: &LoopShared, key: &str, value: Option<&str>) {
    let name = value.and_then(metadata::metadata_node_name);
    let mut inner = shared.inner.borrow_mut();
    if key == metadata::AUDIO_SOURCE_KEY {
        if name.as_deref() == Some(MIX_MIC_NODE_NAME) {
            eprintln!(
                "omamix: warning: Mix Mic is the default source, so the Mix Mic node is being removed. A stored choice of Mix Mic does this on every start. Pick another source, then start omamix again."
            );
            inner.source_seen = true;
            inner.default_source = name;
            inner.quit = true;
            if !inner.ready_sent
                && let Some(tx) = inner.ready_tx.take()
            {
                let _ = tx.send(Boot::Failed(
                    "Mix Mic is the default source, so it was not left published".into(),
                ));
            }
            return;
        }
        if inner.source_seen
            && let DefaultDeviceObservation::Changed { now } =
                metadata::observe_default_source(inner.default_source.as_deref(), name.as_deref())
        {
            eprintln!("omamix: default source is now {now}");
        }
        inner.source_seen = true;
        inner.default_source = name;
    } else if key == metadata::CONFIGURED_AUDIO_SOURCE_KEY {
        inner.configured_seen = true;
        inner.configured_source = name;
    } else if key == "default.audio.sink" {
        if inner.sink_seen
            && let DefaultDeviceObservation::Changed { now } =
                metadata::observe_default_sink(inner.default_sink.as_deref(), name.as_deref())
        {
            eprintln!("omamix: default sink is now {now}");
        }
        inner.sink_seen = true;
        inner.default_sink = name;
    }
}

fn scene_needs_device_check(scene: &Scene) -> bool {
    scene.strips.iter().any(|strip| {
        matches!(
            strip.source,
            SourceKind::CaptureDevice { .. } | SourceKind::DefaultSource
        )
    })
}

fn advance_settle(shared: &LoopShared) {
    let timed_out = {
        let inner = shared.inner.borrow();
        inner.settle_round < 3 && inner.started.elapsed() >= Duration::from_secs(2)
    };
    if timed_out {
        shared.inner.borrow_mut().settle_round = 3;
        return;
    }
    let round = {
        let mut inner = shared.inner.borrow_mut();
        if !inner.saw_settle_done {
            return;
        }
        inner.saw_settle_done = false;
        inner.settle_round
    };
    if round == 1 {
        match issue_sync(shared) {
            Some(seq) => {
                let mut inner = shared.inner.borrow_mut();
                inner.settle_round = 2;
                inner.settle_pending = Some(seq);
            }
            None => shared.inner.borrow_mut().settle_round = 3,
        }
    } else if round == 2 {
        shared.inner.borrow_mut().settle_round = 3;
    }
}

fn issue_sync(shared: &LoopShared) -> Option<pw::spa::utils::result::AsyncSeq> {
    let core_ptr = shared.inner.borrow().core;
    let core = unsafe { &*core_ptr.cast::<pw::core::Core>() };
    core.sync(0).ok()
}

fn note_core_done(shared: &LoopShared, id: u32, seq: pw::spa::utils::result::AsyncSeq) {
    if id != pw::core::PW_ID_CORE {
        return;
    }
    let mut inner = shared.inner.borrow_mut();
    if inner.settle_pending == Some(seq) {
        inner.saw_settle_done = true;
        inner.settle_pending = None;
    }
}

fn list_if_ready(shared: &LoopShared) -> bool {
    let mut inner = shared.inner.borrow_mut();
    if inner.list_tx.is_none() || inner.settle_round < 3 {
        return false;
    }
    let listed = listed_nodes(&inner);
    if let Some(tx) = inner.list_tx.take() {
        let _ = tx.send(listed);
    }
    true
}

fn reject_devices_if_ready(shared: &LoopShared) -> bool {
    let ready = {
        let inner = shared.inner.borrow();
        inner.list_tx.is_none() && !inner.strips_checked && inner.settle_round >= 3
    };
    if !ready {
        return false;
    }
    let result = {
        let mut inner = shared.inner.borrow_mut();
        resolve_requested_devices(&mut inner)
    };
    match result {
        Ok(()) => {
            shared.inner.borrow_mut().strips_checked = true;
            false
        }
        Err(message) => {
            fail_usage(shared, &message);
            true
        }
    }
}

fn resolve_requested_devices(inner: &mut LoopInner) -> Result<(), String> {
    if inner.fallback_missing_devices {
        let catalog = catalog_nodes(inner);
        let missing = route::retarget_absent_devices(&mut inner.scene.strips, &catalog);
        for name in missing {
            eprintln!(
                "omamix: warning: device strip '{name}' is not on this machine, using the default source"
            );
        }
    }
    let wants_default = inner
        .scene
        .strips
        .iter()
        .any(|strip| matches!(strip.source, SourceKind::DefaultSource));
    if wants_default {
        let snapshot = metadata::DefaultSourceSnapshot {
            audio_source: if inner.source_seen {
                inner.default_source.clone()
            } else {
                None
            },
            configured_audio_source: if inner.configured_seen {
                inner.configured_source.clone()
            } else {
                None
            },
        };
        let name = metadata::resolve_default_source(&snapshot)
            .map_err(|_| "no default audio source".to_string())?;
        if !inner.source_seen {
            inner.source_seen = true;
        }
        for strip in &mut inner.scene.strips {
            if matches!(strip.source, SourceKind::DefaultSource) {
                strip.name.clone_from(&name);
                strip.source = SourceKind::CaptureDevice {
                    node_name: name.clone(),
                };
            }
        }
    }
    let catalog = catalog_nodes(inner);
    let wanted: Vec<String> = inner
        .scene
        .strips
        .iter()
        .filter_map(|strip| match &strip.source {
            SourceKind::CaptureDevice { node_name } => Some(node_name.clone()),
            _ => None,
        })
        .collect();
    let missing = route::absent_device_names(&wanted, &catalog);
    if missing.is_empty() {
        Ok(())
    } else if missing.len() == 1 {
        Err(format!("device strip '{}' was not found", missing[0]))
    } else {
        let names = missing
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("device strips {names} were not found"))
    }
}

fn strip_for_peer(inner: &LoopInner, peer: u32) -> Option<Strip> {
    let name = inner
        .nodes
        .get(&peer)
        .map(|node| node.identity.node_name.as_str())
        .unwrap_or("");
    if let Some(strip) = inner.scene.strips.iter().find(|strip| {
        matches!(&strip.source, SourceKind::CaptureDevice { node_name } if node_name == name)
    }) {
        return Some(strip.clone());
    }
    let stream = playback_of(inner, peer);
    if let Some(strip) = inner.scene.strips.iter().find(|strip| {
        matches!(&strip.source, SourceKind::Application(key) if key.matches_stream(&stream))
    }) {
        return Some(strip.clone());
    }
    if resolved_default_name(inner).is_some_and(|default_name| default_name == name)
        && let Some(strip) = inner
            .scene
            .strips
            .iter()
            .find(|strip| matches!(strip.source, SourceKind::DefaultSource))
    {
        return Some(strip.clone());
    }
    if inner
        .scene
        .is_in_other_desktop(&stream, inner.own.as_ref(), None)
    {
        inner
            .scene
            .strips
            .iter()
            .find(|strip| matches!(strip.source, SourceKind::OtherDesktopAudio))
            .cloned()
    } else {
        None
    }
}

fn apply_saved_strip(engine: &mut MixFilter, strip: &Strip, slot: usize) {
    let Ok(slot) = u16::try_from(slot) else {
        return;
    };
    let bank = engine.shared();
    if strip.fader_db != 0.0 {
        let _ = bank
            .strips
            .apply(slot, StripParamUpdate::FaderDb(strip.fader_db));
    }
    if strip.input_gain_db != 0.0 {
        let _ = bank
            .strips
            .apply(slot, StripParamUpdate::TrimDb(strip.input_gain_db));
    }
    if strip.mute {
        let _ = bank.strips.apply(slot, StripParamUpdate::Mute(true));
    }
    if strip.solo {
        let _ = bank.strips.apply(slot, StripParamUpdate::Solo(true));
    }
    if strip.pan != 0.0 {
        let _ = bank.strips.apply(slot, StripParamUpdate::Pan(strip.pan));
    }
    if !strip.enabled {
        let _ = bank.strips.apply(slot, StripParamUpdate::Enabled(false));
    }
    engine.sync_strip_eq(slot, strip.eq_enabled, strip.eq);
    engine.sync_strip_comp(slot, strip.comp);
}

fn apply_saved_master(engine: &mut MixFilter, master: &MasterBus) {
    if master.fader_db() != 0.0 {
        engine.shared().params.set_master_db(master.fader_db());
    }
    engine.sync_master_eq(master.eq_enabled, master.eq);
    engine.sync_master_comp(master.comp);
    let mut limit = master.limit;
    if limit.enabled() != master.limiter_enabled {
        limit.set_enabled(master.limiter_enabled);
    }
    if limit.ceiling_db() != master.limiter_ceiling_dbtp {
        limit.set_ceiling_db(master.limiter_ceiling_dbtp);
    }
    if !limit_is_factory(limit) {
        engine.apply_saved_master_limit(limit);
    }
    engine.publish_chain(&master.chain);
}

fn limit_is_factory(settings: LimitSettings) -> bool {
    settings.enabled()
        && settings.ceiling_db() == LIMIT_CEILING_DEFAULT_DB
        && settings.lookahead_ms() == LIMIT_LOOKAHEAD_DEFAULT_MS
}

fn catalog_nodes(inner: &LoopInner) -> Vec<route::CatalogNode> {
    inner
        .nodes
        .values()
        .map(|node| {
            let mut application_name = node.identity.application_name.clone();
            let mut binary = node.identity.binary.clone();
            if let Some(client) = node
                .identity
                .client_id
                .and_then(|id| inner.clients.get(&id))
            {
                if application_name.is_empty() {
                    application_name.clone_from(&client.application_name);
                }
                if binary.is_empty() {
                    binary = route::resolve_binary("", &client.binary);
                }
            }
            route::CatalogNode {
                node_name: node.identity.node_name.clone(),
                description: node.identity.description.clone(),
                media_class: node.identity.media_class.clone(),
                application_name,
                binary,
                client_id: node.identity.client_id,
                pid: node.identity.pid,
                omamix_owned: node.identity.omamix_owned,
            }
        })
        .collect()
}

fn listed_nodes(inner: &LoopInner) -> Vec<route::ListedNode> {
    route::listable_nodes(&catalog_nodes(inner), inner.own.as_ref())
}

fn fail_usage(shared: &LoopShared, message: &str) {
    let mut inner = shared.inner.borrow_mut();
    inner.quit = true;
    if let Some(tx) = inner.ready_tx.take() {
        let _ = tx.send(Boot::Usage(message.to_string()));
    }
}

fn resolved_default_name(inner: &LoopInner) -> Option<String> {
    let snapshot = metadata::DefaultSourceSnapshot {
        audio_source: if inner.source_seen {
            inner.default_source.clone()
        } else {
            None
        },
        configured_audio_source: if inner.configured_seen {
            inner.configured_source.clone()
        } else {
            None
        },
    };
    metadata::resolve_default_source(&snapshot).ok()
}

fn fail_closed(shared: &LoopShared, message: &str) {
    {
        let mut inner = shared.inner.borrow_mut();
        inner.quit = true;
        if let Some(tx) = inner.ready_tx.take() {
            let _ = tx.send(Boot::Failed(message.to_string()));
        }
    }
    if let Some(view) = &shared.view {
        view.note_failure(message);
    }
}

fn ensure_published(
    shared: &Rc<LoopShared>,
    filter: &Rc<RefCell<Option<MixFilter>>>,
    monitor_keep: &Rc<RefCell<Option<MonitorKeep>>>,
    monitor_on: &Arc<AtomicBool>,
) {
    let ready = {
        let inner = shared.inner.borrow();
        inner.list_tx.is_none()
            && inner.source_seen
            && inner.strips_checked
            && !inner.published
            && !inner.quit
    };
    if !ready || filter.borrow().is_some() {
        return;
    }
    let core_ptr = shared.inner.borrow().core;
    let core = unsafe { &*core_ptr.cast::<pw::core::Core>() };
    let tone = shared.inner.borrow().tone;
    let mut filter_props = PropertiesBox::new();
    for (key, value) in contract::filter_properties() {
        filter_props.insert(key, value);
    }
    let mut engine = match MixFilter::create(core, filter_props, tone) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("omamix: filter: {err}");
            fail_closed(shared, "mix engine was not created");
            return;
        }
    };
    let master = shared.inner.borrow().scene.master.clone();
    apply_saved_master(&mut engine, &master);
    let _ = engine.events_version();
    let shared_audio = engine.shared();
    let filter_id = engine.node_id();
    *filter.borrow_mut() = Some(engine);
    if let Some(id) = filter_id {
        shared.inner.borrow_mut().filter_node = Some(id);
    }

    let mut adapter_props = crate::backend::mix_mic_properties();
    adapter_props.insert("factory.name", "support.null-audio-sink");
    match core.create_object::<pw::node::Node>("adapter", &adapter_props) {
        Ok(node) => {
            let mut inner = shared.inner.borrow_mut();
            inner.adapter = Some(node);
            inner.published = true;
        }
        Err(err) => {
            eprintln!("omamix: mix mic: {err}");
            fail_closed(shared, "Mix Mic was not created");
            return;
        }
    }

    if monitor_on.load(Ordering::Relaxed) && monitor_keep.borrow().is_none() {
        match connect_monitor(core, shared_audio, Arc::clone(monitor_on)) {
            Ok((stream, listener)) => {
                let stream: pw::stream::StreamBox<'static> = unsafe { std::mem::transmute(stream) };
                *monitor_keep.borrow_mut() = Some(MonitorKeep {
                    _listener: listener,
                    _stream: stream,
                });
            }
            Err(err) => eprintln!("omamix: monitor stream: {err}"),
        }
    }
}

fn link_engine(shared: &Rc<LoopShared>) {
    let (filter_node, mix) = {
        let inner = shared.inner.borrow();
        (inner.filter_node, inner.mix_mic_node)
    };
    let (Some(filter_node), Some(mix)) = (filter_node, mix) else {
        return;
    };
    let due = {
        let mut inner = shared.inner.borrow_mut();
        if !inner.engine_link_keys.is_empty() {
            if inner.engine_linked {
                let count = inner
                    .edges
                    .values()
                    .filter(|edge| edge.output_node == filter_node && edge.input_node == mix)
                    .count();
                if count > 0 {
                    inner.engine_saw_edges = true;
                    return;
                }
                if !inner.engine_saw_edges {
                    return;
                }
                eprintln!("omamix: Mix Mic engine link went away; linking it again");
                let keys = std::mem::take(&mut inner.engine_link_keys);
                for key in keys {
                    inner.links.remove(&key);
                }
                inner.engine_linked = false;
                inner.engine_saw_edges = false;
                inner.engine_up = [false, false];
            } else {
                return;
            }
        }
        link::retry_due(inner.engine_tries, &mut inner.engine_wait)
    };
    if !due {
        return;
    }
    let endpoints = {
        let inner = shared.inner.borrow();
        port_endpoints(&inner)
    };
    let Some((out_fl, out_fr)) = link::stereo_output_ports(&endpoints, filter_node) else {
        return;
    };
    let Some((in_fl, in_fr)) = link::stereo_input_ports(&endpoints, mix) else {
        return;
    };
    let mut created = Vec::new();
    for (output_port, input_port) in [(out_fl, in_fl), (out_fr, in_fr)] {
        match create_link(shared, filter_node, mix, output_port, input_port) {
            Ok(key) => created.push(key),
            Err(()) => {
                let mut inner = shared.inner.borrow_mut();
                for key in created {
                    inner.links.remove(&key);
                }
                fail_engine(&mut inner);
                return;
            }
        }
    }
    let mut inner = shared.inner.borrow_mut();
    let mut sides = link::EngineSides {
        up: inner.engine_up,
        proxies: 0,
    };
    link::engine_proxies_created(&mut sides);
    inner.engine_up = sides.up;
    inner.engine_link_keys.extend(created);
    inner.engine_linked = false;
}

fn relink_if_edges_gone(shared: &LoopShared, peer: u32) {
    let mut inner = shared.inner.borrow_mut();
    let filter_node = inner.filter_node;
    let count = inner
        .edges
        .values()
        .filter(|edge| edge.output_node == peer && Some(edge.input_node) == filter_node)
        .count();
    let keys = {
        let Some(tap) = inner.taps.get_mut(&peer) else {
            return;
        };
        if !link::observe_edges(
            &mut tap.linked,
            &mut tap.saw_edges,
            &mut tap.phase,
            &mut tap.tries,
            &mut tap.wait,
            &mut tap.relinks,
            count,
        ) {
            return;
        }
        if tap.tries >= link::RETRY_LIMIT && link::claim_stop_log(&mut tap.stop_logged) {
            eprintln!("omamix: tap for node {peer} stopped retrying");
        } else if tap.tries < link::RETRY_LIMIT {
            eprintln!("omamix: tap link for node {peer} went away; linking it again");
        }
        std::mem::take(&mut tap.link_keys)
    };
    for key in keys {
        inner.links.remove(&key);
    }
}

#[cfg(test)]
mod owned_id_tests {
    use super::*;

    fn node(name: &str, client: Option<u32>, owned: bool, bound: bool) -> NodeRec {
        NodeRec {
            identity: NodeIdentity {
                node_name: name.to_string(),
                client_id: client,
                omamix_owned: owned,
                ..NodeIdentity::default()
            },
            bound,
        }
    }

    #[test]
    fn foreign_name_does_not_replace_our_filter_id() {
        let mut nodes = HashMap::new();
        nodes.insert(9, node(contract::FILTER_NODE_NAME, Some(77), false, true));
        nodes.insert(4, node(contract::FILTER_NODE_NAME, Some(3), true, true));
        assert_eq!(
            select_owned_id(&nodes, Some(4), 3, contract::FILTER_NODE_NAME),
            Some(4)
        );
        let mut foreign_recorded = HashMap::new();
        foreign_recorded.insert(9, node(MIX_MIC_NODE_NAME, Some(77), true, true));
        assert_eq!(
            select_owned_id(&foreign_recorded, Some(9), 3, MIX_MIC_NODE_NAME),
            None
        );
    }

    #[test]
    fn unbound_filter_id_waits_for_client_and_owned() {
        let mut nodes = HashMap::new();
        nodes.insert(4, node(contract::FILTER_NODE_NAME, None, false, false));
        assert_eq!(
            select_owned_id(&nodes, Some(4), 3, contract::FILTER_NODE_NAME),
            Some(4)
        );
        nodes.get_mut(&4).unwrap().bound = true;
        assert_eq!(
            select_owned_id(&nodes, Some(4), 3, contract::FILTER_NODE_NAME),
            None
        );
    }
}

#[cfg(test)]
mod lookahead_token_tests {
    use super::{StdinCmd, bypass_limiter_block, clamp_lookahead_token, parse_stdin_line};
    use omamix_core::effect::{EffectKind, MasterChain};

    #[test]
    fn stdin_lookahead_rejects_non_finite_and_clamps_the_range() {
        assert_eq!(clamp_lookahead_token("5"), Some(5.0));
        assert_eq!(clamp_lookahead_token("0"), Some(0.0));
        assert_eq!(clamp_lookahead_token("20"), Some(20.0));
        assert_eq!(clamp_lookahead_token("-0.5"), Some(0.0));
        assert_eq!(clamp_lookahead_token("20.1"), Some(20.0));
        assert_eq!(clamp_lookahead_token("100"), Some(20.0));
        assert_eq!(clamp_lookahead_token("NaN"), None);
        assert_eq!(clamp_lookahead_token("nan"), None);
        assert_eq!(clamp_lookahead_token("inf"), None);
        assert_eq!(clamp_lookahead_token("Inf"), None);
        assert_eq!(clamp_lookahead_token("+inf"), None);
        assert_eq!(clamp_lookahead_token("-inf"), None);
        assert_eq!(clamp_lookahead_token("infinity"), None);
        assert_eq!(clamp_lookahead_token(""), None);
        assert_eq!(clamp_lookahead_token("nope"), None);
    }

    #[test]
    fn stdin_bypass_targets_the_limiter_block() {
        assert!(matches!(
            parse_stdin_line("bypass limiter"),
            Some(StdinCmd::BypassLimiter(true))
        ));
        assert!(matches!(
            parse_stdin_line("bypass limiter on"),
            Some(StdinCmd::BypassLimiter(true))
        ));
        assert!(matches!(
            parse_stdin_line("bypass limiter off"),
            Some(StdinCmd::BypassLimiter(false))
        ));
        assert!(parse_stdin_line("bypass").is_none());
        assert!(parse_stdin_line("bypass sink").is_none());
        assert!(parse_stdin_line("bypass limiter later").is_none());
        assert!(matches!(
            parse_stdin_line("lookahead 5"),
            Some(StdinCmd::Lookahead(ms)) if (ms - 5.0).abs() < 1.0e-6
        ));
        let mut chain = MasterChain::standard();
        assert!(bypass_limiter_block(&mut chain, true));
        assert!(
            chain
                .blocks()
                .iter()
                .any(|block| { block.kind() == EffectKind::Limiter && block.bypassed() })
        );
        assert!(
            chain
                .blocks()
                .iter()
                .any(|block| { block.kind() == EffectKind::Compressor && !block.bypassed() })
        );
        assert_eq!(chain.latency_samples(48_000, 256), 0);
        assert!(bypass_limiter_block(&mut chain, false));
        assert!(chain.blocks().iter().all(|block| !block.bypassed()));
        assert_eq!(chain.latency_samples(48_000, 256), 240);
    }
}

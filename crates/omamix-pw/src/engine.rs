//! PipeWire main loop on its own thread.
//!
//! Every PipeWire object lives on that thread. The control thread sends
//! [`LoopMsg`] through [`pipewire::channel`], which wakes the loop. A timer
//! on the same thread drains [`crate::rt::CommandQueue`]. `process()` does
//! neither: it writes the master [`crate::rt::AtomicMeter`], the per-strip
//! meters, and the scope rings, reads [`crate::rt::AtomicParams`] and the per-strip
//! [`omamix_core::params::StripParamBank`], and publishes the master quantum
//! in [`crate::rt::MixSlot`].
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omamix_core::graph::Scene;
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
}

fn engine_command_msg(command: EngineCommand) -> LoopMsg {
    match command {
        EngineCommand::Shutdown => LoopMsg::Shutdown,
        EngineCommand::SetMasterFaderDb(db) => LoopMsg::SetMasterFaderDb(db),
        EngineCommand::SetMonitorMix(on) => LoopMsg::SetMonitorMix(on),
        EngineCommand::SetStripParam { slot, update } => LoopMsg::SetStripParam { slot, update },
    }
}

struct Ready {
    node_id: u32,
    client_id: u32,
}

enum Boot {
    Ready(Ready),
    Failed(String),
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
    arm_signals();

    let duration = options.duration;
    let tone = options.tone;
    let thread = std::thread::Builder::new()
        .name("omamix-pw".into())
        .spawn(move || {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                pw_thread(options, pw_rx, ready_tx);
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
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = pw_tx.send(LoopMsg::Shutdown);
    let _ = thread.join();

    Ok(HeadlessReport {
        node_id: ready.node_id,
        client_id: ready.client_id,
    })
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

fn pw_thread(
    options: HeadlessOptions,
    pw_rx: pw::channel::Receiver<LoopMsg>,
    ready_tx: std::sync::mpsc::Sender<Boot>,
) {
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(loop_) => loop_,
        Err(err) => {
            eprintln!("omamix: main loop: {err}");
            return;
        }
    };
    let context = match pw::context::ContextBox::new(mainloop.loop_(), None) {
        Ok(context) => context,
        Err(err) => {
            eprintln!("omamix: context: {err}");
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
            return;
        }
    };
    let registry = match core.get_registry() {
        Ok(registry) => registry,
        Err(err) => {
            eprintln!("omamix: registry: {err}");
            return;
        }
    };

    let filter: Rc<RefCell<Option<MixFilter>>> = Rc::new(RefCell::new(None));
    let monitor_keep: Rc<RefCell<Option<MonitorKeep>>> = Rc::new(RefCell::new(None));

    let queue = CommandQueue::new();
    let _command_sender = queue.sender();
    let shared_loop = Rc::new(LoopShared {
        pending: Mutex::new(Vec::new()),
        inner: RefCell::new(LoopInner {
            instance,
            scene: options.scene,
            tone: options.tone,
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

    let monitor_on = Arc::new(AtomicBool::new(
        shared_loop.inner.borrow().scene.master.monitor_mix,
    ));

    let _attached = pw_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let filter = Rc::clone(&filter);
        let monitor_on = Arc::clone(&monitor_on);
        move |msg| {
            if apply_loop_msg(&msg, &filter, &monitor_on) {
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

    // Drop order after run() returns: timer, channel, monitor, listener,
    // shared_loop (links, bindings, then the filter Rc), registry, core.
    // object.linger=false plus the client disconnect removes anything still
    // exported. Drop the filter explicitly before the core.
    drop(timer);
    drop(_attached);
    drop(listener);
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
}

struct LoopInner {
    instance: String,
    scene: Scene,
    tone: bool,
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
    // Listener drops first (struct fields drop in declaration order).
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
    ensure_published(shared, filter, monitor_keep, monitor_on);
    if shared.inner.borrow().quit {
        return true;
    }
    link_engine(shared);
    reconcile(shared, filter);
    publish_ready(shared);
    quit || shared.inner.borrow().quit
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
        let msg = engine_command_msg(command);
        if apply_loop_msg(&msg, filter, monitor_on) {
            quit = true;
        }
    }
    quit
}

fn apply_event(shared: &Rc<LoopShared>, event: Pending) {
    match event {
        Pending::Global(global) => note_global(shared, global),
        Pending::Remove(id) => note_remove(shared, id),
        Pending::Info { id, props } => note_info(shared, id, &props),
        Pending::LinkState { key, active, error } => note_link_state(shared, key, active, error),
        Pending::Metadata { key, value } => note_metadata(shared, &key, value.as_deref()),
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
    // The registry pointer stays valid until pw_thread drops it, after this timer.
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
    for strip in &inner.scene.strips {
        match &strip.source {
            SourceKind::CaptureDevice { node_name } => {
                for (id, node) in &inner.nodes {
                    if *id == mix || node.identity.node_name != *node_name {
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
                    if route::upstream_includes_mix_mic(*id, &graph) {
                        continue;
                    }
                    peers.push(*id);
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

fn ensure_tap(shared: &Rc<LoopShared>, filter: &Rc<RefCell<Option<MixFilter>>>, peer: u32) {
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
    // The core pointer stays valid until pw_thread drops it, after this timer.
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
    if key == "default.audio.source" {
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

fn fail_closed(shared: &LoopShared, message: &str) {
    let mut inner = shared.inner.borrow_mut();
    inner.quit = true;
    if let Some(tx) = inner.ready_tx.take() {
        let _ = tx.send(Boot::Failed(message.to_string()));
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
        inner.source_seen && !inner.published && !inner.quit
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
    let engine = match MixFilter::create(core, filter_props, tone) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("omamix: filter: {err}");
            fail_closed(shared, "mix engine was not created");
            return;
        }
    };
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

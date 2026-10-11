//! PipeWire main loop on its own thread.
//!
//! Every PipeWire object lives on that thread. The control thread sends
//! [`LoopMsg`] through [`pipewire::channel`], which wakes the loop. A timer
//! on the same thread drains [`crate::rt::CommandQueue`]. `process()` does
//! neither: it writes [`crate::rt::AtomicMeter`], reads [`crate::rt::AtomicParams`],
//! and publishes the master quantum in [`crate::rt::MixSlot`].
//!
//! Mix Mic is the `pw_filter` node (`media.class` Audio/Source). Strip taps
//! are input ports on that node, linked from the peer's output ports.
//! Headphone sends and the monitor are separate `Stream/Output/Audio` streams.
//! They are not a second mixer. The monitor's `process` copies [`crate::rt::MixSlot`],
//! which is the master mix the filter just wrote. A headphone send must copy
//! that strip's own tap, not the master mix, because the master includes the
//! mic. This backend creates the monitor stream when the scene's `monitor_mix`
//! flag is set (`--monitor-mix`). It does not create headphone streams: there
//! is no per-strip slot yet, and playing MixSlot into a headphone send would
//! put the mic in the headphones. Neither stream sets `target.object` or
//! `AUTOCONNECT`, so neither attaches itself to a sink.
//!
//! Unit tests do not call [`run_headless`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omamix_core::graph::Scene;
use omamix_core::link::{self, LinkPhase, PortEndpoint};
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
}

/// Identifiers printed once the filter node is in the graph.
pub struct HeadlessReport {
    pub node_id: u32,
    pub client_id: u32,
    pub default_source_restored: bool,
    pub default_sink_restored: bool,
}

enum LoopMsg {
    Shutdown,
    /// Applied when a caller sends it. Headless shutdown is the message this
    /// process sends; the fader and monitor arms stay so the loop can apply
    /// them without a second channel.
    #[allow(dead_code)]
    SetMasterFaderDb(f32),
    #[allow(dead_code)]
    SetMonitorMix(bool),
}

struct Ready {
    node_id: u32,
    client_id: u32,
}

/// Open a client only when `publish_mix_mic` is set. Blocks until shutdown.
pub fn run_headless(options: HeadlessOptions) -> Result<HeadlessReport, EngineError> {
    if !options.publish_mix_mic {
        println!("omamix: headless, Mix Mic not requested");
        return Ok(HeadlessReport {
            node_id: 0,
            client_id: 0,
            default_source_restored: false,
            default_sink_restored: false,
        });
    }

    let saved_source = pactl_default("source");
    let saved_sink = pactl_default("sink");

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Ready>();
    let (pw_tx, pw_rx) = pw::channel::channel::<LoopMsg>();
    arm_signals();

    let duration = options.duration;
    let thread = std::thread::Builder::new()
        .name("omamix-pw".into())
        .spawn(move || {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                pw_thread(options, pw_rx, ready_tx);
            }));
        })
        .map_err(|err| EngineError::PipeWire(err.to_string()))?;

    let ready = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(ready) => ready,
        Err(_) => {
            let _ = pw_tx.send(LoopMsg::Shutdown);
            let _ = thread.join();
            return Err(EngineError::PipeWire(
                "Mix Mic was not ready within 5s".into(),
            ));
        }
    };
    println!(
        "omamix: Mix Mic node id {} client id {} priority.session=1 priority.driver=1",
        ready.node_id, ready.client_id
    );

    let mut source_changed = false;
    let mut sink_changed = false;
    let deadline = duration.map(|d| Instant::now() + d);
    loop {
        if STOP.swap(false, Ordering::SeqCst) {
            break;
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            break;
        }
        if defaults_drifted(&saved_source, "source") {
            source_changed = true;
            break;
        }
        if defaults_drifted(&saved_sink, "sink") {
            sink_changed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = pw_tx.send(LoopMsg::Shutdown);
    let _ = thread.join();

    let default_source_restored =
        restore_default(source_changed, "source", saved_source.as_deref());
    let default_sink_restored = restore_default(sink_changed, "sink", saved_sink.as_deref());

    Ok(HeadlessReport {
        node_id: ready.node_id,
        client_id: ready.client_id,
        default_source_restored,
        default_sink_restored,
    })
}

fn defaults_drifted(saved: &Option<String>, kind: &str) -> bool {
    match (saved, pactl_default(kind)) {
        (Some(saved), Some(now)) => saved != &now,
        _ => false,
    }
}

fn restore_default(changed: bool, kind: &str, saved: Option<&str>) -> bool {
    let Some(saved) = saved else {
        return false;
    };
    if !changed {
        return false;
    }
    let now = pactl_default(kind);
    if now.as_deref() == Some(saved) {
        return false;
    }
    let arg = format!("set-default-{kind}");
    let status = std::process::Command::new("pactl")
        .args([arg.as_str(), saved])
        .status();
    let restored = status.map(|s| s.success()).unwrap_or(false);
    if restored {
        eprintln!("omamix: restored default {kind} to {saved}");
    } else {
        eprintln!("omamix: default {kind} changed and could not be restored to {saved}");
    }
    restored
}

fn pactl_default(kind: &str) -> Option<String> {
    let output = std::process::Command::new("pactl")
        .arg(format!("get-default-{kind}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
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
    ready_tx: std::sync::mpsc::Sender<Ready>,
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

    let mut filter_props = PropertiesBox::new();
    for (key, value) in contract::filter_properties() {
        filter_props.insert(key, value);
    }
    let filter = match MixFilter::create(&core, filter_props) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!("omamix: filter: {err}");
            return;
        }
    };
    let _ = filter.events_version();
    let shared = filter.shared();
    let filter = Rc::new(RefCell::new(filter));

    let queue = CommandQueue::new();
    let _command_sender = queue.sender();
    let shared_loop = Rc::new(LoopShared {
        pending: Mutex::new(Vec::new()),
        inner: RefCell::new(LoopInner {
            instance,
            scene: options.scene,
            queue,
            ready_tx: Some(ready_tx),
            ready_sent: false,
            own: None,
            mix_mic_node: None,
            nodes: HashMap::new(),
            ports: HashMap::new(),
            edges: HashMap::new(),
            taps: HashMap::new(),
            refused: HashMap::new(),
            bind_queue: Vec::new(),
            bindings: Vec::new(),
            links: HashMap::new(),
            next_link_key: 1,
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
    let monitor = if monitor_on.load(Ordering::Relaxed) {
        match connect_monitor(&core, Arc::clone(&shared), Arc::clone(&monitor_on)) {
            Ok(pair) => Some(pair),
            Err(err) => {
                eprintln!("omamix: monitor stream: {err}");
                None
            }
        }
    } else {
        None
    };

    let _attached = pw_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let filter = Rc::clone(&filter);
        let monitor_on = Arc::clone(&monitor_on);
        move |msg| match msg {
            LoopMsg::Shutdown => mainloop.quit(),
            LoopMsg::SetMasterFaderDb(db) => filter.borrow().shared().params.set_master_db(db),
            LoopMsg::SetMonitorMix(on) => monitor_on.store(on, Ordering::Relaxed),
        }
    });

    let timer = mainloop.loop_().add_timer({
        let shared_loop = Rc::clone(&shared_loop);
        let filter = Rc::clone(&filter);
        let mainloop = mainloop.clone();
        let monitor_on = Arc::clone(&monitor_on);
        move |_| {
            if tick(&shared_loop, &filter, &monitor_on) {
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
    drop(monitor);
    drop(listener);
    drop(shared_loop);
    drop(filter);
    drop(registry);
    drop(core);
    drop(context);
    drop(mainloop);
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
}

struct LoopInner {
    instance: String,
    scene: Scene,
    queue: CommandQueue,
    ready_tx: Option<std::sync::mpsc::Sender<Ready>>,
    ready_sent: bool,
    own: Option<OwnClient>,
    mix_mic_node: Option<u32>,
    nodes: HashMap<u32, NodeRec>,
    ports: HashMap<u32, PortRec>,
    edges: HashMap<u32, LinkEdge>,
    taps: HashMap<u32, Tap>,
    refused: HashMap<u32, bool>,
    bind_queue: Vec<GlobalObject<PropertiesBox>>,
    bindings: Vec<Binding>,
    links: HashMap<u64, LiveLink>,
    next_link_key: u64,
    core: *mut pw::sys::pw_core,
    registry: *mut pw::sys::pw_registry,
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
    added_ports: bool,
    linked: bool,
    link_keys: Vec<u64>,
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
}

#[allow(dead_code)]
enum ProxyListener {
    Node(pw::node::NodeListener),
    Client(pw::client::ClientListener),
    Port(pw::port::PortListener),
    Link(pw::link::LinkListener),
}

fn tick(shared: &Rc<LoopShared>, filter: &Rc<RefCell<MixFilter>>, monitor_on: &AtomicBool) -> bool {
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
    reconcile(shared, filter);
    publish_ready(shared, filter);
    quit
}

fn drain_commands(
    shared: &LoopShared,
    filter: &Rc<RefCell<MixFilter>>,
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
    let params = filter.borrow().shared();
    for command in commands {
        match command {
            EngineCommand::Shutdown => quit = true,
            EngineCommand::SetMasterFaderDb(db) => params.params.set_master_db(db),
            EngineCommand::SetMonitorMix(on) => monitor_on.store(on, Ordering::Relaxed),
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
            if identity.node_name == MIX_MIC_NODE_NAME {
                inner.mix_mic_node = Some(global.id);
            }
            inner.nodes.insert(
                global.id,
                NodeRec {
                    identity,
                    bound: false,
                },
            );
        }
        pw::types::ObjectType::Port => {
            inner
                .ports
                .insert(global.id, port_from_props(global.id, &props));
        }
        pw::types::ObjectType::Link => {
            if let Some(edge) = edge_from_props(&props) {
                inner.edges.insert(global.id, edge);
            }
        }
        pw::types::ObjectType::Client if marks_our_client(&props, &inner.instance) => {
            inner.own = Some(OwnClient {
                client_id: global.id,
                pid: std::process::id(),
            });
        }
        pw::types::ObjectType::Client => {}
        _ => {}
    }
    let should_bind = match global.type_ {
        pw::types::ObjectType::Node => inner.nodes.contains_key(&global.id),
        pw::types::ObjectType::Client => true,
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
    inner.nodes.remove(&id);
    inner
        .ports
        .retain(|gid, port| *gid != id && port.node_id != id);
    inner
        .edges
        .retain(|gid, edge| *gid != id && edge.output_node != id && edge.input_node != id);
    if inner.mix_mic_node == Some(id) {
        inner.mix_mic_node = None;
    }
    let keys = if let Some(tap) = inner.taps.get_mut(&id) {
        let _ = link::peer_removed(tap.phase);
        tap.phase = LinkPhase::PeerVanished;
        std::mem::take(&mut tap.link_keys)
    } else {
        Vec::new()
    };
    for key in keys {
        inner.links.remove(&key);
    }
    inner.taps.remove(&id);
    inner.bindings.retain(|binding| binding.id != id);
}

fn note_info(shared: &LoopShared, id: u32, props: &[(String, String)]) {
    let mut inner = shared.inner.borrow_mut();
    if let Some(node) = inner.nodes.get_mut(&id) {
        let bound = NodeIdentity::from_props(props.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        node.identity.merge_bind(&bound);
        node.bound = true;
        if node.identity.node_name == MIX_MIC_NODE_NAME {
            inner.mix_mic_node = Some(id);
        }
    }
    let instance = inner.instance.clone();
    if marks_our_client(props, &instance) {
        inner.own = Some(OwnClient {
            client_id: id,
            pid: std::process::id(),
        });
    }
    if let Some(port) = inner.ports.get_mut(&id) {
        let parsed = port_from_props(id, props);
        if port.node_id == 0 {
            port.node_id = parsed.node_id;
        }
        if port.port_id.is_none() {
            port.port_id = parsed.port_id;
        }
        if port.direction.is_empty() {
            port.direction = parsed.direction;
        }
        if port.name.is_empty() {
            port.name = parsed.name;
        }
        if port.channel.is_empty() {
            port.channel = parsed.channel;
        }
    }
    if let Some(edge) = edge_from_props(props) {
        inner.edges.entry(id).or_insert(edge);
    }
}

fn note_link_state(shared: &LoopShared, key: u64, active: bool, error: bool) {
    let mut inner = shared.inner.borrow_mut();
    if !inner.links.contains_key(&key) {
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
            tap.phase = link::link_failed(tap.phase);
            tap.linked = false;
            std::mem::take(&mut tap.link_keys)
        } else if active && tap.phase == LinkPhase::Linking {
            tap.phase = link::advance_link(LinkPhase::Linking, true);
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

fn reconcile(shared: &Rc<LoopShared>, filter: &Rc<RefCell<MixFilter>>) {
    let peers = {
        let inner = shared.inner.borrow();
        if inner.own.is_none() || inner.mix_mic_node.is_none() {
            return;
        }
        wanted_peers(&inner)
    };
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
                    if !node.identity.media_class.contains("Source") {
                        continue;
                    }
                    if route::upstream_includes_mix_mic(*id, node.identity.client_id, &graph) {
                        continue;
                    }
                    peers.push(*id);
                }
            }
            SourceKind::Application(_) => {
                for (id, node) in &inner.nodes {
                    if *id == mix {
                        continue;
                    }
                    let stream = node.identity.playback_stream(*id);
                    if route::strip_should_attach(strip, &stream, Some(&own), Some(&graph)) {
                        peers.push(*id);
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
            .iter()
            .map(|(id, node)| node.identity.playback_stream(*id))
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

fn ensure_tap(shared: &Rc<LoopShared>, filter: &Rc<RefCell<MixFilter>>, peer: u32) {
    let refuse = {
        let inner = shared.inner.borrow();
        let mix = inner.mix_mic_node.unwrap_or(0);
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
        let client_id = inner
            .nodes
            .get(&peer)
            .and_then(|node| node.identity.client_id);
        let name = inner
            .nodes
            .get(&peer)
            .map(|node| node.identity.node_name.clone())
            .unwrap_or_default();
        if mix != 0 && route::upstream_includes_mix_mic(peer, client_id, &graph) {
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
        return;
    }

    let (phase, added, peer_ports, our_ports) = {
        let inner = shared.inner.borrow();
        if inner
            .taps
            .get(&peer)
            .is_some_and(|tap| matches!(tap.phase, LinkPhase::Failed | LinkPhase::PeerVanished))
        {
            return;
        }
        let added = inner.taps.get(&peer).is_some_and(|tap| tap.added_ports);
        let phase = inner
            .taps
            .get(&peer)
            .map(|tap| tap.phase)
            .unwrap_or(link::start_link(false));
        let endpoints = port_endpoints(&inner);
        let peer_ports = link::stereo_output_ports(&endpoints, peer);
        let our_ports = our_input_ports(&inner, peer);
        (phase, added, peer_ports, our_ports)
    };

    if !added && peer_ports.is_some() {
        let result = filter.borrow_mut().add_tap_ports(peer);
        let mut inner = shared.inner.borrow_mut();
        let tap = inner.taps.entry(peer).or_insert(Tap {
            phase: link::start_link(false),
            added_ports: false,
            linked: false,
            link_keys: Vec::new(),
        });
        match result {
            Ok(_) => tap.added_ports = true,
            Err(()) => {
                tap.phase = link::link_failed(tap.phase);
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
            added_ports: added || peer_ports.is_some(),
            linked: false,
            link_keys: Vec::new(),
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
    let mix = shared.inner.borrow().mix_mic_node;
    let Some(mix) = mix else {
        return;
    };
    let pairs = [(out_fl, in_fl), (out_fr, in_fr)];
    let mut created = Vec::new();
    for (output_port, input_port) in pairs {
        match create_link(shared, peer, mix, output_port, input_port) {
            Ok(key) => created.push(key),
            Err(()) => {
                let mut inner = shared.inner.borrow_mut();
                if let Some(tap) = inner.taps.get_mut(&peer) {
                    tap.phase = link::link_failed(tap.phase);
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

fn publish_ready(shared: &LoopShared, filter: &Rc<RefCell<MixFilter>>) {
    let mut inner = shared.inner.borrow_mut();
    if inner.ready_sent {
        return;
    }
    let from_filter = filter.borrow().node_id();
    if inner.mix_mic_node.is_none() {
        inner.mix_mic_node = from_filter;
    }
    let (Some(node_id), Some(own)) = (inner.mix_mic_node, inner.own) else {
        return;
    };
    if node_id == 0 {
        return;
    }
    inner.ready_sent = true;
    if let Some(tx) = inner.ready_tx.take() {
        let _ = tx.send(Ready {
            node_id,
            client_id: own.client_id,
        });
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
                channel: port.channel.clone(),
            })
        })
        .collect()
}

fn our_input_ports(inner: &LoopInner, peer: u32) -> Option<(u32, u32)> {
    let mix = inner.mix_mic_node?;
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

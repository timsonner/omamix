# Architecture

Scaffold decisions for omamix. The product requirements are in [SPEC.md](SPEC.md). The laptop measurement those decisions used is in [AUDIO-BASELINE.md](AUDIO-BASELINE.md).

## UI: egui / eframe

**Choice: egui 0.36 with eframe**, `crates/omamix`.

| Toolkit | Fit for this mixer |
| --- | --- |
| egui / eframe | Immediate-mode painting is the meter, waveform, and spectrum path. A fader is custom code. The wheel reads raw `MouseWheel` events and steps 0.5 dB per notch only while that fader is hovered and focused. eframe's default backend is winit + wgpu with Wayland enabled, which matches Hyprland. egui 0.36 has no `raw_scroll_delta`. |
| iced | A canvas widget can draw meters, and winit talks to Wayland, but a dense strip layout is a custom widget either way, with more architecture around each control. |
| gtk4-rs / libadwaita | gtk4 4.22.4 and libadwaita 1.9.3 are installed here, so this was a real option. `GtkScale` changes value when the wheel hovers it, which is the behavior the spec forbids, and live meters mean a custom snapshot widget plus a GTK main loop beside the PipeWire thread loop. libadwaita styling follows GNOME. |

Omarchy theme colors can be a palette table later. That is not in the scaffold.

The shell draws one placeholder strip. The wheel handler reads raw `MouseWheel` events (`input.raw.events`). Positive Y is wheel-up and raises the fader. A mouse notch is one `Line` and one step of 0.5 dB (`WHEEL_STEP_DB`). Trackpad `Point` events accumulate, and a step happens only when they reach 40 points (`POINTS_PER_NOTCH`, egui's native line scroll speed), so a swipe is not one notch per event. `apply_wheel` runs only when that fader is hovered and focused. A fader that still has focus from an earlier click does not move if the pointer is elsewhere. A click that only focuses the fader does not change its value (`PointerGesture::FocusClick`). Drag still moves the fader under the pointer. One notch below −60 dB is −∞. NaN dB becomes mute and +∞ clamps to the fader's max boost before it can become a gain. The GUI was not launched in the scaffold run.

## Graph and I/O: pipewire-rs

**Choice: the `pipewire` crate 0.10.1 (pipewire-rs), feature `v1_2_0`, behind `--features pipewire`.** That feature needs **libpipewire >= 1.2**. On libspa 1.0.x the same crate fails to compile with E0425 because `spa_meta_sync_timeline` is missing. `build.rs` runs `pkg-config --atleast-version=1.2 libpipewire-0.3` (honoring `$PKG_CONFIG`). Cargo runs that script in parallel with pipewire-rs, so a normal build prints this check. A `-j1` build can still show the libspa E0425 first. Ubuntu 24.04's apt package is libpipewire 1.0.5, which is too old. The app owns its nodes. DSP runs in-process behind [`DspStage`](../crates/omamix-core/src/dsp.rs).

### Why an app-owned filter, not module-remap-source or filter-chain

Tim reported that gpu-screen-recorder recorded silence when the default source was a pactl `module-remap-source`, and that naming the source explicitly worked. That module is a server-side Pulse compatibility node. It stays loaded until something unloads it, which fights the "pw-dump diff clean on quit" requirement, including on a crash. This session did not re-test gpu-screen-recorder.

The audio graph is one `pw_filter` (`ClockPlan::SingleFilter`) with one `process` callback, plus one adapter node in front of it. The public node is Mix Mic: `core.create_object` of the `adapter` factory with `factory.name` `support.null-audio-sink`. `mix_mic_properties()` is that node's bag (`media.class` `Audio/Source/Virtual`, `node.name` `omamix.mix-mic`). `factory.name` is set at creation and is not part of the contract bag. The filter is a separate node, `filter_properties()`: `node.name` `omamix.mix-engine`, `media.class` `Audio/DSP`. It is not an `Audio/Source`, `Audio/Source/Virtual`, `Audio/Duplex`, or `Stream/Output/Audio`, so it is not a default-source candidate and it is not tapped as desktop audio. Strip taps are input ports on the filter (`in-<peer>-FL` / `in-<peer>-FR`), linked from the peer's output ports. The filter's `capture_FL` / `capture_FR` outputs link into the adapter's input ports. A capture of Mix Mic pulls the adapter, which pulls the filter. pipewire-rs 0.10 has no `pw_filter` wrapper; `crates/omamix-pw/src/filter.rs` calls the C API. The filter flag is `PW_FILTER_FLAG_RT_PROCESS` only. `PW_FILTER_FLAG_DRIVER` is not set. PipeWire then marks the filter `node.want-driver` and points `node.driver-id` at Mix Mic. The `support.null-audio-sink` factory marks the adapter `node.driver`. A capture of Mix Mic clocks the adapter, which clocks the filter. Neither property bag writes `node.driver` or `node.want-driver`. Output ports are `format.dsp` `32 bit float mono audio`, with `PW_FILTER_PORT_FLAG_MAP_BUFFERS`.

Mix Mic properties:

- `media.class` = `Audio/Source/Virtual`
- `node.name` = `omamix.mix-mic`
- `node.description` = `Mix Mic`
- `node.virtual` = `true`
- `media.role` = `Production`
- `audio.channels` = `2`, `audio.position` = `FL,FR`
- `priority.session` = `1` and `priority.driver` = `1`
- `node.latency` = `1024/48000` (declared; the machine quantum was not measured)
- `node.dont-reconnect` = `true` and `node.autoconnect` = `false`
- `object.linger` = `false`
- `omamix.owned` = `true`
- `adapter.auto-port-config` and `node.param.PortConfig` create the stereo dsp input ports and the monitor outputs on this node. A session manager is not required for those ports. A recorder still needs a link to `capture_FL` / `capture_FR`. WirePlumber makes that link when it is present.
- `node.param.Props` starts at unity: volume 1.0, mute false, `channelVolumes` `[1.0, 1.0]`. That is 0 dB, not a microphone level. The user sets each input.

WirePlumber ranks a default-source candidate by `priority.session`, then `priority.driver`, otherwise 0. The best candidate wins. There is no fallback source at another priority (UNVERIFIED this round: the earlier read of WirePlumber 0.5.17 `rescan.lua`, `find-best-default-node.lua`, and `wireplumber.conf` was not repeated). Priority 1 loses to any higher candidate. If Mix Mic is the only `Audio/Source`, `Audio/Source/Virtual`, or `Audio/Duplex`, it becomes the default source. omamix then destroys the Mix Mic node. A stored choice does the same thing at startup: when `default.audio.source` is already `omamix.mix-mic`, the warning is logged and the node is removed right away, before it is left published. Pick another source, then start omamix again. Removing it leaves WirePlumber's runtime `default.audio.source`. `default.configured.*` is never written. The unplug check is `scripts/unplug-watch.sh`, not a unit test. `priority.driver` is 1 on Mix Mic and on the filter, so Mix Mic loses a driver election against an ALSA device. The adapter still carries `node.driver` from the null-audio-sink factory, which is how a capture of Mix Mic gets a clock. The filter is the follower (`node.want-driver`, `node.driver-id` equal to the Mix Mic node). `node.dont-reconnect` and `node.autoconnect=false` tell the session manager not to rebind the node or to pull existing clients onto it.

omamix binds the PipeWire metadata object named `default` and listens to `property` events for `default.audio.source` and `default.audio.sink`. It never calls `set_property`. `allow_metadata_write` is a trimmed, case-insensitive allow list, and that list is empty, so every metadata write is refused. Mix Mic is created only after `default.audio.source` has been read. If that key never arrives, Mix Mic is not published. The first read is not logged as a change. A later change is logged. If the default source name is `omamix.mix-mic`, omamix logs a warning and destroys the Mix Mic node. That includes the first read, so a stored choice of Mix Mic tears the node down as soon as omamix starts. The session default stays where the session manager left it. The Mix Mic id and the filter id are accepted only when the node has `omamix.owned`, this process's `client.id`, and the matching `node.name`. A foreign node that reuses either name does not replace the id from `pw_filter_get_node_id`.

A source scan in the `metadata` tests reads `crates/`, `scripts/` except `scripts/prove.sh`, and `PKGBUILD`. `scripts/prove.sh` is the private-socket procedure: it links and it writes the private default, and a separate test checks that it does not set a login default, a volume, or a mute. It flags a fixed list: default-device write spellings, the metadata, link, and `pw-cli` command names, `systemctl`, `amixer`, `alsactl`, joined config-directory paths, a `.join` of `wireplumber`, `pipewire`, or `hypr` with or without space after the parenthesis, the method form `path.push` and the UFCS forms `PathBuf::push` and `Path::join`, a raw string of those directory names, a const passed to `.join`, `set_param` and `set_property` in every file, `Command` aliases including `<Command>::new`, `concat!` and slice `.join` strings that assemble those tokens, `format!` the same way, `libc::system`, `libc::fork`, `libc::vfork`, `nix::unistd::fork`, `execv`, `execvp`, and `posix_spawn`. `build.rs` may spawn only `pkg-config`. The scan does not prove that every shell-out is absent, and it does not prove that every split string outside those patterns is caught.

Mix Mic's filter connects with a SPA Props pod: `channelVolumes` `[1.0, 1.0]`, `mute` false, and the matching soft-volume fields. The public adapter carries the same unity values in `node.param.Props`. `process` writes the samples itself and does not multiply by the filter pod. `--tone` adds a 440 Hz sine at amplitude 0.2 on Mix Mic only, for a recorder aimed at that node. That amplitude is the test tone, not a microphone gain.

Headphone sends and the monitor of the final mix are separate `Stream/Output/Audio` streams on the same client. They are not a second mixer and they are not the filter. The filter `process` writes the master quantum into a seqlock (`MixSlot`). The monitor stream's `process` only copies that slot. A headphone send must copy that strip's tap, not the master mix, because the master includes the mic. `--monitor-mix` creates the monitor stream. Headphone streams are not created yet: there is no per-strip slot, and copying `MixSlot` into headphones would play the mic. Both property bags set `node.autoconnect=false`, `node.dont-reconnect=true`, `object.linger=false`, and `omamix.owned=true`, and neither sets `target.object` or `PW_STREAM_FLAG_AUTOCONNECT`, so they do not attach to a sink. They can run on the sink's driver and may repeat or skip a quantum relative to the filter.

Easy Effects, while it was still installed, published its virtual mic as `media.class = Audio/Source/Virtual` with `factory.name = support.null-audio-sink` (`easyeffects_source`). A bare `pw_filter` exposes `format.dsp` ports. WirePlumber does not register that as a Pulse source (`no usable format found`), so `pactl list sources` omits it and a recorder's `--target` falls back to the default source. The adapter node is what shows up in source pickers. It is still not `module-remap-source`. The gsr test in [TESTING.md](TESTING.md) is what decides.

Teardown runs on the loop thread before `mainloop.quit()` returns and the thread drops the core: link proxies, the monitor stream, registry bindings, then the filter (`pw_filter_disconnect`, remove the hook, `pw_filter_destroy`). `object.linger=false` is on the filter, the monitor, and every link. Dropping the core disconnects the client, which removes what that client created. SIGINT and SIGTERM only set an atomic; the control thread then sends `Shutdown` on the PipeWire channel. `process()` catches panics so they do not unwind across the C callback. A hard kill still depends on `object.linger=false` and the client disconnecting when the process dies. The GUI does not open a client (`NullEngine`). `omamix --headless` without `--mix-mic` does not either. Unit tests do not call `run_headless`.

### Per-app capture and re-attach

Playback apps show up as `Stream/Output/Audio` nodes. Node ids change every time the app starts. A strip stores a `RouteKey` of `application.name`, `application.process.binary`, and `media.name` when a tab or window needs to stay distinct. Those three are the re-attach key: a restarted app has a new pid and a new object serial and still matches (`RouteKey::matches_stream`). Pid and object serial are not required for that match. They only separate two streams that are live together (`disambiguates_live`). A node global does include `client.id`, `object.serial`, `node.name`, and `media.class`. It does not include `application.process.binary`, `media.name`, `application.process.id`, or `omamix.owned`. The backend binds the node for those. Whether a node is ours is our `client.id`: the client is created with `application.name=omamix`, `application.process.binary=omamix`, and `omamix.instance=<pid>-<nanos>`, and the Client global whose instance matches is ours, recorded once. `omamix.instance` can be copied onto another client. `omamix.owned` is still what keeps our nodes out of the mix, together with our `client.id` and pid. A node info event does not overwrite that client id. On `global_remove` the strip stays and the link attempt is dropped (`LinkPhase::PeerVanished`); a later global starts a new attempt.

The tap is an input port on the Mix Mic filter, not a second `pw_stream` (a stream would be a second clock). `app_capture_properties` still records the contract that must not be used to steal playback: `media.class` `Stream/Input/Audio`, `node.passive`, `node.autoconnect=false`, `node.dont-reconnect`, `stream.dont-remix`, `object.linger=false`, `omamix.owned`, and no `target.object` or `stream.capture.sink`. `target.object` on a capture stream is how WirePlumber steals a playback app off the user's sink. The link is additive: the app keeps its link to the sink, and the filter listens as well.

The optional "all other desktop audio" strip attaches each remaining playback stream with that same per-stream tap (`Scene::other_desktop_streams`). It does not set `stream.capture.sink` or `target.object`, so it does not monitor the sink. A stream that already has an `Application` strip is left out of "other", which keeps it from being mixed twice. `feeds_mix_mic` refuses a stream unless `media.class` is `Stream/Output/Audio`. An empty class fails closed, so `Stream/Input/Audio` capture nodes stay out. It also refuses `omamix.owned`, our `client.id` or pid, and any node [`upstream_includes_mix_mic`](../crates/omamix-core/src/route.rs) can reach from Mix Mic, including through intermediate nodes. The walk is per node. The automatic strip and an application strip with an empty `media.name` also refuse every stream of a client that captures Mix Mic, including a playback node with no PipeWire edge of its own. An application strip whose route key includes `media.name` stays on the per-node walk, so one named tab can stay a strip while another tab of the same client captures Mix Mic. A missing `node_id` fails closed when a graph is present. `application.process.binary` is read from the client object when the node omits it. On every link-added event the loop walks again and drops a tap whose upstream now includes Mix Mic, and it removes that tap's filter ports. Headphone sends and the monitor share our client, so they stay out of Mix Mic. `owned_output_feeds_mix_mic` reads the property bag and is false while `omamix.owned=true` is in that bag.

Capture devices are stored by `node.name`, which is stable across reboots for ALSA nodes. The name is whatever that machine enumerates. The app does not assume a particular device.

### DSP

v1 effects, in order: noise reduction, high-pass, 10-band graphic EQ, compressor, limiter. The master adds EQ, compressor, and a limiter that defaults on at −1 dBTP. Input trim (±24 dB) and the fader sit outside that list.

**Choice: an in-process `DspChain` of `DspStage`s.** The scaffold's strip template is five identity stages in that order, plus a real `GainStage` so tests can see enabled stages run and disabled stages get skipped. Constant-power pan is a pure function.

A PipeWire `filter-chain` module, or a general LV2 host, was the other option:

- `libpipewire-module-filter-chain.so` is on disk, and SPA filter-graph plugins exist for builtin, ebur128, ladspa, lv2, and sofa. None of those modules were loaded in the daemon during the baseline.
- Loading filter-chain is a server module. Dynamic strips mean rebuilding that graph, and a crash can leave it resident. That is the same teardown class as `module-remap-source`.
- lilv 0.28.0 and LV2 1.18.10 **specs** are installed. There are no LSP, LADSPA, or DeepFilterNet plugin binaries on this machine. A filter-chain or LV2 host would have nothing useful to instantiate for the v1 effects.
- `rnnoise` 0.2 is installed as a C library (`/usr/include/rnnoise.h`, `-lrnnoise`). Noise reduction should call that library from a `DspStage` later, in-process.
- `libebur128` 1.2.6 is installed. The loudness meter can wrap it later. The scaffold does not link it yet.

When Tim installs LSP or an LV2 noise plugin, a stage can host it through lilv without changing the graph model. That dependency is not in the scaffold.

## Real-time thread

The PipeWire main loop runs on its own thread (`omamix-pw`). That thread owns the context, the core, the registry, the filter, and any monitor stream. PipeWire's `Rc` handles are not `Send`, so they are not shared with the control thread.

The control thread sends `Shutdown` (and can send a master-fader or monitor flag) through `pipewire::channel`. That write wakes the loop. The callback runs on the loop thread and applies the message. A timer on the same thread drains `CommandQueue`, which is what `CommandSender::submit` fills. `process()` does not receive on either channel. It writes `AtomicMeter` (peak and RMS) and the `MixSlot`, and it reads `AtomicParams`. The master gain in `AtomicParams` starts at unity: there is no built-in mic boost, and the value is not taken from any one machine's speech level. `NullEngine::drive` is the same drain, and it is what tests call. The GUI still holds a `NullEngine` and does not start the PipeWire thread. `omamix --headless --mix-mic` does.

## Clocking

A capture device's clock and a playback app's clock are not the same device. On XPS15-OMARCHY one example of the capture device is that laptop's USB interface. A PipeWire link does not resample. Linked nodes join one driver group. The USB mic and each app stream rate-match inside their own nodes and adapters. `process()` does not resample; it sums the quantum it was given.

**Choice: one `pw_filter`, one `process` callback (`ClockPlan::SingleFilter`).** That filter is the internal mix engine. Mix Mic is the adapter it feeds. `PW_FILTER_FLAG_DRIVER` is not set, and neither property bag writes `node.driver` or `node.want-driver`. PipeWire fills the split: the null-audio-sink factory sets `node.driver` on Mix Mic, and the filter gets `node.want-driver` with `node.driver-id` pointing at Mix Mic. `priority.driver` is 1 on both nodes, so Mix Mic loses a driver election against an ALSA device. Capturing Mix Mic clocks the adapter, which clocks the filter. Strip taps are input ports on the filter. A separate `pw_stream` per strip would put each strip on its peer's driver. Mix Mic declares `node.latency = 1024/48000` so consumers see a latency; that fraction is not a measured quantum (see the unverified list in AUDIO-BASELINE).

Headphone sends and the monitor are the exception that stays off this clock. Each is its own playback stream. Its `process` copies a slot the filter already wrote. If that stream's driver is the sink, it can repeat or skip a quantum relative to the filter. The monitor copies the master `MixSlot`. A headphone send copies the strip tap, not the master, so the mic is not in the headphones.

## Browser identity and the Mix Mic loop

Chromium tabs and browser studios share `application.name` and `application.process.binary`. A `RouteKey` that only has those two claims every stream of that app, which is why "other desktop" then leaves all of them out: better to miss a tab than to mix it twice. The key also stores `media.name`, pid, and object serial. `matches_stream` uses `media.name` when the key has one, so two tabs can be different strips once the UI writes that down. It does not require pid or object serial; those only split two streams that are live at the same time (`disambiguates_live`).

**Loop guard.** `feeds_mix_mic` returns false when the playback node's `client.id` or pid is [`OwnClient`](../crates/omamix-core/src/route.rs), when `omamix.owned` is set, or when `upstream_includes_mix_mic` walks the link graph and finds Mix Mic upstream of that node. `tap_allowed` with `TapScope::SharedClient` also refuses every stream of a client when any node of that client is Mix Mic or is fed by Mix Mic. That scope is the automatic "other desktop" strip and an application strip whose route key has an empty `media.name`. An application strip whose route key includes `media.name` stays on the per-node walk, so one named tab can stay a strip while another tab of the same client captures Mix Mic. Node globals include `client.id`. Binary, `media.name`, pid, and `omamix.owned` arrive on bind. If a graph is present and the stream has no `node_id`, the walk cannot run and the stream is refused.

## Linking

On `global`, the loop thread records the node. Ports carry `node.id`, `port.id`, `port.direction`, and `audio.channel`. `stereo_output_ports` picks FL/FR, or two unlabeled outputs in `port.id` order, or one unlabeled mono port used for both sides. Link factories take that `port.id`, not the registry global id.

`start_link(false)` stays in `LinkPhase::WaitingForPorts` until the peer's output ports and the filter's `in-<peer>-FL/FR` ports exist. `advance_link` moves Waiting to Linking. The loop thread then calls `link-factory` with `object.linger=false`. Active or paused moves Linking to Linked and clears that tap's try count and wait, the same way a live engine link clears its counters. If the link call fails, `link_failed` goes to `LinkPhase::Failed`. The attempt then retries with a doubling backoff of 1, 2, 4, and 8 ticks of the 50 ms timer, and stops after four failures. Freeing a mixer slot, or a change to that peer's own ports, gives an exhausted failure a new budget of four attempts, at most twice since the last active or paused link. A successful link clears that revival count. A link this process added or removed does not refresh the budget, and neither does another node appearing or disappearing. The line `stopped retrying` is logged once for that peer. A tap that is still inside its backoff keeps that wait. If a link disappears without an error, the tap is unlinked and the same budget decides whether it is tried again. Each time a link that the registry had already shown disappears, a relink count increases. After four, the next wait is at least 16 ticks, and a later success does not clear that count. Reusing a retired filter port instead of freeing it is future hardening: the current path clears the slot on the data loop and then frees the port. `PeerVanished` does not retry. Failure clears `saw_edges`, so a retry is not torn down before the registry shows the new edges. The loop guard runs before any of those retries: a peer whose upstream includes Mix Mic, or a shared client that captures Mix Mic, is dropped rather than linked again. If the peer disappears, `peer_removed` goes to `PeerVanished`, the link proxy is dropped, and a later global for a new node id starts a new attempt. `omamix --headless --mix-mic` with no `--strips` creates the filter, the Mix Mic adapter, and the links from the filter outputs into the adapter. It does not create strip taps. The adapter ports are configured on that node at creation, so they exist without a session manager.

## Crate layout

```
crates/omamix-core   scene, dB math, fader rule, DSP trait   (no PipeWire)
crates/omamix-pw     AudioEngine trait + NullEngine
                     filter, registry, and headless loop with --features pipewire
crates/omamix        eframe binary; --headless publishes Mix Mic only with that feature
```

`cargo test --workspace` does not turn the feature on, and the feature tests do not call `run_headless`. `cargo build -p omamix --features pipewire` links libpipewire >= 1.2.

## What this machine had installed

Read on 2026-10-11 from XPS15-OMARCHY. Versions are from `pipewire --version`, `pacman -Q`, `pkg-config`, and `clang --version`. Loaded modules are from `pw-dump` (`PipeWire:Interface:Module`). The command behind each audio-graph fact is in [AUDIO-BASELINE.md](AUDIO-BASELINE.md).

| Piece | Found |
| --- | --- |
| PipeWire / libpipewire | 1.6.8 (`pkg-config --modversion libpipewire-0.3` and `pipewire --version`). Floor for the feature build is >= 1.2. |
| WirePlumber | 0.5.17 |
| pipewire-pulse | server reports PulseAudio on PipeWire 1.6.8, protocol 35 |
| Default rate | float32le, 2 ch, 48000 Hz |
| clang / libclang | clang 22.1.8, `/usr/lib/libclang.so` |
| Rust | stable 1.99.0 |
| PipeWire modules on disk | includes `filter-chain`, `loopback`, `echo-cancel`, `parametric-equalizer` under `/usr/lib/pipewire-0.3/` |
| Modules actually loaded | rt, protocol-native, profiler, metadata, spa-device-factory, spa-node-factory, client-node, client-device, portal, access, adapter, link-factory, session-manager. No loopback, remap-source, or filter-chain. |
| SPA filter-graph | builtin, ebur128, ladspa, lv2, sofa |
| LV2 | specs only under `/usr/lib/lv2`. No plugin `.so`. |
| lilv | 0.28.0 |
| rnnoise | 1:0.2-1, header and `-lrnnoise`. `pkg-config --modversion rnnoise` prints `unknown`. |
| libebur128 | 1.2.6 |
| LSP plugins | not installed |
| DeepFilterNet | not installed |
| LADSPA plugins | `/usr/lib/ladspa` empty |
| gtk4 / libadwaita | 4.22.4 / 1.9.3 |
| easyeffects | 8.2.9-1 was installed at the start of this session (`pacman -Q easyeffects`) and was running. At 01:29 local time `journalctl --user` showed Tim's terminal started `pacman -Rns easyeffects wiremix`. After that `pacman -Q` reported both packages missing and `wpctl status` showed no filter nodes. The agent did not run that command. |
| wiremix | was installed and, earlier in the session, had capture streams. Removed by the same pacman command. |

Build dependencies for `--features pipewire` are libpipewire >= 1.2, pkg-config, clang, and libclang, plus the Wayland/libxkbcommon/GL libraries the egui binary links. The exact Arch and Debian/Ubuntu package names are in the README. Ubuntu 24.04's apt libpipewire is 1.0.5, which fails this check. `build.rs` is the clear failure in a normal parallel build. CI runs fmt, clippy, and tests with default features only, because that Ubuntu package is too old and an Arch container plus a rolling `pacman` upgrade is not a reliable way to pin libpipewire >= 1.2. The feature build is what this Arch machine runs locally.

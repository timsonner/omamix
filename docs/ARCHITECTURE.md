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

The audio graph is one `pw_filter` (`ClockPlan::SingleFilter`) with one `process` callback. That node **is** Mix Mic. There is not a second node for it. `filter_properties()` is `mix_mic_properties()`: one bag, `media.class` `Audio/Source`, `node.name` `omamix.mix-mic`. Strip taps are input ports on that same node (`in-<peer>-FL` / `in-<peer>-FR`), linked from the peer's output ports. pipewire-rs 0.10 has no `pw_filter` wrapper; `crates/omamix-pw/src/filter.rs` calls the C API. The filter flag is `PW_FILTER_FLAG_RT_PROCESS` only. `PW_FILTER_FLAG_DRIVER` is not set, so the filter follows another node instead of becoming the graph driver. Output ports are `capture_FL` / `capture_FR`, `format.dsp` `32 bit float mono audio`, with `PW_FILTER_PORT_FLAG_MAP_BUFFERS`.

Mix Mic properties:

- `media.class` = `Audio/Source`
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

WirePlumber chooses the default source by the highest `priority.session`. ALSA capture devices on this machine are above 1000 (the X2u sink in the baseline dump was 1109). Priority 1 loses to any higher candidate. A fallback **source** at 500 is sometimes cited and would also beat priority 1; that node is **UNVERIFIED** on WirePlumber 0.5.17 (the stock fallback is a sink). If Mix Mic is the only `Audio/Source`, priority does not keep it from becoming the default. The unplug check is `scripts/unplug-watch.sh`, not a unit test. `priority.driver` is 1 and the driver flag is off, so the node is not elected to clock the graph. `node.dont-reconnect` and `node.autoconnect=false` tell the session manager not to rebind the node or to pull existing clients onto it.

omamix reads the default source and sink (`pactl get-default-source`, `pactl get-default-sink`) and logs when either name changes. A metadata write goes through `allow_metadata_write`, which refuses the key `default` and every `default.*` key. If the default source name is `omamix.mix-mic`, omamix logs a warning and destroys the Mix Mic node. The session default stays where the session manager left it.

Mix Mic's filter connects with a SPA Props pod: `channelVolumes` `[1.0, 1.0]`, `mute` false, and the matching soft-volume fields. `process` writes the samples itself and does not multiply by that pod. `--tone` adds a 440 Hz sine at amplitude 0.2 on Mix Mic only, for a recorder aimed at that node.

Headphone sends and the monitor of the final mix are separate `Stream/Output/Audio` streams on the same client. They are not a second mixer and they are not the filter. The filter `process` writes the master quantum into a seqlock (`MixSlot`). The monitor stream's `process` only copies that slot. A headphone send must copy that strip's tap, not the master mix, because the master includes the mic. `--monitor-mix` creates the monitor stream. Headphone streams are not created yet: there is no per-strip slot, and copying `MixSlot` into headphones would play the mic. Both property bags set `node.autoconnect=false`, `node.dont-reconnect=true`, `object.linger=false`, and `omamix.owned=true`, and neither sets `target.object` or `PW_STREAM_FLAG_AUTOCONNECT`, so they do not attach to a sink. They can run on the sink's driver and may repeat or skip a quantum relative to the filter.

Easy Effects, while it was still installed, published its virtual mic as `media.class = Audio/Source/Virtual` with `factory.name = support.null-audio-sink` (`easyeffects_source`). That class is a real source and it showed up in `wpctl`. omamix uses `Audio/Source` because that is the class the spec asks for, on the filter node whose lifetime is the client's. If gpu-screen-recorder's default-source path ignores `Audio/Source` and accepts `Audio/Source/Virtual`, the property is a one-line change. It is still not `module-remap-source`. The gsr test in [TESTING.md](TESTING.md) is what decides.

Teardown runs on the loop thread before `mainloop.quit()` returns and the thread drops the core: link proxies, the monitor stream, registry bindings, then the filter (`pw_filter_disconnect`, remove the hook, `pw_filter_destroy`). `object.linger=false` is on the filter, the monitor, and every link. Dropping the core disconnects the client, which removes what that client created. SIGINT and SIGTERM only set an atomic; the control thread then sends `Shutdown` on the PipeWire channel. `process()` catches panics so they do not unwind across the C callback. A hard kill still depends on `object.linger=false` and the client disconnecting when the process dies. The GUI does not open a client (`NullEngine`). `omamix --headless` without `--mix-mic` does not either. Unit tests do not call `run_headless`.

### Per-app capture and re-attach

Playback apps show up as `Stream/Output/Audio` nodes. Node ids change every time the app starts. A strip stores a `RouteKey` of `application.name`, `application.process.binary`, and `media.name` when a tab or window needs to stay distinct. Those three are the re-attach key: a restarted app has a new pid and a new object serial and still matches (`RouteKey::matches_stream`). Pid and object serial are not required for that match. They only separate two streams that are live together (`disambiguates_live`). A node global does include `client.id`, `object.serial`, `node.name`, and `media.class`. It does not include `application.process.binary`, `media.name`, `application.process.id`, or `omamix.owned`. The backend binds the node for those. Whether a node is ours is our `client.id`: the client is created with `application.name=omamix`, `application.process.binary=omamix`, and `omamix.instance=<pid>-<nanos>`, and the Client global whose instance matches is ours. Another client cannot stamp that id. On `global_remove` the strip stays and the link attempt is dropped (`LinkPhase::PeerVanished`); a later global starts a new attempt.

The tap is an input port on the Mix Mic filter, not a second `pw_stream` (a stream would be a second clock). `app_capture_properties` still records the contract that must not be used to steal playback: `media.class` `Stream/Input/Audio`, `node.passive`, `node.autoconnect=false`, `node.dont-reconnect`, `stream.dont-remix`, `object.linger=false`, `omamix.owned`, and no `target.object` or `stream.capture.sink`. `target.object` on a capture stream is how WirePlumber steals a playback app off the user's sink. The link is additive: the app keeps its link to the sink, and the filter listens as well.

The optional "all other desktop audio" strip attaches each remaining playback stream with that same per-stream tap (`Scene::other_desktop_streams`). It does not set `stream.capture.sink` or `target.object`, so it does not monitor the sink. A stream that already has an `Application` strip is left out of "other", which keeps it from being mixed twice. `feeds_mix_mic` refuses a stream when `omamix.owned` is set, when its `client.id` or pid is the client this process opened, or when [`upstream_includes_mix_mic`](../crates/omamix-core/src/route.rs) says the node is fed by Mix Mic or shares a client with a node that captures Mix Mic. That walk is the loop guard for an OBS monitor or a browser studio: the playback node often has no PipeWire edge of its own back to Mix Mic, because the loop is inside the other client. A missing `node_id` fails closed when a graph is present. Headphone sends and the monitor share our client, so they stay out of Mix Mic. `owned_output_feeds_mix_mic` reads the property bag and is false while `omamix.owned=true` is in that bag.

Capture devices (the X2u source and anything else) are stored by `node.name`, which is stable across reboots for ALSA nodes.

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

The control thread sends `Shutdown` (and can send a master-fader or monitor flag) through `pipewire::channel`. That write wakes the loop. The callback runs on the loop thread and applies the message. A timer on the same thread drains `CommandQueue`, which is what `CommandSender::submit` fills. `process()` does not receive on either channel. It writes `AtomicMeter` (peak and RMS) and the `MixSlot`, and it reads `AtomicParams`. The default master gain in `AtomicParams` is unity: there is no built-in mic boost. `NullEngine::drive` is the same drain, and it is what tests call. The GUI still holds a `NullEngine` and does not start the PipeWire thread. `omamix --headless --mix-mic` does.

## Clocking

The Shure X2u capture clock and a playback app's clock are not the same device. A PipeWire link does not resample. Linked nodes join one driver group. The USB mic and each app stream rate-match inside their own nodes and adapters. `process()` does not resample; it sums the quantum it was given.

**Choice: one `pw_filter`, one `process` callback (`ClockPlan::SingleFilter`).** That filter node is Mix Mic. It is a follower: `PW_FILTER_FLAG_DRIVER` is not set, and `priority.driver` is 1, so it does not take over the graph clock. Strip taps are input ports on this node. A separate `pw_stream` per strip would put each strip on its peer's driver. Mix Mic declares `node.latency = 1024/48000` so consumers see a latency; that fraction is not a measured quantum (see the unverified list in AUDIO-BASELINE).

Headphone sends and the monitor are the exception that stays off this clock. Each is its own playback stream. Its `process` copies a slot the filter already wrote. If that stream's driver is the sink, it can repeat or skip a quantum relative to the filter. The monitor copies the master `MixSlot`. A headphone send copies the strip tap, not the master, so the mic is not in the headphones.

## Browser identity and the Mix Mic loop

Chromium tabs and browser studios share `application.name` and `application.process.binary`. A `RouteKey` that only has those two claims every stream of that app, which is why "other desktop" then leaves all of them out: better to miss a tab than to mix it twice. The key also stores `media.name`, pid, and object serial. `matches_stream` uses `media.name` when the key has one, so two tabs can be different strips once the UI writes that down. It does not require pid or object serial; those only split two streams that are live at the same time (`disambiguates_live`).

**Loop guard.** `feeds_mix_mic` returns false when the playback node's `client.id` or pid is [`OwnClient`](../crates/omamix-core/src/route.rs), when `omamix.owned` is set, or when `upstream_includes_mix_mic` walks the link graph and finds Mix Mic upstream or finds this node's client downstream of Mix Mic. The second walk is how an OBS monitor or a browser studio is refused: that client's capture of Mix Mic and its playback are not the same node, so a flag on the playback node cannot see the loop. Node globals include `client.id`. Binary, `media.name`, pid, and `omamix.owned` arrive on bind. If a graph is present and the stream has no `node_id`, the walk cannot run and the stream is refused.

## Linking

On `global`, the loop thread records the node. Ports carry `node.id`, `port.id`, `port.direction`, and `audio.channel`. `stereo_output_ports` picks FL/FR, or two unlabeled outputs in `port.id` order, or one unlabeled mono port used for both sides. Link factories take that `port.id`, not the registry global id.

`start_link(false)` stays in `LinkPhase::WaitingForPorts` until the peer's output ports and the filter's `in-<peer>-FL/FR` ports exist. `advance_link` moves Waiting to Linking. The loop thread then calls `link-factory` with `object.linger=false`. Active or paused moves Linking to Linked. If the link call fails, `link_failed` goes to `LinkPhase::Failed` and stays there. If the peer disappears, `peer_removed` goes to `PeerVanished`, the link proxy is dropped, and a later global for a new node id starts a new attempt. `omamix --headless --mix-mic` with no `--strips` creates the filter and no links.

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

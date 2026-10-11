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

### Why an app-owned stream, not module-remap-source or filter-chain

Tim reported that gpu-screen-recorder recorded silence when the default source was a pactl `module-remap-source`, and that naming the source explicitly worked. That module is a server-side Pulse compatibility node. It stays loaded until something unloads it, which fights the "pw-dump diff clean on quit" requirement, including on a crash. This session did not re-test gpu-screen-recorder.

The audio graph is one `pw_filter` (`ClockPlan::SingleFilter`) with one `process` callback. Strips are input ports on that filter. Mix Mic is the filter's source port (`media.class` `Audio/Source`), not a second `pw_stream` on its own driver. `backend::connect_mix_mic` typechecks that source-port contract on a `pw_stream` today (properties, f32 stereo, no autoconnect) so the pod and the property bag are real code. When the filter exists, that function is replaced by the filter's output port with the same properties. Per-strip `pw_stream`s are not the plan: each would run on its peer's driver.

Mix Mic properties:

- `media.class` = `Audio/Source`
- `node.name` = `omamix.mix-mic`
- `node.description` = `Mix Mic`
- `node.virtual` = `true`
- `priority.session` = `1` and `priority.driver` = `1`
- `node.latency` = `1024/48000` (declared; the machine quantum was not measured)
- `node.dont-reconnect` = `true` and `node.autoconnect` = `false`
- `object.linger` = `false`
- `omamix.owned` = `true`
- direction = Output (the process writes samples; other clients capture the node)
- flags = `MAP_BUFFERS | RT_PROCESS`, **without** `AUTOCONNECT`

WirePlumber chooses the default source by the highest `priority.session`. ALSA capture devices are typically 1000 or higher. The fallback script uses 500, and it is what remains after every ALSA source is unplugged. Priority 1 loses to that fallback, so Mix Mic is not selected as the default source in either case. `priority.driver` is the same value so the node is not elected to clock the graph. `node.dont-reconnect` tells the session manager not to rebind the node to the default target. `AUTOCONNECT` is also off, so opening the stream does not ask WirePlumber to reroute existing clients and does not change the default source. Format is f32 stereo at 48 kHz, which is this machine's default sample spec (`pactl info`: float32le, 2 ch, 48000 Hz). The property list is `omamix_pw::contract::mix_mic_properties`. The filter, headphone sends, and the monitor of the final mix each have their own bag, and each sets `omamix.owned`.

Easy Effects, while it was still installed, published its virtual mic as `media.class = Audio/Source/Virtual` with `factory.name = support.null-audio-sink` (`easyeffects_source`). That class is a real source and it showed up in `wpctl`. omamix uses `Audio/Source` because that is the class the spec asks for, on a stream whose lifetime is the client's. If gpu-screen-recorder's default-source path ignores `Audio/Source` and accepts `Audio/Source/Virtual`, the property is a one-line change. It is still not `module-remap-source`. The gsr test in [TESTING.md](TESTING.md) is what decides.

Teardown: dropping the core disconnects the client. PipeWire removes nodes that client created. Every node property bag sets `object.linger = false`, and link objects will set it too once they exist, so a crash does not leave them in the graph. The scaffold does not call `Session::connect`, and unit tests do not either. The UI thread only enqueues `EngineCommand::Shutdown`; it does not drive the engine.

### Per-app capture and re-attach

Playback apps show up as `Stream/Output/Audio` nodes. Node ids change every time the app starts. A strip stores a `RouteKey` of `application.name`, `application.process.binary`, and `media.name` when a tab or window needs to stay distinct. Those three are the re-attach key: a restarted app has a new pid and a new object serial and still matches (`RouteKey::matches_stream`). Pid and object serial only separate two streams that are live together (`disambiguates_live`). A registry `global` does not include binary, `media.name`, pid, or `omamix.owned`. Those arrive after bind. Whether a node is ours is our `client.id`, which another client cannot stamp on its nodes. On `global_remove` the strip stays and waits. Registry callbacks are not wired yet; the match function and the link-phase guard are.

The capture stream is `media.class = Stream/Input/Audio`, `node.passive = true`, `node.autoconnect = false`, `node.dont-reconnect = true`, `stream.dont-remix = true`, `object.linger = false`, `omamix.owned = true`, direction Input, no `AUTOCONNECT`. It does **not** set `target.object`. `target.object` on a capture stream is how WirePlumber steals a playback app off the user's sink. `node.dont-reconnect` keeps the session manager from attaching that tap to the default source after the app node goes away. Port links are additive: the app keeps its link to the sink, and omamix listens as well.

The optional "all other desktop audio" strip attaches each remaining playback stream with that same per-stream tap (`Scene::other_desktop_streams`). It does not set `stream.capture.sink` or `target.object`, so it does not monitor the sink. A stream that already has an `Application` strip is left out of "other", which keeps it from being mixed twice. `feeds_mix_mic` refuses a stream when `omamix.owned` is set or when its `client.id` or pid is the client this process opened. Headphone sends and the monitor share that client, so they stay out of Mix Mic. `owned_output_feeds_mix_mic` reads the property bag for the filter, Mix Mic, each headphone send, and the monitor, and is false while `omamix.owned=true` is in that bag.

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

**Plan.** The PipeWire loop runs on its own thread (`ThreadLoop` in `backend::Session` once something calls `Session::connect`). The UI thread sends `EngineCommand` values through `CommandSender::submit` and never applies them. The loop thread drains that queue. `process()` does not receive on the channel: it reads `AtomicMeter` values (peak and RMS as `AtomicU32` bit patterns) and the parameter block published the same way. `NullEngine::drive` is the stand-in for that loop-thread drain and is what tests call.

**In this commit.** The channel, the atomic meter, and the split between `submit` and `drive` exist. `OmamixApp::on_exit` only submits `Shutdown`. The PipeWire thread is not started, and the UI still owns the `NullEngine` value. Moving that value onto the PipeWire thread is the remaining work.

## Clocking

Strips and the master do not share a driver. The Shure X2u capture clock and the default sink clock (often that same USB device's playback side, sometimes another sink) drift apart, and each playback app is clocked by the sink it is linked to.

**Choice: one `pw_filter`, one `process` callback (`ClockPlan::SingleFilter`).** The mix is summed inside that callback's quantum. The filter is the driver of its own cycle. The USB mic and each playback app keep their own clocks; PipeWire's resample adapter sits on the link into the filter port and converts those peers into the filter quantum. The callback itself does not resample. `node.group` only hints that nodes should share a cycle. Separate `pw_stream`s per strip would put each strip on its peer's driver, which is the ring-buffer design this choice replaces. Mix Mic declares `node.latency = 1024/48000` so consumers see a latency; that fraction is not a measured quantum (see the unverified list in AUDIO-BASELINE). `backend::connect_mix_mic` is the source-port stand-in described above, not a second clock beside the filter.

**In this commit.** The enum and the latency property are in the contract. No filter node is created.

## Browser identity and the Mix Mic loop

Chromium tabs and browser studios share `application.name` and `application.process.binary`. A `RouteKey` that only has those two claims every stream of that app, which is why "other desktop" then leaves all of them out: better to miss a tab than to mix it twice. The key also stores `media.name`, pid, and object serial. `matches_stream` requires each of those when the key has it, so two tabs can be different strips once the UI writes them down.

**Loop guard, in this commit.** `feeds_mix_mic` returns false when the playback node's `client.id` or pid is [`OwnClient`](../crates/omamix-core/src/route.rs), the client this process opened. It also returns false when `omamix.owned` is set on a bound node. It does not look for a capture-of-Mix-Mic flag on the playback node. Registry globals omit `client.id`, pid, binary, `media.name`, and `omamix.owned`; those are read after bind. The backend does not walk the registry yet. The graph tests cover the client match and the owned-output property bags.

## Linking

**Plan.** On `global`, record the node and wait. `start_link(false)` stays in `LinkPhase::WaitingForPorts` until both the playback output port and the tap input port exist. `advance_link` moves Waiting → Linking → Linked. If the app disappears mid-link, `peer_removed` goes to `PeerVanished`. If the link call itself fails, `link_failed` goes to `LinkPhase::Failed` and stays there. A later `global` starts a new attempt. Every link object uses `link_properties()`: `object.linger = false`.

**In this commit.** The phase functions and `object.linger=false` on Mix Mic and on each playback tap. No `pw_link` is created, and nothing calls `Session::connect`.

## Crate layout

```
crates/omamix-core   scene, dB math, fader rule, DSP trait   (no PipeWire)
crates/omamix-pw     AudioEngine trait + NullEngine
                     backend module only with --features pipewire
crates/omamix        eframe binary, depends on the null engine
```

`cargo test --workspace` does not turn the feature on. `cargo build -p omamix --features pipewire` typechecks stream construction against the installed headers, which must be libpipewire >= 1.2.

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

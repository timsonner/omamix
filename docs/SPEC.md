# omamix — PipeWire live mixer with a virtual mic

Requested by Tim, 2026-10-11 ~1:08 AM MT, via Kit. Owner: Coda (Software Dev). Language: Rust (Tim's pick).
Target: Tim's XPS15-OMARCHY (Arch-based Omarchy, Hyprland/Wayland, PipeWire + WirePlumber, pipewire-pulse). Local dev and test on that laptop.

The working name pw-mixer is **omamix**.

## Problem

Tim records screen videos and streams (YouTube, X, Rumble browser studio) with his voice over desktop music (an mpv OST player, browser tabs, Strudel in the browser). He needs one app where he can: pick sources, set levels, process them, see what the sound looks like, and get ONE mixed virtual mic that any app (browser studio, OBS, Omarchy's gpu-screen-recorder) can pick as its input. Existing tools split this across Easy Effects, pavucontrol/wiremix, qpwgraph, and OBS; Tim tried a pactl-loopback script chain and rejected it as too fiddly.

## What we learned (hard requirements fall out of these)

- **UNVERIFIED — mic level.** An early note said the Shure X2u comes in about 20 dB low and that browser music is mastered hot. A read-only check on 2026-10-11 did not confirm that. PipeWire channel volume on the X2u source was unity (linear 1.0, cubic 1.0, 0.00 dB, unmuted) and ALSA exposes no capture-gain control. A 5 s capture from that node was digital silence (peak −90.3 dBFS). The cause is unknown. It needs a retest with Tim speaking. See [AUDIO-BASELINE.md](AUDIO-BASELINE.md). v1 still has per-channel input gain with headroom (±24 dB) so a quiet mic can be raised if a later voiced measurement shows it is needed. The cubic-to-dB conversion itself is specified: a PipeWire slider at 40% is about −24 dB (`linear = cubic³`).
- **Tim reported** that headphone volume is a hardware knob on the X2u, so what he hears is not what's recorded. ALSA on this adapter exposes PCM playback only (no headphone element), which is consistent with that report. The knob position was not read. The app must show true output meters and offer a "hear the final mix" monitor.
- **Tim reported** that gpu-screen-recorder's `default_input` captured silence when the default source was a pactl module-remap-source virtual mic, and that it worked when that source was named explicitly. This session did not re-test that. The virtual mic must be a real PipeWire Audio/Source node (media.class Audio/Source, owned by the app), and must be tested as the system default source with gpu-screen-recorder, Chromium/Firefox getUserMedia, and OBS.
- PipeWire volume sliders are cubic (40% is about −24 dB). Show dB everywhere, not just percent.
- Tim hates mouse-wheel-over-slider changing volume by accident. No scroll-to-change unless a fader is focused/clicked first.
- Apps appear/disappear as they start and stop playing; routing must persist per app (by application.name / binary) and re-attach automatically.

## v1 scope (must have)

1. Channel strips. Add a source as a strip: any capture device (mic) or any playback app/stream (captured per app, not whole-desktop). Optional "all other desktop audio" strip, which attaches each remaining playback stream on its own and never monitors the sink. Streams that already have a strip, and every node omamix owns, stay out of that strip so they are not mixed twice and headphone sends do not feed Mix Mic.
2. Per strip: fader in dB, mute, solo, peak + RMS meter, pan, input gain (±24 dB).
3. Per strip effects (in this order, each toggleable): noise reduction (mic), high-pass, 10-band graphic EQ, compressor, limiter.
4. Master bus: EQ, compressor, limiter (default ON, ceiling −1 dBTP), loudness meter (momentary/short-term LUFS) plus peak.
5. Output: the master appears as a virtual mic ("Mix Mic") usable as the default source. Per-strip "send to my headphones" so he can hear the music without hearing himself, and a "monitor final mix" toggle.
6. Visuals: live waveform/oscilloscope and spectrum per strip and for master.
7. Presets: save/load a full scene (sources, levels, effects), e.g. "Voice + OST", "Strudel live". Restore on launch; auto re-attach apps when they start playing.
8. Clean teardown: quitting removes every node/link it created; nothing left behind in PipeWire, no changes to the user's default sink/source unless he opts in.

## Later (v2+)

- Ducking (music dips when voice is present).
- Record the master straight to a file (WAV/FLAC) for quick checks.
- Hotkeys / CLI (`omamix scene "Voice + OST"`), tray or Omarchy bar widget.
- Per-track recording for post.

## Tech direction

Decided in [ARCHITECTURE.md](ARCHITECTURE.md): Rust, egui/eframe, pipewire-rs with an in-process DSP chain. The master is an app-owned `pw_stream` with `media.class = Audio/Source`. DSP stages sit behind a trait; RNNoise (library is installed) and optional LV2 come later. PipeWire filter-chain / a full LV2 host is not the v1 engine.

Package: plain cargo build for dev; an Arch `PKGBUILD` stub is in the repo (no AUR dependency).

## Definition of done for v1

- On XPS15-OMARCHY: add the Shure X2u + mpv OST + a Chromium tab as strips; set levels; master limiter on.
- Pick the virtual mic in (a) Chromium getUserMedia (webrtc mic test page), (b) Firefox getUserMedia, (c) Omarchy screen recording via gpu-screen-recorder with the virtual mic as default source, (d) OBS. All four capture voice + music at the set balance, no silence, no doubling.
- Meters match what's measured in the recorded file (ffmpeg ebur128) within ~1 dB.
- Quit leaves PipeWire as it was (pw-dump diff clean).
- Gates per Tim's rules: reviewers + Smoke QA on the same head SHA before any merge link goes to Tim.

## Machine / dev setup

- Cloud agents can write and unit-test the code anywhere, but real PipeWire + Hyprland testing needs Tim's laptop.
- Cursor My Machines: once the repo exists, Tim clones it on the XPS and runs `agent worker start --name xps15-omarchy` in that folder (add `--computer` if the agent should drive the desktop for UI checks). Then cloud agents can be pointed at that machine.
- Do NOT touch Tim's audio defaults, Easy Effects config, or Hyprland binds without asking Tim. No sudo without Tim (floating-terminal password prompt).
- Note: Kit retired a previous script approach; parked at ~/.local/share/stream-mix-retired-2026-10-11 on the XPS for reference only.

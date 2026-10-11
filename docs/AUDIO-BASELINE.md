# Audio baseline — XPS15-OMARCHY

Read-only inspection on 2026-10-11, about 01:23–01:30 America/Denver (MDT). Host `XPS15-OMARCHY`. Defaults, volumes, mutes, and config files were not changed. There was no persistent graph change (`pw-record` briefly linked to the Shure X2u capture ports; that link ended when the process exited).

Each fact below names the command that produced it.

## Versions

| Fact | Command |
| --- | --- |
| PipeWire 1.6.8, compiled and linked with libpipewire 1.6.8 | `pipewire --version` |
| libpipewire 1.6.8 | `pkg-config --modversion libpipewire-0.3` |
| WirePlumber 0.5.17 | `pacman -Q wireplumber` |
| Server name "PulseAudio (on PipeWire 1.6.8)", protocol 35 | `pactl info` |
| Default rate float32le, 2 ch, 48000 Hz | `pactl info` (`Default Sample Specification`) |

## Defaults

Unchanged at the start and after the capture. Both commands agreed.

| | node.name | description |
| --- | --- | --- |
| Default sink | `alsa_output.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo` | X2u Adapter Analog Stereo |
| Default source | `alsa_input.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo` | X2u Adapter Analog Stereo |

Commands: `pactl info` (`Default Sink` / `Default Source`) and `wpctl status` (the `*` marks and the "Default Configured Devices" list).

## Nodes

ALSA cards, command `cat /proc/asound/cards`:

| card | id | name |
| --- | --- | --- |
| 0 | Digital | Shure Incorporated Shure Digital (USB) |
| 1 | UDULTCDL | DisplayLink Plugable UD-ULTCDL |
| 2 | PCH | HDA Intel PCH |

PipeWire device names from `pw-dump` (`PipeWire:Interface:Device`): X2u Adapter (`alsa_card.usb-Shure_Incorporated_Shure_Digital-00`), Plugable UD-ULTCDL, Built-in Audio. The Plugable USB serial is omitted here.

Audio nodes from `wpctl status` and `pw-dump` (`media.class`, `node.name`, and the `vol:` column):

| media.class | node.name | wpctl vol |
| --- | --- | --- |
| Audio/Sink | `alsa_output.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo` | 1.00 (default) |
| Audio/Source | `alsa_input.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo` | 1.00 (default) |
| Audio/Sink | `alsa_output.pci-0000_00_1f.3.analog-stereo` (Built-in) | 1.00 |
| Audio/Source | `alsa_input.pci-0000_00_1f.3.analog-stereo` (Built-in) | 0.00 |
| Audio/Sink | Plugable UD-ULTCDL analog stereo (USB serial omitted) | 1.00 |
| Audio/Source | Plugable UD-ULTCDL IEC958 stereo (USB serial omitted) | 1.00 |

Shure X2u source format, from that node's `Format` param in `pw-dump`: S16LE, 48000 Hz, stereo, position FL,FR. Device string `front:0` is in the same node's `Props`.

`pw-dump` at the link-table snapshot had no playback or capture stream nodes (no mpv, no browser audio node). `wpctl status` showed a client named "Chromium input" with no audio node and no links.

Video nodes (not part of the mixer): Integrated_Webcam_HD sources, from `wpctl status`.

## Shure X2u volumes

PipeWire `Props.channelVolumes` is linear amplitude. The cubic slider is `cbrt(linear)`. dB is `20·log10(linear)`. pactl's percent is the cubic value, where 65536 is cubic 1.0 (100%).

Commands: `pw-dump` (node `Props`: `volume`, `mute`, `channelVolumes`, `softVolumes`) and `pactl list sources` / `pactl list sinks` (the `Volume:` and `Mute:` lines). `wpctl status` printed `vol: 1.00` for both.

### Source `alsa_input.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo`

| | FL | FR |
| --- | --- | --- |
| mute | false | false |
| channelVolumes (linear) | 1.0 | 1.0 |
| softVolumes | 1.0 | 1.0 |
| cubic (`cbrt(linear)`) | 1.0 | 1.0 |
| dB | 0.00 | 0.00 |
| Props.volume | 1.0 | |
| pactl | 65536 / 100% / 0.00 dB, both channels, Mute: no, Base Volume 0.00 dB | |

### Sink `alsa_output.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo`

| | FL | FR |
| --- | --- | --- |
| mute | false | false |
| channelVolumes (linear) | 1.001695 | 1.001695 |
| softVolumes | 1.0 | 1.0 |
| cubic | ~1.0006 | ~1.0006 |
| dB from linear | ~0.015 | ~0.015 |
| pactl | 65573 / 100% / 0.01 dB, both channels, Mute: no, Base Volume 0.00 dB | |

65573/65536 ≈ 1.000565 cubic, and `1.000565³ ≈ 1.001695`, which matches `channelVolumes`.

## ALSA mixer, card 0 (Shure X2u)

Commands: `amixer -c 0 scontents` and `amixer -c 0 contents`. Read only.

| numid | control | value |
| --- | --- | --- |
| 3 | PCM Playback Switch | on |
| 4 | PCM Playback Volume | 76,76 of 0–76, **0.00 dB** (range −76.00 dB … 0.00 dB) |
| 2 | Capture Channel Map | read-only, values 3,4 (FL, FR) |
| 1 | Playback Channel Map | read-only, values 3,4 (FL, FR) |

There is no capture volume, capture switch, mic boost, or headphone element. Software capture gain is PipeWire's channel volume (unity). Tim reported that headphone level is a hardware knob on the X2u. ALSA exposing no headphone control is consistent with that report. The knob position was not read.

## What sat in the mic and output paths

Loaded modules, from `pw-dump` objects of type `PipeWire:Interface:Module`: rt, protocol-native, profiler, metadata, spa-device-factory, spa-node-factory, client-node, client-device, portal, access, adapter, link-factory, session-manager. That list had no `module-loopback`, no `module-remap-source`, and no `module-filter-chain`.

### Mic path (first full `pw-dump`, 12 links, all `state=active`)

Easy Effects was running and had tapped the Shure X2u source. The default source was still the raw ALSA node (`pactl info`). The links are a fan-out from the X2u capture ports, not an insert in front of every consumer. Nothing was linked downstream of `easyeffects_source`.

```
alsa_input...Shure_Digital capture_FL/FR
  -> ee_sie_rnnoise          "Noise Reduction (Mic)"
  -> ee_sie_compressor       "Compressor (Mic)"
  -> ee_sie_limiter          "Limiter (Mic)"
  -> ee_sie_spectrum         "Spectrum"
  -> ee_sie_output_level     "Output Level Meter"
  -> easyeffects_source      Audio/Source/Virtual
```

`easyeffects_source` factory was `support.null-audio-sink`, `media.class` `Audio/Source/Virtual`, channel volumes linear 1.0, unmuted (`pw-dump` `Props`). Its group was `ee_source_group`.

### Output path

No links in that `pw-dump`. `easyeffects_sink` (`Audio/Sink`, `support.null-audio-sink`, group `ee_sink_group`) existed, along with `ee_soe_output_level` and `ee_soe_spectrum`, and none of them had links. Playback was the X2u sink directly. `ee_test_signals` also had no links.

An earlier `wpctl status` in the same session, before that dump, showed a `wiremix` client with capture streams on the built-in mic, the Plugable, and the X2u. Those streams were already gone in the dump above.

### After 01:29

Command: `journalctl --user --since "2026-10-11 01:15:00" --until "2026-10-11 01:40:00"`. At 01:29:29 it showed Tim's terminal started `pacman -Rns easyeffects wiremix`. This agent did not run that. A `pw-dump` taken around the capture already showed the rnnoise/compressor/limiter nodes and all links gone, while the easyeffects client was still connected; the next `wpctl status` showed no filter nodes. `pacman -Q easyeffects` and `pacman -Q wiremix` then reported not found. `pactl info` still showed the two X2u nodes as the defaults, and `pw-dump` still showed the volumes above.

Built-in mic, for context only (`pw-dump` `Props` on `alsa_input.pci-0000_00_1f.3.analog-stereo`): `channelVolumes` `[0.0, 0.0]` (cubic 0, −∞ dB) with `mute` false and `Props.volume` 1.0. `wpctl status` showed `vol: 0.00`. That is a different device from the Shure X2u.

## 5 s capture

```sh
pw-record \
  --target alsa_input.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo \
  --media-category Capture \
  --rate 48000 --channels 2 --channel-map FL,FR \
  --format s16 --volume 1.0 \
  -n 240000 \
  /tmp/omamix-x2u-baseline.wav
```

`--target` was the node name. The default source was not changed (`pactl info` before and after). `--volume 1.0` is the recorder stream's volume. `pw-record` briefly linked to the capture ports; after it exited, `pw-dump` no longer showed that link. The file was 5.00 s, pcm_s16le, 48000 Hz, stereo (`ffprobe`), then deleted.

Level commands: `ffmpeg -i /tmp/omamix-x2u-baseline.wav -af astats=metadata=1:reset=0,volumedetect -f null -`, plus a per-channel count of non-zero 16-bit samples.

| | ch0 (FL) | ch1 (FR) | ffmpeg volumedetect |
| --- | --- | --- | --- |
| peak | 1 LSB, **−90.31 dBFS** | 1 LSB, **−90.31 dBFS** | max_volume **−90.3 dB** |
| RMS | **−114.63 dBFS** | **−115.21 dBFS** | mean_volume **−91.0 dB** |
| nonzero samples | 888 / 240000 | 776 / 240000 | n_samples 480000 |

The take is digital silence (peak −90.3 dBFS, one 16-bit count) plus a few 1-LSB samples. The cause is unknown. Nobody was asked to speak, and this node is a capture device, so the file is not a speech-level measurement. It needs a retest with Tim speaking. It does not support "the X2u comes in about 20 dB low." Software volume was 0 dB (`pw-dump` / `pactl`), so the silence is not a PipeWire or ALSA fader turned down.

## What could not be verified

- **Speech level versus mastered browser music.** The 5 s capture was digital silence (peak −90.3 dBFS) and the cause is unknown. The −20 dB claim stays unverified until a retest with Tim speaking, plus a separate capture of the playback he compares it to.
- **Hardware knob position.** Tim reported a hardware headphone knob. `amixer -c 0 contents` shows no headphone or capture control, so the position cannot be read from software.
- **Whether a microphone is electrically present.** Silence is consistent with several causes (gain at minimum, unplugged capsule, idle preamp below 16-bit). The USB device was present (`/proc/asound/cards`) and the source node was running at 48 kHz (`pw-dump` `Format`).
- **PipeWire quantum and latency.** Not read. `pw-metadata` and the node `Latency` / `ProcessLatency` params were not recorded.
- **Easy Effects plugin parameters** (RNNoise strength, compressor makeup, limiter ceiling). Node names and links were in `pw-dump`. The plugin control values live in Easy Effects' own config, which was not opened. The package was then removed, so those nodes cannot be re-read without reinstalling.
- **gpu-screen-recorder against this source.** Not run. Tim reported silence from `module-remap-source` as the default source. The default source was left alone.
- **Wiremix's earlier capture streams.** Seen in one `wpctl status`, absent from the later `pw-dump`. Not reconstructed.
- **Built-in mic and Plugable levels as a musical comparison.** Recorded only as node facts above. They were not captured to a file.

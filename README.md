# SomeNET

SomeNET is an open-source Rust audio streaming app for sending and receiving multichannel audio over IP between creative tools and DAWs.

The current target workflow is:

- send audio from TouchDesigner into REAPER
- receive audio in REAPER with low-latency, stable multichannel playback
- support both direct unicast and multicast-style routing
- stay close to SMPTE ST 2110-30 transport conventions where practical

## Overview

SomeNET is moving toward a transport-first architecture: the standalone app is the primary runtime, the RTP audio engine is the full-width streaming layer, and VST3 remains one host integration option.

- `Send` mode passes audio through locally and transmits an RTP copy over UDP
- `Receive` mode listens for that RTP stream and renders it to the selected audio outputs
- core transport supports up to `96` channels per stream
- standalone app supports native desktop control and CoreAudio device send/receive
- VST3 plugin option supports up to `64` input/output channels
- supports `Unicast` and `Multicast` transport selection
- includes a native macOS editor designed to work in both REAPER and TouchDesigner

The transport uses RTP with 24-bit linear PCM (`L24`) over UDP.

Supported operating modes:

- `48 kHz`, up to `96ch`, using 1 ms audio packets
- `44.1 kHz`, up to `96ch`, using low-latency 44-frame packets
- `96 kHz`, up to `96ch`, using 1 ms audio packets

Standards notes:

- `48 kHz` is the main ST 2110-30 / AES67-aligned path
- `44.1 kHz` is supported intentionally, but it is not ST 2110-30 compliant
- higher channel counts above classic ST 2110-30 profile limits are supported as an engineering choice, with warning text surfaced in the plugin status
- VST3 speaker arrangements are a 64-bit layout, so the VST3 wrapper intentionally caps its host-facing bus width at `64ch`

The sender also writes an SDP file to the system temp directory:

- `somenet.sdp`

## Current Status

- [x] Standalone `somenet` app target with CoreAudio-backed device I/O
- [x] Rust VST3 plugin built on the [`vst3`](https://crates.io/crates/vst3/0.3.0) crate
- [x] Sender and receiver modes
- [x] Unicast and multicast transport selection
- [x] RTP/L24 packetization and decode
- [x] Receiver buffering, concealment, and drift-tolerant playout control
- [x] Background sender and receiver worker threads
- [x] Native standalone desktop UI
- [x] Native macOS GUI
- [x] TouchDesigner -> REAPER workflow validation
- [x] Core transport support for up to `96` channels per stream
- [x] VST3 plugin option capped at `64` host-facing input/output channels
- [x] Low-latency sender startup and receiver playout profile
- [x] PTP-aware clock reference signaling and domain configuration
- [x] Cross-platform release automation checked into the repo
- [x] Distribution signing and notarization hooks for release builds
- [ ] PTP clocking
- [ ] NMOS / control-plane discovery
- [ ] Full ST 2110 system integration
- [ ] Windows-specific QoS handling
- [ ] Dante Controller-style graphical routing and device matrix

## Repository Layout

- [src/lib.rs](src/lib.rs)
  VST3 processor, controller, factory, state, and host-facing status formatting
- [src/bin/somenet.rs](src/bin/somenet.rs)
  standalone SomeNET app runtime, native control UI, device I/O, network routing, and live status
- [src/network.rs](src/network.rs)
  RTP transport, sender/receiver workers, buffering, and encode/decode logic
- [src/macos_gui.rs](src/macos_gui.rs)
  native macOS editor implementation
- [src/params.rs](src/params.rs)
  shared parameter definitions and defaults
- [src/editor_api.rs](src/editor_api.rs)
  opaque bridge used by the macOS editor
- [scripts/install_macos_vst3.sh](scripts/install_macos_vst3.sh)
  macOS bundle build and install script
- [scripts/package_vst3.py](scripts/package_vst3.py)
  cross-platform VST3 bundle packaging helper
- [.github/workflows/ci.yml](.github/workflows/ci.yml)
  matrix CI workflow for build, test, and lint checks
- [.github/workflows/release.yml](.github/workflows/release.yml)
  tag-driven packaging and GitHub release publishing workflow

## Continuous Integration

GitHub Actions is set up to validate the project on:

- macOS
- Windows
- Linux

The CI workflow runs:

- `cargo build --release`
- `cargo test`
- `cargo clippy --all-targets --all-features -- -W clippy::all`

## Standalone App

The main app target is `somenet`. The default app surface is a native desktop control UI; it does not run a local web server.

Launch the control UI:

```bash
cargo run --bin somenet -- ui
```

List audio devices:

```bash
cargo run --bin somenet -- devices
```

Launch receive mode directly from the terminal:

```bash
cargo run --bin somenet -- receive
```

Launch send mode from the default input device:

```bash
cargo run --bin somenet -- send --ip 127.0.0.1 --port 5004
```

Useful options:

- `--channels N`
- `--sample-rate 44100|48000|96000`
- `--input TEXT`
- `--output TEXT`
- `--multicast`
- `--ptp`

The current standalone app is a live status/control utility with native send/receive switching, device selection, and channel/clock controls. A fuller Dante Controller-style routing matrix is planned on top of the same transport runtime.

## Installation

Prebuilt release bundles are attached to tagged GitHub releases. For local VST3 installs from source, platform scripts are provided in [scripts](scripts).

### macOS

User-level install:

```bash
INSTALL_ROOT="$HOME/Library/Audio/Plug-Ins/VST3" scripts/install_macos_vst3.sh
```

System-level install:

```bash
sudo scripts/install_macos_vst3.sh
```

Typical VST3 locations:

- `~/Library/Audio/Plug-Ins/VST3/SomeNET.vst3`
- `/Library/Audio/Plug-Ins/VST3/SomeNET.vst3`

### Linux

User-level install:

```bash
INSTALL_ROOT="$HOME/.vst3" scripts/install_linux_vst3.sh
```

System-level install:

```bash
sudo INSTALL_ROOT="/usr/lib/vst3" scripts/install_linux_vst3.sh
```

Typical VST3 locations:

- `~/.vst3/SomeNET.vst3`
- `/usr/lib/vst3/SomeNET.vst3`

### Windows

From PowerShell:

```powershell
.\scripts\install_windows_vst3.ps1
```

To override the destination:

```powershell
$env:INSTALL_ROOT = "$env:LOCALAPPDATA\Programs\Common\VST3"
.\scripts\install_windows_vst3.ps1
```

Typical VST3 locations:

- `%COMMONPROGRAMFILES%\VST3\SomeNET.vst3`
- `%LOCALAPPDATA%\Programs\Common\VST3\SomeNET.vst3`

All three installers:

- build the release artifact
- package the platform-specific `.vst3` bundle
- install it into the selected VST3 directory

## Host Setup Notes

### REAPER

- rescan VST3 plugins after install if `SomeNET` does not appear immediately
- make the receiving track wide enough for the expected channel count
- match sender and receiver sample rate, channel count, transport, group/source IP, and port

### TouchDesigner

- make sure the actual audio stream feeding the plugin is at the sample rate you expect
- for ST 2110-style operation, use `48,000 Hz`
- if TouchDesigner shows `44,100 Hz`, resample before the VST if required
- if using multichannel input, confirm the VST/audio bus layout matches the desired channel count

## Usage

### Unicast Send -> Receive

Sender:

- `Mode = Send`
- `Transport = Unicast`
- endpoint IP = receiver host IP
- set matching `Port`
- set matching `Channels`

Receiver:

- `Mode = Receive`
- `Transport = Unicast`
- endpoint IP = expected sender IP
- use `0.0.0.0` if you want to accept any sender on that port
- set matching `Port`
- set matching `Channels`

### Multicast Publish -> Subscribe

Sender:

- `Mode = Send`
- `Transport = Multicast`
- endpoint IP = multicast group address
- set matching `Port`

Receiver:

- `Mode = Receive`
- `Transport = Multicast`
- endpoint IP = same multicast group address
- set matching `Port`

## Performance and Debugging

Useful things to watch:

- send packet fill / queue behavior
- receive queue depth
- underruns
- invalid packets
- lost or out-of-order packets

For 64- and 96-channel work:

- use wired Ethernet and avoid Wi-Fi
- keep DAW/host block sizes small, typically `64` or `128` samples
- expect high-channel packets to exceed standard Ethernet MTU unless the network path is tuned for larger frames
- reserve `96ch` operation for the core transport and future standalone app path; the VST3 wrapper is intentionally capped at `64ch`

Security model:

- SomeNET uses unauthenticated, unencrypted UDP/RTP and is intended for trusted local audio networks
- unicast receivers can restrict traffic to one expected source IP, or use `0.0.0.0` to accept any sender on the configured port
- multicast receivers accept packets from members of the joined group, so use switch, VLAN, firewall, and interface scoping controls on shared networks
- do not expose the receive port directly to untrusted networks or the public internet

Network capture:

- use `udp.port == <port>` in Wireshark
- this transport is UDP/RTP, not TCP

Runtime monitor:

- the temp-file runtime monitor is disabled by default
- enable it by launching the host with `SOMENET_DEBUG_RUNTIME=1`

## Releases

Tagging a version like `v0.1.0` triggers the release workflow.

That workflow:

- builds the plugin on macOS, Windows, and Linux
- packages platform-specific `.vst3` bundles
- zips the bundles for distribution
- uploads the archives to the matching GitHub release
- publishes a `SHA256SUMS.txt` file alongside the release assets

Signing and notarization setup is documented in [docs/distribution-signing.md](docs/distribution-signing.md).

## Limitations

- ST 2110-style alignment here is transport-focused, not a full broadcast stack
- `44.1 kHz` support is intentionally non-standard
- macOS is the primary supported development target right now
- the current custom GUI is macOS-only

## Roadmap

- stabilize cross-platform VST3 packaging and CI release builds
- broaden host interoperability testing
- add optional clocking/control-plane integrations where they materially improve interoperability
- keep the transport fast, simple, and unobtrusive in real-world creative workflows

The current latency and clocking plan is tracked in [docs/latency-and-ptp-plan.md](docs/latency-and-ptp-plan.md).

## Contributing

Issues and pull requests are welcome. Development setup, local build/test commands, packaging notes, and release workflow details are in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

SomeNET is released under the [MIT License](LICENSE).

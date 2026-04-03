<p align="center">
  <img src="assets/somethingNET-logo.png" alt="SomethingNet logo" width="720">
</p>

# SomethingNet

SomethingNet is a private Rust VST3 plugin from `Something Audio` for moving multichannel audio between creative tools over a network.

The current target workflow is:

- send audio from TouchDesigner into REAPER
- receive audio in REAPER with low latency and stable multichannel playback
- support direct unicast and multicast-style routing
- stay close to SMPTE ST 2110-30 transport conventions where practical

## What it does

SomethingNet runs as either a sender or receiver inside a VST3 host.

- `Send` mode passes audio through locally and transmits an RTP copy over UDP
- `Receive` mode listens for that RTP stream and renders it to the plugin outputs
- supports up to `16` channels per stream today
- supports `Unicast` and `Multicast` transport selection
- includes a native macOS editor intended to work in both REAPER and TouchDesigner

## Transport model

The plugin uses RTP with 24-bit linear PCM (`L24`) over UDP.

Supported operating modes:

- `48 kHz`, up to `16ch`
- `44.1 kHz`, up to `16ch`
- `96 kHz`, up to `8ch`

Standards notes:

- `48 kHz` transport is the main ST 2110-30 / AES67-aligned path
- `44.1 kHz` is supported intentionally, but it is not ST 2110-30 compliant
- higher channel counts above classic ST 2110-30 profile limits are supported as an engineering choice, with warning text surfaced in the plugin status

The sender also writes an SDP file to the system temp directory:

- `somethingnet.sdp`

## Current status

Implemented:

- Rust VST3 plugin built on the [`vst3`](https://crates.io/crates/vst3/0.3.0) crate
- sender and receiver modes
- unicast and multicast transport selection
- RTP/L24 packetization and decode
- receiver buffering, concealment, and drift-tolerant playout control
- background sender/receiver worker threads
- macOS native GUI
- TouchDesigner -> REAPER workflow validation

Not implemented yet:

- PTP clocking
- NMOS / control-plane discovery
- full ST 2110 system integration
- Windows-specific QoS handling

## Repository layout

- [src/lib.rs](src/lib.rs)
  VST3 processor, controller, factory, state, status formatting
- [src/network.rs](src/network.rs)
  RTP transport, sender/receiver workers, buffering, decode/encode
- [src/macos_gui.rs](src/macos_gui.rs)
  native macOS editor implementation
- [src/params.rs](src/params.rs)
  shared parameter definitions and defaults
- [src/editor_api.rs](src/editor_api.rs)
  opaque bridge used by the macOS editor
- [scripts/install_macos_vst3.sh](scripts/install_macos_vst3.sh)
  macOS bundle build/install script

## Developer setup

### Prerequisites

- macOS for the current bundle/install workflow
- Rust toolchain via `rustup`
- Xcode command line tools
- `clang`
- a VST3 host for testing, typically REAPER and/or TouchDesigner

Install Rust if needed:

```bash
curl https://sh.rustup.rs -sSf | sh
```

Confirm the basic toolchain:

```bash
cargo --version
rustc --version
clang --version
```

### Clone and build

```bash
git clone <private-repo-url>
cd somethingnet
cargo build
```

Release build:

```bash
cargo build --release
```

### Run tests

```bash
cargo test
```

Recommended local code-quality check:

```bash
cargo clippy --all-targets --all-features -- -W clippy::all
```

### Install the plugin on macOS

User-level install:

```bash
INSTALL_ROOT="$HOME/Library/Audio/Plug-Ins/VST3" scripts/install_macos_vst3.sh
```

System-level install:

```bash
sudo scripts/install_macos_vst3.sh
```

The installed bundle is:

- `~/Library/Audio/Plug-Ins/VST3/SomethingNet.vst3`
- or `/Library/Audio/Plug-Ins/VST3/SomethingNet.vst3`

The installer:

- builds the release artifact
- links it into a macOS `.vst3` bundle
- writes `Info.plist`
- performs ad-hoc codesigning if possible

## Development workflow

Typical loop:

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -W clippy::all
cargo test
INSTALL_ROOT="$HOME/Library/Audio/Plug-Ins/VST3" scripts/install_macos_vst3.sh
```

After reinstalling:

- fully reload or rescan plugins in REAPER
- reload the TouchDesigner VST node if needed

## Host setup notes

### REAPER

- rescan VST3 plugins after install if `SomethingNet` does not appear immediately
- make the receiving track wide enough for the expected channel count
- match sender and receiver sample rate, channel count, transport, group/source IP, and port

### TouchDesigner

- make sure the actual audio stream feeding the plugin is at the sample rate you expect
- for ST 2110-style operation, use `48,000 Hz`
- if TouchDesigner shows `44,100 Hz`, resample before the VST if required
- if using multichannel input, confirm the VST/audio bus layout matches the desired channel count

## Basic usage

### Unicast send -> receive

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

### Multicast publish -> subscribe

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

## Performance and debugging

The plugin includes runtime status output and host-visible debug text in the macOS editor.

Useful things to watch:

- send packet fill / queue behavior
- receive queue depth
- underruns
- invalid packets
- lost or out-of-order packets

Network capture:

- use `udp.port == <port>` in Wireshark
- this transport is UDP/RTP, not TCP

## Important limitations

- This is still an internal/private project, not a polished public product
- ST 2110-style alignment here is transport-focused, not a full broadcast stack
- `44.1 kHz` support is intentionally non-standard
- macOS is the primary supported development target right now

## Private repo note

This repository is intended to stay private. It contains internal product code and evolving transport decisions that are not being maintained as a public SDK or open-source package.

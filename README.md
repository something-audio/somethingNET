# ReaStream 2110-30

Rust VST3 sender/receiver plugin for TouchDesigner and REAPER that moves up to 8 channels of PCM audio over unicast RTP.

## Current scope

- Built with the [`vst3`](https://crates.io/crates/vst3/0.3.0) crate.
- Exposes host parameters for:
  - enable/disable streaming
  - mode: `Send` or `Receive`
  - channel count
  - UDP port
  - IPv4 octets
- In `Send` mode the plugin passes audio through unchanged while transmitting a network copy.
- In `Receive` mode the plugin renders the incoming network stream to its outputs.
- Uses `L24` RTP payload packing with fixed 1 ms packetization.
- Writes an SDP file to the system temp directory at `reastream2110-30.sdp`.
- On macOS, the plugin now provides a native editor view with an `Apply` button for TouchDesigner and REAPER.

## ST 2110-30 alignment

This implementation is aligned to the RTP/SDP transport model described by SMPTE ST 2110-30:2025 for PCM audio.

The mode labels below are an implementation choice based on public ST 2110-30/AES67 descriptions and RFC payload/clock signaling references. The linked SMPTE publication page itself was not machine-readable in this environment.
- `48 kHz`, `1 ms`, `1-8 channels`: sender level `A` style transport
- `96 kHz`, `1 ms`, `1-4 channels`: sender level `AX` style transport

The sender uses:

- RTP over UDP unicast
- `L24` payload packing
- channel interleaving across a single RTP stream
- SDP `rtpmap`, `fmtp`, `ptime`, `ts-refclk`, and `mediaclk` attributes

## Known limitations

- This is not a full broadcast-grade ST 2110 stack yet.
- No PTP clock integration is implemented. The SDP currently advertises `ts-refclk:local` and `mediaclk:direct=0`, which is useful for inspection and lab testing but is not a substitute for full ST 2110 system timing.
- No NMOS, SAP, SIP, or receiver-side control plane is included.
- Only IPv4 is implemented.
- The macOS editor currently applies changes when you press `Apply`. It is not yet a fully live-synced reactive editor.

## Build

```bash
cargo build --release
```

## Install in REAPER on macOS

```bash
scripts/install_macos_vst3.sh
```

The script creates:

`~/Library/Audio/Plug-Ins/VST3/ReaStream2110.vst3`

## TouchDesigner -> REAPER

1. In TouchDesigner, insert the plugin and set `Mode` to `Send`.
2. Set `IP` to the REAPER machine's address. For same-machine testing, use `127.0.0.1`.
3. Set the UDP port and channel count.
4. In REAPER, insert the same plugin and set `Mode` to `Receive`.
5. Use the same port, IP, and channel count in REAPER.
6. Put the REAPER track on the same channel width you intend to receive, up to `8` channels.
7. Enable both instances.

The generated SDP file can be used as a starting point for external RTP/AES67/ST 2110-30 analysis tools and receivers.

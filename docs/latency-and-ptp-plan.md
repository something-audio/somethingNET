# Latency and PTP Plan

This document describes the practical path from the current SomethingNet transport to a lower-latency, more clock-aware network audio stack.

## Current State

Today the plugin is intentionally conservative:

- sender startup prebuffering is enabled to smooth bursty host callbacks
- receiver playout targets more than one callback window plus packet safety
- drift correction uses duplicate/drop frame steering
- SDP currently advertises either a local clock or a PTP-traceable clock reference with a SomethingNet-specific PTP domain attribute

That design is robust for creative-tool interoperability, but it leaves avoidable latency on the table.

## Why Dante Gets Closer to 1 ms

Dante is optimized around:

- supported wired Ethernet networks
- tightly controlled receiver latency settings
- PTP-based clock synchronization across endpoints
- device-level implementations rather than generic plugin hosts

General-purpose plugin hosts add:

- host callback buffering
- OS scheduler jitter
- interface-driver latency
- burstier callback cadence than dedicated hardware

That means the right near-term target for SomethingNet is not “1 ms everywhere”, but “predictable low single-digit milliseconds on wired networks with small host buffers”.

## Phase 1: Low-Latency Transport Profile

Goal: reduce baseline latency without making the stream fragile.

Tasks:

- add an explicit latency mode or target buffer parameter in milliseconds
- reduce sender startup prebuffering for an aggressive wired profile
- make receiver target depth track the requested latency, not a fixed conservative callback multiple
- surface actual target latency in milliseconds in the runtime monitor
- document recommended host buffer sizes for low-latency operation

Success criteria:

- stable operation on wired Ethernet at `48 kHz`
- clean playout with small host buffer sizes such as `64` or `128`
- materially lower end-to-end latency than the current conservative defaults

## Phase 2: Better Drift Handling

Goal: improve sound quality when clocks are close but not identical.

Tasks:

- replace duplicate/drop playout correction with fractional resampling
- keep long-term queue depth centered without audible correction artifacts
- expose queue target, correction rate, and effective latency in the runtime status

Success criteria:

- lower artifact risk than the current duplicate/drop strategy
- stable queue behavior over longer sessions

## Phase 3: Real PTP Integration

Goal: move from PTP-aware signaling to actual clock discipline.

Tasks:

- detect and bind to a real PTP source where the operating system exposes one
- support at least one concrete deployment path, likely Linux with `ptp4l` / `phc2sys`, before claiming broad cross-platform support
- advertise a standards-based `ts-refclk` with a real grandmaster identity when available
- carry the chosen PTP domain through the stream/session model
- report PTP lock state, domain, and reference source in the runtime monitor

Current gap:

- the code now supports PTP-aware stream configuration and SDP signaling
- it does not yet discipline the host audio clock to a hardware or daemon-backed PTP source

Success criteria:

- stream timing is referenced to a real PTP clock source
- the plugin can report whether it is actually locked
- SDP reflects a real, not inferred, timing reference

## Phase 4: Network and Interop Hardening

Goal: make low-latency operation sustainable outside the lab.

Tasks:

- validate on wired managed switches
- add packet impairment soak tests
- test multiple senders/receivers and multicast fan-out
- widen host interoperability coverage
- consider AES67 / ST 2110-30 control-plane and discovery work only where it materially improves interoperability

## Practical Recommendations Right Now

For the current codebase:

- use wired Ethernet, not Wi-Fi, for serious latency work
- run `48 kHz` when targeting ST 2110-style operation
- keep host block sizes low
- treat current PTP support as signaling groundwork, not full synchronization


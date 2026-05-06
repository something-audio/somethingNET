# Contributing

Thanks for contributing to SomeNET.

This document covers local setup, development workflow, packaging, and release mechanics. For product overview and runtime usage, see [README.md](README.md).

## Development Setup

Recommended local environment:

- Rust via `rustup`
- `cargo`
- `clang`
- Xcode command line tools on macOS
- a VST3 host for testing, typically REAPER and/or TouchDesigner

Install Rust if needed:

```bash
curl https://sh.rustup.rs -sSf | sh
```

Confirm the toolchain:

```bash
cargo --version
rustc --version
clang --version
```

Clone and build:

```bash
git clone https://github.com/something-audio/SomeNET.git
cd SomeNET
cargo build
```

Release build:

```bash
cargo build --release
```

## Local Verification

Run the standard local checks before opening a pull request:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -W clippy::all
cargo test
```

## Install for Host Testing

macOS user-level install:

```bash
INSTALL_ROOT="$HOME/Library/Audio/Plug-Ins/VST3" scripts/install_macos_vst3.sh
```

macOS system-level install:

```bash
sudo scripts/install_macos_vst3.sh
```

Linux user-level install:

```bash
INSTALL_ROOT="$HOME/.vst3" scripts/install_linux_vst3.sh
```

Windows install from PowerShell:

```powershell
.\scripts\install_windows_vst3.ps1
```

The installers:

- build the release artifact
- package the platform-specific `.vst3` bundle
- install it into the selected VST3 directory
- ad-hoc sign the macOS bundle when `codesign` is available

After reinstalling:

- reload or rescan plugins in REAPER
- reload the TouchDesigner VST node if needed

## Development Workflow

A typical local loop:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -W clippy::all
cargo test
INSTALL_ROOT="$HOME/Library/Audio/Plug-Ins/VST3" scripts/install_macos_vst3.sh
```

If you are changing transport behavior, host compatibility, or cross-platform packaging, include:

- exact host versions
- sample rate
- channel count
- unicast or multicast mode
- reproduction steps

## Continuous Integration

GitHub Actions validates the project on:

- macOS
- Windows
- Linux

The CI workflow runs:

- `cargo build --release`
- `cargo test`
- `cargo clippy --all-targets --all-features -- -W clippy::all`
- cross-platform VST3 packaging smoke checks

## Release Workflow

Tagging a version like `v0.1.0` triggers the release workflow.

That workflow:

- builds the plugin on macOS, Windows, and Linux
- packages platform-specific `.vst3` bundles
- zips the bundles for distribution
- uploads the archives to the matching GitHub release
- publishes a `SHA256SUMS.txt` file alongside the release assets

Packaging helper:

- [scripts/package_vst3.py](scripts/package_vst3.py)

Workflow files:

- [.github/workflows/ci.yml](.github/workflows/ci.yml)
- [.github/workflows/release.yml](.github/workflows/release.yml)

Release signing and notarization requirements are documented in [docs/distribution-signing.md](docs/distribution-signing.md).

## Code Style Notes

- keep the audio thread free of blocking calls, file I/O, and unnecessary allocation
- prefer bounded queues and predictable behavior over cleverness
- keep cross-platform changes explicit; platform-specific helpers are preferable to fragile generic code
- document `unsafe` blocks where they are necessary for VST3/FFI boundaries

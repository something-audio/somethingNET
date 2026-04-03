# Windows GUI Validation With UTM

This is the quickest practical path to validate the Windows plugin UI on an Apple Silicon Mac.

## What You Need

- UTM installed on macOS
- a Windows 11 ARM installer ISO
- a valid Windows license if you intend to keep using the VM
- the latest `SomethingNet.vst3` Windows build artifact from CI or a local Windows build output

Official references:

- [UTM Windows guide](https://docs.getutm.app/guides/windows/)
- [UTM Windows 11 ARM gallery entry](https://mac.getutm.app/gallery/windows-11-arm)
- [UTM home](https://mac.getutm.app/)

## Recommended Path

On Apple Silicon, use Windows 11 ARM with UTM virtualization, not emulated x64 Windows.

UTM recommends:

- `Virtualize`
- `Windows`
- at least `2` CPU cores
- roughly `8 GiB` RAM
- `Install Windows 10 or higher`
- `Install drivers and SPICE tools`

## Step 1. Install UTM

Install UTM from either:

- the Mac App Store
- the official UTM site

## Step 2. Get Windows 11 ARM

The official UTM guide recommends obtaining the installer ISO with CrystalFetch on macOS.

Alternative:

- download the Windows 11 ARM ISO directly from Microsoft if available for your account and region

## Step 3. Create the VM

In UTM:

1. Click `+`
2. Select `Virtualize`
3. Select `Windows`
4. Assign:
   - `8 GiB` RAM
   - `4` CPU cores on this M1 Pro is a sensible starting point
5. Keep `Install Windows 10 or higher` enabled
6. Keep `Install drivers and SPICE tools` enabled
7. Choose the Windows ARM ISO
8. Create a virtual disk of at least `40 GiB`
9. Optionally add a shared directory that points to this repo or to a release-artifacts folder

## Step 4. Install Windows

Inside the VM:

1. Complete Windows setup
2. Let SPICE tools install
3. Reboot if prompted

If the installer complains that the PC cannot run Windows 11:

- confirm you selected the ARM build
- confirm the VM has at least `2` cores
- follow the UTM troubleshooting notes if Secure Boot / TPM checks still appear

## Step 5. Get the Plugin Into the VM

Recommended options:

- download the Windows release ZIP from GitHub Releases inside the VM
- or use the UTM shared folder to copy the packaged Windows `SomethingNet.vst3`

Also install a Windows VST3 host for testing. Good options:

- REAPER
- TouchDesigner on Windows, if that is the real target host

## Step 6. Install the Plugin

On Windows, VST3 plugins normally go to:

`C:\Program Files\Common Files\VST3`

Copy `SomethingNet.vst3` there.

## Step 7. Validate The GUI

Open the plugin in the target host and verify:

- the custom editor opens instead of generic host parameter controls
- the `ARM` latch renders as a button, not a numeric parameter
- `Mode` and `Transport` render as distinct segmented buttons
- runtime panel text is readable
- send/receive tinting is obvious enough to distinguish same-machine instances
- focus, mouse clicks, and `Apply` all work correctly

## What To Capture

For the first Windows validation pass, collect:

- a screenshot of the whole editor
- a screenshot in `Send`
- a screenshot in `Receive`
- note any control that renders as generic host UI rather than the custom editor
- note any missing text, clipped layout, or wrong colors

## Known Constraint

This repo can implement the Windows custom editor, but actual visual validation still depends on running a Windows host. On this Apple Silicon Mac, UTM with Windows 11 ARM is the most realistic route.

# Distribution Signing

SomethingNet now has release-workflow hooks for:

- macOS Developer ID signing and notarization
- Windows code signing
- Linux release-archive signatures

The repository can automate these steps once you provide the appropriate credentials.

## macOS

Implemented:

- sign the `.vst3` bundle with `codesign`
- enable hardened runtime
- submit for notarization with `notarytool`
- staple the notarization ticket to the bundle before archiving

Required from you:

- Apple Developer Program membership
- a `Developer ID Application` certificate exported as `.p12`
- the certificate password
- the exact signing identity string
- your Apple Team ID
- notarization credentials

Supported secrets:

- `APPLE_SIGNING_CERTIFICATE_P12_BASE64`
- `APPLE_SIGNING_CERTIFICATE_PASSWORD`
- `APPLE_CODESIGN_IDENTITY`
- `APPLE_TEAM_ID`
- `APPLE_NOTARY_KEYCHAIN_PROFILE`

Alternative notarization credentials:

- `APPLE_NOTARY_APPLE_ID`
- `APPLE_NOTARY_PASSWORD`
- `APPLE_TEAM_ID`

Recommended setup:

- use a `Developer ID Application` certificate
- use `notarytool` with either a stored keychain profile or an app-specific password

Official references:

- [Apple notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution?changes=_5)

## Windows

Implemented:

- optional signing of the Windows VST3 payload before packaging
- support for either Azure Trusted Signing or a local `.p12` certificate on the GitHub runner

Preferred option:

- Azure Trusted Signing

Secrets for Azure Trusted Signing:

- `AZURE_TENANT_ID`
- `AZURE_CLIENT_ID`
- `AZURE_CLIENT_SECRET`
- `AZURE_TRUSTED_SIGNING_ENDPOINT`
- `AZURE_TRUSTED_SIGNING_ACCOUNT_NAME`
- `AZURE_TRUSTED_SIGNING_CERTIFICATE_PROFILE_NAME`

Fallback option:

- a standard code-signing certificate exported as `.p12`

Secrets for PFX signing:

- `WINDOWS_SIGN_CERTIFICATE_P12_BASE64`
- `WINDOWS_SIGN_CERTIFICATE_PASSWORD`
- `WINDOWS_SIGN_TIMESTAMP_URL`

Official references:

- [Microsoft SignTool](https://learn.microsoft.com/en-us/windows/win32/seccrypto/signtool)
- [Azure Trusted Signing integrations](https://learn.microsoft.com/en-us/azure/trusted-signing/how-to-signing-integrations)

## Linux

There is no broadly enforced “code signing for VST3 bundles” equivalent to macOS Gatekeeper or Windows Authenticode.

Implemented:

- release archives can be detached-signed with GPG in the publish job
- `SHA256SUMS.txt` is generated for every release

Required from you if you want signed Linux release artifacts:

- a GPG private key exported in base64 form
- the passphrase for that key

Supported secrets:

- `RELEASE_GPG_PRIVATE_KEY_BASE64`
- `RELEASE_GPG_PASSPHRASE`

## What This Does Not Solve

Signing improves distribution trust, but it does not replace:

- host compatibility testing
- antivirus reputation on Windows
- sensible release notes and checksums
- real PTP clock lock for low-latency network audio

## Clock Discipline Reality

The plugin can now advertise and persist clock-reference intent, including a PTP mode and PTP domain. That is useful groundwork, but it is not full clock discipline.

Why:

- the DAW or creative host owns the audio callback clock
- the audio device driver owns the hardware sample clock
- a VST plugin cannot unilaterally retime the hardware clock on macOS, Windows, or Linux

The next real step for clock discipline is system-level integration:

- Linux: integrate with `ptp4l` / `phc2sys` or another OS-visible PTP service
- macOS / Windows: rely on an external PTP-capable device/driver path or a dedicated synchronization service
- inside the plugin: replace duplicate/drop correction with a proper fractional-resampling PLL

That roadmap is tracked in [latency-and-ptp-plan.md](latency-and-ptp-plan.md).

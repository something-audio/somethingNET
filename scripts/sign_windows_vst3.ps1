$ErrorActionPreference = "Stop"

param(
    [Parameter(Mandatory = $true)]
    [string]$BundleRoot
)

$signFile = Join-Path $BundleRoot "Contents\x86_64-win\SomeNET.vst3"
if (-not (Test-Path $signFile)) {
    throw "Signable file not found: $signFile"
}

$certBase64 = $env:WINDOWS_SIGN_CERTIFICATE_P12_BASE64
$certPassword = $env:WINDOWS_SIGN_CERTIFICATE_PASSWORD
if ([string]::IsNullOrWhiteSpace($certBase64) -or [string]::IsNullOrWhiteSpace($certPassword)) {
    throw "WINDOWS_SIGN_CERTIFICATE_P12_BASE64 and WINDOWS_SIGN_CERTIFICATE_PASSWORD are required"
}

$signtool = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe" |
    Sort-Object FullName -Descending |
    Select-Object -First 1

if (-not $signtool) {
    throw "Unable to locate signtool.exe from the Windows SDK"
}

$timestampUrl = if ([string]::IsNullOrWhiteSpace($env:WINDOWS_SIGN_TIMESTAMP_URL)) {
    "http://timestamp.acs.microsoft.com"
} else {
    $env:WINDOWS_SIGN_TIMESTAMP_URL
}

$certPath = Join-Path $env:RUNNER_TEMP "somenet-signing-cert.p12"
[IO.File]::WriteAllBytes($certPath, [Convert]::FromBase64String($certBase64))

try {
    & $signtool.FullName sign `
        /fd SHA256 `
        /td SHA256 `
        /tr $timestampUrl `
        /f $certPath `
        /p $certPassword `
        /d "SomeNET VST3 plugin" `
        $signFile

    & $signtool.FullName verify /pa /v $signFile
} finally {
    Remove-Item $certPath -ErrorAction SilentlyContinue
}

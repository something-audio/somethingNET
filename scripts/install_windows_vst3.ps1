$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$pluginName = "SomethingNet"

if (-not $env:INSTALL_ROOT -or [string]::IsNullOrWhiteSpace($env:INSTALL_ROOT)) {
    $installRoot = Join-Path $env:COMMONPROGRAMFILES "VST3"
} else {
    $installRoot = $env:INSTALL_ROOT
}

$pluginBundle = Join-Path $installRoot ($pluginName + ".vst3")

cargo build --release --manifest-path (Join-Path $root "Cargo.toml")
python (Join-Path $root "scripts/package_vst3.py") `
    --platform windows `
    --bundle-root $pluginBundle

Write-Host "Installed $pluginBundle"

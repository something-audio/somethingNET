#!/bin/zsh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${ROOT_DIR}/target/release"
STATIC_ARTIFACT="${TARGET_DIR}/libsomethingnet_vst3.a"
PLUGIN_NAME="SomethingNet"
INSTALL_ROOT="${INSTALL_ROOT:-/Library/Audio/Plug-Ins/VST3}"
PLUGIN_BUNDLE="${INSTALL_ROOT}/${PLUGIN_NAME}.vst3"
PLUGIN_BINARY="${PLUGIN_BUNDLE}/Contents/MacOS/${PLUGIN_NAME}"
PLIST_PATH="${PLUGIN_BUNDLE}/Contents/Info.plist"
PKGINFO_PATH="${PLUGIN_BUNDLE}/Contents/PkgInfo"

cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"

mkdir -p "${PLUGIN_BUNDLE}/Contents/MacOS"
rm -f "${PLUGIN_BINARY}"

clang \
  -bundle \
  -Wl,-force_load,"${STATIC_ARTIFACT}" \
  -Wl,-exported_symbol,_GetPluginFactory \
  -Wl,-exported_symbol,_bundleEntry \
  -Wl,-exported_symbol,_bundleExit \
  -Wl,-undefined,dynamic_lookup \
  -o "${PLUGIN_BINARY}"

cat > "${PLIST_PATH}" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>SomethingNet</string>
  <key>CFBundleIdentifier</key>
  <string>com.somethingaudio.somethingnet</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>SomethingNet</string>
  <key>CFBundlePackageType</key>
  <string>BNDL</string>
  <key>CFBundleSignature</key>
  <string>????</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleSupportedPlatforms</key>
  <array>
    <string>MacOSX</string>
  </array>
  <key>CFBundleVersion</key>
  <string>0.1.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
</dict>
</plist>
EOF

printf 'BNDL????' > "${PKGINFO_PATH}"

chmod +x "${PLUGIN_BINARY}"
codesign --force --sign - "${PLUGIN_BUNDLE}" >/dev/null 2>&1 || true

echo "Installed ${PLUGIN_BUNDLE}"

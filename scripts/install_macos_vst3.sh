#!/bin/zsh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PLUGIN_NAME="SomeNET"
INSTALL_ROOT="${INSTALL_ROOT:-/Library/Audio/Plug-Ins/VST3}"
PLUGIN_BUNDLE="${INSTALL_ROOT}/${PLUGIN_NAME}.vst3"

cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"
python3 "${ROOT_DIR}/scripts/package_vst3.py" \
  --platform macos \
  --bundle-root "${PLUGIN_BUNDLE}"

codesign --force --sign - "${PLUGIN_BUNDLE}" >/dev/null 2>&1 || true

echo "Installed ${PLUGIN_BUNDLE}"

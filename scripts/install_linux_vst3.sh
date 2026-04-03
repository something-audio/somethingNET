#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PLUGIN_NAME="SomethingNet"
INSTALL_ROOT="${INSTALL_ROOT:-$HOME/.vst3}"
PLUGIN_BUNDLE="${INSTALL_ROOT}/${PLUGIN_NAME}.vst3"

cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"
python3 "${ROOT_DIR}/scripts/package_vst3.py" \
  --platform linux \
  --bundle-root "${PLUGIN_BUNDLE}"

echo "Installed ${PLUGIN_BUNDLE}"

#!/bin/zsh
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 /path/to/SomeNET.vst3" >&2
  exit 1
fi

BUNDLE_PATH="$1"
IDENTITY="${APPLE_CODESIGN_IDENTITY:-}"
KEYCHAIN_PATH="${APPLE_CODESIGN_KEYCHAIN:-}"

if [[ -z "${IDENTITY}" ]]; then
  echo "APPLE_CODESIGN_IDENTITY is required for macOS signing" >&2
  exit 1
fi

SIGN_ARGS=(
  --force
  --deep
  --strict
  --options runtime
  --timestamp
  --sign "${IDENTITY}"
)

if [[ -n "${KEYCHAIN_PATH}" ]]; then
  SIGN_ARGS+=(--keychain "${KEYCHAIN_PATH}")
fi

codesign "${SIGN_ARGS[@]}" "${BUNDLE_PATH}"
codesign --verify --deep --strict --verbose=2 "${BUNDLE_PATH}"

if [[ -n "${APPLE_NOTARY_KEYCHAIN_PROFILE:-}" ]]; then
  TMP_ARCHIVE="$(mktemp "${TMPDIR:-/tmp}/somenet-notary-XXXXXX.zip")"
  rm -f "${TMP_ARCHIVE}"
  ditto -c -k --keepParent "${BUNDLE_PATH}" "${TMP_ARCHIVE}"
  xcrun notarytool submit "${TMP_ARCHIVE}" \
    --keychain-profile "${APPLE_NOTARY_KEYCHAIN_PROFILE}" \
    --wait
  xcrun stapler staple "${BUNDLE_PATH}"
  rm -f "${TMP_ARCHIVE}"
elif [[ -n "${APPLE_NOTARY_APPLE_ID:-}" && -n "${APPLE_NOTARY_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
  TMP_ARCHIVE="$(mktemp "${TMPDIR:-/tmp}/somenet-notary-XXXXXX.zip")"
  rm -f "${TMP_ARCHIVE}"
  ditto -c -k --keepParent "${BUNDLE_PATH}" "${TMP_ARCHIVE}"
  xcrun notarytool submit "${TMP_ARCHIVE}" \
    --apple-id "${APPLE_NOTARY_APPLE_ID}" \
    --password "${APPLE_NOTARY_PASSWORD}" \
    --team-id "${APPLE_TEAM_ID}" \
    --wait
  xcrun stapler staple "${BUNDLE_PATH}"
  rm -f "${TMP_ARCHIVE}"
fi

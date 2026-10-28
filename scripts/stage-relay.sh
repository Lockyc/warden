#!/usr/bin/env bash
# stage-relay.sh [universal] — build warden-relay and stage it at crates/warden-app/binaries/warden-relay,
# the path tauri.conf.json's bundle.macOS.files copies into Contents/MacOS. Host arch by default;
# `universal` (arm64 + x86_64, lipo'd) matches the release bundle's architectures.
set -euo pipefail
cd "$(dirname "$0")/.."

out=crates/warden-app/binaries
mkdir -p "$out"

if [ "${1:-}" = "universal" ]; then
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release -p warden-relay --target "$t"
  done
  lipo -create -output "$out/warden-relay" \
    target/aarch64-apple-darwin/release/warden-relay target/x86_64-apple-darwin/release/warden-relay
else
  cargo build --release -p warden-relay
  cp target/release/warden-relay "$out/warden-relay"
fi

# `bundle.macOS.files` copies the relay in verbatim and tauri's bundle signing seals it without
# re-signing it, so the relay needs its own hardened-runtime signature for notarization to accept
# the nested executable. Same env switch as the app: no identity, no signing.
if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
  ts=--timestamp
  [ "$APPLE_SIGNING_IDENTITY" = "-" ] && ts=--timestamp=none
  codesign --force --options runtime "$ts" --sign "$APPLE_SIGNING_IDENTITY" "$out/warden-relay"
fi

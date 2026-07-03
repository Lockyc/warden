#!/usr/bin/env bash
# gen-latest-json.sh — emit the tauri-updater manifest (latest.json) for the current warden release.
#
# Usage: scripts/gen-latest-json.sh <version> <out-path>
#
# Reads the bundler's signed updater artifact (warden.app.tar.gz.sig) and writes a manifest the
# updater fetches from https://github.com/lockyc/warden/releases/latest/download/latest.json. The
# `.sig` only exists when `cargo tauri build` ran with the updater signing env set
# (TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]); without it this errors rather than emit an unsigned
# manifest. macOS/Apple Silicon only → a single darwin-aarch64 platform entry.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:?usage: gen-latest-json.sh <version> <out-path>}"
OUT="${2:?usage: gen-latest-json.sh <version> <out-path>}"

SIG_FILE="target/release/bundle/macos/warden.app.tar.gz.sig"
if [ ! -f "$SIG_FILE" ]; then
  echo "gen-latest-json: no updater signature at $SIG_FILE" >&2
  echo "  → build with TAURI_SIGNING_PRIVATE_KEY[_PASSWORD] set first." >&2
  exit 1
fi

SIG="$(cat "$SIG_FILE")"
URL="https://github.com/lockyc/warden/releases/download/v${VERSION}/warden.app.tar.gz"

cat > "$OUT" <<JSON
{
  "version": "${VERSION}",
  "notes": "See the release notes at https://github.com/lockyc/warden/releases/tag/v${VERSION}",
  "platforms": {
    "darwin-aarch64": {
      "signature": "${SIG}",
      "url": "${URL}"
    }
  }
}
JSON

echo "gen-latest-json: wrote $OUT (v${VERSION}, darwin-aarch64)"

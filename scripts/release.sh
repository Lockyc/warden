#!/usr/bin/env bash
# Build, notarize, zip, and attach the warden.app bundle to its GitHub release.
#
# The version is single-sourced in crates/warden-app/Cargo.toml — tauri.conf.json has no
# `version` key, so the bundle inherits it. Run this AFTER the release commit is tagged and
# pushed and `gh release create v<version>` has published the notes (see CLAUDE.md › Releases);
# this script only builds + attaches the macOS artifact.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/warden-app/Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "release: could not read version from crates/warden-app/Cargo.toml" >&2; exit 1; }
TAG="v$VERSION"
ZIP="warden-${VERSION}-macos.zip"
APP="target/release/bundle/macos/warden.app"

# A release artifact MUST be signed + notarized — an unsigned zip is Gatekeeper-blocked on
# other Macs, so refuse rather than ship one that looks official but won't open. (Contributors
# building for local use go through `just build`/`just deploy`, which tolerate unsigned.)
if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  echo "release: APPLE_SIGNING_IDENTITY is unset — the build would be unsigned/un-notarized." >&2
  echo "         Set APPLE_SIGNING_IDENTITY + APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID" >&2
  echo "         (or APPLE_API_KEY/APPLE_API_ISSUER/APPLE_API_KEY_PATH) before releasing." >&2
  exit 1
fi

# The release must exist (notes published) before we attach to it.
if ! gh release view "$TAG" >/dev/null 2>&1; then
  echo "release: GitHub release $TAG not found — run 'gh release create $TAG' first." >&2
  exit 1
fi

echo "→ building + notarizing warden $VERSION (cargo tauri build) …"
( cd crates/warden-app && cargo tauri build )
[ -d "$APP" ] || { echo "release: bundle not found at $APP" >&2; exit 1; }

echo "→ zipping $APP → $ZIP (ditto, preserves the stapled notarization ticket)"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"

echo "→ uploading $ZIP to release $TAG"
gh release upload "$TAG" "$ZIP" --clobber

echo "✓ attached $ZIP to $TAG"

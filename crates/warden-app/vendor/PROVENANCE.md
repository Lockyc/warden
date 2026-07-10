# Vendored libghostty

`GhosttyKit.xcframework` (macOS slice only) + `ghostty.h` are **[Ghostty](https://github.com/ghostty-org/ghostty)
compiled from a pinned, unmodified upstream commit** by our own CI:
**[github.com/lockyc/libghostty-build](https://github.com/lockyc/libghostty-build)**. Ghostty is
MIT-licensed; the notice travels with the compiled bytes in `LICENSE-ghostty` (this directory).

## This build

- **Source:** `lockyc/libghostty-build` release [`ghostty-35e1a01`](https://github.com/lockyc/libghostty-build/releases/tag/ghostty-35e1a01).
- **Ghostty commit (unmodified upstream):** `35e1a0160c4f6797e1bb1ef8e7a2b8c6b114ab58` (`main`, incl.
  PR #13264 scrollback compression; milestone 1.4.0).
- **Release asset `GhosttyKit.xcframework.zip` sha256:** `c706698258655d782b7c4f74b9fd0fb5bff582e465defc9408872b6a8de33338`.
- **Committed `macos-arm64_x86_64/libghostty.a` sha256:** `1f145621fe8fc90253856a68d2b948f2c58d9f7fca7b433313c3311ee5644940`.
- **Toolchain:** Zig 0.15.2 on a GitHub `macos-15` runner (Sequoia SDK — Zig 0.15.2 cannot link the
  macOS 26 SDK, so the runner-OS choice is load-bearing). Built via Ghostty's own
  `-Demit-xcframework` target.
- **Provenance:** the release carries a GitHub **build-provenance attestation** over the zip
  (`gh attestation verify GhosttyKit.xcframework.zip --repo lockyc/libghostty-build`).

## What was repackaged (bytes unmodified)

The compiled libghostty bytes are exactly upstream's; `libghostty-build` only re-wraps them:

- Ghostty's native xcframework ships the macOS archive as `ghostty-internal.a` (renamed from
  `libghostty.a` since v1.3.1) and bundles iOS slices. We extract the macOS-universal archive, name it
  `libghostty.a` (warden links `-lghostty` → `libghostty.a`), and keep only the `macos-arm64_x86_64`
  slice.
- **Debug-stripped** (`strip -S`): Zig ReleaseFast emits ~280MB of DWARF; stripped to ~48MB with the
  exported `ghostty_*` C API intact.

## Updating

Bump `GHOSTTY_REF` in `libghostty-build`, let its CI republish, then `just revendor-ghostty` (verifies
the sha256 and swaps this directory) and refresh the shas above. **libghostty's embedding C API is
officially unstable** — after a version jump, diff `Headers/ghostty.h` against `crates/warden-app/src/ffi/mod.rs`
(the `size_of!` guards in `ffi/mod.rs` fail the build on struct-size drift; enum discriminants are not
guarded — eyeball the action tags).

## Security note

warden links this archive in-process with full user privileges (it spawns the user's shells). It is a
build **we** control from a commit **we** pin, with build-provenance attestation — not a third party's
self-attested prebuilt. Verify the attestation (above) and the committed `.a` sha256 if in doubt.

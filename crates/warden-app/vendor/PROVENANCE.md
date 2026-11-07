---
type: reference
links:
  - rel: part-of
    to: docs/surface.md
    note: docs/surface.md documents the revendor procedure that updates this file
---

# Vendored libghostty

`GhosttyKit.xcframework` (macOS slice only, its `Headers/ghostty.h` included) + `resources/` are
**[Ghostty](https://github.com/ghostty-org/ghostty) compiled from a pinned, unmodified upstream
commit** by our own CI: **[github.com/lockyc/libghostty-build](https://github.com/lockyc/libghostty-build)**.
Ghostty is MIT-licensed; the notice travels with the compiled bytes in `LICENSE-ghostty` (this
directory). The library and the resources come from the **same release** — keep them in lockstep.

## This build

- **Source:** `lockyc/libghostty-build` release [`ghostty-0fa3d57`](https://github.com/lockyc/libghostty-build/releases/tag/ghostty-0fa3d57).
- **Ghostty commit (unmodified upstream):** `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec` (tag
  `vt-1.0.0`, 2026-11-06 — the libghostty-vt 1.0 release).
- **Release asset `GhosttyKit.xcframework.zip` sha256:** `392e653ac1e35c3a4fa8113bea5ef8f1f65a8733ab224d93546c4c9a7c8cc79f`.
- **Release asset `GhosttyResources.zip` sha256:** `b136573cbf3c96d03a9d49337485296a2cf666707a4a475ec501dcf751840659`.
- **Release asset `libghostty-vt-macos.zip` sha256:** `bc796969a52effe9f4b3b4d88af54a6698224b8531bd2fa55d22a7e18d94f8b7` — Ghostty's standalone VT library (`-Demit-lib-vt`), universal and debug-stripped, vendored to `crates/warden-relay/vendor/libghostty-vt/` (`lib/libghostty-vt.a` sha256 `e642851f782e2a3d345abe75f678adefc5a03ddaac026877ea453d228b9a56b9`, plus `include/ghostty/`).
- **Committed `macos-arm64_x86_64/libghostty.a` sha256:** `f0b74183c6189dd7af94da6eba298c8ed84c6e2adb1006e17c11d3af08f103f2`.
- **Toolchain:** Zig 0.17.0 (Ghostty's `minimum_zig_version` at that commit) on a GitHub `macos-15`
  runner (Sequoia SDK — the SDK the build is verified against). Built via Ghostty's own
  `-Demit-xcframework` target.
- **Provenance:** the release carries a GitHub **build-provenance attestation** over each of the
  three zips (`gh attestation verify <zip> --repo lockyc/libghostty-build`).

## `resources/` — required, not optional

`resources/` holds libghostty's **runtime** resources from the same release: `terminfo/`
(tic-compiled `xterm-ghostty`) and `ghostty/shell-integration/`. `tauri.conf.json` bundles them to
`warden.app/Contents/Resources/{terminfo,ghostty}`, and unbundled dev runs are pointed at this
directory via `GHOSTTY_RESOURCES_DIR` (`main.rs::configure_ghostty_resources`).

**Ship them or warden is a broken libghostty host.** At surface spawn libghostty climbs from the
executable for the sentinel `Contents/Resources/terminfo/78/xterm-ghostty`; miss it and it *silently*
exports `TERM=xterm-256color` instead of `xterm-ghostty`. Every program in the tab then thinks it's a
plain xterm and loses **synchronized output (DEC 2026)**, styled/coloured underlines, and shell
integration. Synchronized output is the sharp edge: it is the only signal that makes libghostty pause
rendering, so without it tmux's unbracketed redraws get sampled half-drawn — and an unfocused surface,
which paints its hollow cursor on *every* frame, flickers that cursor at mid-repaint positions (the
"cursor flashes around the screen when the window isn't focused" bug). The failure is silent, so
`just revendor-ghostty` hard-fails if the sentinel is missing.

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
all three sha256s and swaps this directory) and refresh the shas above. **A rebuild is not reproducible:
re-running the CI on the *same* ref clobbers the release assets with byte-different ones** (the Zig
static archive embeds build paths/timestamps), so a revendor then churns the committed 48MB
`libghostty.a` for no functional change. Re-run the build only to change the pin or the packaging —
not to "refresh" it. **libghostty's embedding C API is
officially unstable** — after a version jump, diff `Headers/ghostty.h` against `crates/warden-app/src/ffi/mod.rs`
(the `size_of!` guards in `ffi/mod.rs` fail the build on struct-size drift; enum discriminants are not
guarded — eyeball the action tags). libghostty-vt is the opposite: its C ABI is semver-stable from
1.0, so within `vt-1.x` the hand-written declarations in `warden-relay/src/vt.rs` hold, and only a
major bump needs them re-checked against `include/ghostty/vt/`.

## Security note

warden links this archive in-process with full user privileges (it spawns the user's shells). It is a
build **we** control from a commit **we** pin, with build-provenance attestation — not a third party's
self-attested prebuilt. Verify the attestation (above) and the committed `.a` sha256 if in doubt.

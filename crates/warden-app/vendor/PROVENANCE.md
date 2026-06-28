# Vendored libghostty (spike artifact)

`GhosttyKit.xcframework` (macOS slice only) + `ghostty.h` are a THROWAWAY spike artifact.

Ghostty is MIT-licensed; redistributing its compiled bytes here carries its
license notice, in `LICENSE-ghostty` (this directory).

- Source: third-party prebuilt `Lakr233/libghostty-spm`, release `storage.1.2.7`
  (https://github.com/Lakr233/libghostty-spm/releases/download/storage.1.2.7/GhosttyKit.xcframework.zip)
  zip sha256 `1c3d62a635ac62f5402fd5083e7f5e2628f3da50f490b8456d37186163986df6`.
- Committed `macos-arm64_x86_64/libghostty.a` sha256 `09b12da97e2a564f675d2844c9216e50be53fdd92d5c3b674ac014ebd1daea48`.
- **Integrity caveat:** the zip hash only matches the vendor's own `Package.swift` — that proves
  download integrity against Lakr233's manifest, NOT that the bytes are genuine, unmodified upstream
  Ghostty. This is **vendor self-attestation, not independent verification.** The binary links and
  runs in-process with full user privileges (it spawns the user's shells), so treat it as untrusted-
  but-pragmatic for the spike; the Plan-2 upstream source build is what actually removes this risk.
- The vendored `GhosttyKit.xcframework/Info.plist` still lists the `ios-*` slices in
  `AvailableLibraries` even though only `macos-arm64_x86_64/` was kept — stale vendor metadata,
  harmless (the build hardcodes the macOS slice path).
- Why prebuilt, not source: building GhosttyKit from upstream needs Zig 0.15.2, which cannot
  link against this machine's macOS 26.5 SDK (Xcode 26.5) — a bleeding-edge-OS toolchain gap,
  not a project problem. Documented in the spike spec §5.1.
- This build carries non-upstream iOS patches and is independently versioned. It is fine for
  proving the embed (the spike's question); it is NOT the production pin. Plan 2 must replace it
  with a controlled upstream source build once the OS/Zig situation allows (or via a CI runner).
- Only the `macos-arm64_x86_64` slice (universal static `libghostty.a`) is kept; iOS slices pruned.

# warden — deferred follow-ups

Known, intentionally-deferred work. Each item is a conscious deferral, not an oversight — recorded here so it isn't lost. Remove an item when it's done.

## For Plan 2 (the Tauri app) to add when it wires its consumer

These are trivial, zero-retrofit additions the consumer makes the moment it needs them; deferred from the config-core crate per YAGNI.

- **Re-export `Colour`, `watch::Watcher`, and `resolve::ResolveError` at the crate root** (`crates/warden-config/src/lib.rs`). Today a consumer reaching `Profile.colour`, constructing a `Watcher`, or matching on `LoadError::Resolve(ResolveError)` must reach into submodules. Add `pub use` lines when Plan 2 imports them.
- **Add `serde::Serialize` (and likely `Deserialize`) to the model types** (`Config`, `Profile`, `Tab`, `Warning`, `Colour`). Tauri IPC `#[command]` return types must be `Serialize` to cross to the web chrome. `serde` is already a dependency — add the `derive`/`serialize` feature and the derives when the IPC layer lands, rather than maintaining parallel DTOs. Tied to Plan 2's IPC-vs-DTO design, so deferred until that choice is made.

## Watcher robustness (deferred to the consumer)

- **No debounce / coalescing** in `Watcher` (`crates/warden-config/src/watch.rs`). The callback fires for every filesystem event matching the config file name. Editors that write in place (rather than atomic temp-file + rename) can produce a transient `load()` parse error (a partial read mid-write) and/or multiple callbacks per save. Atomic-save editors are unaffected. Debounce/coalescing is intentionally left to the consumer (Plan 2), which owns the reload UX and already keeps last-good config on a parse error. Also documented at the call site.

## Lower-priority / edge cases

- **`warden validate` exit code does not distinguish "ok" from "ok with warnings"** (`crates/warden-config/src/bin/warden.rs`) — both exit 0. Not spec-mandated; revisit if a CI consumer needs to gate on nonexistent-dir warnings.
- **Soft degradation when `dirs::home_dir()` returns `None`** (HOME unset, e.g. some CI): `~/…` is left literal (later surfaces as a dir-missing warning) and `config_path()` falls back to a relative `.config/warden/config.toml`. Degenerate environments only.
- **Tilde expansion handles only `~/`** (`resolve.rs`), not bare `~` or `~user`. Within current spec scope (examples only use `~/…`).
- **`colour.rs` defensive `map_err` arms** are unreachable after the hex-digit guard, but retained deliberately: they preserve the "`parse` returns `Result`, never panics" contract without an `.expect()`. Not dead-code to remove.

## Test-isolation note (inert)

- `config_path_respects_env` (`crates/warden-config/src/load.rs`) mutates the process-global `WARDEN_CONFIG`. Cleanup is panic-safe (removed before the assert), but if a second test that reads `config_path()`/`WARDEN_CONFIG` is ever added, serialize them (e.g. `serial_test`) or refactor `config_path` to take an injected override — Rust runs tests in parallel threads. Inert today (no other reader).

## Spike S → Plan 2 (the `warden-app` Tauri surface embed)

The `crates/warden-app` spike proved the Tauri + libghostty surface embed on macOS (all checkpoints human-verified). It is the **seed of Plan 2**, not production. Deferred work, by priority:

- **libghostty is a throwaway prebuilt.** Vendored artifact is `Lakr233/libghostty-spm` `storage.1.2.7` — third-party, iOS-patched, ~Ghostty 1.2.7 — committed as a 39MB static `libghostty.a`. Replace with a controlled **upstream source build** (pinned commit). The source build is currently blocked on this machine: Ghostty pins **Zig 0.15.2**, which cannot link the **macOS 26.5 SDK** (undefined libSystem symbols incl. `__availability_version_check`, reproduces on a trivial hello-world). Revisit when a Zig that supports the macOS 26 SDK also builds the chosen Ghostty ref, or build on a CI runner with a compatible SDK. Also: don't commit the binary — use Git LFS or fetch-in-`build.rs`.
- **Make the `TerminalSurface` seam object-safe.** `close(self)` (by value) makes `Box<dyn TerminalSurface>` impossible, so the registry stores the concrete `GhosttySurface` — compile-time backend swap only, not runtime polymorphism (can't hold heterogeneous surfaces). Use `close(&mut self)` or Drop-based teardown. Pairs with: **add `Drop` for `GhosttySurface`** (today a surface freed only via explicit `close`/`close_all`; a dropped-without-close surface leaks the libghostty surface + its shell pty).
- **Move key routing off the process global.** `surface/ghostty.rs` uses a `SURFACE: AtomicPtr` global written by `focus()` to decide which surface gets keystrokes. Track the active surface in the `Registry` instead (per-window, not process-global) so multiple windows / dynamic tabs route correctly.
- **Move libghostty lifecycle init behind the seam.** `ffi::ghostty_init` is still called from `main.rs`; the surface layer should own libghostty init so `main.rs` never names libghostty (the seam constraint).
- **Wire `warden-config`.** Replace the hardcoded `specs()` with real config: one window per profile, tabs from config, banner colour from `Profile.colour`, hot-reload via the watcher + `reconcile`. The registry already takes a tab-spec list, so this is additive. (Needs the config-core re-exports + `serde` derives listed above.)
- **Chrome polish.** Match the curator app's sidebar design; single-source the sidebar width (currently `160px` duplicated in `ui/index.html` CSS and `main.rs`'s initial rect). Keyboard translation is minimal (ASCII; no IME/dead-keys/special-key mapping) — harden it.
- **FFI hardening.** C-populated `#[repr(C)] enum` fields in `ffi/mod.rs` (`platform_tag`/`backend`/`context`) risk invalid-discriminant UB if libghostty ever returns an unknown value; use integer newtypes (as already done for `mods`). The `size_of` asserts guard total size, not field offsets.

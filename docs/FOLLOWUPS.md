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

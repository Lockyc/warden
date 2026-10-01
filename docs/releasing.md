---
type: reference
links:
  - rel: part-of
    to: CLAUDE.md
    note: CLAUDE.md points here for packaging, signing, releases and the updater
---

# Packaging, releases & in-app updates

Read before cutting a release or touching `install.sh`, `tauri.conf.json`'s bundle/updater
keys, or the `deploy`/`release` recipes. The release scripts (`scripts/{release,gen-latest-json,
install-app}.sh`) are generated from shell-core and git-ignored — edit them there.

## Packaging and signing

`just build` produces the release `warden.app` (needs `cargo install tauri-cli --version ^2`);
`just deploy` installs it to `/Applications` via `install.sh` and relaunches.

**Signing is env-driven, not pinned in `tauri.conf.json`.** With `APPLE_SIGNING_IDENTITY` (a
Developer ID Application cert) plus `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID` (or
`APPLE_API_KEY`/`APPLE_API_ISSUER`/`APPLE_API_KEY_PATH`) in the environment, `cargo tauri build`
signs with hardened runtime and notarizes + staples. Without them the bundle is ad-hoc, and
`scripts/install-app.sh` strips the quarantine xattr so a local copy runs. So a build environment
missing the vars silently ships unsigned.

**Footgun: on macOS 26 an ad-hoc warden storms syspolicyd — which is why `just deploy` re-signs.**
macOS 26's per-exec provenance check (`qtn_proc`) fails for an ad-hoc, team-less bundle *and every
process it spawns*, and warden spawns every shell command you run (measured ~4100 failures/10s,
syspolicyd ~37% CPU). A real Team ID takes the check's cached fast path, so the `deploy` recipe
finds a *Developer ID Application* identity via `security find-identity` and re-signs the installed
bundle with it (`--options runtime`, no notarization). No cert in the keychain → it warns and
leaves the bundle ad-hoc. This re-sign is warden-local because a blanket hardened-runtime re-sign
isn't safe for every consuming app; the generic quarantine strip lives in shell-core.

**`just deploy` launches via shell-core's `scripts/launch-app.sh`, never a bare `open`**, which
forwards the deploying terminal's environment (`TERM_PROGRAM`, `TERMINFO`, `GHOSTTY_*`, `SHELL`, …)
into every tab. The script carries the why.

The bundle icon is `crates/warden-app/icons/icon.icns`, from the SVG masters via
`assets/build-icons.sh`, which also enforces the 824×824 safe area on the 1024 canvas (design the
SVG edge-to-edge; the script wraps it).

## Install from source

`install.sh` is self-sufficient: `bash install.sh` in a checkout builds the current tree; the
`curl … | bash` form clones/updates `~/.warden`. Both install the Tauri CLI if missing, build,
install to `/Applications` via `scripts/install-app.sh`, and seed `~/.config/warden/config.toml`
if absent. `just deploy` delegates to it; the guided path is `/warden:install`
(`.claude/commands/warden/install.md`).

## Releases

**The version is single-sourced in `crates/warden-app/Cargo.toml`.** `tauri.conf.json` has no
`version` key so the bundle inherits it — don't re-add one. `warden-config` versions
independently; the tag tracks `warden-app`.

`main` tracks the latest release and stays a clean ancestor of `dev`. Cutting one: on `dev`, bump
the version + refresh `Cargo.lock` → `chore(release): warden <v>` → `git tag v<v>` →
fast-forward `main` → push `dev`, `main`, the tag → `gh release create v<v>` with notes since the
previous tag → **`just release`** (`scripts/release.sh`), which builds the notarized app, zips it
(`ditto`) to `warden-<v>-macos.zip`, and attaches it plus the updater artifacts
(`warden.app.tar.gz`, `.sig`, `latest.json` from `scripts/gen-latest-json.sh`). `release.sh`
refuses to run without `APPLE_SIGNING_IDENTITY` or `TAURI_SIGNING_PRIVATE_KEY`, so an unsigned or
un-updatable release can't pass as official. Run `just gate` green first — the release build
compiles but doesn't test.

## In-app updates

`tauri-plugin-updater` + `tauri-plugin-process`. **The updater flow is chrome-core's** (check,
install/relaunch, the `UPDATE_CHECK_INTERVAL_MS` re-check, the update bar); warden supplies only its
identity and gate:

- `auto_update` passed to chrome-core as `autoUpdate` at init; **warden ▸ Check for Updates…**
  emits `warden:check-update` → `sb.checkForUpdateNow()`. Confirm-to-install.
- Endpoint `plugins.updater.endpoints` →
  `https://github.com/lockyc/warden/releases/latest/download/latest.json`; minisign public key in
  `plugins.updater.pubkey`; private key at `~/.tauri/warden-updater.key` (never committed),
  supplied as `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
- The chrome holds `updater:default` + `process:allow-restart` in `capabilities/default.json`.
- The release is a universal binary, so `latest.json` carries both `darwin-aarch64` and
  `darwin-x86_64` for the one tarball (shell-core's `gen-latest-json.sh` owns that).

**Footgun: `createUpdaterArtifacts` is enabled only by `release.sh`'s `--config` override, never in
the committed `tauri.conf.json`** — baked in, every `cargo tauri build` demands the signing key and
keyless `install.sh`/`just build`/`just deploy` break.

`auto_update` is not in `main.rs`'s reload hook: chrome-core arms its checks once at init, so a
toggle takes effect next launch. That is a wiring choice for a rarely-flipped setting (roadmap:
[`FOLLOWUPS.md`](FOLLOWUPS.md)), not the global-setting hot-reload bug.

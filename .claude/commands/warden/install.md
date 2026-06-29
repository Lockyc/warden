You are installing or updating **warden** — a config-driven terminal multiplexer
("curator for terminals") for macOS. A single TOML file defines windows and the
project tabs inside them; warden materializes a native window per `[[window]]`,
each tab an embedded libghostty terminal, and hot-reloads on save.

GitHub: `https://github.com/lockyc/warden`

Build it from source and install `warden.app` to `/Applications`. Source lives in a
persistent clone at `~/.warden`; updates are `git pull` + rebuild. The vendored
`libghostty.a` is in-tree, so the build needs no Zig — just Xcode CLT, Rust, and
the Tauri CLI (a cargo global, since warden has no npm).

---

## Steps

### 1. Detect context

Check whether the current working directory is a warden checkout:

```bash
[ -f install.sh ] && [ -f crates/warden-app/tauri.conf.json ] && echo "IN_REPO" || echo "NOT_IN_REPO"
```

**If IN_REPO:** you will run the local `install.sh` in step 4 (it builds this checkout).
**If NOT_IN_REPO:** you will run the published installer over curl in step 4 (it manages a `~/.warden` clone).

### 2. Check prerequisites and offer to install

Probe each build prerequisite:

```bash
command -v git        >/dev/null 2>&1 && echo "git: ok"        || echo "git: MISSING"
command -v cargo      >/dev/null 2>&1 && echo "cargo: ok"      || echo "cargo: MISSING"
command -v cargo-tauri >/dev/null 2>&1 && echo "tauri-cli: ok" || echo "tauri-cli: MISSING"
xcode-select -p       >/dev/null 2>&1 && echo "clt: ok"        || echo "clt: MISSING"
command -v brew       >/dev/null 2>&1 && echo "brew: ok"       || echo "brew: MISSING"
```

For each MISSING prerequisite (other than brew), use AskUserQuestion to offer to install it.
Only run an install command on confirmation:

- **Xcode Command Line Tools** (also provides `git`): `xcode-select --install`
  (this opens a GUI installer — tell the user to finish it, then continue).
- **Rust** (`cargo`): `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`,
  then advise sourcing `~/.cargo/env` or restarting the shell.
- **Tauri CLI** (`cargo-tauri`): `cargo install tauri-cli --version '^2' --locked`
  (compiles the CLI — takes a few minutes). `install.sh` backstops this too, so it
  is safe to skip here and let the core install handle it.

warden has **no Node/npm dependency** — do not probe for it. If a prerequisite the
user declined to install is still missing (other than the Tauri CLI, which
`install.sh` installs anyway), warn that `install.sh` will refuse to build until it
is present, and ask whether to continue anyway.

### 3. Probe current state

For smarter messaging:

```bash
[ -d /Applications/warden.app ] && echo "app: installed" || echo "app: absent"
if [ ! -e ~/.warden ]; then echo "src: fresh";
elif [ -d ~/.warden/.git ]; then echo "src: clone";
else echo "src: not-a-clone"; fi
pgrep -f "/Applications/warden.app/" >/dev/null && echo "running: yes" || echo "running: no"
```

If `src: not-a-clone`, tell the user `~/.warden` exists but is not a git clone; `install.sh`
will refuse to touch it. They must move it aside before continuing (NOT_IN_REPO path only).

### 4. Run the core install

**If IN_REPO:**

```bash
PATH="$HOME/.cargo/bin:$PATH" bash install.sh
```

**If NOT_IN_REPO:**

```bash
curl -fsSL https://raw.githubusercontent.com/lockyc/warden/main/install.sh | PATH="$HOME/.cargo/bin:$PATH" bash
```

(The `PATH="$HOME/.cargo/bin:$PATH"` prefix ensures a Rust toolchain / Tauri CLI you may
have just installed via rustup/cargo in step 2 is found — a fresh shell won't have picked
up rustup's profile changes yet.)

IN_REPO builds the current checkout; NOT_IN_REPO clones/updates `~/.warden` and builds from it.
Both back the Tauri CLI install if absent, run `cargo tauri build`, install the app to
`/Applications/warden.app`, and seed `~/.config/warden/config.toml` if absent. The build takes
a few minutes. **If it fails, show the full output and stop** — do not run later steps.

### 5. Configure

`install.sh` has already seeded `~/.config/warden/config.toml` from the example if it was
absent. Use AskUserQuestion to offer to open it for editing now:

- **Open in editor** → `open -e ~/.config/warden/config.toml`
- **Reveal in Finder** → `open -R ~/.config/warden/config.toml`
- **Skip** — leave it for later.

Briefly note the format: each `[[window]]` has a `title` (+ optional `colour`) and holds
`[[window.tab]]` entries; a tab needs a `dir` and may set `cmd` (a command typed into the
shell), `load_on_open`, and `probe`. Tabs may be grouped under `[[window.group]]`. Global
`shell`/`cmd`/`tab_digit_keys` sit at the top. The file **hot-reloads on save** — no restart
needed. (warden has no in-app config menu; edit the file directly.)

### 6. Offer to launch

Use AskUserQuestion: **"Launch warden now?"**

- **Launch** → `open /Applications/warden.app`
- **Not now** — skip.

### 7. Self-install this command

So `/warden:install` is available in future Claude Code sessions:

```bash
mkdir -p ~/.claude/commands/warden
```

Copy `install.md` verbatim to `~/.claude/commands/warden/install.md`. Source it from the
cwd checkout if IN_REPO (`.claude/commands/warden/install.md`), otherwise from
`~/.warden/.claude/commands/warden/install.md` (present after step 4 cloned it).

### 8. Summary

Read the version from `crates/warden-app/tauri.conf.json` (the `version` field) — use the cwd
checkout if IN_REPO, else `~/.warden/crates/warden-app/tauri.conf.json`.

Print:

**Installed**
- warden vX.Y.Z → `/Applications/warden.app` ✓
- Source clone → `~/.warden` (NOT_IN_REPO) or "built from checkout" (IN_REPO)
- Config → `~/.config/warden/config.toml` (seeded from example / already existed)

**Next steps**
- Edit `~/.config/warden/config.toml` to define your windows + tabs (hot-reloads on save).
- Each `[[window]]` is a native macOS window with a colour + title banner; each `[[window.tab]]`
  is a terminal opened in `dir` running `cmd`. Pairs well with
  [agentmux](https://github.com/lockyc/agentmux) (`amux`) as the tab command.
- Update any time by re-running `/warden:install` (or `curl … | bash`) — it git-pulls and rebuilds.

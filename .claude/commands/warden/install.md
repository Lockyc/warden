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
`/Applications/warden.app`, seed `~/.config/warden/config.toml` if absent, and install the agent
hook to `~/.local/share/warden/agent-hooks/` (wired in step 6). The build takes
a few minutes. **If it fails, show the full output and stop** — do not run later steps.

### 5. Configure

`install.sh` has already seeded `~/.config/warden/config.toml` from the starter config if it was
absent. Use AskUserQuestion to offer to open it for editing now:

- **Open in editor** → `open -e ~/.config/warden/config.toml`
- **Reveal in Finder** → `open -R ~/.config/warden/config.toml`
- **Skip** — leave it for later.

Briefly note the format: each `[[window]]` has a `title` (+ optional `colour`) and holds
`[[window.tab]]` entries; a tab needs a `dir` and may set `cmd` (a command typed into the
shell), `load_on_open`, and `probe`. Tabs may be grouped under `[[window.group]]`. Global
`shell`/`cmd`/`tab_digit_keys` sit at the top. The file **hot-reloads on save** — no restart
needed. (warden has no in-app config menu; edit the file directly.)

### 6. Wire the agent-tab hook

An agent tab on a Rex server (a tab with `server = "local"`) can sleep and wake only once its
agent has reported how to resume it. Claude Code reports through a `SessionStart` hook,
`~/.local/share/warden/agent-hooks/claude-session-start.sh` (installed in step 4); it does nothing
outside a Rex terminal. Use AskUserQuestion: **"Wire warden's agent hook into Claude Code?"**
(Recommended; **Skip** leaves agent tabs unable to sleep).

On confirmation, check `command -v jq` (offer `brew install jq` if missing), then run this. It
wires every Claude Code profile in `$HOME` — `~/.claude` and each `~/.claude-*` dir — and:

- **is idempotent**: an entry whose command contains `warden/agent-hooks/` (the marker) is never
  added twice, and other tools' `SessionStart` hooks are kept;
- **skips a `~/.claude-*` dir with no `settings.json`** — it may not be a profile — but seeds
  `~/.claude/settings.json`, the default profile's, when absent;
- **edits each real file once**: profiles whose `settings.json` resolves (`realpath`) to the same
  file are wired through the first;
- **writes with `cat >`, never `mv`**, so a symlinked `settings.json` stays a symlink.

```bash
HOOK='~/.local/share/warden/agent-hooks/claude-session-start.sh'
seen=
for dir in "$HOME/.claude" "$HOME"/.claude-*; do
  [ -d "$dir" ] || continue
  f="$dir/settings.json"
  if [ ! -e "$f" ]; then
    if [ "$dir" != "$HOME/.claude" ]; then echo "skipped (no settings.json): $dir"; continue; fi
    echo '{}' > "$f"
  fi
  real=$(realpath "$f") || continue
  case "$seen" in *"|$real|"*) echo "same file as an earlier profile: $f"; continue ;; esac
  seen="$seen|$real|"
  if jq -e 'any(.hooks.SessionStart[]?.hooks[]?; (.command // "") | contains("warden/agent-hooks/"))' \
    "$f" > /dev/null; then echo "already wired: $f"; continue; fi
  tmp=$(mktemp) && jq --arg cmd "$HOOK" \
    '.hooks.SessionStart += [{"hooks": [{"type": "command", "command": $cmd}]}]' "$f" > "$tmp" \
    && cat "$tmp" > "$f" && echo "wired: $f"
  rm -f "$tmp"
done
```

Report each line it printed. The hook takes effect at the next Claude Code session start. To
remove it from a file, write it back (again via `cat >`) through
`jq '.hooks.SessionStart |= map(select(any(.hooks[]?; (.command // "") | contains("warden/agent-hooks/")) | not))'`.

This wires this Mac only; for a Rex server on another machine, see "The hook on a remote Rex host"
in `docs/FOLLOWUPS.md`.

### 7. Offer to launch

Use AskUserQuestion: **"Launch warden now?"**

- **Launch** → `open /Applications/warden.app`
- **Not now** — skip.

### 8. Self-install this command

So `/warden:install` is available in future Claude Code sessions:

```bash
mkdir -p ~/.claude/commands/warden
```

Copy `install.md` verbatim to `~/.claude/commands/warden/install.md`. Source it from the
cwd checkout if IN_REPO (`.claude/commands/warden/install.md`), otherwise from
`~/.warden/.claude/commands/warden/install.md` (present after step 4 cloned it).

### 9. Summary

Read the version from `crates/warden-app/Cargo.toml` (the `[package]` `version`) — use the cwd
checkout if IN_REPO, else `~/.warden/crates/warden-app/Cargo.toml`.

Print:

**Installed**
- warden vX.Y.Z → `/Applications/warden.app` ✓
- Source clone → `~/.warden` (NOT_IN_REPO) or "built from checkout" (IN_REPO)
- Config → `~/.config/warden/config.toml` (seeded from the starter config / already existed)
- Agent hook → each Claude Code `settings.json` wired / already wired / skipped

**Next steps**
- Edit `~/.config/warden/config.toml` to define your windows + tabs (hot-reloads on save).
- Each `[[window]]` is a native macOS window with a colour + title banner; each `[[window.tab]]`
  is a terminal opened in `dir` running `cmd`. Pairs well with
  [agentmux](https://github.com/lockyc/agentmux) (`amux`) as the tab command.
- Update any time by re-running `/warden:install` (or `curl … | bash`) — it git-pulls and rebuilds.

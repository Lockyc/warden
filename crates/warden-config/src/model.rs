use crate::Colour;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub windows: Vec<Window>,
    /// When true, warden rewrites the config file formatted on each clean
    /// hot-reload. Default false. Whole-file concern — no per-window cascade.
    pub format_on_save: bool,
    /// What ⌘1/⌘2 do in the app menu. Whole-app concern — no per-window cascade.
    pub tab_digit_keys: TabDigitKeys,
    /// Seconds between background session-probe passes. 0 = focus/refresh-only.
    /// Default 5. Global concern — no per-window cascade.
    pub probe_interval: u64,
    /// Chrome sizing mode. Whole-app concern — no per-window cascade.
    pub density: Density,
}

/// UI density — a whole-app presentation mode that scales the chrome's type and
/// spacing as a unit. The crate only carries the choice; the app's chrome owns the
/// actual sizes (it maps this to a `data-density` attribute → CSS variables).
///
/// - `Comfortable` (default): the standard sizing.
/// - `Compact`: proportionally condensed type + spacing for denser tab lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

impl Density {
    /// The token the chrome's `data-density` attribute uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Density::Comfortable => "comfortable",
            Density::Compact => "compact",
        }
    }
}

/// Behaviour of the ⌘1/⌘2 menu accelerators (a whole-app keybinding mode).
///
/// - `Jump` (default, standard macOS convention): ⌘1–⌘9 jump straight to the
///   tab at that 1-based position.
/// - `Cycle`: ⌘1 = next tab, ⌘2 = previous tab (aliasing ⌘⇧] / ⌘⇧[), which
///   reclaims the digit-1/2 chords, so direct jumps shift to ⌘3–⌘9 (positions
///   1–2 then have no jump chord).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabDigitKeys {
    #[default]
    Jump,
    Cycle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub title: String,
    pub colour: Colour,
    pub width: u32,
    pub height: u32,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub key: String,
    pub title: String,
    pub dir: PathBuf,
    /// The shell to spawn in this tab (resolved cascade, e.g. `"/opt/homebrew/bin/fish -l"`;
    /// defaults to the caller's login shell when unset). It runs as an interactive shell under
    /// the terminal's PTY, so shell functions/aliases resolve.
    pub shell: String,
    /// Optional startup command auto-run inside `shell` (the tab's `cmd`). `None` = bare
    /// shell. It is *typed into* the interactive shell — not exec'd directly — so a shell
    /// function like `amux` works and the shell stays live after the command exits.
    pub startup: Option<String>,
    pub load_on_open: bool,
    /// The name of the `[[window.group]]` this tab belongs to, or `None` for a loose
    /// (ungrouped) tab. Purely presentational — the chrome sections the sidebar by it;
    /// it carries no behaviour and is not part of `key` (identity stays the title).
    pub group: Option<String>,
    /// Optional session-presence probe command for this tab (cascaded
    /// tab→window→global). `None` = no probe (the tab shows no session dot).
    /// Opaque to the crate — the app runs it and reads its exit code.
    pub probe: Option<String>,
    /// Optional session-kill command for this tab (cascaded tab→window→global,
    /// `""` opts out). `None` = no kill affordance. Opaque to the crate — the app
    /// runs it via `sh -c` when the user confirms killing the tab's session.
    pub kill: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub window: String,
    pub message: String,
}

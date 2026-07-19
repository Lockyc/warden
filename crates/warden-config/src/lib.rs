//! warden-config: parse, validate, resolve, and reconcile warden's TOML config.

pub mod load;
pub mod model;
pub mod raw;
pub mod reconcile;
pub mod resolve;
pub mod watch;

// House-style formatter + colour parsing, plus `write_default_config` (the shared home
// surface's "Create a starter config" path — see main.rs's `shell_home_create_config`), are
// shared with curator and lector via the config-core crate. Re-exported at the root so the rest
// of warden-config (and warden-app) keep using `warden_config::{Colour, ColourError,
// format_file, format_str}` unchanged.
//
// Project-tree root discovery (`discover_projects`/`tree_path`, the `RootDir`/`DiscoveredProject`
// types, `DEFAULT_ROOT_DEPTH`) is shared with lector the same way — leaf-free, so warden's own
// `Tab` never appears in config-core's signatures. warden-app maps a `DiscoveredProject` onto its
// own `Tab` shape (scanner.rs); resolve.rs delegates a raw root's name/dir/depth validation to
// `config_core::resolve_root_dir` directly, mapping `RootError` onto warden's own `ResolveError`
// variants with the enclosing window's context.
pub use config_core::{
    discover_projects, fmt_cli, format_file, format_str, tree_path, write_default_config, Colour,
    ColourError, DiscoveredProject, RootDir, SeedError, DEFAULT_ROOT_DEPTH,
};
pub use load::{config_path, load, load_with, LoadError, Loaded};
pub use model::{Config, Density, Root, Tab, TabDigitKeys, Warning, Window};
pub use reconcile::{reconcile, Reconciliation, TabMeta, WindowUpdate};
pub use resolve::ResolveError;
pub use watch::Watcher;

#[cfg(test)]
mod root_reexport_tests {
    /// Compile-time proof the crate-root re-exports resolve (the Plan 2 consumer
    /// imports these directly rather than reaching into submodules).
    #[test]
    fn root_reexports_resolve() {
        #[allow(unused_imports)]
        use crate::{Colour, ResolveError, Watcher};
    }
}

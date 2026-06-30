//! warden-config: parse, validate, resolve, and reconcile warden's TOML config.

pub mod load;
pub mod model;
pub mod raw;
pub mod reconcile;
pub mod resolve;
pub mod watch;

// House-style formatter + colour parsing are shared with curator via the config-core crate.
// Re-exported at the root so the rest of warden-config (and warden-app) keep using
// `warden_config::{Colour, ColourError, format_file, format_str}` unchanged.
pub use config_core::{format_file, format_str, Colour, ColourError};
pub use load::{config_path, load, load_with, LoadError, Loaded};
pub use model::{Config, Density, Tab, TabDigitKeys, Warning, Window};
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

//! warden-config: parse, validate, resolve, and reconcile warden's TOML config.

pub mod raw;
pub mod colour;
pub mod model;
pub mod resolve;
pub mod reconcile;
pub mod load;
pub mod watch;

pub use load::{load, config_path, Loaded, LoadError};
pub use model::{Config, Profile, Tab, Warning};
pub use reconcile::{reconcile, Reconciliation, ProfileUpdate};
pub use colour::Colour;
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

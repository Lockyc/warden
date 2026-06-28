//! warden-config: parse, validate, resolve, and reconcile warden's TOML config.

pub mod colour;
pub mod load;
pub mod model;
pub mod raw;
pub mod reconcile;
pub mod resolve;
pub mod watch;

pub use colour::Colour;
pub use load::{config_path, load, LoadError, Loaded};
pub use model::{Config, Profile, Tab, Warning};
pub use reconcile::{reconcile, ProfileUpdate, Reconciliation};
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

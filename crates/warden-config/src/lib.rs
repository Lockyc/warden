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

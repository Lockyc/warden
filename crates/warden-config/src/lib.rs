//! warden-config: parse, validate, resolve, and reconcile warden's TOML config.

pub mod raw;

#[cfg(test)]
mod smoke {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}

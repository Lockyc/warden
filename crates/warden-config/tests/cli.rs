//! Integration tests for the `warden` CLI binary's exit-code and mutation
//! contract. The load/resolve/format *logic* is unit-tested in the library;
//! this pins the binary wiring — the exit codes (`0` ok / `1` load-or-format
//! error / `2` usage) and the `fmt --check` non-mutation guarantee that
//! `just gate` depends on.

use std::path::Path;
use std::process::Command;

/// Path to the compiled `warden` binary for this test build.
const BIN: &str = env!("CARGO_BIN_EXE_warden");

fn run(args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("spawn warden binary")
}

/// A minimal config that resolves cleanly (a nonexistent `dir` is only a
/// warning, so `validate` still exits 0).
const VALID: &str = r#"shell = "zsh"

[[window]]
title = "w"

[[window.tab]]
dir = "~/does-not-exist"
"#;

fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn no_args_is_usage_error_exit_2() {
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

#[test]
fn unknown_subcommand_is_usage_error_exit_2() {
    let out = run(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn validate_ok_exit_0() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = write(tmp.path(), "config.toml", VALID);
    let out = run(&["validate", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok:"));
}

#[test]
fn validate_malformed_toml_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = write(tmp.path(), "config.toml", "[[window\nbroken =");
    let out = run(&["validate", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("error"));
}

#[test]
fn validate_missing_file_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("nope.toml");
    let out = run(&["validate", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn fmt_formats_then_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = write(tmp.path(), "config.toml", VALID);

    // First format succeeds (exit 0).
    let out = run(&["fmt", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));

    // Now the file is formatted — `--check` must agree (exit 0, no change).
    let formatted = std::fs::read_to_string(&cfg).unwrap();
    let out = run(&["fmt", "--check", cfg.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "freshly-formatted file should pass --check"
    );
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        formatted,
        "--check must not mutate"
    );
}

#[test]
fn fmt_check_detects_unformatted_and_does_not_mutate() {
    let tmp = tempfile::tempdir().unwrap();
    // Start from the canonical form, then mangle in a way the house style
    // definitely reverts: drop the mandated trailing newline.
    let cfg = write(tmp.path(), "config.toml", VALID);
    run(&["fmt", cfg.to_str().unwrap()]); // canonicalize
    let formatted = std::fs::read_to_string(&cfg).unwrap();
    let unformatted = formatted.trim_end_matches('\n').to_string();
    std::fs::write(&cfg, &unformatted).unwrap();

    let out = run(&["fmt", "--check", cfg.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "unformatted file should fail --check"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("would reformat"));
    // --check is non-mutating: the file is still the unformatted bytes.
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), unformatted);
}

#[test]
fn fmt_check_rejects_non_toml_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = write(tmp.path(), "config.toml", "this is not = = toml [[[");
    let out = run(&["fmt", "--check", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not valid TOML"));
}

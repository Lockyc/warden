//! warden's house TOML formatter — wraps the taplo crate with a fixed style so
//! the config file stays tidy. `format_str` is pure; `format_file` applies it to
//! a file atomically and only when the bytes change (so a watcher driving it
//! can't loop).

use std::io::Write;
use std::path::Path;
use taplo::formatter::{format, Options};

/// Format TOML in warden's house style — reproduces and enforces the
/// hand-formatted look of `examples/config.toml`: nested-table indentation,
/// aligned `=` and trailing comments, authored key order preserved.
pub fn format_str(input: &str) -> String {
    let mut o = Options::default();
    o.indent_tables = true;
    o.indent_entries = true;
    o.align_entries = true;
    o.align_comments = true;
    o.reorder_keys = false;
    o.column_width = 100;
    format(input, o)
}

/// Format `path` in place. Reads, formats, and rewrites **only if the bytes
/// change** (so a watcher driving this can't loop — an already-formatted file is
/// a no-op). The rewrite is atomic: a temp file in the same dir + rename.
/// Returns whether it wrote.
pub fn format_file(path: &Path) -> std::io::Result<bool> {
    let original = std::fs::read_to_string(path)?;
    let formatted = format_str(&original);
    if formatted == original {
        return Ok(false);
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(formatted.as_bytes())?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_nested_tables_and_aligns() {
        let out = format_str("a=1\n[[w]]\nx=2\n");
        assert_eq!(out, "a = 1\n[[w]]\n  x = 2\n");
    }

    #[test]
    fn is_idempotent() {
        let messy = "a=1\n[[w]]\nx=2\ntitle=\"t\"\n";
        let once = format_str(messy);
        assert_eq!(format_str(&once), once);
    }

    #[test]
    fn format_file_rewrites_messy_then_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a=1\n[[w]]\nx=2\n").unwrap();

        // First pass: messy → rewritten.
        assert!(format_file(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a = 1\n[[w]]\n  x = 2\n"
        );

        // Second pass: already formatted → no write, returns false.
        assert!(!format_file(&path).unwrap());
    }
}

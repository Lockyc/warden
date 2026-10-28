//! Links the vendored libghostty-vt static library (Ghostty's snapshot decoder and formatter).

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/libghostty-vt/lib");
    println!(
        "cargo:rerun-if-changed={}",
        dir.join("libghostty-vt.a").display()
    );
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    // simdutf and highway inside the archive are C++.
    println!("cargo:rustc-link-lib=c++");
}

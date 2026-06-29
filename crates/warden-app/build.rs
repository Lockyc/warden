fn main() {
    tauri_build::build();

    // tauri-build does NOT emit rerun-if-changed for frontendDist ("ui"), and the
    // assets are embedded by `generate_context!` in main.rs at compile time — so a
    // frontend-only edit (e.g. index.html) would otherwise never re-embed unless a
    // Rust file also changed. Force a rebuild on any frontend asset change.
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/diagnostic.html");

    #[cfg(target_os = "macos")]
    {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let slice = format!("{manifest}/vendor/GhosttyKit.xcframework/macos-arm64_x86_64");
        println!("cargo:rustc-link-search=native={slice}");
        println!("cargo:rustc-link-lib=static=ghostty");
        // System frameworks libghostty pulls in. Start with this set; extend if the linker
        // reports undefined symbols from additional frameworks.
        for fw in [
            "Metal",
            "QuartzCore",
            "CoreGraphics",
            "CoreText",
            "AppKit",
            "Foundation",
            "IOSurface",
            "CoreFoundation",
        ] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
        println!("cargo:rustc-link-lib=dylib=c++");
        println!("cargo:rerun-if-changed={slice}/libghostty.a");
    }
}

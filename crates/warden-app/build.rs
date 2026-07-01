fn main() {
    tauri_build::build();

    // Materialize the shared chrome into ui/ (frontendDist) so generate_context! embeds it. The
    // generated files are git-ignored — reproducible from the pinned chrome-core rev + this recipe,
    // so a plain clone still builds (cargo fetches chrome-core; this writes it out).
    std::fs::write("ui/chrome-core.css", chrome_core::SIDEBAR_CSS).expect("write chrome-core.css");
    std::fs::write("ui/chrome-core.js", chrome_core::SIDEBAR_JS).expect("write chrome-core.js");

    // tauri-build does NOT emit rerun-if-changed for frontendDist ("ui"), and the assets are
    // embedded by `generate_context!` in main.rs at compile time — so a frontend-only edit would
    // otherwise never re-embed unless a Rust file also changed. Watch the HAND-WRITTEN assets only:
    // watching `ui` broadly would self-trigger a rerun every build, since build.rs writes the
    // generated chrome-core.{css,js} into ui/.
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

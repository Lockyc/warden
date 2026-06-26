fn main() {
    tauri_build::build();

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

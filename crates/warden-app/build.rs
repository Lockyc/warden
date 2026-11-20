fn main() {
    tauri_build::build();

    // Materialize the shared chrome into ui/ (frontendDist) so generate_context! embeds it. The
    // generated files are git-ignored — reproducible from the pinned chrome-core rev + this recipe,
    // so a plain clone still builds (cargo fetches chrome-core; this writes it out).
    std::fs::write("ui/chrome-core.css", chrome_core::SIDEBAR_CSS).expect("write chrome-core.css");
    std::fs::write("ui/chrome-core.js", chrome_core::SIDEBAR_JS).expect("write chrome-core.js");

    // Materialize the shared release scripts into repo-root scripts/ from the pinned shell-core rev
    // (git-ignored; the tracked scripts/tooling.env supplies warden's params) and stamp the build
    // (BUILD_GIT_SHA/BUILD_DATE → About). Same embed-and-materialize pattern as chrome-core above.
    shell_core::materialize_scripts(std::path::Path::new("../../scripts"))
        .expect("materialize shell-core scripts");
    shell_core::build_stamp();

    // tauri-build does NOT emit rerun-if-changed for frontendDist ("ui"), and the assets are
    // embedded by `generate_context!` in main.rs at compile time — so a frontend-only edit would
    // otherwise never re-embed unless a Rust file also changed. Watch the HAND-WRITTEN assets only:
    // watching `ui` broadly would self-trigger a rerun every build, since build.rs writes the
    // generated chrome-core.{css,js} into ui/.
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/scratch-run.js");
    println!("cargo:rerun-if-changed=ui/route.js");
    println!("cargo:rerun-if-changed=ui/agent-list.js");

    // Derive the libghostty action-tag discriminants from the vendored header instead of
    // hand-transcribing them. `ghostty_action_tag_e` is a plain C enum with no explicit values, so
    // each tag equals its 0-based position — and upstream inserts members mid-list (libghostty's
    // embedding API is officially unstable). Hand-copied numbers therefore rot SILENTLY on a
    // revendor: the `const _` size_of asserts guard the structs, but nothing guarded these, and a
    // shifted tag misroutes a whole action (see the FOOTGUN in ffi/mod.rs). The header is the one
    // source of truth; this regenerates on every revendor.
    generate_action_tags();

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

/// Enum members warden decodes or answers with (action tags, open-url kinds, the clipboard
/// callbacks' enums, the input/platform/target enums and the mouse shapes), keyed by their
/// `typedef enum { … } <name>;`. Names only — every
/// VALUE comes from the vendored header, so a revendor that reorders an enum can't leave a stale
/// number behind. A name that no longer exists upstream fails the build here rather than silently
/// decoding the wrong value.
const DECODED: &[(&str, &[&str])] = &[
    (
        "ghostty_action_tag_e",
        &[
            "GHOSTTY_ACTION_DESKTOP_NOTIFICATION",
            "GHOSTTY_ACTION_MOUSE_SHAPE",
            "GHOSTTY_ACTION_PWD",
            "GHOSTTY_ACTION_RING_BELL",
            "GHOSTTY_ACTION_OPEN_URL",
            "GHOSTTY_ACTION_SHOW_CHILD_EXITED",
        ],
    ),
    (
        "ghostty_action_open_url_kind_e",
        &[
            "GHOSTTY_ACTION_OPEN_URL_KIND_UNKNOWN",
            "GHOSTTY_ACTION_OPEN_URL_KIND_TEXT",
            "GHOSTTY_ACTION_OPEN_URL_KIND_HTML",
            "GHOSTTY_ACTION_OPEN_URL_KIND_OSC8",
        ],
    ),
    ("ghostty_clipboard_e", &["GHOSTTY_CLIPBOARD_STANDARD"]),
    (
        "ghostty_clipboard_request_e",
        &["GHOSTTY_CLIPBOARD_REQUEST_PASTE"],
    ),
    (
        "ghostty_clipboard_read_result_e",
        &[
            "GHOSTTY_CLIPBOARD_READ_STARTED",
            "GHOSTTY_CLIPBOARD_READ_UNAVAILABLE",
            "GHOSTTY_CLIPBOARD_READ_UNSUPPORTED",
        ],
    ),
    ("ghostty_target_tag_e", &["GHOSTTY_TARGET_SURFACE"]),
    ("ghostty_platform_e", &["GHOSTTY_PLATFORM_MACOS"]),
    (
        "ghostty_surface_context_e",
        &["GHOSTTY_SURFACE_CONTEXT_WINDOW"],
    ),
    (
        "ghostty_input_action_e",
        &[
            "GHOSTTY_ACTION_RELEASE",
            "GHOSTTY_ACTION_PRESS",
            "GHOSTTY_ACTION_REPEAT",
        ],
    ),
    (
        "ghostty_input_mouse_state_e",
        &["GHOSTTY_MOUSE_RELEASE", "GHOSTTY_MOUSE_PRESS"],
    ),
    (
        "ghostty_input_mouse_button_e",
        &[
            "GHOSTTY_MOUSE_LEFT",
            "GHOSTTY_MOUSE_RIGHT",
            "GHOSTTY_MOUSE_MIDDLE",
        ],
    ),
    (
        "ghostty_action_mouse_shape_e",
        &[
            "GHOSTTY_MOUSE_SHAPE_POINTER",
            "GHOSTTY_MOUSE_SHAPE_CROSSHAIR",
            "GHOSTTY_MOUSE_SHAPE_TEXT",
            "GHOSTTY_MOUSE_SHAPE_ALIAS",
            "GHOSTTY_MOUSE_SHAPE_COPY",
            "GHOSTTY_MOUSE_SHAPE_NOT_ALLOWED",
            "GHOSTTY_MOUSE_SHAPE_GRAB",
            "GHOSTTY_MOUSE_SHAPE_GRABBING",
            "GHOSTTY_MOUSE_SHAPE_COL_RESIZE",
            "GHOSTTY_MOUSE_SHAPE_ROW_RESIZE",
        ],
    ),
];

fn generate_action_tags() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // The header inside the xcframework, the one shipped with the linked `libghostty.a` and
    // replaced with it by `just revendor-ghostty`; see docs/surface.md.
    let slices = format!("{manifest}/vendor/GhosttyKit.xcframework");
    let mut headers: Vec<std::path::PathBuf> = std::fs::read_dir(&slices)
        .expect("read vendor/GhosttyKit.xcframework")
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("Headers/ghostty.h"))
        .filter(|p| p.is_file())
        .collect();
    headers.sort();
    let header_path = headers
        .first()
        .expect("vendor/GhosttyKit.xcframework/*/Headers/ghostty.h not found")
        .clone();
    let header = std::fs::read_to_string(&header_path).expect("read the xcframework's ghostty.h");
    for other in &headers[1..] {
        // Every slice of one xcframework carries the same header; a divergence would mean the
        // slices are from different builds, which no tag value could be right for.
        let h = std::fs::read_to_string(other).expect("read slice header");
        assert!(
            h == header,
            "xcframework slices disagree on ghostty.h: {} vs {}",
            header_path.display(),
            other.display()
        );
    }
    for p in &headers {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let mut out =
        String::from("// @generated by build.rs from the xcframework's ghostty.h — do not edit.\n");
    for (typedef, wanted) in DECODED {
        let members = enum_members(&header, typedef);
        for want in *wanted {
            let value = members
                .iter()
                .find(|(n, _)| n == want)
                .map(|(_, v)| *v)
                .unwrap_or_else(|| {
                    panic!("ghostty.h: {typedef} has no {want} — did the revendor rename it?")
                });
            out.push_str(&format!("pub const {want}: u32 = {value};\n"));
        }
    }

    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("action_tags.rs");
    std::fs::write(dest, out).expect("write action_tags.rs");
}

/// The members of `typedef enum { … } <typedef>;` with their values, as C assigns them: an
/// explicit `= <integer>`, else one more than the member before (0 for the first).
fn enum_members(header: &str, typedef: &str) -> Vec<(String, u32)> {
    let end = header
        .find(&format!("}} {typedef};"))
        .unwrap_or_else(|| panic!("ghostty.h: {typedef} enum not found"));
    let open = "typedef enum {";
    let start = header[..end]
        .rfind(open)
        .unwrap_or_else(|| panic!("ghostty.h: {typedef} has no opening typedef enum"))
        + open.len();

    let mut members = Vec::new();
    let mut next = 0u32;
    for member in header[start..end].split(',') {
        // Strip line comments, then whitespace; the trailing chunk after the last comma is empty.
        let member: String = member
            .lines()
            .map(|l| l.split("//").next().unwrap_or("").trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if member.is_empty() {
            continue;
        }
        let (name, value) = match member.split_once('=') {
            // Only a plain decimal literal is evaluated; an expression (`1 << 2`, another
            // member's name) fails the build rather than yielding a guess.
            Some((n, v)) => (
                n.trim().to_string(),
                v.trim().parse::<u32>().unwrap_or_else(|_| {
                    panic!("ghostty.h: {typedef} assigns {member}, which build.rs can't evaluate")
                }),
            ),
            None => (member, next),
        };
        next = value + 1;
        members.push((name, value));
    }
    members
}

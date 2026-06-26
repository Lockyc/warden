mod ffi;

fn main() {
    // Linkage smoke test: call ghostty_init with real process argc/argv to prove the
    // libghostty symbols resolve. A non-zero return is printed but not fatal at
    // Checkpoint 0 — full Ghostty app initialisation happens in Task 3.
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::raw::c_char;

        let args: Vec<CString> = std::env::args()
            .map(|a| CString::new(a).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();
        let mut c_argv: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr() as *mut c_char).collect();
        c_argv.push(std::ptr::null_mut());

        let ret = unsafe { ffi::ghostty_init(args.len(), c_argv.as_mut_ptr()) };
        if ret != 0 {
            eprintln!("warden: ghostty_init returned {} (non-fatal at checkpoint 0)", ret);
        }
    }

    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running warden");
}

//! Raw mode on stdin, so keys reach the block untranslated, restored however the relay ends.

use std::mem::MaybeUninit;
use std::sync::OnceLock;

/// The terminal settings to restore; set once, before any handler that reads it is installed.
static SAVED: OnceLock<libc::termios> = OnceLock::new();

/// Signals that end the relay without unwinding; their handler restores the terminal first.
const FATAL_SIGNALS: [libc::c_int; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// Raw mode while alive, when stdin is a terminal; a no-op otherwise.
pub struct RawMode {
    active: bool,
}

impl RawMode {
    pub fn enable() -> RawMode {
        let inactive = RawMode { active: false };
        // SAFETY: plain termios/signal calls on fd 0 with valid pointers.
        unsafe {
            if libc::isatty(0) != 1 {
                return inactive;
            }
            let mut saved = MaybeUninit::<libc::termios>::uninit();
            if libc::tcgetattr(0, saved.as_mut_ptr()) != 0 {
                return inactive;
            }
            let saved = saved.assume_init();
            if SAVED.set(saved).is_err() {
                return inactive;
            }
            for sig in FATAL_SIGNALS {
                libc::signal(sig, on_fatal_signal as *const () as libc::sighandler_t);
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return inactive;
            }
        }
        RawMode { active: true }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if self.active {
            restore();
        }
    }
}

fn restore() {
    if let Some(saved) = SAVED.get() {
        // SAFETY: tcsetattr is async-signal-safe; `saved` came from tcgetattr.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
    }
}

extern "C" fn on_fatal_signal(sig: libc::c_int) {
    restore();
    // SAFETY: signal and raise are async-signal-safe; re-raising with the default action ends the
    // process with the status the signal would have given it.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

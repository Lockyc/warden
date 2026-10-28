//! libghostty-vt: turns the Ghostty snapshot that opens a Rex terminal stream into VT a host
//! terminal can replay.

use std::ffi::{c_int, c_void};
use std::ptr;

/// Why a snapshot couldn't be turned into VT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtError {
    /// The bytes end before the snapshot does; call again once more of the stream has arrived.
    Incomplete,
    /// The snapshot is damaged or not a snapshot; libghostty-vt's result code.
    Malformed(c_int),
}

impl std::fmt::Display for VtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VtError::Incomplete => f.write_str("snapshot incomplete"),
            VtError::Malformed(code) => write!(f, "snapshot unreadable (libghostty-vt {code})"),
        }
    }
}

/// The size of the terminal the VT is replayed into: its grid, and a cell's pixel size (0 when
/// unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

type Terminal = *mut c_void;
type Decoder = *mut c_void;
type Formatter = *mut c_void;

const SUCCESS: c_int = 0;
const IO_ERROR: c_int = -5;
const DECODER_DATA_SOURCE_OFFSET: c_int = 2;
const FORMAT_VT: c_int = 1;

#[repr(C)]
struct Reader {
    read: extern "C" fn(*mut c_void, *mut u8, usize, *mut usize) -> bool,
    userdata: *mut c_void,
}

#[repr(C)]
struct ScreenExtra {
    size: usize,
    cursor: bool,
    style: bool,
    hyperlink: bool,
    protection: bool,
    kitty_keyboard: bool,
    charsets: bool,
}

#[repr(C)]
struct TerminalExtra {
    size: usize,
    palette: bool,
    modes: bool,
    scrolling_region: bool,
    tabstops: bool,
    pwd: bool,
    keyboard: bool,
    screen: ScreenExtra,
}

#[repr(C)]
struct FormatterOptions {
    size: usize,
    emit: c_int,
    unwrap: bool,
    trim: bool,
    extra: TerminalExtra,
    selection: *const c_void,
}

extern "C" {
    fn ghostty_snapshot_decoder_new(alloc: *const c_void, dec: *mut Decoder, r: Reader) -> c_int;
    fn ghostty_snapshot_decoder_decode(dec: Decoder, term: *mut Terminal) -> c_int;
    fn ghostty_snapshot_decoder_get(dec: Decoder, data: c_int, out: *mut c_void) -> c_int;
    fn ghostty_snapshot_decoder_free(dec: Decoder);
    fn ghostty_terminal_free(term: Terminal);
    fn ghostty_terminal_resize(term: Terminal, cols: u16, rows: u16, cw: u32, ch: u32) -> c_int;
    fn ghostty_formatter_terminal_new(
        alloc: *const c_void,
        fmt: *mut Formatter,
        term: Terminal,
        opts: FormatterOptions,
    ) -> c_int;
    fn ghostty_formatter_format_alloc(
        fmt: Formatter,
        alloc: *const c_void,
        out: *mut *mut u8,
        len: *mut usize,
    ) -> c_int;
    fn ghostty_formatter_free(fmt: Formatter);
    fn ghostty_free(alloc: *const c_void, ptr: *mut u8, len: usize);
}

/// The decoder's view of the buffered stream: hands out what has arrived, then reports a read
/// failure, which the decoder returns as `IO_ERROR` — distinguishing "not here yet" from a damaged
/// snapshot (a clean end-of-file would read as truncation, `INVALID_VALUE`).
struct Source<'a> {
    bytes: &'a [u8],
    pos: usize,
}

extern "C" fn read_source(ud: *mut c_void, buf: *mut u8, cap: usize, out: *mut usize) -> bool {
    // SAFETY: `ud` is the `Source` that `snapshot_to_vt` keeps alive across the decode; `buf` has
    // room for `cap` bytes and `out` is valid, per the GhosttyReaderFn contract.
    let src = unsafe { &mut *(ud as *mut Source) };
    let rest = &src.bytes[src.pos..];
    if rest.is_empty() {
        return false;
    }
    let n = rest.len().min(cap);
    unsafe {
        ptr::copy_nonoverlapping(rest.as_ptr(), buf, n);
        *out = n;
    }
    src.pos += n;
    true
}

/// Frees libghostty-vt handles however `snapshot_to_vt` returns.
struct Handles {
    decoder: Decoder,
    terminal: Terminal,
    formatter: Formatter,
}

impl Drop for Handles {
    fn drop(&mut self) {
        // SAFETY: each handle is null or live; the formatter borrows the terminal, so it goes
        // first, and the terminal outlives the decoder that borrowed it during the decode.
        unsafe {
            ghostty_formatter_free(self.formatter);
            ghostty_snapshot_decoder_free(self.decoder);
            ghostty_terminal_free(self.terminal);
        }
    }
}

/// Decodes the snapshot at the start of `bytes` (READY plus every history page, through FINISH)
/// and formats the restored terminal as VT: screen and scrollback, with cursor, modes, style,
/// palette, keyboard and kitty keyboard modes, scrolling region, charsets and tabstops. Returns
/// the VT and the snapshot's length in `bytes`; what follows it is stream output. The terminal
/// takes the size the snapshot records, then — given the `pane` it replays into — that size,
/// reflowing the primary screen, so the replay's rows and cursor fit the pane. Rex isn't
/// resized: warden-app owns the block's size.
pub fn snapshot_to_vt(bytes: &[u8], pane: Option<PaneSize>) -> Result<(Vec<u8>, usize), VtError> {
    let mut src = Source { bytes, pos: 0 };
    let mut h = Handles {
        decoder: ptr::null_mut(),
        terminal: ptr::null_mut(),
        formatter: ptr::null_mut(),
    };
    let reader = Reader {
        read: read_source,
        userdata: &mut src as *mut Source as *mut c_void,
    };
    // SAFETY: `src` outlives the decoder (freed by `h` before `src` drops, declared after it).
    let rc = unsafe { ghostty_snapshot_decoder_new(ptr::null(), &mut h.decoder, reader) };
    check(rc)?;
    let rc = unsafe { ghostty_snapshot_decoder_decode(h.decoder, &mut h.terminal) };
    if rc == IO_ERROR {
        return Err(VtError::Incomplete);
    }
    check(rc)?;
    let mut consumed: usize = 0;
    let rc = unsafe {
        ghostty_snapshot_decoder_get(
            h.decoder,
            DECODER_DATA_SOURCE_OFFSET,
            &mut consumed as *mut usize as *mut c_void,
        )
    };
    check(rc)?;
    if let Some(p) = pane.filter(|p| p.cols > 0 && p.rows > 0) {
        // SAFETY: `h.terminal` is the live terminal the decode just produced.
        let rc = unsafe { ghostty_terminal_resize(h.terminal, p.cols, p.rows, p.cell_w, p.cell_h) };
        check(rc)?;
    }

    let opts = FormatterOptions {
        size: size_of::<FormatterOptions>(),
        emit: FORMAT_VT,
        unwrap: false,
        trim: false,
        extra: TerminalExtra {
            size: size_of::<TerminalExtra>(),
            palette: true,
            modes: true,
            scrolling_region: true,
            tabstops: true,
            pwd: false,
            keyboard: true,
            screen: ScreenExtra {
                size: size_of::<ScreenExtra>(),
                cursor: true,
                style: true,
                hyperlink: false,
                protection: false,
                kitty_keyboard: true,
                charsets: true,
            },
        },
        selection: ptr::null(),
    };
    let rc =
        unsafe { ghostty_formatter_terminal_new(ptr::null(), &mut h.formatter, h.terminal, opts) };
    check(rc)?;
    let mut out: *mut u8 = ptr::null_mut();
    let mut len: usize = 0;
    let rc =
        unsafe { ghostty_formatter_format_alloc(h.formatter, ptr::null(), &mut out, &mut len) };
    check(rc)?;
    let vt = if out.is_null() {
        Vec::new()
    } else {
        // SAFETY: on success `out` holds `len` bytes allocated with the default allocator.
        let v = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
        unsafe { ghostty_free(ptr::null(), out, len) };
        v
    };
    Ok((vt, consumed))
}

fn check(rc: c_int) -> Result<(), VtError> {
    if rc == SUCCESS {
        Ok(())
    } else {
        Err(VtError::Malformed(rc))
    }
}

/// Test support: a snapshot of a fresh `cols`×`rows` terminal after `text` was written to it.
#[cfg(test)]
pub fn encode_snapshot(text: &[u8], cols: u16, rows: u16) -> Vec<u8> {
    extern "C" {
        fn ghostty_terminal_new(alloc: *const c_void, t: *mut Terminal, c: u16, r: u16) -> c_int;
        fn ghostty_terminal_vt_write(t: Terminal, data: *const u8, len: usize);
        fn ghostty_snapshot_encode_alloc(
            t: Terminal,
            alloc: *const c_void,
            out: *mut *mut u8,
            len: *mut usize,
        ) -> c_int;
    }
    let mut term: Terminal = ptr::null_mut();
    // SAFETY: plain FFI with live handles; test-only.
    unsafe {
        assert_eq!(
            ghostty_terminal_new(ptr::null(), &mut term, cols, rows),
            SUCCESS
        );
        ghostty_terminal_vt_write(term, text.as_ptr(), text.len());
        let mut out: *mut u8 = ptr::null_mut();
        let mut len = 0usize;
        assert_eq!(
            ghostty_snapshot_encode_alloc(term, ptr::null(), &mut out, &mut len),
            SUCCESS
        );
        let snap = std::slice::from_raw_parts(out, len).to_vec();
        ghostty_free(ptr::null(), out, len);
        ghostty_terminal_free(term);
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_text_to_vt() {
        let snap = encode_snapshot(b"hello\r\nworld", 80, 24);
        let mut bytes = snap.clone();
        bytes.extend_from_slice(b"TAIL");
        let (vt, consumed) = snapshot_to_vt(&bytes, None).expect("decode");
        let vt = String::from_utf8_lossy(&vt);
        assert!(vt.contains("hello"), "{vt:?}");
        assert!(vt.contains("world"), "{vt:?}");
        assert_eq!(consumed, snap.len());
    }

    /// The longest run of `b` in `s`.
    fn longest_run(s: &str, b: char) -> usize {
        s.split(|c| c != b).map(str::len).max().unwrap_or(0)
    }

    #[test]
    fn a_snapshot_wider_than_the_pane_replays_at_the_pane_width() {
        // A row filled with a background colour (a TUI's status bar), then a line longer than the
        // pane is wide.
        let long = "x".repeat(100);
        let text = format!("\x1b[44m\x1b[K\x1b[0m\r\n{long}\r\nshort\r\nend");
        let snap = encode_snapshot(text.as_bytes(), 120, 24);
        let pane = PaneSize {
            cols: 40,
            rows: 24,
            cell_w: 0,
            cell_h: 0,
        };
        let (vt, _) = snapshot_to_vt(&snap, Some(pane)).expect("decode");
        let vt = String::from_utf8_lossy(&vt);
        assert!(
            longest_run(&vt, ' ') <= 40,
            "a row padded past the pane's 40 columns"
        );
        assert_eq!(vt.matches('x').count(), 100, "the long line reflows whole");
        // Reflowed at 40 columns the 120-cell coloured row and the long line take three rows each,
        // so "end" — and the cursor after it — is on row 8, not the session's row 4.
        assert!(
            vt.ends_with("\x1b[8;4H\x1b[0m"),
            "cursor: {:?}",
            &vt[vt.len().saturating_sub(40)..]
        );
    }

    #[test]
    fn truncated_snapshot_is_incomplete() {
        let snap = encode_snapshot(b"hello", 80, 24);
        for cut in [0, 5, 10, snap.len() / 2, snap.len() - 1] {
            assert_eq!(
                snapshot_to_vt(&snap[..cut], None),
                Err(VtError::Incomplete),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn non_snapshot_is_malformed() {
        assert!(matches!(
            snapshot_to_vt(b"definitely not a ghostty snapshot", None),
            Err(VtError::Malformed(_))
        ));
    }
}

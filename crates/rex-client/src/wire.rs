//! Pure codecs: the data handshake messages, the terminal stream's APC demux, and the input filter.

use base64::Engine;
use serde_json::Value;

use crate::RexError;

/// Decodes a `data.connect` ticket: 16 bytes as 22 base64url characters, no padding.
pub fn decode_ticket(ticket: &str) -> Result<[u8; 16], RexError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(ticket)
        .map_err(|e| RexError::Protocol(format!("bad ticket: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| RexError::Protocol("ticket is not 16 bytes".into()))
}

/// Encodes 16 bytes the way `data.connect` writes a ticket.
pub fn encode_ticket(ticket: &[u8; 16]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ticket)
}

/// HELLO: `0x01` then the ticket.
pub fn encode_hello(ticket: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.push(0x01);
    out.extend_from_slice(ticket);
    out
}

/// RESUME: `0x02`, the latest reconnect token, then the stream bytes received so far (u64 LE).
pub fn encode_resume(token: &[u8; 16], received: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(25);
    out.push(0x02);
    out.extend_from_slice(token);
    out.extend_from_slice(&received.to_le_bytes());
    out
}

/// A data connection's WELCOME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Welcome {
    /// The reconnect token for the next RESUME.
    pub token: [u8; 16],
    /// How long the server keeps the stream for a resume.
    pub resume_window_ms: u32,
    /// The channel nonce that marks Rex's own APCs in the stream.
    pub nonce: u64,
}

/// WELCOME: `0x81`, token, 3 zero bytes, window (u32 LE @20), nonce (u64 LE @24) — exactly 32 bytes.
pub fn encode_welcome(w: &Welcome) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(0x81);
    out.extend_from_slice(&w.token);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&w.resume_window_ms.to_le_bytes());
    out.extend_from_slice(&w.nonce.to_le_bytes());
    out
}

/// Decodes the server's handshake answer: a WELCOME, or a REJECT as `RexError::Rejected(reason)`.
pub fn decode_handshake(msg: &[u8]) -> Result<Welcome, RexError> {
    match msg {
        [0x81, rest @ ..] if msg.len() == 32 => {
            let mut token = [0u8; 16];
            token.copy_from_slice(&rest[..16]);
            let mut window = [0u8; 4];
            window.copy_from_slice(&msg[20..24]);
            let mut nonce = [0u8; 8];
            nonce.copy_from_slice(&msg[24..32]);
            Ok(Welcome {
                token,
                resume_window_ms: u32::from_le_bytes(window),
                nonce: u64::from_le_bytes(nonce),
            })
        }
        [0x82, reason, ..] => Err(RexError::Rejected(*reason)),
        _ => Err(RexError::Protocol(format!(
            "unexpected data handshake message ({} bytes)",
            msg.len()
        ))),
    }
}

/// A Rex message carried in the terminal stream (`ESC _ rex1;<nonce>;<json> ESC \`).
#[derive(Debug, Clone, PartialEq)]
pub enum RexMessage {
    /// The terminal's size; `owner` is the block's owner when it was set.
    Resize {
        cols: u16,
        rows: u16,
        owner: Option<String>,
    },
    /// The terminal's default colours — the whole message, each one replacing the last.
    Theme(Value),
    /// The program exited; no more program output follows, but the stream stays open.
    ChildExited { exit_code: Option<i32> },
    /// A type this client doesn't know (skip it).
    Other,
}

impl RexMessage {
    fn parse(json: &[u8]) -> Option<RexMessage> {
        let v: Value = serde_json::from_slice(json).ok()?;
        Some(match v.get("type").and_then(Value::as_str) {
            Some("resize") => {
                let dim = |k: &str| v.get(k)?.as_u64()?.try_into().ok();
                match (dim("cols"), dim("rows")) {
                    (Some(cols), Some(rows)) => RexMessage::Resize {
                        cols,
                        rows,
                        owner: v.get("owner").and_then(Value::as_str).map(str::to_owned),
                    },
                    _ => RexMessage::Other,
                }
            }
            Some("theme") => RexMessage::Theme(v),
            Some("child_exited") => RexMessage::ChildExited {
                exit_code: v
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .and_then(|c| c.try_into().ok()),
            },
            _ => RexMessage::Other,
        })
    }
}

/// Splits a terminal stream into program bytes and Rex messages. Rex's APCs carry the channel
/// nonce; one with another nonce was printed by a program and passes through as program bytes.
/// An APC (or a possible start of one) cut off at the end of a chunk is held for the next push.
pub struct StreamDemux {
    marker: Vec<u8>,
    held: Vec<u8>,
}

impl StreamDemux {
    pub fn new(nonce: u64) -> StreamDemux {
        StreamDemux {
            marker: format!("\x1b_rex1;{nonce};").into_bytes(),
            held: Vec::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> (Vec<u8>, Vec<RexMessage>) {
        self.held.extend_from_slice(chunk);
        let buf = &self.held;
        let marker = &self.marker;
        let mut out = Vec::with_capacity(buf.len());
        let mut msgs = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if buf[i..].starts_with(marker) {
                match find(&buf[i..], b"\x1b\\") {
                    Some(end) => {
                        // Malformed JSON under our own nonce is dropped, not shown.
                        if let Some(m) = RexMessage::parse(&buf[i + marker.len()..i + end]) {
                            msgs.push(m);
                        }
                        i += end + 2;
                    }
                    None => break, // APC split across chunks: wait for its terminator.
                }
            } else if buf[i] == 0x1b && marker.starts_with(&buf[i..]) {
                break; // a possible marker prefix at the end of the buffer
            } else {
                out.push(buf[i]);
                i += 1;
            }
        }
        self.held.drain(..i);
        (out, msgs)
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Removes sequences a terminal generates on its own rather than the user typing them: focus
/// in/out (CSI I/O), cursor position reports (CSI r;c R), device attributes (CSI ? … c,
/// CSI > … c), mode reports (CSI … $ y), kitty keyboard flag reports (CSI ? … u — key reports
/// carry no `?`), window reports (CSI … t), and OSC/DCS/APC strings. The Rex server answers
/// queries itself, so forwarding these would type garbage into the program.
pub fn strip_terminal_replies(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() {
            match b[i + 1] {
                b'[' => {
                    // CSI: params/intermediates 0x20-0x3f, final 0x40-0x7e.
                    let mut j = i + 2;
                    while j < b.len() && (0x20..=0x3f).contains(&b[j]) {
                        j += 1;
                    }
                    if j < b.len() {
                        let params = &b[i + 2..j];
                        let fin = b[j];
                        let private = params.first().is_some_and(|c| *c == b'?' || *c == b'>');
                        let reply = (params.is_empty() && (fin == b'I' || fin == b'O'))
                            || (fin == b'R' && params.contains(&b';'))
                            || (fin == b'c' && private)
                            || (fin == b'u' && params.first() == Some(&b'?'))
                            || (fin == b'y' && params.last() == Some(&b'$'))
                            || fin == b't';
                        if reply {
                            i = j + 1;
                            continue;
                        }
                    }
                }
                b']' | b'P' | b'_' => {
                    // String sequence, ended by BEL or ST (ESC \).
                    let mut j = i + 2;
                    while j < b.len()
                        && b[j] != 0x07
                        && !(b[j] == 0x1b && b.get(j + 1) == Some(&b'\\'))
                    {
                        j += 1;
                    }
                    i = if j < b.len() && b[j] == 0x07 {
                        j + 1
                    } else {
                        (j + 2).min(b.len())
                    };
                    continue;
                }
                _ => {}
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64url(s: &str) -> [u8; 16] {
        decode_ticket(s).expect("valid ticket")
    }

    #[test]
    fn hello_is_17_bytes_with_ticket() {
        let hello = encode_hello(&b64url("AAECAwQFBgcICQoLDA0ODw"));
        let mut want = vec![0x01u8];
        want.extend(0u8..=15);
        assert_eq!(hello, want);
    }

    #[test]
    fn welcome_decodes_token_window_nonce() {
        let mut frame = vec![0x81u8];
        frame.extend(100u8..116);
        frame.extend([0, 0, 0]);
        frame.extend(60_000u32.to_le_bytes());
        frame.extend(0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(frame.len(), 32);
        let w = decode_handshake(&frame).expect("welcome");
        let token: Vec<u8> = (100u8..116).collect();
        assert_eq!(w.token.to_vec(), token);
        assert_eq!(w.resume_window_ms, 60_000);
        assert_eq!(w.nonce, 0x0102_0304_0506_0708);
    }

    #[test]
    fn reject_decodes_reason() {
        assert_eq!(decode_handshake(&[0x82, 0x02]), Err(RexError::Rejected(2)));
    }

    #[test]
    fn demux_strips_matching_apc_and_keeps_program_bytes() {
        let mut d = StreamDemux::new(7);
        let (bytes, msgs) = d.push(
            b"one\x1b_rex1;7;{\"type\":\"child_exited\",\"exit_code\":3}\x1b\\two\x1b_rex1;7;{\"type\":\"theme\",\"background\":\"000000\"}\x1b\\three\x1b_rex1;7;{\"type\":\"novel\"}\x1b\\",
        );
        assert_eq!(bytes, b"onetwothree");
        assert_eq!(
            msgs,
            vec![
                RexMessage::ChildExited { exit_code: Some(3) },
                RexMessage::Theme(serde_json::json!({"type":"theme","background":"000000"})),
                RexMessage::Other,
            ]
        );
    }

    #[test]
    fn demux_passes_through_apc_with_wrong_nonce() {
        let mut d = StreamDemux::new(7);
        let input = b"a\x1b_rex1;99;{\"type\":\"resize\",\"cols\":1,\"rows\":1}\x1b\\b";
        let (bytes, msgs) = d.push(input);
        assert_eq!(bytes, input.to_vec());
        assert!(msgs.is_empty());
    }

    #[test]
    fn demux_holds_apc_split_across_chunks() {
        let mut d = StreamDemux::new(7);
        let (b1, m1) = d.push(b"ab\x1b_rex1;7;{\"type\":");
        assert_eq!(b1, b"ab");
        assert!(m1.is_empty());
        let (b2, m2) = d.push(b"\"resize\",\"cols\":80,\"rows\":24,\"owner\":null}\x1b\\cd");
        assert_eq!(b2, b"cd");
        assert_eq!(
            m2,
            vec![RexMessage::Resize {
                cols: 80,
                rows: 24,
                owner: None
            }]
        );
    }

    #[test]
    fn demux_holds_marker_prefix_at_chunk_end() {
        let mut d = StreamDemux::new(7);
        let (b1, _) = d.push(b"x\x1b_re");
        assert_eq!(b1, b"x");
        let (b2, m2) = d.push(
            b"x1;7;{\"type\":\"resize\",\"cols\":2,\"rows\":3,\"owner\":\"client:a\"}\x1b\\y",
        );
        assert_eq!(b2, b"y");
        assert_eq!(
            m2,
            vec![RexMessage::Resize {
                cols: 2,
                rows: 3,
                owner: Some("client:a".into())
            }]
        );
        // A lone ESC that turns out not to start the marker is released on the next push.
        let (b3, _) = d.push(b"\x1b");
        assert!(b3.is_empty());
        let (b4, _) = d.push(b"[A");
        assert_eq!(b4, b"\x1b[A");
    }

    #[test]
    fn strip_replies_drops_focus_cpr_da_mode_kitty_window_and_strings() {
        let input = b"\x1b[O\x1b[I\x1b[12;40R\x1b[?62;22c\x1b[?1;2$y\x1b[?5u\x1b[8;24;80t\x1b]11;rgb:0/0/0\x07x";
        assert_eq!(strip_terminal_replies(input), b"x");
    }

    #[test]
    fn strip_replies_keeps_keys() {
        let input = b"\x1b[A\x1b[1;5D\x1b[97;5u";
        assert_eq!(strip_terminal_replies(input), input.to_vec());
    }
}

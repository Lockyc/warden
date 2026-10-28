//! `warden-relay`: shows one Rex terminal block in the terminal it runs in (a warden pane).
//!
//! It writes the block's stream to stdout — the opening snapshot replayed as VT, then the
//! program's output with Rex's own messages removed — and types stdin into the block. It never
//! resizes the block: warden-app owns its size.

mod term;
mod vt;

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use rex_client::wire::{strip_terminal_replies, RexMessage, StreamDemux};
use rex_client::{ClientInfo, ControlClient, DataStream, Endpoint, RexError, DATA_PROTOCOL};
use serde_json::json;

const USAGE: &str =
    "usage: warden-relay --endpoint <unix:PATH|https://HOST:PORT> --session <id> --block <id>";
/// Bad command line (sysexits `EX_USAGE`).
const EX_USAGE: i32 = 64;
/// The server couldn't be reached at start (sysexits `EX_TEMPFAIL`).
const EX_TEMPFAIL: i32 = 75;
/// Written before every snapshot, so a restarted stream redraws over the old screen.
const CLEAR: &[u8] = b"\x1b[H\x1b[2J";
/// One dim line per outage, written when the first reconnect attempt has failed.
const RECONNECTING: &[u8] = "\r\n\x1b[2m[rex: reconnecting…]\x1b[22m\r\n".as_bytes();
/// Seconds between reconnect attempts; the last repeats.
const BACKOFF_SECS: [u64; 4] = [1, 2, 4, 8];
/// A data connection's close status for "the block closed; don't reconnect".
const BLOCK_CLOSED: u16 = 4000;
/// A data connection's close status for "start a new stream with a new ticket".
const RECONNECT_REQUIRED: u16 = 1013;

struct Args {
    endpoint: Endpoint,
    session: String,
    block: String,
}

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("warden-relay: {e}\n{USAGE}");
            std::process::exit(EX_USAGE);
        }
    };
    let raw = term::RawMode::enable();
    let code = run(&args);
    drop(raw);
    std::process::exit(code);
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut endpoint, mut session, mut block) = (None, None, None);
    while let Some(flag) = it.next() {
        let slot = match flag.as_str() {
            "--endpoint" => &mut endpoint,
            "--session" => &mut session,
            "--block" => &mut block,
            _ => return Err(format!("unexpected argument {flag:?}")),
        };
        *slot = Some(it.next().ok_or(format!("{flag} needs a value"))?);
    }
    let endpoint = endpoint.ok_or("--endpoint is required")?;
    Ok(Args {
        endpoint: endpoint
            .parse()
            .map_err(|e| format!("bad --endpoint {endpoint:?}: {e}"))?,
        session: session.ok_or("--session is required")?,
        block: block.ok_or("--block is required")?,
    })
}

/// How the relay stops: the process exit status.
struct Stop(i32);

impl From<io::Error> for Stop {
    /// stdout is the pane; once it's gone there is nothing left to relay to.
    fn from(_: io::Error) -> Stop {
        Stop(1)
    }
}

fn run(args: &Args) -> i32 {
    let link = Arc::new(Link {
        endpoint: args.endpoint.clone(),
        session: args.session.clone(),
        client: Mutex::new(None),
    });
    if let Err(e) = link.client() {
        eprintln!("warden-relay: {e}");
        return match e {
            RexError::Unreachable | RexError::Closed(_) | RexError::Timeout => EX_TEMPFAIL,
            _ => 1,
        };
    }
    let input = link.clone();
    let block = args.block.clone();
    thread::spawn(move || relay_stdin(&input, &block));
    match relay_stream(args, &link, &mut io::stdout()) {
        Ok(never) => match never {},
        Err(Stop(code)) => code,
    }
}

/// The control connection, reconnected on demand by whichever thread next needs it.
struct Link {
    endpoint: Endpoint,
    session: String,
    client: Mutex<Option<Arc<ControlClient>>>,
}

impl Link {
    /// The live control client, connecting (and attaching to the session) if there is none. A
    /// dropped one is resumed when the server still knows it, which keeps its data streams alive.
    fn client(&self) -> Result<Arc<ControlClient>, RexError> {
        let mut slot = lock(&self.client);
        if let Some(c) = slot.as_ref().filter(|c| c.closed().is_none()) {
            return Ok(c.clone());
        }
        let info = ClientInfo {
            kind: "cli",
            name: "warden-relay".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        let token = slot
            .as_ref()
            .and_then(|c| c.reconnect_token().map(str::to_owned));
        let c = match token {
            Some(t) => ControlClient::resume(&self.endpoint, &info, &t)?,
            None => ControlClient::connect(&self.endpoint, &info)?,
        };
        if !c.resumed() {
            c.call("session.attach", json!({"session_id": self.session}))?;
        }
        let c = Arc::new(c);
        *slot = Some(c.clone());
        Ok(c)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Types stdin into the block until stdin ends. Input that can't be sent (the server is away) is
/// dropped, as keys pressed into a disconnected terminal are.
fn relay_stdin(link: &Link, block: &str) {
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 4096];
    let mut pending = Vec::new();
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        pending.extend_from_slice(&buf[..n]);
        // `write_input` carries text, so a character cut by the read boundary waits for its rest.
        let whole = complete_utf8_len(&pending);
        let input = strip_terminal_replies(&pending[..whole]);
        pending.drain(..whole);
        if input.is_empty() {
            continue;
        }
        if let Ok(c) = link.client() {
            let _ = c.write_input(&link.session, block, &input);
        }
    }
}

/// The length of `b` without a trailing UTF-8 sequence that is cut short.
fn complete_utf8_len(b: &[u8]) -> usize {
    for back in 1..=b.len().min(4) {
        let i = b.len() - back;
        let need = match b[i] {
            0x80..=0xBF => continue,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 1,
        };
        return if back < need { i } else { b.len() };
    }
    b.len()
}

/// Relays streams until one ends the relay: each new stream (a HELLO) starts with a snapshot; a
/// dropped one is resumed, or restarted when the server refuses the resume.
fn relay_stream(
    args: &Args,
    link: &Link,
    out: &mut impl Write,
) -> Result<std::convert::Infallible, Stop> {
    let mut backoff = Backoff::default();
    loop {
        let mut stream = match open_stream(args, link) {
            Ok(s) => s,
            Err(RexError::Api {
                message, status, ..
            }) if status < 500 => {
                eprintln!("warden-relay: {message}");
                return Err(Stop(1));
            }
            Err(_) => {
                backoff.wait(out)?;
                continue;
            }
        };
        backoff.reset();
        let mut feed = Feed::new(stream.nonce());
        loop {
            let code = loop {
                match stream.read() {
                    Ok(Some(chunk)) => {
                        if feed.push(&chunk, out)? {
                            return Err(Stop(0));
                        }
                    }
                    Ok(None) => break stream.close_code().unwrap_or(1006),
                    Err(_) => break 1006,
                }
            };
            match code {
                BLOCK_CLOSED => return Err(Stop(0)),
                RECONNECT_REQUIRED => break,
                _ => match resume(args, &stream, &mut backoff, out)? {
                    Some(s) => stream = s,
                    None => break,
                },
            }
        }
    }
}

fn open_stream(args: &Args, link: &Link) -> Result<DataStream, RexError> {
    let ticket = link.client()?.call(
        "data.connect",
        json!({"session_id": args.session, "block_id": args.block,
               "data_ref_id": "terminal", "subprotocol": DATA_PROTOCOL}),
    )?;
    let ticket = ticket["ticket"]
        .as_str()
        .ok_or_else(|| RexError::Protocol("data.connect returned no ticket".into()))?;
    DataStream::open(&args.endpoint, ticket)
}

/// Resumes `old` within its resume window. `None` when the server refused (or the window passed):
/// the stream must start over.
fn resume(
    args: &Args,
    old: &DataStream,
    backoff: &mut Backoff,
    out: &mut impl Write,
) -> Result<Option<DataStream>, Stop> {
    let deadline = Instant::now() + old.resume_window();
    loop {
        match DataStream::resume(&args.endpoint, old.token(), old.received()) {
            Ok(s) => {
                backoff.reset();
                return Ok(Some(s));
            }
            Err(RexError::Rejected(_)) => return Ok(None),
            Err(_) if Instant::now() >= deadline => return Ok(None),
            Err(_) => backoff.wait(out)?,
        }
    }
}

/// One stream's bytes on their way to stdout: the snapshot, buffered until whole, then output
/// through the demux.
struct Feed {
    snapshot: Option<Vec<u8>>,
    demux: StreamDemux,
}

impl Feed {
    fn new(nonce: u64) -> Feed {
        Feed {
            snapshot: Some(Vec::new()),
            demux: StreamDemux::new(nonce),
        }
    }

    /// Writes what `chunk` completes; true once the block's program has exited.
    fn push(&mut self, chunk: &[u8], out: &mut impl Write) -> Result<bool, Stop> {
        let rest = match self.snapshot.as_mut() {
            None => chunk.to_vec(),
            Some(buf) => {
                buf.extend_from_slice(chunk);
                match vt::snapshot_to_vt(buf) {
                    Ok((screen, len)) => {
                        out.write_all(CLEAR)?;
                        out.write_all(&screen)?;
                        let rest = buf.split_off(len);
                        self.snapshot = None;
                        rest
                    }
                    Err(vt::VtError::Incomplete) => return Ok(false),
                    Err(e) => {
                        eprintln!("warden-relay: {e}");
                        return Err(Stop(1));
                    }
                }
            }
        };
        let (bytes, msgs) = self.demux.push(&rest);
        out.write_all(&bytes)?;
        out.flush()?;
        Ok(msgs
            .iter()
            .any(|m| matches!(m, RexMessage::ChildExited { .. })))
    }
}

/// Waits between reconnect attempts, and says so once per outage.
#[derive(Default)]
struct Backoff {
    attempt: usize,
}

impl Backoff {
    fn wait(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.attempt == 0 {
            out.write_all(RECONNECTING)?;
            out.flush()?;
        }
        let secs = BACKOFF_SECS[self.attempt.min(BACKOFF_SECS.len() - 1)];
        self.attempt += 1;
        thread::sleep(Duration::from_secs(secs));
        Ok(())
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_utf8_len_holds_back_a_cut_character() {
        let e = "é".as_bytes();
        let euro = "€".as_bytes();
        let crab = "🦀".as_bytes();
        assert_eq!(complete_utf8_len(b"ab"), 2);
        assert_eq!(complete_utf8_len(&[b'a', e[0]]), 1);
        assert_eq!(complete_utf8_len(e), 2);
        assert_eq!(complete_utf8_len(&euro[..2]), 0);
        assert_eq!(complete_utf8_len(&crab[..3]), 0);
        assert_eq!(complete_utf8_len(crab), 4);
        // Invalid bytes aren't held: they'd never complete.
        assert_eq!(complete_utf8_len(&[0xFF]), 1);
        assert_eq!(complete_utf8_len(&[0x80, 0x80]), 2);
        assert_eq!(complete_utf8_len(b""), 0);
    }

    #[test]
    fn parse_args_reads_the_three_flags() {
        let a = parse_args(
            [
                "--endpoint",
                "unix:/tmp/s.sock",
                "--session",
                "s",
                "--block",
                "b",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("parse");
        assert_eq!(a.endpoint.to_string(), "unix:/tmp/s.sock");
        assert_eq!((a.session.as_str(), a.block.as_str()), ("s", "b"));
        assert!(parse_args(["--session", "s"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["--bogus"].into_iter().map(String::from)).is_err());
    }
}

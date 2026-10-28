//! A data connection: one block's output stream, read blocking.

use std::time::Duration;

use tungstenite::Message;

use crate::endpoint::{self, io_err, Ws};
use crate::wire;
use crate::{Endpoint, RexError, DATA_PROTOCOL};

/// How long the WELCOME may take after the HELLO or RESUME.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
/// Status for a connection that ended without a close frame.
const ABNORMAL: u16 = 1006;
/// The longest a read waits with nothing arriving. The server pings data connections every
/// [`crate::SERVER_PING_INTERVAL`], so two missed pings plus a ping's grace means the connection
/// is dead — a half-open TCP stream after a sleep never errors on its own.
const IDLE_TIMEOUT: Duration = Duration::from_secs(
    2 * crate::SERVER_PING_INTERVAL.as_secs() + crate::SERVER_PING_WAIT.as_secs(),
);

/// One terminal block's stream. Dropping it leaves the stream waiting on the server for a
/// RESUME (within the welcome's resume window); the server ends it only on a 1000 close.
pub struct DataStream {
    ws: Ws,
    welcome: wire::Welcome,
    received: u64,
    close_code: Option<u16>,
}

impl DataStream {
    /// Opens a new stream with a `data.connect` ticket (base64url, as the result carries it).
    pub fn open(endpoint: &Endpoint, ticket: &str) -> Result<DataStream, RexError> {
        let ticket = wire::decode_ticket(ticket)?;
        Self::handshake(endpoint, wire::encode_hello(&ticket), 0)
    }

    /// Continues a dropped stream with the latest WELCOME's token and the exact count of stream
    /// bytes used so far, across every connection of the stream ([`DataStream::received`]).
    pub fn resume(
        endpoint: &Endpoint,
        token: [u8; 16],
        received: u64,
    ) -> Result<DataStream, RexError> {
        Self::handshake(endpoint, wire::encode_resume(&token, received), received)
    }

    fn handshake(endpoint: &Endpoint, msg: Vec<u8>, received: u64) -> Result<DataStream, RexError> {
        let mut ws = endpoint::open(endpoint, "/api/data", DATA_PROTOCOL)?;
        ws.send(Message::binary(msg)).map_err(ws_err)?;
        ws.set_read_timeout(Some(WELCOME_TIMEOUT)).map_err(io_err)?;
        let welcome = loop {
            match ws.read() {
                Ok(Message::Binary(b)) => break wire::decode_handshake(&b)?,
                Ok(Message::Close(f)) => {
                    return Err(RexError::Closed(f.map_or(1005, |f| f.code.into())))
                }
                Ok(_) => {}
                Err(e) => return Err(ws_err(e)),
            }
        };
        ws.set_read_timeout(Some(IDLE_TIMEOUT)).map_err(io_err)?;
        Ok(DataStream {
            ws,
            welcome,
            received,
            close_code: None,
        })
    }

    /// The next piece of the stream, blocking until one arrives. `None` once the connection has
    /// closed; [`DataStream::close_code`] then says how (1006 for a drop without a close frame,
    /// or for [`IDLE_TIMEOUT`] of silence, which a RESUME can continue).
    pub fn read(&mut self) -> Result<Option<Vec<u8>>, RexError> {
        if self.close_code.is_some() {
            return Ok(None);
        }
        loop {
            match self.ws.read() {
                Ok(Message::Binary(b)) => {
                    self.received += b.len() as u64;
                    return Ok(Some(b.to_vec()));
                }
                Ok(Message::Close(f)) => {
                    self.close_code = Some(f.map_or(1005, |f| f.code.into()));
                    let _ = self.ws.flush(); // send the close reply tungstenite queued
                    return Ok(None);
                }
                Ok(_) => {}
                Err(
                    tungstenite::Error::ConnectionClosed
                    | tungstenite::Error::AlreadyClosed
                    | tungstenite::Error::Io(_)
                    | tungstenite::Error::Protocol(
                        tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                    ),
                ) => {
                    self.close_code = Some(ABNORMAL);
                    return Ok(None);
                }
                Err(e) => return Err(RexError::Protocol(e.to_string())),
            }
        }
    }

    /// Shortens the idle timeout so a test needn't wait minutes for it.
    #[cfg(test)]
    fn set_idle_timeout(&mut self, d: Duration) -> Result<(), RexError> {
        self.ws.set_read_timeout(Some(d)).map_err(io_err)
    }

    /// The close status once [`DataStream::read`] has returned `None`.
    pub fn close_code(&self) -> Option<u16> {
        self.close_code
    }

    /// The channel nonce marking Rex's own APCs in this stream (for [`wire::StreamDemux`]).
    pub fn nonce(&self) -> u64 {
        self.welcome.nonce
    }

    /// The latest WELCOME's reconnect token, for a RESUME.
    pub fn token(&self) -> [u8; 16] {
        self.welcome.token
    }

    /// Stream bytes returned so far, counting from the start of the stream (not this connection).
    pub fn received(&self) -> u64 {
        self.received
    }

    /// How long the server keeps the stream for a RESUME after a drop.
    pub fn resume_window(&self) -> Duration {
        Duration::from_millis(self.welcome.resume_window_ms.into())
    }
}

fn ws_err(e: tungstenite::Error) -> RexError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            RexError::Closed(ABNORMAL)
        }
        tungstenite::Error::Io(e) => io_err(e),
        e => RexError::Protocol(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{DataHandshake, FakeServer};
    use std::time::Duration;

    #[test]
    fn data_open_then_resume_counts_bytes() {
        let fake = FakeServer::start();
        fake.on_data_open(|_| b"snap".to_vec());
        let ep = fake.endpoint();
        let mut s = DataStream::open(&ep, &fake.mint_ticket()).expect("open");
        assert_eq!(s.nonce(), fake.nonce());
        assert_eq!(s.read().expect("read"), Some(b"snap".to_vec()));
        fake.push_data(b"hello");
        assert_eq!(s.read().expect("read"), Some(b"hello".to_vec()));
        assert_eq!(s.received(), 9);

        fake.drop_data();
        assert_eq!(s.read().expect("read"), None);
        assert_eq!(s.close_code(), Some(1006));
        fake.push_data(b" world"); // while disconnected: replayed by the resume

        let old_token = s.token();
        let mut r = DataStream::resume(&ep, old_token, s.received()).expect("resume");
        assert_eq!(r.read().expect("read"), Some(b" world".to_vec()));
        assert_eq!(r.received(), 15);
        assert_eq!(r.nonce(), s.nonce());
        assert_ne!(r.token(), old_token);
        assert_eq!(
            fake.wait_data_handshakes(2, Duration::from_secs(5))[1],
            DataHandshake::Resume {
                token: old_token,
                received: 9
            }
        );
    }

    #[test]
    fn data_open_rejected_reports_reason() {
        let fake = FakeServer::start();
        fake.reject_next_hello(2);
        let ep = fake.endpoint();
        assert_eq!(
            DataStream::open(&ep, &fake.mint_ticket()).err(),
            Some(RexError::Rejected(2))
        );
        // A ticket the server never issued.
        assert_eq!(
            DataStream::open(&ep, "AAECAwQFBgcICQoLDA0ODw").err(),
            Some(RexError::Rejected(1))
        );
    }

    /// A manual round trip against the real local server, in a session it creates and destroys
    /// itself: `cargo test -p rex-client -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a running local Rex server"]
    fn real_server_stream_round_trip() {
        use crate::{ClientInfo, ControlClient};
        use serde_json::json;

        let ep = Endpoint::local();
        let info = ClientInfo {
            kind: "cli",
            name: "rex-client-test".into(),
            version: "0.0.0".into(),
        };
        let c = ControlClient::connect(&ep, &info).expect("connect");
        let created = c
            .call(
                "session.create",
                json!({"label": "rex-client-test", "initial_windows": [
                    {"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}
                ]}),
            )
            .expect("create");
        let session = created["session_id"].as_str().expect("session").to_owned();
        let block = created["initial_windows"][0]["block_ids"][0]
            .as_str()
            .expect("block")
            .to_owned();
        c.call("session.attach", json!({"session_id": session}))
            .expect("attach");
        // Destroying the session ends the stream, so a read that would block forever returns.
        let (done, cancel) = std::sync::mpsc::channel::<()>();
        let destroy = || {
            let _ = c.call("session.destroy", json!({"session_id": session}));
        };
        let result = std::thread::scope(|scope| {
            scope.spawn(move || {
                if cancel.recv_timeout(Duration::from_secs(8)).is_err() {
                    destroy();
                }
            });
            let r = round_trip(&c, &ep, &session, &block);
            let _ = done.send(());
            r
        });
        destroy();
        let (resized, out) = result.expect("round trip");
        assert!(resized, "no resize message in the stream");
        assert!(
            out.contains("rex-client-$((6*7))"),
            "typed text never echoed: {out:?}"
        );
    }

    fn round_trip(
        c: &crate::ControlClient,
        ep: &Endpoint,
        session: &str,
        block: &str,
    ) -> Result<(bool, String), RexError> {
        use crate::wire::{RexMessage, StreamDemux};
        use serde_json::json;
        {
            let ticket = c.call(
                "data.connect",
                json!({"session_id": session, "block_id": block,
                       "data_ref_id": "terminal", "subprotocol": DATA_PROTOCOL}),
            )?;
            let mut s = DataStream::open(ep, ticket["ticket"].as_str().unwrap_or(""))?;
            let mut demux = StreamDemux::new(s.nonce());
            c.write_input(session, block, b"echo rex-client-$((6*7))\r")?;
            let mut out = Vec::new();
            let mut resized = false;
            // The pty's echo of the typed text: proves the write reached the terminal. (The
            // command's own output depends on the user's shell startup, so it isn't awaited.)
            let echoed = |out: &[u8]| String::from_utf8_lossy(out).contains("rex-client-$((6*7))");
            while !(resized && echoed(&out)) {
                let Some(chunk) = s.read()? else { break };
                let (bytes, msgs) = demux.push(&chunk);
                out.extend(bytes);
                resized |= msgs.iter().any(|m| matches!(m, RexMessage::Resize { .. }));
            }
            Ok((resized, String::from_utf8_lossy(&out).into_owned()))
        }
    }

    #[test]
    fn a_silent_stream_drops_as_resumable() {
        // A half-open TCP stream after a laptop sleep: nothing arrives, not even the server's
        // pings. The read must end as a drop a RESUME continues, not block forever.
        let fake = FakeServer::start();
        let mut s = DataStream::open(&fake.endpoint(), &fake.mint_ticket()).expect("open");
        s.set_idle_timeout(Duration::from_millis(200))
            .expect("knob");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = s.read();
            let _ = tx.send((r, s.close_code()));
        });
        let (r, code) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the read gave up on a silent stream");
        assert_eq!(r, Ok(None));
        assert_eq!(code, Some(ABNORMAL));
    }

    #[test]
    fn close_frame_ends_the_stream_with_its_code() {
        let fake = FakeServer::start();
        let mut s = DataStream::open(&fake.endpoint(), &fake.mint_ticket()).expect("open");
        fake.close_data(4000, "block closed");
        assert_eq!(s.read().expect("read"), None);
        assert_eq!(s.close_code(), Some(4000));
    }
}

//! Where a Rex server listens, and opening a WebSocket to it.

use std::fmt;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tungstenite::client::ClientRequestBuilder;
use tungstenite::http::Uri;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Connector, Message, WebSocket};

use crate::RexError;

/// How long connecting and the WebSocket upgrade may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A Rex server: the local Unix socket, or a remote server over HTTPS.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Unix(PathBuf),
    Https { host: String, port: u16 },
}

impl Endpoint {
    /// This Mac's server: `~/Library/Application Support/rex/server.sock`.
    pub fn local() -> Endpoint {
        Endpoint::Unix(
            dirs::home_dir()
                .unwrap_or_default()
                .join("Library/Application Support/rex/server.sock"),
        )
    }
}

/// `unix:<path>` or `https://<host>:<port>` — the form `FromStr` reads back.
impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Unix(p) => write!(f, "unix:{}", p.display()),
            Endpoint::Https { host, port } => write!(f, "https://{host}:{port}"),
        }
    }
}

impl FromStr for Endpoint {
    type Err = RexError;

    fn from_str(s: &str) -> Result<Endpoint, RexError> {
        if let Some(path) = s.strip_prefix("unix:") {
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        let bad = || {
            RexError::Protocol(format!(
                "bad endpoint {s:?}: want unix:PATH or https://HOST:PORT"
            ))
        };
        let rest = s.strip_prefix("https://").ok_or_else(bad)?;
        let rest = rest.trim_end_matches('/');
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| bad())?),
            None => (rest, 443),
        };
        if host.is_empty() {
            return Err(bad());
        }
        Ok(Endpoint::Https {
            host: host.to_owned(),
            port,
        })
    }
}

/// An open WebSocket on either transport (both boxed: each carries its read/write buffers inline).
pub(crate) enum Ws {
    Unix(Box<WebSocket<UnixStream>>),
    Tls(Box<WebSocket<MaybeTlsStream<TcpStream>>>),
}

macro_rules! each {
    ($self:expr, $ws:ident => $e:expr) => {
        match $self {
            Ws::Unix($ws) => $e,
            Ws::Tls($ws) => $e,
        }
    };
}

impl Ws {
    pub(crate) fn read(&mut self) -> tungstenite::Result<Message> {
        each!(self, ws => ws.read())
    }

    pub(crate) fn write(&mut self, m: Message) -> tungstenite::Result<()> {
        each!(self, ws => ws.write(m))
    }

    pub(crate) fn flush(&mut self) -> tungstenite::Result<()> {
        each!(self, ws => ws.flush())
    }

    pub(crate) fn send(&mut self, m: Message) -> tungstenite::Result<()> {
        each!(self, ws => ws.send(m))
    }

    pub(crate) fn close(
        &mut self,
        code: Option<tungstenite::protocol::CloseFrame>,
    ) -> tungstenite::Result<()> {
        each!(self, ws => ws.close(code))
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        match self {
            Ws::Unix(ws) => ws.get_ref().set_nonblocking(on),
            Ws::Tls(ws) => tcp(ws)?.set_nonblocking(on),
        }
    }

    pub(crate) fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        match self {
            Ws::Unix(ws) => ws.get_ref().set_read_timeout(t),
            Ws::Tls(ws) => tcp(ws)?.set_read_timeout(t),
        }
    }

    pub(crate) fn raw_fd(&self) -> io::Result<RawFd> {
        match self {
            Ws::Unix(ws) => Ok(ws.get_ref().as_raw_fd()),
            Ws::Tls(ws) => Ok(tcp(ws)?.as_raw_fd()),
        }
    }
}

fn tcp(ws: &WebSocket<MaybeTlsStream<TcpStream>>) -> io::Result<&TcpStream> {
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Ok(s),
        MaybeTlsStream::Rustls(s) => Ok(s.get_ref()),
        _ => Err(io::Error::other("unsupported TLS stream")),
    }
}

/// Opens a WebSocket at `path` with `subprotocol`. The socket is left blocking, with no timeouts.
pub(crate) fn open(endpoint: &Endpoint, path: &str, subprotocol: &str) -> Result<Ws, RexError> {
    let ws = match endpoint {
        Endpoint::Unix(sock) => {
            let stream = UnixStream::connect(sock).map_err(|_| RexError::Unreachable)?;
            set_timeouts(&stream, Some(HANDSHAKE_TIMEOUT))?;
            let req = request(&format!("ws://localhost{path}"), subprotocol)?;
            let (ws, _) = tungstenite::client(req, stream).map_err(handshake_err)?;
            set_timeouts(ws.get_ref(), None)?;
            Ws::Unix(Box::new(ws))
        }
        Endpoint::Https { host, port } => {
            let stream = connect_tcp(host, *port)?;
            stream
                .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
                .and_then(|_| stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT)))
                .map_err(io_err)?;
            let req = request(&format!("wss://{host}:{port}{path}"), subprotocol)?;
            let connector = Connector::Rustls(tls_config()?);
            let (ws, _) = tungstenite::client_tls_with_config(req, stream, None, Some(connector))
                .map_err(handshake_err)?;
            let ws = Ws::Tls(Box::new(ws));
            ws.set_read_timeout(None).map_err(io_err)?;
            if let Ws::Tls(inner) = &ws {
                tcp(inner)
                    .and_then(|s| s.set_write_timeout(None))
                    .map_err(io_err)?;
            }
            ws
        }
    };
    Ok(ws)
}

fn request(uri: &str, subprotocol: &str) -> Result<ClientRequestBuilder, RexError> {
    let uri: Uri = uri
        .parse()
        .map_err(|e| RexError::Protocol(format!("bad URI {uri}: {e}")))?;
    Ok(ClientRequestBuilder::new(uri).with_sub_protocol(subprotocol))
}

fn set_timeouts(s: &UnixStream, t: Option<Duration>) -> Result<(), RexError> {
    s.set_read_timeout(t)
        .and_then(|_| s.set_write_timeout(t))
        .map_err(io_err)
}

fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, RexError> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| RexError::Unreachable)?;
    for addr in addrs {
        if let Ok(s) = TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT) {
            return Ok(s);
        }
    }
    Err(RexError::Unreachable)
}

/// rustls with the ring provider and the system's trust roots, built once.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, RexError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            let (added, _) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                return Err("no system trust roots".into());
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
        .map_err(RexError::Protocol)
}

fn handshake_err<R: tungstenite::handshake::HandshakeRole>(
    e: tungstenite::HandshakeError<R>,
) -> RexError {
    match e {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Io(e)) => io_err(e),
        tungstenite::HandshakeError::Failure(e) => {
            RexError::Protocol(format!("WebSocket handshake: {e}"))
        }
        tungstenite::HandshakeError::Interrupted(_) => RexError::Timeout,
    }
}

pub(crate) fn io_err(e: io::Error) -> RexError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => RexError::Timeout,
        _ => RexError::Protocol(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_round_trips_through_display() {
        for e in [
            Endpoint::Unix("/Users/x/Library/Application Support/rex/server.sock".into()),
            Endpoint::Https {
                host: "nucleus.tail1234.ts.net".into(),
                port: 8443,
            },
        ] {
            assert_eq!(e.to_string().parse::<Endpoint>(), Ok(e));
        }
        assert_eq!(
            "https://rex.example".parse::<Endpoint>(),
            Ok(Endpoint::Https {
                host: "rex.example".into(),
                port: 443
            })
        );
        assert!("ftp://x".parse::<Endpoint>().is_err());
    }

    #[test]
    fn local_is_the_rex_socket() {
        let Endpoint::Unix(p) = Endpoint::local() else {
            panic!("local is a Unix socket");
        };
        assert!(p.ends_with("Library/Application Support/rex/server.sock"));
    }

    #[test]
    fn missing_socket_is_unreachable() {
        let e = open(
            &Endpoint::Unix("/nonexistent/rex.sock".into()),
            "/api/control",
            "rex.control.v1",
        );
        assert!(matches!(e, Err(RexError::Unreachable)));
    }
}

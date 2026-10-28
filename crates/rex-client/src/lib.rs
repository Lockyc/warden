//! A blocking client for Rex, Superlogical's server-side terminal multiplexer: the control
//! connection (JSON requests, responses and events) and data connections (a block's output
//! stream), at control API revision 7. No async runtime — background work is `std::thread` plus
//! `std::sync::mpsc`.

mod control;
mod data;
mod endpoint;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod wire;

pub use control::{ClientInfo, ControlClient, Event};
pub use data::DataStream;
pub use endpoint::Endpoint;

/// The control connection's WebSocket subprotocol.
pub const CONTROL_PROTOCOL: &str = "rex.control.v1";
/// The terminal data stream's WebSocket subprotocol.
pub const DATA_PROTOCOL: &str = "rex.data.terminal.v1";
/// The control API revision this client speaks.
pub const API_REVISION: u64 = 7;

/// How often the server pings every connection, control and data alike.
const SERVER_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);
/// How long the server waits for each ping's answer.
const SERVER_PING_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Everything that can go wrong talking to a Rex server.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RexError {
    /// The server's socket or host couldn't be reached.
    #[error("Rex server unreachable")]
    Unreachable,
    /// A data handshake was refused with this REJECT reason.
    #[error("Rex refused the data connection (reason {0:#04x})")]
    Rejected(u8),
    /// A method call failed; branch on `code`, falling back to `status` for a code you don't know.
    #[error("{message} ({code})")]
    Api {
        code: String,
        message: String,
        status: u16,
    },
    /// The connection closed with this WebSocket status (1006 when it dropped without a close frame).
    #[error("Rex connection closed ({0})")]
    Closed(u16),
    /// No answer in time.
    #[error("Rex request timed out")]
    Timeout,
    /// The peer broke the protocol, or a handshake failed.
    #[error("Rex protocol error: {0}")]
    Protocol(String),
}

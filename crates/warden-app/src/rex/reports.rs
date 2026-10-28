//! Agent reports: what a relay forwards when the program in its block announces how to resume
//! itself (the `warden1;resume=` APC, caught by `rex_client::wire::StreamDemux`).
//!
//! Relays connect to one Unix socket owned by warden-app and write a JSON line per report:
//! `{"session":…,"block":…,"resume":…}`. [`listen`] turns those lines into [`AgentReport`]s.

// `listen` has no caller until the hub consumes reports.
#![cfg_attr(not(test), allow(dead_code))]

use std::io::{self, BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use serde_json::Value;
use tauri::{AppHandle, Manager};

/// The socket's file name in the app config dir.
const SOCKET_NAME: &str = "agent-reports.sock";

/// Where warden-app listens for reports, `None` without an app config dir. The path stays under
/// macOS's 104-byte `sun_path` limit only for short home paths; a longer one can't be bound or
/// connected to, so panes then report nowhere.
pub fn socket_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join(SOCKET_NAME))
}

/// A block's program reporting the command that resumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    pub session_id: String,
    pub block_id: String,
    pub resume: String,
}

/// Binds `path` (replacing a stale socket file) and delivers each report a relay writes to it.
/// Connections are served on their own threads; a line that isn't a report is skipped. The
/// listener ends when the receiver is dropped.
pub fn listen(path: &Path) -> io::Result<mpsc::Receiver<AgentReport>> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("warden-agent-reports".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let tx = tx.clone();
                let _ = thread::Builder::new()
                    .name("warden-agent-report".into())
                    .spawn(move || serve(conn, &tx));
            }
        })?;
    Ok(rx)
}

fn serve(conn: UnixStream, tx: &mpsc::Sender<AgentReport>) {
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { return };
        if let Some(report) = parse(&line) {
            if tx.send(report).is_err() {
                return;
            }
        }
    }
}

fn parse(line: &str) -> Option<AgentReport> {
    let v: Value = serde_json::from_str(line).ok()?;
    let field = |k: &str| v.get(k)?.as_str().map(str::to_owned);
    Some(AgentReport {
        session_id: field("session")?,
        block_id: field("block")?,
        resume: field("resume")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    fn report(session: &str, resume: &str) -> AgentReport {
        AgentReport {
            session_id: session.into(),
            block_id: "block:1".into(),
            resume: resume.into(),
        }
    }

    #[test]
    fn delivers_each_line_and_skips_malformed_ones() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("r.sock");
        let rx = listen(&sock).unwrap();
        let mut a = UnixStream::connect(&sock).unwrap();
        a.write_all(b"not json\n{\"session\":\"s1\",\"block\":\"block:1\",\"resume\":\"claude --resume A\"}\n{\"session\":\"s1\"}\n").unwrap();
        let mut b = UnixStream::connect(&sock).unwrap();
        b.write_all(
            b"{\"session\":\"s2\",\"block\":\"block:1\",\"resume\":\"claude --resume B\"}\n",
        )
        .unwrap();
        let mut got = vec![
            rx.recv_timeout(WAIT).unwrap(),
            rx.recv_timeout(WAIT).unwrap(),
        ];
        got.sort_by(|x, y| x.session_id.cmp(&y.session_id));
        assert_eq!(
            got,
            vec![
                report("s1", "claude --resume A"),
                report("s2", "claude --resume B")
            ]
        );
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn replaces_a_stale_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("r.sock");
        std::fs::write(&sock, b"stale").unwrap();
        let rx = listen(&sock).unwrap();
        let mut c = UnixStream::connect(&sock).unwrap();
        c.write_all(b"{\"session\":\"s\",\"block\":\"block:1\",\"resume\":\"x\"}\n")
            .unwrap();
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), report("s", "x"));
    }
}

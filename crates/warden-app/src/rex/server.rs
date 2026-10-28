//! Turning a config `server` value into an endpoint, and starting this Mac's Rex server when it
//! isn't running.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rex_client::{ControlClient, Endpoint, RexError};
use serde_json::{json, Value};

/// The config `server` value naming this Mac's Rex server.
pub const LOCAL: &str = "local";

/// Rex.app's bundle identifier, which Launch Services finds wherever the app is installed.
const REX_BUNDLE_ID: &str = "com.superlogical.rex";

/// The `rex` CLI inside Rex.app.
const REX_CLI: &str = "Contents/Helpers/Rex Server.rex/Contents/MacOS/rex";

/// How long autostart may take. It runs on the path that opens a tab, which must answer within
/// 3 s even when Rex can't be started.
const AUTOSTART_DEADLINE: Duration = Duration::from_millis(2500);

/// `"local"` is this Mac's socket; anything else is a host label from the local server's
/// `hosts.list` (matched ignoring case, as Rex does), resolved to its first endpoint warden can
/// connect to.
pub fn resolve(server: &str, local: &ControlClient) -> Result<Endpoint, RexError> {
    if server == LOCAL {
        return Ok(Endpoint::local());
    }
    let list = local.call("hosts.list", json!({}))?;
    let host = list["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|h| {
            h["label"]
                .as_str()
                .is_some_and(|l| l.eq_ignore_ascii_case(server))
        })
        .ok_or_else(|| RexError::Api {
            code: "not_found".into(),
            message: format!("no host labelled {server:?} in Rex's host list"),
            status: 404,
        })?;
    host["endpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(host_endpoint)
        .ok_or_else(|| {
            RexError::Protocol(format!("host {server:?} has no endpoint warden can reach"))
        })
}

/// A `hosts.list` endpoint (any form `rex -S` accepts) as an [`Endpoint`]: `unix:` and
/// `https://` forms parse as they are, and a bare `host[:port]` is HTTPS. Other schemes are
/// skipped.
fn host_endpoint(s: &str) -> Option<Endpoint> {
    if s.starts_with("unix:") || s.starts_with("https://") {
        s.parse().ok()
    } else if s.contains("://") {
        None
    } else {
        format!("https://{s}").parse().ok()
    }
}

/// Start this Mac's Rex server by running `rex server status` from Rex.app's bundle: the CLI
/// starts a missing local server by default. Bounded by [`AUTOSTART_DEADLINE`].
pub fn autostart_local() -> Result<(), RexError> {
    let cli = rex_app()?.join(REX_CLI);
    if !cli.is_file() {
        return Err(not_installed());
    }
    let child = Command::new(&cli)
        .args(["server", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| RexError::Protocol(format!("couldn't run {}: {e}", cli.display())))?;
    wait_bounded(child, AUTOSTART_DEADLINE);
    Ok(())
}

/// Where Launch Services has Rex.app. Asked through `NSWorkspace`, not `mdfind`: Spotlight's
/// index can be switched off, and then `mdfind` finds nothing while Launch Services still knows.
fn rex_app() -> Result<PathBuf, RexError> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;
    // SAFETY: plain Launch Services queries; NSWorkspace is safe to use from any thread.
    let path = unsafe {
        NSWorkspace::sharedWorkspace()
            .URLForApplicationWithBundleIdentifier(&NSString::from_str(REX_BUNDLE_ID))
            .and_then(|url| url.path())
    };
    path.map(|p| PathBuf::from(p.to_string()))
        .ok_or_else(not_installed)
}

fn not_installed() -> RexError {
    RexError::Api {
        code: "not_installed".into(),
        message: "Rex isn't installed on this Mac".into(),
        status: 0,
    }
}

/// Wait for `child` until `deadline` passes, then kill it. The server it starts is its own
/// process, so killing a slow CLI leaves a starting server running.
fn wait_bounded(mut child: std::process::Child, deadline: Duration) {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_endpoint_forms() {
        assert_eq!(
            host_endpoint("remote-host:7297"),
            Some(Endpoint::Https {
                host: "remote-host".into(),
                port: 7297
            })
        );
        assert_eq!(
            host_endpoint("unix:/tmp/s.sock"),
            Some(Endpoint::Unix("/tmp/s.sock".into()))
        );
        assert_eq!(host_endpoint("ssh://remote-host"), None);
    }

    /// Manual: finds Rex.app through Launch Services and runs its CLI (`cargo test -p warden-app
    /// autostart -- --ignored`). Against a running server this only reports its status.
    #[test]
    #[ignore = "needs Rex installed"]
    fn autostart_finds_rex() {
        assert!(rex_app().is_ok());
        assert_eq!(autostart_local(), Ok(()));
    }
}

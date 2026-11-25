//! `orbitd --local --stdio`: the far end of a remote connection.
//!
//! A client on another computer runs this over SSH (or `wsl.exe`). It makes
//! sure this computer's background host runs on the data folder, starting
//! it detached when it does not (so it outlives the SSH session, and the
//! next connection attaches to the same host, its agents and terminals),
//! then relays length-prefixed frames between stdin/stdout and the host's
//! local transport until either side closes.
//!
//! The client does not know the host's token: whoever can run programs as
//! this user over SSH already has everything the token protects. The relay
//! puts the token into the client's first frame (a `Hello` or a control
//! request) itself.
//!
//! A host of another protocol (this binary was just updated) is restarted
//! into this one right away when nothing runs on it; otherwise it is asked
//! to restart once its work is done, and the client is told so.

use std::path::Path;
use std::time::{Duration, Instant};

use orbit_proto::{ClientMsg, RestartWhen};

use crate::endpoint::{self, EndpointInfo};
use crate::host::{self, HostError};
use crate::transport::{self, FrameReader, FrameWriter};

/// Runs the relay; returns the process exit code. Errors go to stderr,
/// which the client shows.
pub fn run(data_dir: &Path, binary: &Path) -> i32 {
    let info = match ensure_host(data_dir, binary) {
        Ok(info) => info,
        Err(why) => {
            eprintln!("orbitd: {why}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("orbitd: {err}");
            return 1;
        }
    };
    let stdin = FrameReader::new(Box::new(tokio::io::stdin()));
    let stdout = FrameWriter::new(Box::new(tokio::io::stdout()));
    match runtime.block_on(relay(&info, stdin, stdout)) {
        Ok(()) => 0,
        Err(why) => {
            eprintln!("orbitd: {why}");
            1
        }
    }
}

/// The running host's endpoint, after starting or restarting it as needed.
fn ensure_host(data_dir: &Path, binary: &Path) -> Result<EndpointInfo, String> {
    let mut child = None;
    let started = Instant::now();
    loop {
        match host::ensure_running(data_dir, Some(binary), &mut child) {
            Ok(info) => return Ok(info),
            Err(HostError::Outdated(old)) => {
                let status = host::control(&old, "status", None)
                    .map_err(|e| format!("The host here runs Orbit {} and didn’t answer: {e}", old.version))?;
                if let Some(work) = status.summary() {
                    let _ = host::control(&old, "restart", Some(RestartWhen::WhenIdle));
                    return Err(format!(
                        "The host here runs Orbit {} and {work}. It restarts into Orbit {} when they finish; connect again then.",
                        old.version,
                        env!("CARGO_PKG_VERSION")
                    ));
                }
                host::control(&old, "restart", Some(RestartWhen::Now)).map_err(|e| format!("Couldn’t restart the old host: {e}"))?;
                while endpoint::process_alive(old.pid) {
                    if started.elapsed() > Duration::from_secs(20) {
                        return Err(format!("The old host (Orbit {}) didn’t stop.", old.version));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

/// The client's first frame with the host's token in it.
fn with_token(first: &[u8], token: &str) -> Result<Vec<u8>, String> {
    if host::is_control(first) {
        let mut request: host::ControlRequest = serde_json::from_slice(first).map_err(|e| format!("bad control request: {e}"))?;
        request.token = token.to_owned();
        return serde_json::to_vec(&request).map_err(|e| e.to_string());
    }
    match orbit_proto::decode::<ClientMsg>(first) {
        Ok(ClientMsg::Hello { version, epoch, since, .. }) => {
            Ok(orbit_proto::encode(&ClientMsg::Hello { version, token: token.to_owned(), epoch, since }))
        }
        _ => Err(format!(
            "this orbitd speaks protocol {}; the client sent something else first (update Orbit on one side)",
            orbit_proto::PROTOCOL_VERSION
        )),
    }
}

/// Relays frames between the client (`input`, `output`) and the host.
pub async fn relay(info: &EndpointInfo, mut input: FrameReader, mut output: FrameWriter) -> Result<(), String> {
    let Some(first) = input.next().await.map_err(|e| e.to_string())? else { return Ok(()) };
    let first = with_token(&first, &info.token)?;
    let (mut from_host, mut to_host) =
        transport::connect(info.client_url()).await.map_err(|e| format!("Couldn’t reach the host here: {e}"))?;
    to_host.send(&first).await.map_err(|e| e.to_string())?;
    let up = async {
        while let Some(frame) = input.next().await? {
            to_host.send(&frame).await?;
        }
        to_host.close().await;
        Ok::<(), std::io::Error>(())
    };
    let down = async {
        while let Some(frame) = from_host.next().await? {
            output.send(&frame).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    // Either side closing ends the relay; the host stays.
    let result = tokio::select! {
        r = up => r,
        r = down => r,
    };
    match result {
        Ok(()) => Ok(()),
        // The client went away mid-write (SSH dropped): not an error here.
        Err(err) if matches!(err.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_filled_in() {
        let hello = orbit_proto::encode(&ClientMsg::Hello { version: 13, token: String::new(), epoch: 4, since: 9 });
        let out = with_token(&hello, "secret").unwrap();
        assert_eq!(
            orbit_proto::decode::<ClientMsg>(&out).unwrap(),
            ClientMsg::Hello { version: 13, token: "secret".into(), epoch: 4, since: 9 }
        );
        let control = br#"{"token":"","op":"status"}"#;
        let out: host::ControlRequest = serde_json::from_slice(&with_token(control, "secret").unwrap()).unwrap();
        assert_eq!((out.token.as_str(), out.op.as_str()), ("secret", "status"));
        assert!(with_token(b"\xff\xff", "secret").is_err());
    }
}

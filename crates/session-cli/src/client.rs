//! session-cli client: connect to the daemon and speak the wire protocol.
//!
//! This module owns the client side of a daemon connection: resolving the
//! socket path from [`Config`], opening an interprocess local stream, and
//! completing the `Hello` handshake. On success the caller gets a [`Client`],
//! which is a connected local stream wrapped in a protocol reader/writer —
//! [`Client::send`] writes [`ClientToDaemon`] frames, [`Client::recv`] reads
//! [`DaemonToClient`] frames (NDJSON, one frame per line).
//!
//! The daemon (owning side) lives in `session_core::daemon`. Both ends use the
//! same frame types ([`ClientToDaemon`]/[`DaemonToClient`]) and the same
//! [`interprocess`] local-stream backend (unix socket on macOS/Linux, named
//! pipe on Windows), so this client can join any daemon the engine starts.

use interprocess::local_socket::{
    tokio::prelude::*,
    tokio::Stream as TokioStream,
    GenericFilePath, ToFsName,
};
use session_core::config::Config;
use session_core::protocol::{check_version, ClientToDaemon, DaemonToClient, PROTOCOL_VERSION};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long to wait for the daemon's `Hello` during the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected, handshaked client connection to the daemon.
///
/// It owns the split halves of the local socket and exposes a protocol
/// reader/writer over them (the "connected local stream + protocol
/// reader/writer" this card delivers). All payloads are carried as frames, so
/// a consumer never touches the raw socket bytes directly — it uses
/// [`send`](Client::send)/[`recv`](Client::recv).
pub struct Client {
    reader: BufReader<tokio::io::ReadHalf<TokioStream>>,
    writer: tokio::io::WriteHalf<TokioStream>,
}

/// Why [`Client::connect`] failed. The launcher matches on [`ConnectError::Io`]
/// to decide whether to start a fresh daemon (socket absent / a stale corpse /
/// refused), whereas a version or handshake error means a daemon IS present but
/// uncooperative and should not be masked by starting a second one.
#[derive(Debug)]
pub enum ConnectError {
    /// The socket could not be connected: no file, a stale corpse socket, or
    /// the peer refused. This is the "no live daemon" signal.
    Io(String),
    /// Connected, but the peer did not complete the `Hello` handshake.
    Handshake(String),
    /// The peer advertises a different protocol version than we speak.
    Version(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Io(msg) => write!(f, "cannot connect to daemon socket: {msg}"),
            ConnectError::Handshake(msg) => write!(f, "daemon handshake failed: {msg}"),
            ConnectError::Version(msg) => write!(f, "daemon protocol version mismatch: {msg}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl Client {
    /// Connect to the daemon at `config.socket_path` and complete the `Hello`
    /// handshake (advertise our version, verify the daemon's).
    pub async fn connect(config: &Config) -> Result<Self, ConnectError> {
        // Resolve the socket path into an interprocess filesystem socket name.
        let name = config
            .socket_path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| {
                ConnectError::Io(format!("invalid socket path {}: {e}", config.socket_path.display()))
            })?;

        let stream = TokioStream::connect(name)
            .await
            .map_err(|e| ConnectError::Io(e.to_string()))?;

        Self::handshake(stream).await
    }

    /// Actually perform the `Hello` exchange over an established stream.
    async fn handshake(stream: TokioStream) -> Result<Self, ConnectError> {
        // Split into independent read/write halves so the reader can wait on
        // the daemon while the writer is at the caller's disposal. Use tokio's
        // split (returns nameable ReadHalf/WriteHalf) over interprocess::Stream.
        let (read_half, write_half) = tokio::io::split(stream);
        let mut writer = write_half;
        let mut reader = BufReader::new(read_half);

        // Advertise our protocol version.
        let hello = ClientToDaemon::Hello {
            version: PROTOCOL_VERSION,
        };
        let frame = hello
            .to_frame()
            .map_err(|e| ConnectError::Handshake(e.to_string()))?;
        writer
            .write_all(&frame)
            .await
            .map_err(|e| ConnectError::Handshake(format!("failed to send Hello: {e}")))?;

        // Expect the daemon's Hello (with a timeout so a silent peer doesn't hang us).
        let mut line = String::new();
        let n = tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.read_line(&mut line))
            .await
            .map_err(|_| ConnectError::Handshake("timed out waiting for daemon Hello".into()))?
            .map_err(|e| ConnectError::Handshake(format!("failed to read daemon Hello: {e}")))?;
        if n == 0 {
            return Err(ConnectError::Handshake(
                "daemon closed the stream during handshake".into(),
            ));
        }

        match DaemonToClient::from_frame(line.as_bytes()) {
            Ok(DaemonToClient::Hello { daemon_version }) => {
                check_version(daemon_version).map_err(|e| ConnectError::Version(e.to_string()))?;
                Ok(Self { reader, writer })
            }
            Ok(other) => Err(ConnectError::Handshake(format!(
                "expected Hello, received {other:?}"
            ))),
            Err(e) => Err(ConnectError::Handshake(format!("bad Hello frame: {e}"))),
        }
    }

    /// Send one protocol frame to the daemon.
    pub async fn send(&mut self, frame: ClientToDaemon) -> anyhow::Result<()> {
        let bytes = frame.to_frame()?;
        self.writer.write_all(&bytes).await?;
        Ok(())
    }

    /// Read the next protocol frame from the daemon.
    ///
    /// Returns `Ok(None)` when the daemon closes the stream (EOF) — callers
    /// treat that as "attached session ended / daemon gone".
    pub async fn recv(&mut self) -> anyhow::Result<Option<DaemonToClient>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let frame = DaemonToClient::from_frame(line.as_bytes())?;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use session_core::config::Config;
    use session_core::protocol::{ClientToDaemon, DaemonToClient};
    use std::time::Duration;
    use tempfile::tempdir;

    fn temp_config(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::default();
        cfg.socket_dir = dir.path().to_path_buf();
        cfg.socket_path = dir.path().join("session.sock");
        cfg
    }

    // GATE: launch the daemon on a temp socket, connect (Hello round-trip), run
    // a real List->Listing exchange, then shut the daemon down.
    #[tokio::test]
    async fn gate_client_connect_and_hello_roundtrip() {
        let dir = tempdir().unwrap();
        let cfg = temp_config(&dir);

        // Start the daemon in-process (the same `session_core::daemon::run` the
        // `session --daemon` binary wires to) on the temp socket.
        let daemon_cfg = cfg.clone();
        let daemon = tokio::spawn(async move {
            let _ = session_core::daemon::run(daemon_cfg).await;
        });

        // Connect is the Hello round-trip: it sends Hello, reads Hello, and
        // checks the daemon's version. Retry until the daemon has bound.
        let mut client = None;
        for _ in 0..250 {
            if let Ok(c) = Client::connect(&cfg).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut client = client.expect("daemon did not become reachable");

        // Prove the connection is live beyond the handshake: List -> Listing.
        client.send(ClientToDaemon::List).await.unwrap();
        let frame = client
            .recv()
            .await
            .unwrap()
            .expect("expected a Listing frame");
        assert!(
            matches!(&frame, DaemonToClient::Listing { sessions } if sessions.is_empty()),
            "fresh daemon should list nothing, got {frame:?}"
        );

        daemon.abort();
    }

    // Connecting to a socket that doesn't exist must report Io (the launcher's
    // signal to spawn a daemon), not a handshake/version error.
    #[tokio::test]
    async fn connect_to_absent_socket_is_io() {
        let dir = tempdir().unwrap();
        let cfg = temp_config(&dir);
        // No daemon running; the socket path was never created.
        let err = match Client::connect(&cfg).await {
            Ok(_) => panic!("connect to an absent socket must fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ConnectError::Io(_)),
            "absent socket should be ConnectError::Io, got {err:?}"
        );
    }
}

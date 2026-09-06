//! Daemon server: the long-lived session owner and protocol endpoint.
//!
//! The daemon binds a **per-user local socket** and owns every session in a
//! [`Registry`]. The CLI (or the Shell proxy) connects to it as a client, does
//! a `Hello` handshake, then sends [`ClientToDaemon`] frames (`Attach`,
//! `AttachCreate`, `Input`, `Resize`, `Detach`, `List`, `Kill`, `KillAll`) and
//! receives [`DaemonToClient`] frames (`Hello`, `Attached`, `Output`, `Killed`,
//! `Listing`, `Error`).
//!
//! # Socket backend (decided)
//!
//! The bootstrap spike resolved the cross-platform decision: **interprocess**,
//! configured as a *filesystem local socket* — a real Unix socket file on
//! macOS/Linux, a named pipe on Windows. It is used here with the **tokio**
//! feature (`interprocess::local_socket::tokio`), so the listener and each
//! accepted stream are first-class `AsyncRead`/`AsyncWrite` values driven by the
//! reactor (no blocking threads). The socket file lives at
//! [`Config::socket_path`], in a per-user directory; on Unix a stale file left
//! by a crashed instance is re-taken automatically via [`ListenerOptions`]'s
//! name-reclamation plus `try_overwrite`, and the on-disk file is cleaned up
//! when the listener drops.
//!
//! # Single-instance guard
//!
//! Before binding, the daemon probes the socket: if something is already
//! listening AND answers a `Hello` handshake with a matching protocol version,
//! another daemon is running on this socket — so this process exits without
//! starting a second one. Otherwise (no socket, a corpse socket whose listener
//! is gone, or a peer that doesn't speak the protocol) the daemon takes over
//! and becomes the singleton owner.
//!
//! # Per-connection model
//!
//! Each accepted connection is handled by its own task:
//!
//! * the read path (`BufReader<RecvHalf>`) parses NDJSON [`ClientToDaemon`]
//!   frames and routes them against the shared `Arc<Mutex<Registry>>`;
//! * the write path is a dedicated task owning `SendHalf`, consuming a channel
//!   of serialised [`DaemonToClient`] frames from the reader task and from any
//!   output forwarder;
//! * once a client attaches, a forwarder task subscribes to that session's
//!   [`broadcast::Receiver<SessionEvent>`] and streams `Output` bytes to the
//!   writer channel, so the client is kept in sync without the reader task
//!   blocking on the socket for every keystroke.
//!
//! The `ClientId` is synthesised per connection (a fresh UUID) so the session's
//! primary-client winsize authority can be driven correctly: the first attached
//! connection is the primary, and a resize from any client is applied only when
//! it holds the primary seat.
//!
//! # Working directory
//!
//! Pseudo-terminals are spawned in the daemon's current directory. Because the
//! daemon is decoupled from whatever directory the launching CLI happened to be
//! in, the daemon guarantees a usable working directory at startup (falling back
//! to the user's home directory if the inherited one is not usable), so session
//! children always have somewhere sane to start.

use crate::config::Config;
use crate::protocol::{
    check_version, ClientToDaemon, DaemonToClient, SessionInfo as WireSessionInfo, PROTOCOL_VERSION,
};
use crate::registry::Registry;
use crate::session::{ClientId, SessionEvent};
use anyhow::Result;
use interprocess::local_socket::{
    tokio::prelude::*,
    tokio::Stream as TokioStream,
    GenericFilePath, ListenerOptions, ToFsName,
};
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError as BroadcastRecvError;
use tokio::sync::mpsc;
use uuid::Uuid;

/// How long the single-instance probe waits for a live daemon to answer a
/// `Hello` before concluding that no daemon is running.
const PROBE_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);
/// How long the probe will wait to connect at all.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// The long-lived session daemon: owns the socket + registry.
pub struct Daemon {
    config: Config,
}

impl Daemon {
    /// A daemon configured with `config`.
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Run the daemon loop and block until a fatal error (or, when a daemon is
    /// already running on the same socket, return immediately).
    pub async fn run(self) -> Result<()> {
        run(self.config).await
    }
}

/// Run the daemon loop on `config`'s socket. Blocks forever serving clients;
/// returns `Ok(())` only when another daemon is already alive on the socket.
pub async fn run(config: Config) -> Result<()> {
    std::fs::create_dir_all(&config.socket_dir)?;
    ensure_working_dir()?;

    // Single-instance guard: if a live daemon already answers a Hello on this
    // socket, we must NOT start a second one.
    if probe_alive(&config.socket_path).await? {
        log::info!(
            "session daemon already running on {}; exiting",
            config.socket_path.display()
        );
        return Ok(());
    }

    // Bind. `try_overwrite` + interprocess name reclamation let us re-take a
    // stale socket file left by a previously-crashed instance.
    let socket_path = config.socket_path.clone();
    let name = socket_path
        .as_path()
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| anyhow::anyhow!("bad socket path {}: {e}", socket_path.display()))?;
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_tokio()
        .map_err(|e| anyhow::anyhow!("failed to bind socket {}: {e}", socket_path.display()))?;

    log::info!("session daemon listening on {}", socket_path.display());

    let registry = Arc::new(Mutex::new(Registry::from_config(&config)));

    loop {
        match listener.accept().await {
            Ok(stream) => {
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, registry).await {
                        log::warn!("session daemon connection error: {e}");
                    }
                });
            }
            Err(e) => {
                log::warn!("session daemon accept error: {e}");
            }
        }
    }
}

/// Guarantee the daemon has a usable working directory for session children.
///
/// If the inherited (launching) directory is not usable — a common case for a
/// process that has outlived the parent's chdir, been daemonized, or whose cwd
/// was removed — fall back to the user's home directory so spawned ptys always
/// have a sane start location.
fn ensure_working_dir() -> Result<()> {
    if std::env::current_dir().is_err() {
        if let Some(home) = dirs::home_dir() {
            std::env::set_current_dir(&home)?;
            log::info!("session daemon cwd unusable; using {}", home.display());
        }
    }
    Ok(())
}

/// Return `true` if a live daemon answers a `Hello` handshake on `socket_path`.
///
/// This is the single-instance probe: it connects, sends a `Hello`, and asks
/// for a `Hello` response. A matching response means a daemon is already
/// running; anything else (connect failure, wrong peer, no response) means the
/// caller should become the daemon.
async fn probe_alive(socket_path: &Path) -> Result<bool> {
    let name = match socket_path.to_fs_name::<GenericFilePath>() {
        Ok(n) => n,
        Err(_) => return Ok(false),
    };

    // Connect (with a short cap so a dead/stale socket doesn't hang the probe).
    let mut conn = match tokio::time::timeout(PROBE_CONNECT_TIMEOUT, TokioStream::connect(name)).await {
        Ok(Ok(c)) => c,
        // AddrNotAvailable (no socket), ConnectionRefused (corpse socket), or a
        // timeout all mean "no live daemon" -> we take over.
        _ => return Ok(false),
    };

    let hello = ClientToDaemon::Hello {
        version: PROTOCOL_VERSION,
    };
    let hello_frame = match hello.to_frame() {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    if conn.write_all(&hello_frame).await.is_err() {
        return Ok(false);
    }

    let mut reader = BufReader::new(&mut conn);
    let mut line = String::new();
    match tokio::time::timeout(PROBE_HANDSHAKE_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(n)) if n > 0 => {
            if let Ok(DaemonToClient::Hello { .. }) = DaemonToClient::from_frame(line.as_bytes()) {
                Ok(true)
            } else {
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

/// Serve one client connection: handshake, then route frames on the registry.
async fn handle_connection(stream: TokioStream, registry: Arc<Mutex<Registry>>) -> Result<()> {
    let client_id: ClientId = Uuid::new_v4().to_string();
    let (read_half, write_half) = stream.split();

    // Dedicated writer task: every response (Hello, Attached, Output, Listing,
    // Error) flows through this channel so the reader/forwarders never fight
    // over the write half.
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_handle = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(frame) = writer_rx.recv().await {
            if write_half.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    let mut reader = BufReader::new(read_half);

    // ---- Handshake: the first frame MUST be Hello with a matching version.
    let first = read_frame(&mut reader).await?;
    let Some(ClientToDaemon::Hello { version }) = first else {
        let _ = send_frame(
            &writer_tx,
            DaemonToClient::Error {
                message: "expected Hello".into(),
            },
        )
        .await;
        writer_handle.abort();
        return Ok(());
    };
    if let Err(e) = check_version(version) {
        let _ = send_frame(&writer_tx, DaemonToClient::Error { message: e.to_string() }).await;
        writer_handle.abort();
        return Ok(());
    }
    send_frame(
        &writer_tx,
        DaemonToClient::Hello {
            daemon_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    // Which session this client is currently attached to (if any).
    let mut attached: Option<Uuid> = None;
    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;

    // ---- Main frame loop.
    loop {
        let frame = read_frame(&mut reader).await?;
        let Some(frame) = frame else { break }; // client closed / EOF

        match frame {
            ClientToDaemon::Hello { .. } => {
                let _ = send_frame(
                    &writer_tx,
                    DaemonToClient::Error {
                        message: "unexpected Hello (already handshaked)".into(),
                    },
                )
                .await;
            }

            ClientToDaemon::List => {
                let sessions = registry
                    .lock()
                    .unwrap()
                    .list()
                    .into_iter()
                    .map(to_wire_session_info)
                    .collect();
                send_frame(&writer_tx, DaemonToClient::Listing { sessions }).await?;
            }

            ClientToDaemon::Attach { session_id } => {
                let Some(uuid) = parse_session_id(&session_id) else {
                    let _ = send_frame(
                        &writer_tx,
                        DaemonToClient::Error {
                            message: format!("invalid session id: {session_id}"),
                        },
                    )
                    .await;
                    continue;
                };
                // Bind to a `let` so the registry lock guard is dropped BEFORE
                // the `.await`s in the arms below (a guard held across an await
                // would make the future non-Send).
                let attach_result = registry.lock().unwrap().attach(&uuid, client_id.clone());
                match attach_result {
                    Ok(handle) => {
                        // Stop forwarding from any previously-attached session.
                        if let Some(h) = forwarder.take() {
                            h.abort();
                        }
                        attached = Some(uuid);
                        send_frame(
                            &writer_tx,
                            DaemonToClient::Attached {
                                session_id: uuid.to_string(),
                                redraw_bytes: handle.redraw,
                            },
                        )
                        .await?;
                        // Stream incremental output from this session to the client.
                        forwarder = Some(spawn_output_forwarder(writer_tx.clone(), handle.receiver));
                    }
                    Err(e) => {
                        let _ = send_frame(
                            &writer_tx,
                            DaemonToClient::Error {
                                message: e.to_string(),
                            },
                        )
                        .await;
                    }
                }
            }

            ClientToDaemon::AttachCreate { session_id, cmd } => {
                // The registry assigns a fresh id on create (generated Uuids);
                // the requested id is carried for round-trip symmetry but the
                // created session's real id is what we report back.
                let argv = cmd.as_deref().map(split_command);
                // Bind to `let`s so each lock guard is dropped before any await.
                let create_result = registry.lock().unwrap().create_session(argv);
                match create_result {
                    Ok(created) => {
                        if let Some(h) = forwarder.take() {
                            h.abort();
                        }
                        attached = Some(created);
                        let attach_result =
                            registry.lock().unwrap().attach(&created, client_id.clone());
                        match attach_result {
                            Ok(handle) => {
                                send_frame(
                                    &writer_tx,
                                    DaemonToClient::Attached {
                                        session_id: created.to_string(),
                                        redraw_bytes: handle.redraw,
                                    },
                                )
                                .await?;
                                forwarder =
                                    Some(spawn_output_forwarder(writer_tx.clone(), handle.receiver));
                            }
                            Err(e) => {
                                let _ = send_frame(
                                    &writer_tx,
                                    DaemonToClient::Error {
                                        message: e.to_string(),
                                    },
                                )
                                .await;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = send_frame(
                            &writer_tx,
                            DaemonToClient::Error {
                                message: format!(
                                    "failed to create session{}: {e}",
                                    if session_id.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" {session_id}")
                                    }
                                ),
                            },
                        )
                        .await;
                    }
                }
            }

            ClientToDaemon::Input { bytes } => {
                let Some(uuid) = attached else {
                    let _ = send_frame(
                        &writer_tx,
                        DaemonToClient::Error {
                            message: "not attached to a session".into(),
                        },
                    )
                    .await;
                    continue;
                };
                let result = registry
                    .lock()
                    .unwrap()
                    .find(&uuid)
                    .map(|s| s.write(&bytes, &client_id));
                if let Some(Err(e)) = result {
                    let _ = send_frame(&writer_tx, DaemonToClient::Error { message: e.to_string() }).await;
                }
            }

            ClientToDaemon::Resize { rows, cols } => {
                let Some(uuid) = attached else {
                    let _ = send_frame(
                        &writer_tx,
                        DaemonToClient::Error {
                            message: "not attached to a session".into(),
                        },
                    )
                    .await;
                    continue;
                };
                let result = registry
                    .lock()
                    .unwrap()
                    .find_mut(&uuid)
                    .map(|s| s.set_size(rows, cols, &client_id));
                if let Some(Err(e)) = result {
                    let _ = send_frame(&writer_tx, DaemonToClient::Error { message: e.to_string() }).await;
                }
            }

            ClientToDaemon::Detach => {
                if let Some(h) = forwarder.take() {
                    h.abort();
                }
                if let Some(uuid) = attached.take() {
                    if let Some(s) = registry.lock().unwrap().find_mut(&uuid) {
                        s.detach(&client_id);
                    }
                }
            }

            ClientToDaemon::Kill { session_id } => {
                let Some(uuid) = parse_session_id(&session_id) else {
                    let _ = send_frame(
                        &writer_tx,
                        DaemonToClient::Error {
                            message: format!("invalid session id: {session_id}"),
                        },
                    )
                    .await;
                    continue;
                };
                // Bind to a `let` so the lock guard is dropped before the awaits.
                let kill_result = registry.lock().unwrap().kill(&uuid);
                match kill_result {
                    Ok(()) => {
                        if attached == Some(uuid) {
                            attached = None;
                            if let Some(h) = forwarder.take() {
                                h.abort();
                            }
                        }
                        send_frame(&writer_tx, DaemonToClient::Killed { session_id })
                            .await?;
                    }
                    Err(e) => {
                        let _ = send_frame(
                            &writer_tx,
                            DaemonToClient::Error {
                                message: e.to_string(),
                            },
                        )
                        .await;
                    }
                }
            }

            ClientToDaemon::KillAll => {
                registry.lock().unwrap().kill_all();
                attached = None;
                if let Some(h) = forwarder.take() {
                    h.abort();
                }
                let _ = send_frame(
                    &writer_tx,
                    DaemonToClient::Error {
                        message: "killed all sessions".into(),
                    },
                )
                .await;
            }
        }
    }

    // Client disconnected: detach from the session (so the primary-client
    // winsize authority is promoted cleanly) and tear down our tasks.
    if let Some(uuid) = attached {
        if let Some(s) = registry.lock().unwrap().find_mut(&uuid) {
            s.detach(&client_id);
        }
    }
    if let Some(h) = forwarder.take() {
        h.abort();
    }
    writer_handle.abort();
    Ok(())
}

/// Stream a session's broadcast `Output` events to the writer channel until the
/// session exits or the client goes away.
fn spawn_output_forwarder(
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut receiver: tokio::sync::broadcast::Receiver<SessionEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(SessionEvent::Output(bytes)) => {
                    if let Ok(frame) = (DaemonToClient::Output { bytes }).to_frame() {
                        if writer_tx.send(frame).is_err() {
                            break; // writer task gone -> client disconnected
                        }
                    }
                }
                // The session exited; there is no explicit "exit" frame in the
                // protocol, so the forwarding stream simply ends.
                Ok(SessionEvent::Exit(_)) => break,
                // We fell behind (many output chunks). Skip the gap and keep up.
                Err(BroadcastRecvError::Lagged(_)) => continue,
                Err(BroadcastRecvError::Closed) => break,
            }
        }
    })
}

/// Read one NDJSON [`ClientToDaemon`] frame off a buffered stream. `Ok(None)`
/// means the client closed (EOF).
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<ClientToDaemon>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let frame =
        ClientToDaemon::from_frame(line.as_bytes()).map_err(|e| anyhow::anyhow!("bad frame: {e}"))?;
    Ok(Some(frame))
}

/// Serialise and enqueue a [`DaemonToClient`] frame on the writer channel.
async fn send_frame(tx: &mpsc::UnboundedSender<Vec<u8>>, msg: DaemonToClient) -> Result<()> {
    let frame = msg.to_frame().map_err(|e| anyhow::anyhow!("frame error: {e}"))?;
    tx.send(frame).map_err(|_| anyhow::anyhow!("client gone"))?;
    Ok(())
}

/// Parse a protocol session id string into a `Uuid`.
fn parse_session_id(s: &str) -> Option<Uuid> {
    Uuid::parse_str(s).ok()
}

/// Map a registry `SessionInfo` onto the protocol's wire `SessionInfo`.
fn to_wire_session_info(info: crate::registry::SessionInfo) -> WireSessionInfo {
    WireSessionInfo {
        id: info.id.to_string(),
        title: info.title,
        attached: info.attached_count,
    }
}

/// Split a command string into argv (`argv[0]` = program, the rest = args).
///
/// This is a whitespace split; the CLI is responsible for quoting when it
/// builds the `cmd` string from a vector of arguments.
fn split_command(cmd: &str) -> Vec<OsString> {
    cmd.split_whitespace().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config_in(dir: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.socket_dir = dir.to_path_buf();
        cfg.socket_path = dir.join("session.sock");
        cfg
    }

    // Retry `connect` until the daemon has bound and accepts.
    async fn connect_with_retry(path: &Path) -> TokioStream {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match path.to_fs_name::<GenericFilePath>() {
                Ok(name) => match TokioStream::connect(name).await {
                    Ok(s) => return s,
                    Err(_) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(e) => panic!("connect failed after retries: {e}"),
                },
                Err(e) => panic!("bad socket name: {e}"),
            }
        }
    }

    // GATE: start the daemon on a temp socket, send Hello -> Hello, then List
    // -> Listing, asserting the listing arrives.
    #[tokio::test]
    async fn gate_daemon_hello_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_in(dir.path());

        let daemon = tokio::spawn(async move {
            let _ = run(cfg).await;
        });
        // The daemon never returns (infinite accept loop) unless it errors; a
        // return here would mean it detected itself as already-running or
        // failed to bind. Keep the handle so the runtime can cancel it.

        let stream = connect_with_retry(&dir.path().join("session.sock")).await;
        // Move the stream into a buffered reader and write through `get_mut()` so
        // we can interleave reads and writes without a borrow conflict.
        let mut reader = BufReader::new(stream);

        // Hello handshake.
        reader
            .get_mut()
            .write_all(&(ClientToDaemon::Hello {
                version: PROTOCOL_VERSION,
            })
            .to_frame()
            .unwrap())
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        match DaemonToClient::from_frame(line.as_bytes()).unwrap() {
            DaemonToClient::Hello { daemon_version } => {
                assert_eq!(daemon_version, PROTOCOL_VERSION);
            }
            other => panic!("expected Hello, got {other:?}"),
        }

        // List -> an empty listing (no sessions yet).
        reader
            .get_mut()
            .write_all(&ClientToDaemon::List.to_frame().unwrap())
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        match DaemonToClient::from_frame(line.as_bytes()).unwrap() {
            DaemonToClient::Listing { sessions } => {
                assert!(sessions.is_empty(), "fresh daemon should list nothing");
            }
            other => panic!("expected Hello, got {other:?}"),
        }

        // Shut the daemon down.
        daemon.abort();
    }

    // The singleton guard: a second `run` on the same socket must exit `Ok(())`
    // rather than bind a second listener.
    #[tokio::test]
    async fn singleton_second_daemon_exits() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_in(dir.path());

        let first_cfg = cfg.clone();
        let first = tokio::spawn(async move {
            let _ = run(first_cfg).await;
        });

        // Wait until the first daemon is actually listening.
        let _client = connect_with_retry(&dir.path().join("session.sock")).await;

        // A second `run` on the same socket must return Ok(()) quickly.
        let second = run(cfg).await;
        assert!(second.is_ok(), "second daemon should exit Ok(()), got {second:?}");

        first.abort();
    }

    // Round-trip: create a session, attach, feed input, and observe the child's
    // echo come back as an Output frame.
    #[tokio::test]
    async fn create_attach_input_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_in(dir.path());
        let daemon = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        let stream = connect_with_retry(&dir.path().join("session.sock")).await;
        let mut reader = BufReader::new(stream);

        // Hello.
        reader
            .get_mut()
            .write_all(&(ClientToDaemon::Hello {
                version: PROTOCOL_VERSION,
            })
            .to_frame()
            .unwrap())
            .await
            .unwrap();
        // Read the daemon's Hello response.
        let mut hello_line = String::new();
        reader.read_line(&mut hello_line).await.unwrap();
        assert!(matches!(
            DaemonToClient::from_frame(hello_line.as_bytes()).unwrap(),
            DaemonToClient::Hello { .. }
        ));

        // AttachCreate a `cat` session; read the Attached frame (replay).
        let sid = "00000000-0000-0000-0000-000000000001".to_string();
        reader
            .get_mut()
            .write_all(
                &(ClientToDaemon::AttachCreate {
                    session_id: sid.clone(),
                    cmd: Some("cat".into()),
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let DaemonToClient::Attached { session_id: got_sid, redraw_bytes } =
            DaemonToClient::from_frame(line.as_bytes()).unwrap()
        else {
            panic!("expected Attached, got {}", line);
        };
        assert!(Uuid::parse_str(&got_sid).is_ok(), "attached carries a real uuid: {got_sid}");
        assert!(!redraw_bytes.is_empty(), "replay should be non-empty");

        // Send input; `cat` echoes it. Watch for the echo as an Output frame.
        reader
            .get_mut()
            .write_all(
                &(ClientToDaemon::Input {
                    bytes: b"echo-me\n".to_vec(),
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let probe = b"echo-me";
        let mut got = Vec::new();
        while std::time::Instant::now() < deadline {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            match DaemonToClient::from_frame(line.as_bytes()).unwrap() {
                DaemonToClient::Output { bytes } => {
                    got.extend_from_slice(&bytes);
                    if got.windows(probe.len()).any(|w| w == probe) {
                        break;
                    }
                }
                DaemonToClient::Error { message } => panic!("unexpected error: {message}"),
                _ => {}
            }
        }
        assert!(
            got.windows(probe.len()).any(|w| w == probe),
            "expected cat to echo input, got {}",
            String::from_utf8_lossy(&got)
        );

        daemon.abort();
    }
}

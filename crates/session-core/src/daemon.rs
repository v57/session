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
//! Before binding, the daemon takes an exclusive lock on a `.lock` file next to
//! the socket and holds it for its lifetime; if the lock is held, another
//! daemon owns this socket and this process exits. This makes probe-then-bind
//! atomic against a concurrent start. The daemon then also probes the socket:
//! if something is already listening AND answers a `Hello` handshake with a
//! matching protocol version (a daemon from a build without the lock file),
//! this process exits too. Otherwise (no socket, a corpse socket whose listener
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
//!
//! # Passwords
//!
//! A session created with `protect` (see [`crate::password`]) refuses
//! `Attach`, resume via `AttachCreate`, `Kill`, `Rename` and `Cwd` unless the
//! frame carries its password, answering `PasswordRequired` instead; the client
//! asks the user and repeats the request. The check and the operation run under
//! the same registry lock. `KillAll` leaves protected sessions running.

use crate::config::Config;
use crate::password::{Denied, WRONG_PASSWORD_DELAY};
use crate::protocol::{
    check_version, ClientToDaemon, DaemonToClient, SessionInfo as WireSessionInfo, PROTOCOL_VERSION,
};
use crate::registry::Registry;
use crate::session::{ClientId, SessionEvent};
use anyhow::Result;
use interprocess::local_socket::{
    tokio::prelude::*,
    tokio::Stream as TokioStream,
    ListenerOptions,
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
/// How often the background reaper drops sessions whose child process has
/// exited, so the registry never accumulates zombie sessions and `list` stays
/// clean even when nothing touches a finished session.
const REAP_INTERVAL: Duration = Duration::from_millis(200);

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

    // Single-instance guard, part 1: an exclusive lock held for the daemon's
    // whole lifetime. Two daemons racing to start (several clients auto-
    // launching at once) can't both pass the probe below and then overwrite
    // each other's socket; the loser exits here.
    let lock_path = config.socket_path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| anyhow::anyhow!("cannot open lock file {}: {e}", lock_path.display()))?;
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            log::info!(
                "session daemon already running (lock {} held); exiting",
                lock_path.display()
            );
            return Ok(());
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(anyhow::anyhow!("cannot lock {}: {e}", lock_path.display()));
        }
    }
    let _instance_lock = lock_file;

    // Part 2: if a live daemon already answers a Hello on this socket (one
    // started by a build that predates the lock file), we must NOT start a
    // second one.
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
    let name = crate::config::socket_name(&socket_path)
        .map_err(|e| anyhow::anyhow!("bad socket path {}: {e}", socket_path.display()))?;
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_tokio()
        .map_err(|e| anyhow::anyhow!("failed to bind socket {}: {e}", socket_path.display()))?;

    log::info!("session daemon listening on {}", socket_path.display());

    let registry = Arc::new(Mutex::new(Registry::from_config(&config)));

    // Background reaper: drop sessions whose child process has exited, so a
    // session that ends (e.g. the shell `exit`s, a one-shot command finishes)
    // is removed within ~REAP_INTERVAL and `list` never reports a zombie.
    {
        let reaper = Arc::clone(&registry);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAP_INTERVAL);
            loop {
                tick.tick().await;
                reaper.lock().unwrap().reap_exited();
            }
        });
    }

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
    let name = match crate::config::socket_name(socket_path) {
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
    let mut attached: Option<String> = None;
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
                // Reap finished sessions first (guard dropped before the await)
                // so a zombie is never reported.
                let sessions = {
                    let mut reg = registry.lock().unwrap();
                    reg.reap_exited();
                    reg.list()
                        .into_iter()
                        .map(to_wire_session_info)
                        .collect::<Vec<_>>()
                };
                send_frame(&writer_tx, DaemonToClient::Listing { sessions }).await?;
            }

            ClientToDaemon::Cwd {
                session_id,
                password,
            } => {
                let _turn = password_turn(&registry, &session_id, password.as_deref()).await;
                // Only the password check and pid lookup hold the registry
                // lock. The cwd query itself can shell out (`lsof` on macOS),
                // so it runs on the blocking pool instead of stalling every
                // other connection.
                let pid = {
                    let reg = registry.lock().unwrap();
                    reg.authorize(&session_id, password.as_deref()).and_then(|()| {
                        reg.find(&session_id)
                            .ok_or_else(|| anyhow::anyhow!("no such session: {session_id}"))?
                            .process_id()
                            .ok_or_else(|| anyhow::anyhow!("session has no process id"))
                    })
                };
                let result = match pid {
                    Ok(pid) => tokio::task::spawn_blocking(move || crate::cwd::process_cwd(pid))
                        .await?
                        .map(|cwd| cwd.to_string_lossy().into_owned())
                        .ok_or_else(|| anyhow::anyhow!("could not read cwd for pid {pid}")),
                    Err(e) => Err(e),
                };
                match result {
                    Ok(cwd) => {
                        send_frame(&writer_tx, DaemonToClient::Cwd { session_id, cwd }).await?
                    }
                    Err(e) => send_failure(&writer_tx, &session_id, e).await,
                }
            }

            ClientToDaemon::Rename { from, to, password } => {
                // Rename refuses attached sessions and taken names; the
                // registry moves the key AND the session's stored id. Only a
                // headless client issues rename (attach proxies never do), but
                // keep this connection's own binding honest in case it ever
                // renames the session it is attached to (daemon-side no-op).
                let _turn = password_turn(&registry, &from, password.as_deref()).await;
                let rename_result = {
                    let mut reg = registry.lock().unwrap();
                    reg.authorize(&from, password.as_deref())
                        .and_then(|()| reg.rename(&from, &to))
                };
                match rename_result {
                    Ok(()) => {
                        if attached.as_deref() == Some(from.as_str()) {
                            attached = Some(to.clone());
                        }
                        send_frame(&writer_tx, DaemonToClient::Renamed { session_id: to })
                            .await?;
                    }
                    Err(e) => send_failure(&writer_tx, &from, e).await,
                }
            }

            ClientToDaemon::Attach {
                session_id,
                password,
            } => {
                let _turn = password_turn(&registry, &session_id, password.as_deref()).await;
                // Reap finished sessions, then check the password and attach
                // under the same lock, so the session can't change in between.
                let attach_result = {
                    let mut reg = registry.lock().unwrap();
                    reg.reap_exited();
                    reg.authorize(&session_id, password.as_deref())
                        .and_then(|()| reg.attach(&session_id, client_id.clone()))
                };
                match attach_result {
                    Ok(handle) => {
                        // Stop forwarding from any previously-attached session.
                        if let Some(h) = forwarder.take() {
                            h.abort();
                        }
                        attached = Some(session_id.clone());
                        let fw_id = session_id.clone();
                        send_frame(
                            &writer_tx,
                            DaemonToClient::Attached {
                                session_id,
                                redraw_bytes: handle.redraw,
                            },
                        )
                        .await?;
                        // Stream incremental output from this session to the client.
                        forwarder = Some(spawn_output_forwarder(
                            writer_tx.clone(),
                            fw_id,
                            handle.receiver,
                        ));
                    }
                    Err(e) => send_failure(&writer_tx, &session_id, e).await,
                }
            }

            ClientToDaemon::AttachCreate {
                session_id,
                cmd,
                password,
                protect,
            } => {
                // Create-or-attach: if a session with this name is already live,
                // attach to it and IGNORE the command (resume). Otherwise create
                // a new session under that name running `cmd` (or the default
                // shell), then attach. The requested name IS the session id.
                //
                // Everything happens under one registry lock: a protected
                // session gets its password before anyone else can attach, and
                // an existing one is checked and attached with no gap between.
                let argv = cmd.as_deref().map(split_command);
                let _turn = password_turn(&registry, &session_id, password.as_deref()).await;
                let attach_result = {
                    let mut reg = registry.lock().unwrap();
                    // A finished session is gone: drop it so set_name re-creates
                    // fresh rather than resurrecting a zombie.
                    reg.reap_exited();
                    if reg.contains(&session_id) {
                        reg.authorize(&session_id, password.as_deref()).and_then(|()| {
                            if protect && !reg.is_protected(&session_id) {
                                Err(anyhow::anyhow!(
                                    "session '{session_id}' already exists without a password"
                                ))
                            } else {
                                Ok(())
                            }
                        })
                    } else if protect && password.as_deref().is_none_or(str::is_empty) {
                        Err(anyhow::anyhow!("a protected session needs a non-empty password"))
                    } else {
                        reg.create_session(&session_id, argv)
                            .map_err(|e| anyhow::anyhow!("failed to create session {session_id}: {e}"))
                            .and_then(|()| match password.as_deref().filter(|_| protect) {
                                Some(password) => reg.protect(&session_id, password),
                                None => Ok(()),
                            })
                    }
                    .and_then(|()| reg.attach(&session_id, client_id.clone()))
                };
                match attach_result {
                    Ok(handle) => {
                        if let Some(h) = forwarder.take() {
                            h.abort();
                        }
                        attached = Some(session_id.clone());
                        let fw_id = session_id.clone();
                        send_frame(
                            &writer_tx,
                            DaemonToClient::Attached {
                                session_id,
                                redraw_bytes: handle.redraw,
                            },
                        )
                        .await?;
                        forwarder = Some(spawn_output_forwarder(
                            writer_tx.clone(),
                            fw_id,
                            handle.receiver,
                        ));
                    }
                    Err(e) => send_failure(&writer_tx, &session_id, e).await,
                }
            }

            ClientToDaemon::Input { bytes } => {
                let Some(ref sid) = attached else {
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
                    .find(sid)
                    .map(|s| s.write(&bytes, &client_id));
                if let Some(Err(e)) = result {
                    let _ = send_frame(&writer_tx, DaemonToClient::Error { message: e.to_string() }).await;
                }
            }

            ClientToDaemon::Resize { rows, cols } => {
                let Some(ref sid) = attached else {
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
                    .find_mut(sid)
                    .map(|s| s.set_size(rows, cols, &client_id));
                if let Some(Err(e)) = result {
                    let _ = send_frame(&writer_tx, DaemonToClient::Error { message: e.to_string() }).await;
                }
            }

            ClientToDaemon::Detach => {
                if let Some(h) = forwarder.take() {
                    h.abort();
                }
                if let Some(sid) = attached.take() {
                    if let Some(s) = registry.lock().unwrap().find_mut(&sid) {
                        s.detach(&client_id);
                    }
                }
            }

            ClientToDaemon::Kill {
                session_id,
                password,
            } => {
                let _turn = password_turn(&registry, &session_id, password.as_deref()).await;
                // Bind to a `let` so the lock guard is dropped before the awaits.
                let kill_result = {
                    let mut reg = registry.lock().unwrap();
                    reg.authorize(&session_id, password.as_deref())
                        .and_then(|()| reg.kill(&session_id))
                };
                match kill_result {
                    Ok(()) => {
                        if attached.as_deref() == Some(session_id.as_str()) {
                            attached = None;
                            if let Some(h) = forwarder.take() {
                                h.abort();
                            }
                        }
                        send_frame(&writer_tx, DaemonToClient::Killed { session_id })
                            .await?;
                    }
                    Err(e) => send_failure(&writer_tx, &session_id, e).await,
                }
            }

            ClientToDaemon::KillAll => {
                // Password-protected sessions are left running.
                let kept = {
                    let mut reg = registry.lock().unwrap();
                    let kept = reg.kill_all();
                    if attached.as_deref().is_some_and(|sid| !reg.contains(sid)) {
                        attached = None;
                        if let Some(h) = forwarder.take() {
                            h.abort();
                        }
                    }
                    kept
                };
                let message = match kept {
                    0 => "killed all sessions".to_string(),
                    n => format!(
                        "killed all sessions except {n} password-protected (kill those by name)"
                    ),
                };
                let _ = send_frame(&writer_tx, DaemonToClient::Error { message }).await;
            }
        }
    }

    // Client disconnected: detach from the session (so the primary-client
    // winsize authority is promoted cleanly) and tear down our tasks.
    if let Some(sid) = attached {
        if let Some(s) = registry.lock().unwrap().find_mut(&sid) {
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
/// session exits or the client goes away. On session exit it sends an [`Exit`]
/// frame so the client can close its view instead of lingering in raw mode.
fn spawn_output_forwarder(
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    session_id: String,
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
                // The session's child exited: tell the client so its proxy closes
                // instead of staying attached to a dead shell.
                Ok(SessionEvent::Exit(_)) => {
                    if let Ok(frame) = (DaemonToClient::Exit {
                        session_id: session_id.clone(),
                    })
                    .to_frame()
                    {
                        let _ = writer_tx.send(frame);
                    }
                    break;
                }
                // We fell behind (many output chunks). Skip the gap and keep up.
                Err(BroadcastRecvError::Lagged(_)) => continue,
                Err(BroadcastRecvError::Closed) => break,
            }
        }
    })
}

/// Wait for this request's turn at `session_id`'s password, when the session is
/// protected and a password was sent. The turn is held through the check and,
/// after a wrong password, through [`WRONG_PASSWORD_DELAY`] (see
/// [`send_failure`]), so guesses at one session are answered one at a time no
/// matter how many connections send them.
async fn password_turn(
    registry: &Mutex<Registry>,
    session_id: &str,
    password: Option<&str>,
) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    password?;
    let turn = registry.lock().unwrap().password_turn(session_id)?;
    Some(turn.lock_owned().await)
}

/// Report a failed request on `session_id`: a password refusal ([`Denied`])
/// becomes `PasswordRequired`, after the wrong-password delay; anything else an
/// `Error`.
async fn send_failure(tx: &mpsc::UnboundedSender<Vec<u8>>, session_id: &str, e: anyhow::Error) {
    let frame = match e.downcast_ref::<Denied>() {
        Some(&denied) => {
            let wrong = denied == Denied::Wrong;
            if wrong {
                tokio::time::sleep(WRONG_PASSWORD_DELAY).await;
            }
            DaemonToClient::PasswordRequired {
                session_id: session_id.to_string(),
                wrong,
            }
        }
        None => DaemonToClient::Error { message: e.to_string() },
    };
    let _ = send_frame(tx, frame).await;
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

/// Map a registry `SessionInfo` onto the protocol's wire `SessionInfo`.
fn to_wire_session_info(info: crate::registry::SessionInfo) -> WireSessionInfo {
    WireSessionInfo {
        id: info.id,
        title: info.title,
        attached: info.attached_count,
        protected: info.protected,
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
            match crate::config::socket_name(path) {
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

    // The instance lock makes probe-then-bind atomic: while another daemon
    // holds it, `run` must exit `Ok(())` without touching the socket (two
    // racing daemons would otherwise both bind, the second unlinking the
    // first's socket and orphaning its sessions).
    #[tokio::test]
    async fn held_instance_lock_makes_run_exit_without_binding() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_in(dir.path());
        let lock = std::fs::File::create(cfg.socket_path.with_extension("lock")).unwrap();
        lock.try_lock().unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), run(cfg.clone()))
            .await
            .expect("run must return promptly while the lock is held");
        assert!(result.is_ok(), "lock holder present -> exit Ok(()), got {result:?}");
        assert!(!cfg.socket_path.exists(), "must not bind while the lock is held");
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
        let sid = "cat-demo".to_string();
        reader
            .get_mut()
            .write_all(
                &(ClientToDaemon::AttachCreate {
                    session_id: sid.clone(),
                    cmd: Some(crate::testing::ECHO_CMD.into()),
                    password: None,
                    protect: false,
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
        assert_eq!(got_sid, sid, "attached should echo the requested session id");
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

    // GATE for create-or-attach semantics: `session <name> <cmd>` must resume a
    // live session (attach and IGNORE cmd) rather than create a second one, and
    // must report the requested name as the session id.
    #[tokio::test]
    async fn attach_or_create_resumes_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_in(dir.path());
        let daemon = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        // First client creates "work-session" running a silent long-lived cmd.
        let s1 = connect_with_retry(&dir.path().join("session.sock")).await;
        let mut r1 = BufReader::new(s1);
        r1.get_mut()
            .write_all(
                &(ClientToDaemon::Hello {
                    version: PROTOCOL_VERSION,
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        let mut hello = String::new();
        r1.read_line(&mut hello).await.unwrap();
        assert!(matches!(
            DaemonToClient::from_frame(hello.as_bytes()).unwrap(),
            DaemonToClient::Hello { .. }
        ));

        r1.get_mut()
            .write_all(
                &(ClientToDaemon::AttachCreate {
                    session_id: "work-session".into(),
                    cmd: Some(crate::testing::IDLE_CMD.into()),
                    password: None,
                    protect: false,
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        r1.read_line(&mut line).await.unwrap();
        let got1 = match DaemonToClient::from_frame(line.as_bytes()).unwrap() {
            DaemonToClient::Attached { session_id, .. } => session_id,
            other => panic!("expected Attached, got {other:?}"),
        };
        assert_eq!(got1, "work-session", "first attach-create reports the requested name");

        // A second client attaching with the SAME name but a DIFFERENT cmd must
        // resume the existing session (cmd ignored), not clone it.
        let s2 = connect_with_retry(&dir.path().join("session.sock")).await;
        let mut r2 = BufReader::new(s2);
        r2.get_mut()
            .write_all(
                &(ClientToDaemon::Hello {
                    version: PROTOCOL_VERSION,
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        let mut hello = String::new();
        r2.read_line(&mut hello).await.unwrap();
        assert!(matches!(
            DaemonToClient::from_frame(hello.as_bytes()).unwrap(),
            DaemonToClient::Hello { .. }
        ));

        r2.get_mut()
            .write_all(
                &(ClientToDaemon::AttachCreate {
                    session_id: "work-session".into(),
                    cmd: Some(crate::testing::RERUN_CMD.into()),
                    password: None,
                    protect: false,
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        r2.read_line(&mut line).await.unwrap();
        let got2 = match DaemonToClient::from_frame(line.as_bytes()).unwrap() {
            DaemonToClient::Attached { session_id, .. } => session_id,
            other => panic!("expected Attached, got {other:?}"),
        };
        assert_eq!(
            got2, "work-session",
            "same name must resume the existing session, ignoring cmd"
        );

        // The listing must show exactly ONE session named "work-session".
        r2.get_mut()
            .write_all(&ClientToDaemon::List.to_frame().unwrap())
            .await
            .unwrap();
        let mut line = String::new();
        r2.read_line(&mut line).await.unwrap();
        match DaemonToClient::from_frame(line.as_bytes()).unwrap() {
            DaemonToClient::Listing { sessions } => {
                assert_eq!(
                    sessions.len(),
                    1,
                    "create-or-attach must not clone a session"
                );
                assert_eq!(sessions[0].id, "work-session");
            }
            other => panic!("expected Listing, got {other:?}"),
        }

        daemon.abort();
    }

    // GATE for the cwd RPC: after creating a session, `Cwd` returns the session
    // child's live working directory (the daemon's inherited cwd, absolute).
    #[tokio::test]
    async fn cwd_reports_session_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_in(dir.path());
        let daemon = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        let stream = connect_with_retry(&dir.path().join("session.sock")).await;
        let mut r = BufReader::new(stream);
        r.get_mut()
            .write_all(
                &(ClientToDaemon::Hello {
                    version: PROTOCOL_VERSION,
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert!(matches!(
            DaemonToClient::from_frame(line.as_bytes()).unwrap(),
            DaemonToClient::Hello { .. }
        ));

        // Create a silent long-lived session.
        r.get_mut()
            .write_all(
                &(ClientToDaemon::AttachCreate {
                    session_id: "cwd-test".into(),
                    cmd: Some(crate::testing::IDLE_CMD.into()),
                    password: None,
                    protect: false,
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert!(matches!(
            DaemonToClient::from_frame(line.as_bytes()).unwrap(),
            DaemonToClient::Attached { .. }
        ));

        // Ask for its cwd.
        r.get_mut()
            .write_all(
                &(ClientToDaemon::Cwd {
                    session_id: "cwd-test".into(),
                    password: None,
                })
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        // Skip the child's output (ConPTY prints mode sequences on Windows).
        let reply = loop {
            line.clear();
            r.read_line(&mut line).await.unwrap();
            match DaemonToClient::from_frame(line.as_bytes()).unwrap() {
                DaemonToClient::Output { .. } => continue,
                other => break other,
            }
        };
        match reply {
            DaemonToClient::Cwd { session_id, cwd } => {
                assert_eq!(session_id, "cwd-test");
                assert!(!cwd.is_empty(), "cwd should not be empty");
                assert!(
                    std::path::Path::new(&cwd).is_absolute(),
                    "cwd should be absolute: {cwd}"
                );
            }
            other => panic!("expected Cwd, got {other:?}"),
        }

        daemon.abort();
    }

    // A protected session: created with `protect`, it is listed as protected,
    // refuses attach/cwd/kill without its password (PasswordRequired, with
    // `wrong` set after a bad guess, which is answered only after the delay),
    // accepts the right one, and survives kill-all.
    #[tokio::test]
    async fn protected_session_requires_its_password() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_in(dir.path());
        let daemon = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        async fn open(path: &Path) -> BufReader<TokioStream> {
            let mut r = BufReader::new(connect_with_retry(path).await);
            ask(&mut r, ClientToDaemon::Hello { version: PROTOCOL_VERSION }).await;
            r
        }
        async fn ask(r: &mut BufReader<TokioStream>, frame: ClientToDaemon) -> DaemonToClient {
            r.get_mut().write_all(&frame.to_frame().unwrap()).await.unwrap();
            let mut line = String::new();
            r.read_line(&mut line).await.unwrap();
            DaemonToClient::from_frame(line.as_bytes()).unwrap()
        }
        let sock = dir.path().join("session.sock");
        let required = |wrong| DaemonToClient::PasswordRequired {
            session_id: "vault".into(),
            wrong,
        };

        // Creating protected without a password is refused.
        let mut owner = open(&sock).await;
        let create = |password: Option<&str>| ClientToDaemon::AttachCreate {
            session_id: "vault".into(),
            cmd: Some(crate::testing::IDLE_CMD.into()),
            password: password.map(str::to_string),
            protect: true,
        };
        assert!(matches!(ask(&mut owner, create(None)).await, DaemonToClient::Error { .. }));
        assert!(matches!(
            ask(&mut owner, create(Some("s3cret"))).await,
            DaemonToClient::Attached { .. }
        ));

        let mut other = open(&sock).await;
        match ask(&mut other, ClientToDaemon::List).await {
            DaemonToClient::Listing { sessions } => {
                assert_eq!(sessions.len(), 1);
                assert!(sessions[0].protected, "listing must mark it protected");
            }
            f => panic!("expected Listing, got {f:?}"),
        }
        let attach = |password: Option<&str>| ClientToDaemon::Attach {
            session_id: "vault".into(),
            password: password.map(str::to_string),
        };
        assert_eq!(ask(&mut other, attach(None)).await, required(false));
        let started = std::time::Instant::now();
        assert_eq!(ask(&mut other, attach(Some("guess"))).await, required(true));
        assert!(started.elapsed() >= WRONG_PASSWORD_DELAY, "a wrong guess is slowed down");
        // The resume path (AttachCreate without protect) is guarded too.
        let resume = ClientToDaemon::AttachCreate {
            session_id: "vault".into(),
            cmd: None,
            password: None,
            protect: false,
        };
        assert_eq!(ask(&mut other, resume).await, required(false));
        let cwd = ClientToDaemon::Cwd {
            session_id: "vault".into(),
            password: None,
        };
        assert_eq!(ask(&mut other, cwd).await, required(false));
        let rename = ClientToDaemon::Rename {
            from: "vault".into(),
            to: "x".into(),
            password: None,
        };
        assert_eq!(ask(&mut other, rename).await, required(false));
        let kill = |password: Option<&str>| ClientToDaemon::Kill {
            session_id: "vault".into(),
            password: password.map(str::to_string),
        };
        assert_eq!(ask(&mut other, kill(None)).await, required(false));
        match ask(&mut other, ClientToDaemon::KillAll).await {
            DaemonToClient::Error { message } => assert!(message.contains("except 1"), "{message}"),
            f => panic!("expected the kill-all ack, got {f:?}"),
        }
        assert!(matches!(
            ask(&mut other, attach(Some("s3cret"))).await,
            DaemonToClient::Attached { .. }
        ));

        // An existing unprotected name can't be "protected" by a second create.
        let mut third = open(&sock).await;
        let plain = ClientToDaemon::AttachCreate {
            session_id: "plain".into(),
            cmd: Some(crate::testing::IDLE_CMD.into()),
            password: None,
            protect: false,
        };
        assert!(matches!(ask(&mut third, plain).await, DaemonToClient::Attached { .. }));
        let mut fourth = open(&sock).await;
        let reprotect = ClientToDaemon::AttachCreate {
            session_id: "plain".into(),
            cmd: None,
            password: Some("pw".into()),
            protect: true,
        };
        match ask(&mut fourth, reprotect).await {
            DaemonToClient::Error { message } => assert!(message.contains("without a password")),
            f => panic!("expected an Error, got {f:?}"),
        }

        let mut killer = open(&sock).await;
        assert_eq!(
            ask(&mut killer, kill(Some("s3cret"))).await,
            DaemonToClient::Killed { session_id: "vault".into() }
        );
        daemon.abort();
    }
}

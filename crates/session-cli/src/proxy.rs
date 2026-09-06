//! Raw-mode stdio byte proxy: attach to a session and move bytes both ways.
//!
//! This is the transport the Shell app (and any terminal) uses when it attaches
//! to a [`session_core`] daemon session. It is deliberately *transparent*: it
//! does **not** parse or render anything. It puts the surrounding terminal into
//! raw mode (crossterm, on Unix and Windows), sends the attach/create frame, and
//! then:
//!
//! * daemon `Output` / `redraw_bytes` frames  ->  written straight to `stdout`
//!   (the surrounding terminal renders them);
//! * raw `stdin` bytes                         ->  sent as daemon `Input`;
//! * the terminal size is read (TIOCGWINSZ on Unix, `GetConsoleScreenBufferInfo`
//!   on Windows — both reached through [`crossterm::terminal::size`]) and sent
//!   as `Resize` immediately and again whenever the window changes (SIGWINCH on
//!   Unix, a size poll on Windows);
//! * on stdin EOF / SIGTERM / a client-side close the proxy sends `Detach` and
//!   exits — it never kills the session.
//!
//! # Concurrency: split connection
//!
//! The single [`session_core::protocol::ClientToDaemon`]/[`DaemonToClient`]
//! socket is split into an independent read half (owned by a background
//! daemon→stdout task) and write half (owned by the main stdin→daemon loop).
//! This is required because reading a frame off the socket blocks, and a single
//! `&mut` connection cannot be bridged between a blocking `recv` and a
//! concurrent `send` in the same [`tokio::select!`] (two arms would both borrow
//! it mutably). Splitting lets input keep flowing even while no output is
//! arriving.
//!
//! # A note on TERM
//!
//! The session child is spawned by the *daemon* (via `AttachCreate`), whose
//! process environment is out of this proxy's reach. To still deliver a
//! truecolor-capable terminal to the child, the proxy prefixes a custom command
//! with `env TERM=<term> ...` so the daemon's whitespace-split argv starts with
//! `env`, which sets `TERM` for the spawned program. The default-shell case
//! The default-shell case (`cmd == None`) has no such channel and is documented as a limitation. See
//! [`attach_frame`].

// The CLI dispatch card (t_ad20f5d9) wires this module into `main.rs` and calls
// [`attach`]; until then the public proxy API is forward-declared and not yet
// referenced by the binary, so it is flagged dead code. That clears once the
// dispatch lands.
#![allow(dead_code)]

use anyhow::anyhow;
use interprocess::local_socket::{
    tokio::prelude::*,
    tokio::Stream as TokioStream,
    GenericFilePath, ToFsName,
};
use session_core::config::Config;
use session_core::protocol::{check_version, ClientToDaemon, DaemonToClient, PROTOCOL_VERSION};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Terminal size in character cells, `(rows, cols)` — the order the wire
/// protocol's `Resize` frame and [`session_core`]'s winsize expect.
pub type Size = (u16, u16);

/// How the proxy's run ended. The session is left running in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyExit {
    /// The client detached on stdin EOF / SIGTERM. The session lives on.
    Detached,
    /// The daemon closed the connection (session ended, or the daemon exited).
    Closed,
}

/// Options controlling an attach-proxy run.
#[derive(Debug, Clone)]
pub struct ProxyOptions {
    /// Enter the terminal's raw mode via crossterm. Default `true`; tests pass
    /// `false` because a pipe is not a TTY.
    pub raw_mode: bool,
    /// TERM value to expose to the session child (best-effort; see module docs).
    /// `None` leaves the child's `TERM` untouched.
    pub term: Option<String>,
    /// Size used when the real terminal size cannot be read (e.g. not a TTY).
    pub fallback_size: Size,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
            raw_mode: true,
            term: Some(default_term()),
            fallback_size: (24, 80),
        }
    }
}

/// A connected, Hello-handshaked daemon connection split into its socket halves.
///
/// * `writer` — a single write half used for `Attach`, `Resize`, `Input`,
///   `Detach`. Owned by the main forwarding loop.
/// * `reader` — a buffered read half moved into the daemon→stdout task.
struct Conn {
    writer: tokio::io::WriteHalf<TokioStream>,
    reader: BufReader<tokio::io::ReadHalf<TokioStream>>,
}

impl Conn {
    /// Connect to the daemon at `config.socket_path`, complete the `Hello`
    /// handshake, and split the socket into its read/write halves.
    async fn connect(config: &Config) -> anyhow::Result<Conn> {
        let name = config
            .socket_path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| anyhow!("invalid socket path {}: {e}", config.socket_path.display()))?;
        let stream = TokioStream::connect(name)
            .await
            .map_err(|e| anyhow!("cannot connect to daemon socket: {e}"))?;

        // Split so the reader can block on the daemon while the writer stays free.
        let (read_half, mut write_half) = tokio::io::split(stream);

        // Handshake: advertise our version, verify the daemon's.
        let frame = ClientToDaemon::Hello {
            version: PROTOCOL_VERSION,
        }
        .to_frame()?;
        write_half.write_all(&frame).await?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("daemon closed the socket during handshake"));
        }
        match DaemonToClient::from_frame(line.as_bytes())? {
            DaemonToClient::Hello { daemon_version } => {
                check_version(daemon_version).map_err(|e| anyhow!("{e}"))?;
            }
            other => return Err(anyhow!("expected Hello during handshake, got {other:?}")),
        }

        Ok(Conn {
            writer: write_half,
            reader,
        })
    }

    /// Send one protocol frame to the daemon.
    async fn send(&mut self, msg: ClientToDaemon) -> anyhow::Result<()> {
        let frame = msg.to_frame()?;
        self.writer.write_all(&frame).await?;
        Ok(())
    }
}

/// Read the real terminal size via crossterm, as `(rows, cols)`.
///
/// crossterm queries the OS directly: `TIOCGWINSZ` on Unix and
/// `GetConsoleScreenBufferInfo` on Windows, so this is the single cross-platform
/// size probe. Returns an error when stdout is not a TTY.
pub fn terminal_size() -> std::io::Result<Size> {
    let (cols, rows) = crossterm::terminal::size()?;
    Ok((rows.max(1), cols.max(1)))
}

/// A default TERM value for the session child: an existing rich TERM is kept,
/// otherwise fall back to a truecolor-capable value.
fn default_term() -> String {
    if let Ok(t) = std::env::var("TERM") {
        let t = t.trim();
        if !t.is_empty() && is_rich_term(t) {
            return t.to_string();
        }
    }
    "xterm-256color".to_string()
}

/// Is `term` already rich enough for truecolor/256-color apps?
fn is_rich_term(term: &str) -> bool {
    term.contains("256color")
        || term.contains("truecolor")
        || term.contains("24bit")
        || matches!(term, "alacritty" | "kitty" | "wezterm" | "iterm" | "st-256color")
}

/// Build the attach/create frame for a proxy run.
///
/// A custom command is wrapped with `env TERM=<term> ...` so the daemon-spawned
/// child inherits a truecolor-capable `TERM` (the daemon's own environment is not
/// reachable from here). `term == None` leaves the command untouched.
fn attach_frame(session: &str, cmd: Option<&str>, term: Option<&str>) -> ClientToDaemon {
    match cmd {
        Some(c) => ClientToDaemon::AttachCreate {
            session_id: session.to_string(),
            cmd: Some(with_term_prefix(c, term)),
        },
        None => ClientToDaemon::Attach {
            session_id: session.to_string(),
        },
    }
}

/// Prefix `cmd` with `env TERM=<term>` when a TERM override is requested, unless
/// the command already sets `TERM` itself.
fn with_term_prefix(cmd: &str, term: Option<&str>) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return cmd.to_string();
    }
    if let Some(term) = term {
        if !trimmed.starts_with("env ") {
            return format!("env TERM={term} {trimmed}");
        }
    }
    cmd.to_string()
}

/// The production attach entry point used by the CLI dispatch (and by Shell).
///
/// Connects to the daemon at `config`, enters raw mode, sends the attach/create
/// frame plus an initial `Resize`, then runs the byte proxy until stdin EOF /
/// SIGTERM / a daemon close, sending `Detach` before returning. Raw mode is
/// always restored, even on error.
pub fn attach(
    config: &Config,
    session: &str,
    cmd: Option<&str>,
    opts: ProxyOptions,
) -> anyhow::Result<ProxyExit> {
    // Best-effort: expose a rich TERM in this process's own environment too, so
    // anything the proxy itself launches sees truecolor (the session child env is
    // handled via the `env` prefix in `attach_frame`).
    if let Some(term) = opts.term.as_deref() {
        if std::env::var("TERM")
            .map(|t| t.is_empty() || t == "dumb" || t == "xterm" || t == "vt100")
            .unwrap_or(true)
        {
            std::env::set_var("TERM", term);
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let conn = Conn::connect(config).await?;

        // Report the real terminal size immediately (spec: "send Resize
        // immediately"). Fall back to a sane default for non-TTY stdout.
        let initial_size = terminal_size().unwrap_or(opts.fallback_size);

        // SIGWINCH / terminate watchers that push into the proxy loop below.
        let (resize_tx, resize_rx) = mpsc::unbounded_channel();
        let (term_tx, term_rx) = mpsc::unbounded_channel();
        spawn_resize_watcher(resize_tx);
        spawn_terminate_watcher(term_tx);

        // The attach (or create-attach) frame; a custom command may carry the
        // TERM prefix so the daemon-spawned child sees a truecolor terminal.
        let frame = attach_frame(session, cmd, opts.term.as_deref());

        if opts.raw_mode {
            if let Err(e) = crossterm::terminal::enable_raw_mode() {
                return Err(anyhow!("cannot enter raw mode (is stdout a terminal?): {e}"));
            }
        }

        let result = proxy_loop(
            conn,
            tokio::io::stdin(),
            tokio::io::stdout(),
            frame,
            initial_size,
            resize_rx,
            term_rx,
        )
        .await;

        if opts.raw_mode {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        result
    })
}

/// The testable core of the proxy.
///
/// Sends the attach/create frame and an initial `Resize` over `conn`'s writer,
/// spawns the daemon→`stdout` task, then drives the stdin→`Input` / resize /
/// terminate loop. Returns once the client detaches or the daemon closes. This
/// is generic over the byte endpoints so a pipe harness can substitute in-memory
/// streams and never needs a real TTY or raw mode.
async fn proxy_loop<S, W>(
    conn: Conn,
    mut stdin: S,
    stdout: W,
    attach: ClientToDaemon,
    initial_size: Size,
    mut resize_rx: mpsc::UnboundedReceiver<Size>,
    mut term_rx: mpsc::UnboundedReceiver<()>,
) -> anyhow::Result<ProxyExit>
where
    S: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let Conn { mut writer, reader } = conn;

    // Attach (or create-attach), then report the terminal size immediately.
    writer.write_all(&attach.to_frame()?).await?;
    writer
        .write_all(
            &ClientToDaemon::Resize {
                rows: initial_size.0,
                cols: initial_size.1,
            }
            .to_frame()?,
        )
        .await?;

    // Daemon -> stdout: the surrounding terminal renders the bytes.
    let (done_tx, mut done_rx) = oneshot::channel::<()>();
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        let mut stdout = stdout;
        let result = forward_daemon_to_stdout(&mut reader, &mut stdout).await;
        let _ = done_tx.send(());
        result
    });

    // stdin -> daemon, plus resize / terminate.
    let mut buf = [0u8; 4096];
    let outcome = loop {
        tokio::select! {
            n = stdin.read(&mut buf) => match n {
                Ok(0) => break ProxyExit::Detached,            // stdin EOF -> detach
                Ok(n) => {
                    let frame = ClientToDaemon::Input { bytes: buf[..n].to_vec() };
                    if writer.write_all(&frame.to_frame()?).await.is_err() {
                        break ProxyExit::Detached;
                    }
                }
                Err(e) => {
                    eprintln!("session: stdin error: {e}");
                    break ProxyExit::Detached;
                }
            },
            size = resize_rx.recv() => {
                if let Some((rows, cols)) = size {
                    let frame = ClientToDaemon::Resize { rows, cols };
                    if writer.write_all(&frame.to_frame()?).await.is_err() {
                        break ProxyExit::Detached;
                    }
                }
            }
            _ = term_rx.recv() => break ProxyExit::Detached,   // SIGTERM -> detach
            _ = &mut done_rx => break ProxyExit::Closed,       // daemon closed / session ended
        }
    };

    // We're leaving: tell the daemon, which detaches this client without killing
    // the session. If the daemon already closed, this is a harmless no-op error.
    let _ = writer.write_all(&ClientToDaemon::Detach.to_frame()?).await;
    reader_task.abort();
    Ok(outcome)
}

/// Read daemon frames and write their raw payloads to `stdout`.
///
/// `Attached.redraw_bytes` (the full-screen replay) and `Output.bytes`
/// (incremental terminal output) are both written transparently. Protocol errors,
/// `Error` frames, and `Killed` frames are surfaced on stderr. Returns when the
/// daemon closes the connection (EOF) or an I/O error occurs.
async fn forward_daemon_to_stdout<R, W>(
    reader: &mut BufReader<R>,
    stdout: &mut W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // daemon closed the stream
        }
        match DaemonToClient::from_frame(line.as_bytes())? {
            DaemonToClient::Attached { redraw_bytes, .. } => {
                stdout.write_all(&redraw_bytes).await?;
                stdout.flush().await?;
            }
            DaemonToClient::Output { bytes } => {
                stdout.write_all(&bytes).await?;
                stdout.flush().await?;
            }
            DaemonToClient::Error { message } => eprintln!("session: {message}"),
            DaemonToClient::Killed { session_id } => {
                eprintln!("session: session {session_id} was killed")
            }
            _ => {} // Hello/Listing/other strays are ignored after attach
        }
    }
}

/// Watch for SIGWINCH (Unix) and push the re-read terminal size into `tx`.
#[cfg(unix)]
fn spawn_resize_watcher(tx: mpsc::UnboundedSender<Size>) {
    tokio::spawn(async move {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
            Ok(mut sig) => {
                while sig.recv().await.is_some() {
                    if let Ok(size) = terminal_size() {
                        let _ = tx.send(size);
                    }
                }
            }
            Err(_) => {
                // Registration failed; keep the channel open (no resize events).
                let _ = tx;
                std::future::pending::<()>().await;
            }
        }
    });
}

/// Watch for SIGWINCH on Windows by polling the console size periodically.
#[cfg(not(unix))]
fn spawn_resize_watcher(tx: mpsc::UnboundedSender<Size>) {
    tokio::spawn(async move {
        let mut last = terminal_size().ok();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let now = terminal_size().ok();
            if now != last {
                if let Some(size) = now {
                    let _ = tx.send(size);
                }
                last = now;
            }
        }
    });
}

/// Watch for SIGTERM (Unix) and push a single termination signal into `tx`.
#[cfg(unix)]
fn spawn_terminate_watcher(tx: mpsc::UnboundedSender<()>) {
    tokio::spawn(async move {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                if sig.recv().await.is_some() {
                    let _ = tx.send(());
                }
                // Keep the channel open so the proxy keeps serving until exit.
                let _ = tx;
                std::future::pending::<()>().await;
            }
            Err(_) => {
                let _ = tx;
                std::future::pending::<()>().await;
            }
        }
    });
}

/// No SIGTERM to watch on non-Unix; keep the channel open (never fires).
#[cfg(not(unix))]
fn spawn_terminate_watcher(tx: mpsc::UnboundedSender<()>) {
    tokio::spawn(async move {
        let _ = tx;
        std::future::pending::<()>().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
    use session_core::config::Config;
    use std::path::Path;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    fn temp_config(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::default();
        cfg.socket_dir = dir.path().to_path_buf();
        cfg.socket_path = dir.path().join("session.sock");
        cfg
    }

    // A fake daemon on a temp socket. It completes a Hello round-trip, then
    // records every ClientToDaemon frame it receives and echoes each Input back
    // as an Output frame (so the daemon->stdout direction is observable). Frames
    // are pushed to the returned receiver for the test to assert on.
    async fn spawn_fake_daemon(sock: &Path) -> mpsc::UnboundedReceiver<ClientToDaemon> {
        let name = sock.to_fs_name::<GenericFilePath>().unwrap();
        let listener = ListenerOptions::new()
            .name(name)
            .try_overwrite(true)
            .create_tokio()
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let stream = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let (mut read_half, mut write_half) = tokio::io::split(stream);

            // Hello round-trip.
            let mut line = String::new();
            let mut reader = BufReader::new(&mut read_half);
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let _ = ClientToDaemon::from_frame(line.as_bytes()).unwrap();
            let hello = DaemonToClient::Hello {
                daemon_version: PROTOCOL_VERSION,
            };
            let _ = write_half.write_all(&hello.to_frame().unwrap()).await;

            // Record + echo loop.
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let frame = match ClientToDaemon::from_frame(line.as_bytes()) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let _ = tx.send(frame.clone());
                match &frame {
                    ClientToDaemon::Attach { .. } | ClientToDaemon::AttachCreate { .. } => {
                        let repl = DaemonToClient::Attached {
                            session_id: "s1".to_string(),
                            redraw_bytes: b"\x1b[2J".to_vec(),
                        };
                        let _ = write_half.write_all(&repl.to_frame().unwrap()).await;
                    }
                    ClientToDaemon::Input { bytes } => {
                        let echo = DaemonToClient::Output { bytes: bytes.clone() };
                        let _ = write_half.write_all(&echo.to_frame().unwrap()).await;
                    }
                    _ => {}
                }
            }
        });
        rx
    }

    // GATE: a real pipe harness. A fake daemon records every frame the proxy
    // sends; in-memory duplex streams stand in for stdin/stdout so no TTY or raw
    // mode is needed. Asserts bytes are forwarded both ways, an initial resize is
    // sent, a resize change is re-sent, and stdin EOF detaches.
    #[tokio::test]
    async fn gate_proxy_forwards_bytes_resizes_and_detaches() {
        let dir = tempdir().unwrap();
        let cfg = temp_config(&dir);
        let mut received = spawn_fake_daemon(&cfg.socket_path).await;

        let (in_r, mut in_w) = tokio::io::duplex(1024); // proxy stdin <- test writes
        let (mut out_r, out_w) = tokio::io::duplex(1024); // proxy stdout -> test reads

        let conn = Conn::connect(&cfg).await.expect("connect to fake daemon");
        let (resize_tx, resize_rx) = mpsc::unbounded_channel();
        let (_term_tx, term_rx) = mpsc::unbounded_channel();

        let proxy = tokio::spawn(async move {
            proxy_loop(
                conn,
                in_r,
                out_w,
                ClientToDaemon::AttachCreate {
                    session_id: "s1".to_string(),
                    cmd: None,
                },
                (24, 80),
                resize_rx,
                term_rx,
            )
            .await
        });

        // Drive a resize change; the proxy must re-send a Resize frame.
        resize_tx.send((50, 130)).unwrap();

        // Feed a keystroke; the fake daemon echoes it back as Output.
        in_w.write_all(b"hello-from-stdin\n").await.unwrap();

        // ---- Assert daemon -> stdout (the echo comes back as Output bytes). ----
        let mut output = Vec::new();
        let mut buf = [0u8; 256];
        let probe = b"hello-from-stdin";
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), out_r.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => output.extend_from_slice(&buf[..n]),
                _ => {}
            }
            if output.windows(probe.len()).any(|w| w == probe) {
                break;
            }
        }
        assert!(
            output.windows(probe.len()).any(|w| w == probe),
            "daemon Output bytes were not forwarded to stdout, got: {:?}",
            String::from_utf8_lossy(&output)
        );

        // ---- Assert stdin -> daemon, initial+changed Resize, and Detach on EOF. ----
        let mut saw_input = false;
        let mut saw_resize_init = false;
        let mut saw_resize_change = false;
        let mut saw_detach = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !(saw_input && saw_resize_init && saw_resize_change && saw_detach)
        {
            match tokio::time::timeout(Duration::from_millis(100), received.recv()).await {
                Ok(Some(ClientToDaemon::Input { bytes })) => {
                    if bytes.windows(probe.len()).any(|w| w == probe) {
                        saw_input = true;
                    }
                }
                Ok(Some(ClientToDaemon::Resize { rows, cols })) => {
                    if rows == 24 && cols == 80 {
                        saw_resize_init = true;
                    }
                    if rows == 50 && cols == 130 {
                        saw_resize_change = true;
                    }
                }
                Ok(Some(ClientToDaemon::Detach)) => saw_detach = true,
                Ok(_) => {}
                _ => {}
            }
        }
        assert!(saw_input, "stdin byte was not forwarded as Input");
        assert!(
            saw_resize_init,
            "initial Resize was not sent after attach (expected 24x80)"
        );
        assert!(
            saw_resize_change,
            "changed Resize was not sent on resize signal (expected 50x130)"
        );
        assert!(!saw_detach, "Detach must not be sent until stdin EOF");

        // Close stdin -> EOF -> the proxy must send Detach and exit.
        drop(in_w);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !saw_detach && std::time::Instant::now() < deadline {
            if let Ok(Some(ClientToDaemon::Detach)) =
                tokio::time::timeout(Duration::from_millis(100), received.recv()).await
            {
                saw_detach = true;
                break;
            }
        }
        assert!(saw_detach, "stdin EOF did not cause a Detach");

        let exit = tokio::time::timeout(Duration::from_secs(5), proxy)
            .await
            .expect("proxy took too long to exit")
            .expect("proxy task panicked")
            .expect("proxy_loop returned an error");
        assert_eq!(exit, ProxyExit::Detached);
    }

    #[test]
    fn attach_frame_wraps_custom_cmd_with_rich_term() {
        let frame = attach_frame("s1", Some("vim"), Some("xterm-256color"));
        assert_eq!(
            frame,
            ClientToDaemon::AttachCreate {
                session_id: "s1".to_string(),
                cmd: Some("env TERM=xterm-256color vim".to_string()),
            }
        );
    }

    #[test]
    fn attach_frame_without_term_leaves_cmd_untouched() {
        let frame = attach_frame("s1", Some("vim"), None);
        assert_eq!(
            frame,
            ClientToDaemon::AttachCreate {
                session_id: "s1".to_string(),
                cmd: Some("vim".to_string()),
            }
        );
        // Attach (no command) is unaffected by TERM.
        assert_eq!(
            attach_frame("s1", None, Some("xterm-256color")),
            ClientToDaemon::Attach { session_id: "s1".to_string() }
        );
    }

    #[test]
    fn with_term_prefix_does_not_double_wrap() {
        assert_eq!(
            with_term_prefix("bash -l", Some("xterm-256color")),
            "env TERM=xterm-256color bash -l"
        );
        // An explicit `env` command is left alone.
        assert_eq!(
            with_term_prefix("env FOO=1 vim", Some("xterm-256color")),
            "env FOO=1 vim"
        );
        // Empty command stays empty.
        assert_eq!(with_term_prefix("  ", Some("xterm-256color")), "  ");
    }

    #[test]
    fn default_term_is_rich() {
        let t = default_term();
        assert!(!t.is_empty());
        assert!(is_rich_term(&t), "default TERM {t:?} should be rich");
    }
}

//! A terminal session: one PTY child + its emulator + the attached clients.
//!
//! This is the aggregate the daemon holds. A [`Session`] owns a single
//! `portable-pty` child (the login shell, or a custom command), drives it
//! through a [`crate::emulator::Emulator`], and fans the child's output out to
//! every currently-attached client over a `tokio::broadcast` channel.
//!
//! # Architecture
//!
//! To keep the read path (which needs `&mut Pty`) separate from the write /
//! resize path (which needs `&Pty`), the `Pty` is moved into a background
//! **pump task** that owns it exclusively. The [`Session`] itself is a control
//! handle:
//!
//!   * `attach` / `detach` / `set_size` mutate the session's client set and the
//!     emulator (`&self`/`&mut self`, non-blocking).
//!   * `write` / `set_size` (primary) / `kill` send a [`PumpCmd`] over an
//!     MPSC channel to the pump, which applies it to the pty.
//!   * The pump loops on `pty.read()` (new child output) and `cmd_rx.recv()`
//!     (controls). New output is fed into the emulator under a brief std-mutex
//!     guard (no await while held) and then broadcast as a raw-byte
//!     [`SessionEvent::Output`]. On child exit the pump emits
//!     [`SessionEvent::Exit`] and stops.
//!
//! # Client / primary policy
//!
//! Clients are ordered by attach time; the **oldest** is the *primary* and owns
//! the pty winsize. Only the primary's `set_size` is applied to the pty (plus a
//! `SIGWINCH`, so TUIs re-read their size), but every client's size is recorded
//! so that when the primary detaches the next-oldest is promoted and *its* last
//! known size is applied immediately.
//!
//! # Replay
//!
//! `attach` returns a [`AttachHandle`]: a full-screen replay (screen + scrollback
//! serialised by [`Emulator::render_snapshot`]) plus a fresh broadcast receiver
//! that streams incremental [`SessionEvent::Output`] bytes afterwards. Buffered
//! events produced before the subscribe are discarded (they are already inside
//! the replay snapshot).

use crate::emulator::Emulator;
use crate::password::Protection;
use crate::pty::Pty;
use crate::winsize::Winsize;
use portable_pty::ExitStatus as PtyExitStatus;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;
use tokio::sync::{broadcast, mpsc};

/// Identifies an attached client. The daemon sets this to a per-connection id.
pub type ClientId = String;

/// An event the session broadcasts to every attached client.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Incremental raw bytes produced by the child's pty (feed to the client's
    /// renderer; the emulator has already ingested them).
    Output(Vec<u8>),
    /// The child exited with this status. No further `Output` events follow.
    Exit(PtyExitStatus),
}

/// The payload a fresh attach delivers: the replay (screen + scrollback) plus a
/// live receiver that streams subsequent incremental output.
#[derive(Debug)]
pub struct AttachHandle {
    /// Full-screen re-render (grid + scrollback) for the client to paint at once.
    pub redraw: Vec<u8>,
    /// Incremental output stream, starting at the moment of attach.
    pub receiver: broadcast::Receiver<SessionEvent>,
}

/// Control commands the pump applies to the pty it owns.
enum PumpCmd {
    /// Write raw input (keystrokes) to the child's stdin.
    Write(Vec<u8>),
    /// Apply a winsize to the pty and signal the foreground process group.
    Resize(Winsize),
    /// Terminate the child (SIGHUP to its process group).
    Kill,
}

/// An attached client, tracked in attach order with its last reported size.
#[derive(Clone, Debug)]
struct Client {
    id: ClientId,
    size: Winsize,
}

/// One PTY-backed terminal session and its clients.
pub struct Session {
    id: String,
    create_time: SystemTime,
    title: String,
    /// The child's OS process id, captured at spawn (used to query the live cwd).
    process_id: Option<u32>,
    /// Shared with the pump task; guards are held only for non-awaiting work.
    emulator: Arc<StdMutex<Emulator>>,
    /// Source of every [`broadcast::Receiver`] handed out on attach/subscribe.
    broadcast: broadcast::Sender<SessionEvent>,
    /// Channel to the pump task (which owns the pty).
    cmd_tx: mpsc::UnboundedSender<PumpCmd>,
    /// Attached clients in attach order; `clients[0]` is the primary.
    clients: StdMutex<Vec<Client>>,
    exited: Arc<AtomicBool>,
    exit_status: Arc<StdMutex<Option<PtyExitStatus>>>,
    /// Set when the session is password protected.
    protection: Option<Protection>,
}

impl Session {
    /// Spawn a new session around a `portable-pty` child and start its pump.
    ///
    /// `argv == None` runs the user's login shell; `argv == Some(...)` runs a
    /// specific command (`argv[0]` is the program). `scrollback_cap` is the
    /// number of scrollback lines the emulator retains.
    ///
    /// MUST be called from within a Tokio runtime (it spawns the pump task).
    pub fn spawn(
        name: &str,
        argv: Option<Vec<OsString>>,
        scrollback_cap: usize,
    ) -> anyhow::Result<Self> {
        let pty = Pty::spawn(argv.clone())?;
        // Capture the pid before the pty is moved into the pump task.
        let pid = pty.process_id();
        // Seed the emulator at the pty's current size so replay is dimensionally
        // correct before the primary client reports its own (by then
        // `set_size` re-flows).
        let size = pty.size().unwrap_or_else(|_| Winsize::new(24, 80));
        let emulator = Arc::new(StdMutex::new(Emulator::new(
            size.rows.max(1) as usize,
            size.cols.max(1) as usize,
            scrollback_cap,
        )));
        let (broadcast, _) = broadcast::channel(1024);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<PumpCmd>();
        let exited = Arc::new(AtomicBool::new(false));
        let exit_status = Arc::new(StdMutex::new(None));

        let pump_emulator = Arc::clone(&emulator);
        let pump_broadcast = broadcast.clone();
        let pump_exited = Arc::clone(&exited);
        let pump_status = Arc::clone(&exit_status);
        tokio::spawn(async move {
            run_pump(pty, cmd_rx, pump_broadcast, pump_emulator, pump_exited, pump_status).await;
        });

        Ok(Self {
            id: name.to_string(),
            create_time: SystemTime::now(),
            title: make_title(argv),
            process_id: pid,
            emulator,
            broadcast,
            cmd_tx,
            clients: StdMutex::new(Vec::new()),
            exited,
            exit_status,
            protection: None,
        })
    }

    /// The session's unique id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Change the session's id (used by rename; the registry key moves too).
    /// Safe only while no client is attached — attached connections bind to
    /// the name they attached with, so `Registry::rename` refuses that case.
    pub fn set_id(&mut self, id: impl Into<String>) {
        self.id = id.into();
    }

    /// The child's OS process id, if the system reported one. Used by the daemon
    /// to query the session's live cwd (and other process state).
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    /// When the session was created.
    pub fn create_time(&self) -> SystemTime {
        self.create_time
    }

    /// Require `password` for attach, kill, rename and cwd from now on.
    pub fn protect(&mut self, password: &str) {
        self.protection = Some(Protection::new(password));
    }

    /// The session's password protection, if it has one.
    pub fn protection(&self) -> Option<&Protection> {
        self.protection.as_ref()
    }

    /// The child command the session runs (used as its display title).
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Number of currently-attached clients.
    pub fn attached(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// The primary client (oldest attached), if any.
    pub fn primary(&self) -> Option<ClientId> {
        self.clients.lock().unwrap().first().map(|c| c.id.clone())
    }

    /// True once the child has exited.
    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// The child's exit status, once it has exited.
    pub fn exit_status(&self) -> Option<PtyExitStatus> {
        self.exit_status.lock().unwrap().clone()
    }

    /// Current emulator viewport dimensions `(rows, cols)`.
    pub fn dimensions(&self) -> (usize, usize) {
        let emu = self.emulator.lock().unwrap();
        (emu.rows(), emu.cols())
    }

    /// A fresh receiver that observes the session's output event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        let mut rx = self.broadcast.subscribe();
        // Drop events buffered before this subscription (the caller usually has
        // a replay snapshot that already covers them).
        while rx.try_recv().is_ok() {}
        rx
    }

    /// Attach a client: returns the full-screen replay plus a live receiver.
    ///
    /// The client is added to the attached set; the oldest attached client is
    /// the primary. The replay is a serialised screen + scrollback sized to the
    /// emulator; the receiver streams all subsequent incremental output.
    pub fn attach(&mut self, client_id: ClientId) -> anyhow::Result<AttachHandle> {
        if self.exited.load(Ordering::SeqCst) {
            anyhow::bail!("session has exited; cannot attach");
        }
        let redraw = self.emulator.lock().unwrap().render_snapshot();
        let mut receiver = self.broadcast.subscribe();
        // Discard any events buffered before attach (the replay snapshot covers
        // them); the receiver is now positioned at the live edge.
        while receiver.try_recv().is_ok() {}
        {
            let mut clients = self.clients.lock().unwrap();
            if !clients.iter().any(|c| c.id == client_id) {
                clients.push(Client {
                    id: client_id,
                    size: Winsize::new(24, 80),
                });
            }
        }
        Ok(AttachHandle { redraw, receiver })
    }

    /// Detach a client. If it was the primary, promote the next-oldest and
    /// immediately apply that client's last-known size to the pty.
    pub fn detach(&mut self, client_id: &ClientId) {
        let next_size = {
            let mut clients = self.clients.lock().unwrap();
            let was_primary = clients.first().map(|c| c.id == *client_id).unwrap_or(false);
            clients.retain(|c| c.id != *client_id);
            // If the primary departed, promote the next-oldest and apply ITS
            // last-known size immediately.
            if was_primary {
                clients.first().map(|c| c.size)
            } else {
                None
            }
        };
        if let Some(ws) = next_size {
            self.cmd_tx
                .send(PumpCmd::Resize(ws))
                .ok();
            self.emulator
                .lock()
                .unwrap()
                .resize(ws.rows.max(1) as usize, ws.cols.max(1) as usize);
        }
    }

    /// Write raw input bytes (keystrokes) to the pty. Any client may write.
    pub fn write(&self, bytes: &[u8], _client_id: &ClientId) -> anyhow::Result<()> {
        if self.exited.load(Ordering::SeqCst) {
            anyhow::bail!("session has exited");
        }
        self.cmd_tx
            .send(PumpCmd::Write(bytes.to_vec()))
            .map_err(|_| anyhow::anyhow!("session is closed"))
    }

    /// Resize the session. Only the primary client's size is applied to the pty
    /// (with a `SIGWINCH` so the foreground program re-reads it) and re-flows
    /// the emulator; non-primary sizes are recorded so an upcoming promotion
    /// applies them.
    pub fn set_size(&mut self, rows: u16, cols: u16, client_id: &ClientId) -> anyhow::Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let ws = Winsize::new(rows, cols);
        let is_primary = {
            let mut clients = self.clients.lock().unwrap();
            let is_primary = clients.first().map(|c| c.id == *client_id).unwrap_or(false);
            if let Some(c) = clients.iter_mut().find(|c| c.id == *client_id) {
                c.size = ws;
            }
            is_primary
        };
        if is_primary {
            // Only the primary drives the pty geometry.
            self.cmd_tx
                .send(PumpCmd::Resize(ws))
                .map_err(|_| anyhow::anyhow!("session is closed"))?;
            self.emulator
                .lock()
                .unwrap()
                .resize(rows as usize, cols as usize);
        }
        Ok(())
    }

    /// Terminate the child. On the next pump tick the exit status is observed,
    /// broadcast as [`SessionEvent::Exit`], and the session is closed.
    pub fn kill(&mut self) {
        let _ = self.cmd_tx.send(PumpCmd::Kill);
    }
}

/// Build a display title from the spawn argv.
fn make_title(argv: Option<Vec<OsString>>) -> String {
    match argv {
        Some(a) if !a.is_empty() => {
            let parts: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
            parts.join(" ")
        }
        _ => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "shell".to_string());
            shell
        }
    }
}

/// The pump task: owns the pty, applies control commands, and fans child output
/// into the emulator + broadcast. Runs until the child exits (read EOF) or the
/// session's command channel closes (the Session was dropped).
async fn run_pump(
    mut pty: Pty,
    mut cmd_rx: mpsc::UnboundedReceiver<PumpCmd>,
    bcast: broadcast::Sender<SessionEvent>,
    emulator: Arc<StdMutex<Emulator>>,
    exited: Arc<AtomicBool>,
    exit_status: Arc<StdMutex<Option<PtyExitStatus>>>,
) {
    loop {
        // `pty.read()` borrows the pty (read path, `&mut`); `cmd_rx.recv()`
        // borrows the channel (`&mut`). These are disjoint values, so the
        // `select!` can poll both at once. Neither arm body touches `pty`, so
        // the later `pty.write`/`resize`/`kill` happen after the read future is
        // dropped (borrow released) — no aliasing conflict.
        //
        // The two branches are told apart by `out`: an `Some` means the READ
        // branch won (cmd was never polled this round, so `cmd` stays `None`);
        // `None` means the CMD branch won. A polled-and-returned-`None` cmd is
        // the only signal that the sender was dropped (Session torn down).
        let mut cmd: Option<PumpCmd> = None;
        let mut out: Option<Option<Vec<u8>>> = None;
        tokio::select! {
            c = cmd_rx.recv() => {
                cmd = c;
            }
            o = pty.read() => {
                out = Some(o);
            }
        }

        if let Some(out) = out {
            match out {
                Some(data) => {
                    // Feed the emulator, then broadcast the raw bytes to all clients.
                    emulator.lock().unwrap().ingest(&data);
                    let _ = bcast.send(SessionEvent::Output(data));
                }
                None => {
                    // Child closed its output side: it is exiting. Reap below.
                    break;
                }
            }
        } else {
            // CMD branch won.
            match cmd {
                Some(cmd) => match cmd {
                    PumpCmd::Write(bytes) => {
                        let _ = pty.write(&bytes);
                    }
                    PumpCmd::Resize(ws) => {
                        let _ = pty.resize(ws);
                    }
                    PumpCmd::Kill => {
                        let _ = pty.kill();
                    }
                },
                // The command sender was dropped: this Session is being torn down.
                None => {
                    let _ = pty.kill();
                    break;
                }
            }
        }
    }

    // Child is gone: report the exit status and stop. This is the last event;
    // no further `Output` events are produced.
    let status = pty.wait().await;
    exited.store(true, Ordering::SeqCst);
    *exit_status.lock().unwrap() = Some(status.clone());
    let _ = bcast.send(SessionEvent::Exit(status));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn shell_argv(snippet: &str) -> Vec<OsString> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        vec![
            OsString::from(shell),
            OsString::from("-c"),
            OsString::from(snippet),
        ]
    }

    // GATE: a real shell child; attach two in-memory clients; assert BOTH see
    // output and that input from client B reaches the pty (verified by the
    // child echoing it back and both clients observing that echo).
    #[tokio::test]
    async fn attach_two_clients_see_output_and_b_input_reaches_pty() {
        // `cat` reads stdin and echoes it, so input from B round-trips to a
        // broadcast Output event both clients observe.
        let argv = shell_argv("cat");
        let mut session = Session::spawn("test-session", Some(argv), 2000).expect("spawn session");

        let AttachHandle { redraw: _ra, receiver: mut rx_a } =
            session.attach("a".into()).expect("attach a");
        let AttachHandle { redraw: _rb, receiver: mut rx_b } =
            session.attach("b".into()).expect("attach b");

        // Input from client B must reach the pty.
        let b_id = ClientId::from("b");
        session
            .write(b"hello-from-b\n", &b_id)
            .expect("write from b");

        let probe = b"hello-from-b";
        let mut a_out = Vec::new();
        let mut b_out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);

        while std::time::Instant::now() < deadline {
            let a_hit = a_out.windows(probe.len()).any(|w| w == probe);
            let b_hit = b_out.windows(probe.len()).any(|w| w == probe);
            if a_hit && b_hit {
                break;
            }
            tokio::select! {
                ev = rx_a.recv() => {
                    if let Ok(SessionEvent::Output(bytes)) = ev {
                        a_out.extend_from_slice(&bytes);
                    }
                }
                ev = rx_b.recv() => {
                    if let Ok(SessionEvent::Output(bytes)) = ev {
                        b_out.extend_from_slice(&bytes);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }

        let a = String::from_utf8_lossy(&a_out);
        let b = String::from_utf8_lossy(&b_out);
        assert!(
            a.contains("hello-from-b"),
            "client A should see output from the shell child, got: {a:?}"
        );
        assert!(
            b.contains("hello-from-b"),
            "client B should see output from the shell child, got: {b:?}"
        );
    }

    // Only the primary client's size is applied to the pty/emulator; a
    // non-primary resize is recorded but does not change the viewport.
    #[tokio::test]
    async fn set_size_primary_only() {
        let argv = shell_argv("cat");
        let mut session = Session::spawn("test-session", Some(argv), 2000).expect("spawn session");
        session.attach("primary".into()).expect("attach primary");
        session.attach("other".into()).expect("attach other");

        // Non-primary resize must NOT reshape the viewport.
        session
            .set_size(50, 200, &"other".into())
            .expect("non-primary set_size");
        assert_eq!(session.dimensions().0, 24, "non-primary must not resize");
        assert_eq!(session.dimensions().1, 80, "non-primary must not resize");

        // Primary resize is applied.
        session
            .set_size(50, 200, &"primary".into())
            .expect("primary set_size");
        assert_eq!(session.dimensions().0, 50, "primary should resize rows");
        assert_eq!(session.dimensions().1, 200, "primary should resize cols");
    }

    // When the primary detaches, the next-oldest is promoted and its last-known
    // size is applied to the pty immediately.
    #[tokio::test]
    async fn detach_promotes_next_and_applies_its_size() {
        let argv = shell_argv("cat");
        let mut session = Session::spawn("test-session", Some(argv), 2000).expect("spawn session");
        session.attach("old".into()).expect("attach old"); // primary at 24x80
        session.attach("new".into()).expect("attach new");
        session
            .set_size(60, 240, &"new".into())
            .expect("record new's size");

        // The primary "old" detaches; "new" is promoted and its 60x240 applied.
        session.detach(&"old".into());
        assert_eq!(session.primary().as_deref(), Some("new"));
        assert_eq!(session.dimensions().0, 60, "promoted size rows applied");
        assert_eq!(session.dimensions().1, 240, "promoted size cols applied");
    }
}

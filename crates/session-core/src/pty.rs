//! PTY transport backed by portable-pty.
//!
//! Owns spawning the child (the user's login shell, or a custom command from
//! `session <id> <cmd...>`) plus the master end that drives it: an
//! async-friendly read stream, a write path, winsize control, and child-exit
//! tracking.
//!
//! Layout of the three dedicated threads:
//!   * read  — a blocking thread reads bytes off the master and forwards them
//!             over a `tokio::sync::mpsc` channel, so the caller can
//!             `recv().await` (or `blocking_recv()` in a sync context) without
//!             tying up the async runtime.
//!   * write — a thread consumes an unbounded channel of byte chunks and
//!             writes them to the master. Dropping the channel (or the `Pty`)
//!             drops the master writer, which sends EOF to the slave.
//!   * wait  — a thread blocks on `Child::wait` and reports the exit status
//!             (async via channel, plus an idempotent sync read back) so the
//!             session can close and broadcast.
//!
//! The master fd produced by portable-pty is BLOCKING (only FD_CLOEXEC is set),
//! so reads block until data or EOF and never yield a spurious `WouldBlock`.

use crate::winsize::Winsize;
use portable_pty::{
    native_pty_system, ChildKiller, CommandBuilder, ExitStatus as PtyExitStatus, MasterPty,
    PtySize,
};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc;

/// A single PTY-backed child process and the master side that drives it.
pub struct Pty {
    /// Master end, retained (and usable via `&self`) for resize/winsize control
    /// and to keep the pty alive for the lifetime of the session.
    master: Box<dyn MasterPty + Send>,
    /// Output bytes produced by the child (read thread → caller).
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Input bytes queued for the child (caller → write thread).
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Asynchronous child-exit notification (wait thread → caller).
    exit_rx: mpsc::UnboundedReceiver<PtyExitStatus>,
    /// Idempotent sync copy of the exit status, set once by the wait thread.
    exit_status: Arc<Mutex<Option<PtyExitStatus>>>,
    /// Cheap "is the child gone?" flag, set once by the wait thread.
    exited: Arc<AtomicBool>,
    /// The child's process id, if the system reported one.
    process_id: Option<u32>,
    /// Handle to kill the child independently of the blocked wait thread.
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The child's working directory, best-effort (for `session list`/titles).
    cwd: Option<PathBuf>,
}

impl Pty {
    /// Spawn a PTY child and return the driving `Pty`.
    ///
    /// `argv == None` runs the user's login shell (portable-pty resolves it via
    /// `$SHELL` and the password database on Unix). `argv == Some(...)` runs a
    /// specific command where `argv[0]` is the program and `argv[1..]` are its
    /// arguments — the same shape as `session <id> <cmd...>`.
    pub fn spawn(argv: Option<Vec<OsString>>) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        // Portable-pty defaults to 24x80; the session card re-sizes on attach.
        let pair = pty_system.openpty(PtySize::default())?;
        let master = pair.master;
        let slave = pair.slave;

        let cmd = build_command(argv);
        let mut child = slave.spawn_command(cmd)?;

        let process_id = child.process_id();
        let killer = child.clone_killer();
        // Best-effort capture of the child's cwd for `list`/titles. The child
        // inherits this process's cwd, so record it once at spawn time.
        let cwd = std::env::current_dir().ok();

        // output: reader thread -> rx
        let (out_tx, rx) = mpsc::unbounded_channel();
        let reader = master.try_clone_reader()?;
        spawn_thread("pty-read", move || read_loop(reader, out_tx));

        // input: writer thread <- tx
        let (writer_tx, in_rx) = mpsc::unbounded_channel();
        let writer = master.take_writer()?;
        spawn_thread("pty-write", move || write_loop(writer, in_rx));

        // child-exit: waiter thread -> exit channel + sync flag/status
        let (exit_tx, exit_rx) = mpsc::unbounded_channel();
        let exit_status: Arc<Mutex<Option<PtyExitStatus>>> = Arc::new(Mutex::new(None));
        let exited: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        {
            let exit_status = Arc::clone(&exit_status);
            let exited = Arc::clone(&exited);
            spawn_thread("pty-wait", move || {
                let status = match child.wait() {
                    Ok(st) => st,
                    Err(err) => {
                        log::warn!("pty child wait failed: {err}");
                        PtyExitStatus::with_exit_code(1)
                    }
                };
                exited.store(true, Ordering::SeqCst);
                *exit_status.lock().unwrap() = Some(status.clone());
                let _ = exit_tx.send(status);
            });
        }

        Ok(Self {
            master,
            rx,
            writer_tx,
            exit_rx,
            exit_status,
            exited,
            process_id,
            killer: Mutex::new(killer),
            cwd,
        })
    }

    /// Await the next chunk of child output. `None` means the child closed its
    /// output side (typically it exited).
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }

    /// Non-blocking read of any immediately-available output chunk.
    pub fn try_read(&mut self) -> Option<Vec<u8>> {
        self.rx.try_recv().ok()
    }

    /// Blocking read for sync callers and tests (must NOT be called from within
    /// an async runtime).
    pub fn read_blocking(&mut self) -> Option<Vec<u8>> {
        self.rx.blocking_recv()
    }

    /// Write bytes to the child's stdin. Non-blocking: the bytes are queued to
    /// the writer thread. Returns `BrokenPipe` once the child has gone.
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer_tx
            .send(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pty closed"))
    }

    /// Set the pty winsize and notify the child so the foreground process
    /// re-reads its size. The master ioctl performs the `TIOCSWINSZ`; we then
    /// deliver `SIGWINCH` to the foreground process group explicitly so TUI
    /// programs resize even on platforms that don't signal on the ioctl.
    pub fn resize(&self, ws: Winsize) -> anyhow::Result<()> {
        let size = PtySize {
            rows: ws.rows,
            cols: ws.cols,
            pixel_width: ws.xpixel,
            pixel_height: ws.ypixel,
        };
        self.master.resize(size)?;
        #[cfg(unix)]
        {
            if let Some(pgid) = self.master.process_group_leader() {
                unsafe {
                    unix_sig::sigwinch_group(pgid);
                }
            }
        }
        Ok(())
    }

    /// The child's current winsize, read back from the kernel.
    pub fn size(&self) -> anyhow::Result<Winsize> {
        let s = self.master.get_size()?;
        Ok(Winsize {
            rows: s.rows,
            cols: s.cols,
            xpixel: s.pixel_width,
            ypixel: s.pixel_height,
        })
    }

    /// The child's working directory, best-effort (used for `list`/titles).
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The child's process id, if the system reported one.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    /// True once the child has exited.
    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// A copy of the child's exit status, available idempotently once it exits.
    pub fn exit_status(&self) -> Option<PtyExitStatus> {
        self.exit_status.lock().unwrap().clone()
    }

    /// Await the child's exit status.
    pub async fn wait(&mut self) -> PtyExitStatus {
        match self.exit_rx.recv().await {
            Some(st) => st,
            None => PtyExitStatus::with_exit_code(1),
        }
    }

    /// Non-blocking: return the exit status if the child has already exited.
    pub fn try_wait(&mut self) -> Option<PtyExitStatus> {
        self.exit_rx.try_recv().ok()
    }

    /// Blocking wait for the child's exit status (for sync callers and tests;
    /// must NOT be called from within an async runtime).
    pub fn wait_blocking(&mut self) -> PtyExitStatus {
        match self.exit_rx.blocking_recv() {
            Some(st) => st,
            None => PtyExitStatus::with_exit_code(1),
        }
    }

    /// Terminate the child (portable-pty sends `SIGHUP` to the process group).
    pub fn kill(&mut self) -> io::Result<()> {
        self.killer.lock().unwrap().kill()
    }
}

/// Build the `CommandBuilder`: the login shell by default, else the custom argv.
fn build_command(argv: Option<Vec<OsString>>) -> CommandBuilder {
    match argv {
        None => CommandBuilder::new_default_prog(),
        Some(mut argv) => {
            if argv.is_empty() {
                CommandBuilder::new_default_prog()
            } else {
                let prog = argv.remove(0);
                let mut cb = CommandBuilder::new(prog);
                cb.args(argv);
                cb
            }
        }
    }
}

/// Read bytes off the master until EOF (portable-pty maps the closed-slave `EIO`
/// to `Ok(0)`), forwarding each chunk to the output channel. Exits whenever the
/// receiver has been dropped (the `Pty` is gone).
fn read_loop(mut reader: Box<dyn Read + Send>, out: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Consume queued input bytes and write them to the master writer. Dropping the
/// writer (on channel close or write failure) sends EOF to the slave.
fn write_loop(mut writer: Box<dyn Write + Send>, mut input: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(chunk) = input.blocking_recv() {
        if writer.write_all(&chunk).is_err() {
            break;
        }
    }
}

/// Spawn a detached, named thread; a failure to spawn is non-fatal here.
fn spawn_thread<F>(name: &str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    let _ = thread::Builder::new().name(name.to_string()).spawn(f);
}

/// Minimal raw-libc signal helper. We can't add `libc` to the manifest (the
/// bootstrap card owns every dependency), so `SIGWINCH` delivery is declared
/// directly against the C library that is always linked on Unix.
#[cfg(unix)]
mod unix_sig {
    use std::os::raw::c_int;

    /// `SIGWINCH` is 28 on every supported Unix target (Linux, macOS/iOS, BSDs).
    const SIGWINCH: c_int = 28;

    extern "C" {
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    /// Send `SIGWINCH` to the foreground process group (negative pgid). The
    /// return value is ignored by the caller: `ESRCH` just means the group is
    /// already gone.
    pub unsafe fn sigwinch_group(pgid: c_int) -> c_int {
        kill(-pgid, SIGWINCH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    // GATE: spawn `$SHELL -c 'echo hi'`, read the output bytes, then resize to a
    // new size and assert no error. Also confirms the read-back size and that
    // the child's exit status is observed.
    #[test]
    fn spawn_read_resize_and_exit() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let argv = vec![
            OsString::from(shell),
            OsString::from("-c"),
            OsString::from("echo hi"),
        ];
        let mut pty = Pty::spawn(Some(argv)).expect("spawn pty");

        // Read output until we've seen "hi" or the stream closes.
        let mut got: Vec<u8> = Vec::new();
        while let Some(chunk) = pty.read_blocking() {
            got.extend_from_slice(&chunk);
            if got.windows(2).any(|w| w == b"hi") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&got);
        assert!(text.contains("hi"), "expected child output 'hi', got: {text:?}");

        // Resize to a brand-new size; the winsize ioctl + SIGWINCH must not error.
        pty.resize(Winsize::new(50, 200)).expect("resize must not error");

        // The kernel must report back the new size.
        let read_back = pty.size().expect("get_size must not error");
        assert_eq!(read_back.rows, 50, "rows not applied");
        assert_eq!(read_back.cols, 200, "cols not applied");

        // The `echo` child exits 0; observe it via the wait path.
        let status = pty.wait_blocking();
        assert!(status.success(), "child should exit 0, got: {status}");
        assert!(pty.is_exited(), "is_exited should be true after wait");
        assert_eq!(pty.exit_status().map(|s| s.success()), Some(true));
    }
}

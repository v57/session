//! session-cli daemon launcher: ensure a daemon is running, starting one if the
//! socket is absent.
//!
//! The daemon is a long-lived, detached process that owns every session
//! ([`session_core::daemon`]). The CLI must never require the user to start it
//! by hand, so this module implements the "connect, or start it, then connect"
//! contract:
//!
//! 1. Try [`crate::client::Client::connect`]. If a live daemon already answers
//!    `Hello`, **connect wins** and we return immediately without spawning.
//! 2. If the socket is absent or a stale corpse (connect reports
//!    [`ConnectError::Io`]), spawn a detached `session --daemon` process.
//! 3. Wait for the socket to accept + answer `Hello`, with exponential backoff
//!    and a deadline. If it never comes up, report a clear error
//!    ("daemon did not start") so the caller can surface it gracefully.
//!
//! Spawning is abstracted behind [`DaemonSpawner`] so tests can run the daemon
//! in-process instead of requiring the compiled binary (whose `--daemon` branch
//! is wired by a sibling card). In real use [`CommandSpawner`] detaches the
//! `session --daemon` subprocess (own process group, null stdio) so it survives
//! the CLI exiting.

use crate::client::{self, Client, ConnectError};
use anyhow::{bail, Context, Result};
use session_core::config::Config;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Max time to wait for a freshly-spawned daemon to become reachable before
/// declaring "daemon did not start".
pub const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Exponential backoff bounds (milliseconds) while polling for the socket.
const RETRY_BASE_MS: u64 = 20;
const RETRY_MAX_MS: u64 = 250;

/// Abstraction for "launch a daemon process", so the launcher logic can be
/// unit-tested without the compiled `session --daemon` binary.
pub trait DaemonSpawner: Send + Sync {
    /// Start a daemon. Idempotent — callers may invoke it again if a daemon
    /// crashed during startup (the daemon's own singleton guard makes a second
    /// start a no-op).
    fn spawn(&self, config: &Config) -> Result<()>;
}

/// Spawn the real `session --daemon` binary, detached, and return immediately.
pub struct CommandSpawner;

impl DaemonSpawner for CommandSpawner {
    fn spawn(&self, _config: &Config) -> Result<()> {
        // `session --daemon` is this same binary ([[bin]] name = "session").
        let exe = std::env::current_exe().context("resolve own binary path")?;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Command::new(exe)
                .arg("--daemon")
                // Sessions inherit the daemon's environment: never the password.
                .env_remove(crate::password::ENV)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // Own process group: a SIGHUP delivered to the CLI's controlling
                // terminal foreground group won't also kill the daemon.
                .process_group(0)
                .spawn()
                .context("spawn `session --daemon`")?;
        }
        #[cfg(windows)]
        windows::spawn_daemon(&exe).context("spawn `session --daemon`")?;
        Ok(())
    }
}

/// Starting the daemon on Windows so it outlives whatever started it.
///
/// `std::process::Command` always lets the child inherit every inheritable
/// handle of this process, not just the three it is given. A daemon holding,
/// say, the pipe `session list` writes to, or an ssh session's channel, keeps
/// it open for as long as it runs, so whoever waits for EOF (PowerShell, sshd,
/// Shell's remote scan) waits forever. So the daemon is started with
/// `CreateProcessW` directly: no inherited handles, no std handles at all.
#[cfg(windows)]
mod windows {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    type Handle = *mut c_void;

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_len: u16,
        reserved2: *mut u8,
        std_input: Handle,
        std_output: Handle,
        std_error: Handle,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *const c_void,
            thread_attributes: *const c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *const c_void,
            current_directory: *const u16,
            startup_info: *const StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    /// A console of its own, without a window: console APIs (ConPTY) work, and
    /// closing the user's terminal, which sends CTRL_CLOSE_EVENT to everything
    /// on its console, doesn't end the daemon.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// Ctrl+C in the user's terminal never reaches the daemon.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    /// Leave the job of an ssh session or terminal, which may kill its
    /// processes when it ends. Not every job allows it, so it is tried first.
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    pub fn spawn_daemon(exe: &Path) -> std::io::Result<()> {
        let mut command_line = wide(OsStr::new(&format!("\"{}\" --daemon", exe.display())));
        command_line.push(0);
        let environment = environment_block();
        let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT;
        let startup = StartupInfoW {
            cb: std::mem::size_of::<StartupInfoW>() as u32,
            reserved: std::ptr::null_mut(),
            desktop: std::ptr::null_mut(),
            title: std::ptr::null_mut(),
            x: 0,
            y: 0,
            x_size: 0,
            y_size: 0,
            x_count_chars: 0,
            y_count_chars: 0,
            fill_attribute: 0,
            flags: 0,
            show_window: 0,
            reserved2_len: 0,
            reserved2: std::ptr::null_mut(),
            std_input: std::ptr::null_mut(),
            std_output: std::ptr::null_mut(),
            std_error: std::ptr::null_mut(),
        };
        for flags in [flags | CREATE_BREAKAWAY_FROM_JOB, flags] {
            let mut info = ProcessInformation {
                process: std::ptr::null_mut(),
                thread: std::ptr::null_mut(),
                process_id: 0,
                thread_id: 0,
            };
            // SAFETY: every pointer is valid for the call; CreateProcessW may
            // write to `command_line`, which is a mutable NUL-terminated buffer.
            let ok = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    command_line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    flags,
                    environment.as_ptr() as *const c_void,
                    std::ptr::null(),
                    &startup,
                    &mut info,
                )
            };
            if ok != 0 {
                unsafe {
                    CloseHandle(info.thread);
                    CloseHandle(info.process);
                }
                return Ok(());
            }
        }
        Err(std::io::Error::last_os_error())
    }

    /// This process's environment minus the session password, as a
    /// CreateProcessW block: `NAME=value\0` entries, ended by another `\0`.
    fn environment_block() -> Vec<u16> {
        let mut block = Vec::new();
        for (name, value) in std::env::vars_os() {
            if name.to_string_lossy().eq_ignore_ascii_case(crate::password::ENV) {
                continue;
            }
            block.extend(wide(&name));
            block.push(u16::from(b'='));
            block.extend(wide(&value));
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().collect()
    }
}

/// Ensure a daemon is reachable and return a connected, handshaked client.
///
/// - Already running (`Hello` answers) -> just connect (**connect wins**).
/// - Socket absent or stale -> spawn a detached daemon, wait (resilient
///   connect/Hello) for it to accept, then return a client.
/// - Never comes up -> error ("daemon did not start").
pub async fn ensure_daemon<S: DaemonSpawner>(config: &Config, spawner: &S) -> Result<Client> {
    ensure_daemon_with_timeout(config, spawner, SPAWN_WAIT_TIMEOUT).await
}

/// [`ensure_daemon`] with an explicit wait window (tests use a short one).
async fn ensure_daemon_with_timeout<S: DaemonSpawner>(
    config: &Config,
    spawner: &S,
    timeout: Duration,
) -> Result<Client> {
    // Fast path: is a daemon already up? (also handles a stale socket that a
    // new listener has already re-taken — connect wins either way).
    match client::Client::connect(config).await {
        Ok(client) => return Ok(client),
        // Socket absent / stale corpse / refused: a daemon is NOT up -> start one.
        Err(ConnectError::Io(_)) => {}
        Err(ConnectError::Version(e)) => {
            // A daemon from another session release is running. Starting a
            // second one would orphan its sessions; tell the user instead.
            bail!(
                "a session daemon from another version is running ({e}). Its sessions keep \
                 running; to switch, end them and that daemon (e.g. `pkill -f 'session --daemon'`, \
                 or end session.exe in Task Manager on Windows), then run this again"
            );
        }
        Err(e) => {
            // A daemon IS present but uncooperative (broken handshake). Do NOT
            // mask that by starting a second one.
            bail!(
                "daemon at {} is running but unresponsive: {e}",
                config.socket_path.display()
            );
        }
    }

    // Start a detached daemon, then wait for it to become reachable.
    spawner
        .spawn(config)
        .context("failed to start the session daemon")?;
    wait_for_daemon(config, timeout).await
}

/// Poll the socket with exponential backoff until a daemon answers `Hello` or
/// the deadline passes.
async fn wait_for_daemon(config: &Config, timeout: Duration) -> Result<Client> {
    let deadline = Instant::now() + timeout;
    let mut delay_ms = RETRY_BASE_MS;

    loop {
        match client::Client::connect(config).await {
            Ok(client) => return Ok(client),
            Err(e) => {
                if Instant::now() >= deadline {
                    bail!(
                        "session daemon did not become reachable on {} within {timeout:?}: {e}",
                        config.socket_path.display()
                    );
                }
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(RETRY_MAX_MS);
            }
        }
    }
}

/// Convenience using the real detached binary (the production path).
pub async fn ensure_daemon_running(config: &Config) -> Result<Client> {
    ensure_daemon(config, &CommandSpawner).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use session_core::config::Config;
    use session_core::protocol::{ClientToDaemon, DaemonToClient};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    /// Spawn the daemon in-process (no binary needed) and count launches.
    struct InProcessSpawner {
        config: Config,
        spawned: AtomicUsize,
    }

    impl InProcessSpawner {
        fn new(config: Config) -> Self {
            Self {
                config,
                spawned: AtomicUsize::new(0),
            }
        }
    }

    impl DaemonSpawner for InProcessSpawner {
        fn spawn(&self, _config: &Config) -> Result<()> {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            let cfg = self.config.clone();
            tokio::spawn(async move {
                let _ = session_core::daemon::run(cfg).await;
            });
            Ok(())
        }
    }

    fn temp_config(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::default();
        cfg.socket_dir = dir.path().to_path_buf();
        cfg.socket_path = dir.path().join("session.sock");
        cfg
    }

    // Wait for an in-process daemon to actually bind and accept Hello.
    async fn wait_reachable(cfg: &Config) {
        for _ in 0..250 {
            if crate::client::Client::connect(cfg).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("in-process daemon never became reachable");
    }

    // GATE: when the socket is absent, ensure_daemon must spawn exactly one
    // daemon, wait for it, and return a live (handshaked + List-able) client.
    #[tokio::test]
    async fn gate_ensure_daemon_starts_when_absent() {
        let dir = tempdir().unwrap();
        let cfg = temp_config(&dir);
        assert!(!cfg.socket_path.exists(), "socket must not pre-exist");

        let spawner = InProcessSpawner::new(cfg.clone());
        let mut client = ensure_daemon(&cfg, &spawner)
            .await
            .expect("ensure_daemon should start a daemon and connect");

        assert_eq!(
            spawner.spawned.load(Ordering::SeqCst),
            1,
            "ensure_daemon must spawn the daemon exactly once when absent"
        );
        // The returned client is already handshaked; prove it's live with List.
        client.send(ClientToDaemon::List).await.unwrap();
        let frame = client
            .recv()
            .await
            .unwrap()
            .expect("expected a Listing frame");
        assert!(
            matches!(frame, DaemonToClient::Listing { ref sessions } if sessions.is_empty()),
            "fresh daemon should list nothing, got {frame:?}"
        );
    }

    // When a daemon is already running, ensure_daemon must NOT spawn a new one
    // ("connect wins" path).
    #[tokio::test]
    async fn gate_connect_wins_when_daemon_already_running() {
        let dir = tempdir().unwrap();
        let cfg = temp_config(&dir);

        // Start a daemon directly, independent of the launcher.
        let daemon_cfg = cfg.clone();
        let daemon = tokio::spawn(async move {
            let _ = session_core::daemon::run(daemon_cfg).await;
        });
        wait_reachable(&cfg).await;

        let spawner = InProcessSpawner::new(cfg.clone());
        let mut client = ensure_daemon(&cfg, &spawner)
            .await
            .expect("ensure_daemon should connect to the running daemon");

        assert_eq!(
            spawner.spawned.load(Ordering::SeqCst),
            0,
            "connect-wins must NOT spawn a new daemon when one is already up"
        );
        client.send(ClientToDaemon::List).await.unwrap();
        let frame = client
            .recv()
            .await
            .unwrap()
            .expect("expected a Listing frame");
        assert!(matches!(frame, DaemonToClient::Listing { .. }));

        daemon.abort();
    }

    // ensure_daemon with no daemon and no way to start one must surface the
    // "daemon did not start" error (a do-nothing spawner), not hang.
    #[tokio::test]
    async fn daemon_does_not_start_is_an_error() {
        let dir = tempdir().unwrap();
        let cfg = temp_config(&dir);

        struct NoopSpawner;
        impl DaemonSpawner for NoopSpawner {
            fn spawn(&self, _config: &Config) -> Result<()> {
                Ok(()) // never actually starts a daemon
            }
        }

        // The default 10s timeout would make this slow; use a short window.
        let spawner = NoopSpawner;
        let result =
            ensure_daemon_with_timeout(&cfg, &spawner, Duration::from_millis(300)).await;
        let msg = match result {
            Ok(_client) => panic!("noop spawner should never produce a running daemon"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("did not become reachable") || msg.contains("did not start"),
            "expected a clear daemon-not-started error, got: {msg}"
        );
    }
}

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
        let mut cmd = Command::new(exe);
        cmd.arg("--daemon")
            // Sessions inherit the daemon's environment: never the password.
            .env_remove(crate::password::ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group: a SIGHUP delivered to the CLI's controlling
            // terminal foreground group won't also kill the daemon.
            cmd.process_group(0);
        }
        cmd.spawn().context("spawn `session --daemon`")?;
        Ok(())
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

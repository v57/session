//! Daemon configuration: per-user socket path, scrollback cap.
//! Fleshed out by the registry/config card (will use `dirs` for the socket path).
//!
//! Defaults are baked in (user may flip): per-user socket, scrollback cap 2000.

use std::path::PathBuf;

/// Daemon defaults. The user may flip these (see KANBAN.md).
#[derive(Debug, Clone)]
pub struct Config {
    /// Scrollback lines kept by the emulator. Default 2000.
    pub scrollback_cap: usize,
    /// Local socket path the daemon listens on and the CLI connects to.
    pub socket_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scrollback_cap: 2000,
            socket_path: PathBuf::from("/tmp/session.sock"),
        }
    }
}

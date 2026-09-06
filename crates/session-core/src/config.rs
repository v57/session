//! Daemon configuration: per-user socket dir/path, scrollback cap, idle policy.
//!
//! The local socket lives in a per-user directory so different accounts never
//! collide and the socket survives a reboot:
//!
//!   * Unix: `$XDG_RUNTIME_DIR` if set, else the platform per-user local data
//!     dir via [`dirs`] (macOS `~/Library/Application Support`) + `/session`.
//!   * Windows: `%LOCALAPPDATA%\session` (again via [`dirs`]).
//!
//! Scrollback cap and idle policy are baked-in defaults the user may flip via an
//! optional JSON config file (see [`Config::load`]).

use std::path::PathBuf;
use std::time::Duration;

/// Default scrollback lines each session's emulator retains.
pub const DEFAULT_SCROLLBACK_CAP: usize = 2000;
/// Default socket file name inside the socket dir.
pub const DEFAULT_SOCKET_NAME: &str = "session.sock";
/// Optional config file name inside the platform config dir.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// What to do with a session that has no attached clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdlePolicy {
    /// Never reap idle sessions (default). A session persists until killed.
    None,
    /// Reap a session once it has been idle (no attached clients) this long.
    After(Duration),
}

/// Daemon configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Per-user directory that holds the socket (and any per-user state).
    pub socket_dir: PathBuf,
    /// Local socket path the daemon listens on and the CLI connects to.
    pub socket_path: PathBuf,
    /// Scrollback lines retained by each session's emulator.
    pub scrollback_cap: usize,
    /// Idle session policy; [`IdlePolicy::None`] by default.
    pub idle_policy: IdlePolicy,
}

impl Default for Config {
    fn default() -> Self {
        let socket_dir = default_socket_dir();
        let socket_path = socket_dir.join(DEFAULT_SOCKET_NAME);
        Self {
            socket_dir,
            socket_path,
            scrollback_cap: DEFAULT_SCROLLBACK_CAP,
            idle_policy: IdlePolicy::None,
        }
    }
}

impl Config {
    /// Load the config: platform defaults, overridden by an optional JSON file
    /// in the platform config dir. Any I/O or parse error falls back to
    /// defaults, so the daemon always starts with a usable config.
    pub fn load() -> Self {
        let mut cfg = Config::default();
        let Some(path) = config_file_path() else {
            return cfg;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return cfg;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return cfg;
        };
        if let Some(sc) = value.get("scrollback_cap").and_then(|v| v.as_u64()) {
            cfg.scrollback_cap = sc as usize;
        }
        if let Some(secs) = value.get("idle_reap_seconds").and_then(|v| v.as_u64()) {
            if secs > 0 {
                cfg.idle_policy = IdlePolicy::After(Duration::from_secs(secs));
            }
        }
        cfg
    }
}

/// Resolve the per-user base directory for the socket and session state.
///
/// On Unix we prefer `$XDG_RUNTIME_DIR` (a per-user, per-login runtime dir
/// intended exactly for sockets). Otherwise — and always on Windows — we fall
/// back to the platform's per-user local data directory (via [`dirs`]) joined
/// with a `session` subdirectory, so the socket is namespaced to this tool.
fn default_socket_dir() -> PathBuf {
    #[cfg(unix)]
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return PathBuf::from(runtime).join("session");
        }
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("session")
}

/// Path to the optional config file, if the platform has a config dir.
fn config_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("session").join(CONFIG_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.scrollback_cap, DEFAULT_SCROLLBACK_CAP);
        assert_eq!(cfg.idle_policy, IdlePolicy::None);
        // The socket always lives inside the socket dir and is a socket file.
        assert!(cfg.socket_path.starts_with(&cfg.socket_dir));
        assert_eq!(
            cfg.socket_path.file_name().and_then(|f| f.to_str()),
            Some(DEFAULT_SOCKET_NAME)
        );
    }

    #[test]
    fn idle_policy_variants_compare() {
        assert_eq!(
            IdlePolicy::After(Duration::from_secs(300)),
            IdlePolicy::After(Duration::from_secs(300))
        );
        assert_ne!(IdlePolicy::None, IdlePolicy::After(Duration::from_secs(1)));
    }
}

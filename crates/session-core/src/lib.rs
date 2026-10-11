//! session-core: the engine behind the `session` persistent-session proxy.
//!
//! This crate owns the daemon side of a keybinding-free, faithful session
//! multiplexer: a portable-pty child, a vte-driven emulator grid + scrollback,
//! a session registry, and a cross-platform local socket the CLI attaches to.
//!
//! The modules below are wired by the bootstrap card as STUBS and fleshed out by
//! later engine cards. Nothing here is complete yet.
#![allow(dead_code)]

pub mod config;
pub mod cwd;
pub mod daemon;
pub mod emulator;
pub mod password;
pub mod protocol;
pub mod pty;
pub mod registry;
pub mod session;
pub mod winsize;

/// Crate version, surfaced to the CLI.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::winsize::Winsize;

    #[test]
    fn winsize_smoke() {
        let ws = Winsize::new(24, 80);
        assert_eq!(ws.rows, 24);
        assert_eq!(ws.cols, 80);
    }
}

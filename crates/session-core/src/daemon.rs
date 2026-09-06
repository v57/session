//! Daemon server: owns all sessions and accepts client connections over the
//! cross-platform local socket. Fleshed out by the daemon card.
//!
//! Socket decision (spiked on the bootstrap card): interprocess for a unified
//! local stream (unix socket on macOS/Linux, named pipe on Windows). If the
//! spike showed interprocess is too heavy, this card falls back to a
//! `tokio` + `cfg(OS)` approach and notes it here. See the comment at the bottom
//! of this module for the resolved choice.

/// Placeholder daemon. The daemon card implements the listener + broadcast loop
/// (tokio + interprocess, or tokio + cfg(OS)) for all sessions.
pub struct Daemon;

impl Daemon {
    pub fn new() -> Self {
        Self
    }
}

// SPIKE RESULT (bootstrap): interprocess is declared with the `tokio` feature
// above and builds cleanly on macOS. It is retained as the cross-platform local
// stream abstraction (unix socket on macOS/Linux, named pipe on Windows). No
// cfg(OS) fallback is needed.

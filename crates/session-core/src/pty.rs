//! PTY transport backed by portable-pty. Fleshed out by the PTY transport card.
//!
//! Owns spawning the child (login shell or a custom command) and the master fd
//! that yields the child's output and accepts stdin + winsize control.

/// Placeholder PTY transport.
/// The PTY-transport card replaces this with a real portable-pty-backed
/// implementation.
pub struct PtyTransport;

impl PtyTransport {
    pub fn new() -> Self {
        Self
    }
}

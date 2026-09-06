//! Serde message frames between the daemon and attached clients.
//! Fleshed out by the protocol card.
//!
//! Frames carry the full-screen redraw (grid + scrollback replay), incremental
//! output bytes, winsize reports, and the session-created handshake.

/// Frame kind — a placeholder for the real serde-serialized protocol envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Full-screen redraw (replay of grid + scrollback) sent on attach/repaint.
    Redraw,
    /// Incremental output bytes to broadcast to all attached clients.
    Data,
    /// A client reporting its new terminal size.
    Resize,
    /// Session-created handshake.
    Hello,
}

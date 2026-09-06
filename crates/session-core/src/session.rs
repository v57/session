//! A session = one PTY child + its emulator + the set of attached clients.
//! Fleshed out by the session-type card.
//!
//! This is where the full-emulator + replay model lives: output is fed through
//! the emulator, and a full redraw is re-emitted on every attach/repaint.

/// Placeholder session. The session-type card implements the real thing:
/// pty + emulator + clients + broadcast.
pub struct Session;

impl Session {
    pub fn new() -> Self {
        Self
    }
}

//! Primary-client winsize authority. Pure-logic, unit-testable (no pty needed).
//! Fleshed out by the winsize card.
//!
//! The OLDEST attached client (the primary) owns the pty winsize; on its
//! disconnect the next-oldest size is applied immediately.

/// Terminal size in character cells, with optional pixel dims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Winsize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            xpixel: 0,
            ypixel: 0,
        }
    }
}

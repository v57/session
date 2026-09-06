//! Terminal emulator grid + scrollback, driven by vte. Fleshed out by the
//! emulator card, which will pick between the pre-declared `vte` and `vt100`
//! crates without a manifest edit (default recommended: custom vte grid).

/// Placeholder emulator state (grid + scrollback). The emulator card owns the
/// real vte-driven grid and reflow/wrap handling.
pub struct Emulator;

impl Emulator {
    pub fn new() -> Self {
        Self
    }
}

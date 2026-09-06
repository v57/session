//! Primary-client winsize authority. Pure logic, unit-testable (no pty needed).
//!
//! The OLDEST attached client is the *primary* and is the sole owner of the pty
//! winsize. A resize request from any other client is recorded (so its last-known
//! size is available when it is later promoted) but is NOT applied to the pty.
//! When the primary detaches, the next-oldest client is promoted and its last-known
//! size is returned for immediate application.
//!
//! This module is deliberately free of any `portable-pty` dependency: it is a pure
//! state machine over the client set. The session layer decides *how* to apply the
//! returned size (resize the pty + reflow the emulator), this module decides *whether*
//! and *to what*.

/// Terminal size in character cells, with optional pixel dims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Winsize {
    /// A size in character cells with no pixel dimensions.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            xpixel: 0,
            ypixel: 0,
        }
    }
}

/// What a client's `set_size` request should do to the pty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeEffect {
    /// The caller is the primary: resize the pty to this size (and reflow).
    Apply(Winsize),
    /// The caller is not the primary: the size was recorded so it can be applied
    /// on a later promotion, but the pty must NOT be resized right now.
    Recorded,
}

/// A tracked client: its id plus its last-reported size.
#[derive(Clone, Debug)]
struct Client {
    id: String,
    size: Winsize,
}

/// The primary-client winsize authority.
///
/// Clients are kept in attach order; index 0 is always the primary. `set_size`
/// records every client's size but only reports `Apply` for the primary;
/// `on_detach` removes a client and, when the primary departed, returns the
/// promoted client's last-known size so the pty can be resized immediately.
#[derive(Debug, Default)]
pub struct WinsizeAuthority {
    clients: Vec<Client>,
}

impl WinsizeAuthority {
    /// An empty authority (no attached clients).
    pub fn new() -> Self {
        Self {
            clients: Vec::new(),
        }
    }

    /// Register a freshly-attached client in attach order.
    ///
    /// Idempotent: re-attaching an id that is already present keeps its original
    /// attach position and size, so it does not disturb the primary ordering.
    pub fn attach(&mut self, client_id: &str) {
        if !self.clients.iter().any(|c| c.id == client_id) {
            self.clients.push(Client {
                id: client_id.to_string(),
                size: Winsize::new(24, 80),
            });
        }
    }

    /// The number of currently-tracked clients.
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Whether there are no tracked clients.
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// The current primary (the oldest attached client), if any.
    pub fn primary(&self) -> Option<&str> {
        self.clients.first().map(|c| c.id.as_str())
    }

    /// Whether the given client is currently the primary.
    pub fn is_primary(&self, client_id: &str) -> bool {
        self.clients.first().map(|c| c.id == client_id).unwrap_or(false)
    }

    /// The last-known size of a client, if it is tracked.
    pub fn size_of(&self, client_id: &str) -> Option<Winsize> {
        self.clients.iter().find(|c| c.id == client_id).map(|c| c.size)
    }

    /// Record a client's desired size and report the pty effect.
    ///
    /// Every tracked client's size is stored (so it is available on promotion),
    /// but only the primary's request yields [`SizeEffect::Apply`].
    pub fn set_size(&mut self, client_id: &str, rows: u16, cols: u16) -> SizeEffect {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let ws = Winsize::new(rows, cols);
        let is_primary = self.is_primary(client_id);
        if let Some(c) = self.clients.iter_mut().find(|c| c.id == client_id) {
            c.size = ws;
        }
        if is_primary {
            SizeEffect::Apply(ws)
        } else {
            SizeEffect::Recorded
        }
    }

    /// Remove a client and, if it was the primary, report the next-oldest
    /// client's last-known size for immediate application.
    ///
    /// Returns `Some(size)` only when the primary departed and a client remains;
    /// otherwise `None` (the departed client was not the primary, no client was
    /// removed, or the last client just left).
    pub fn on_detach(&mut self, client_id: &str) -> Option<Winsize> {
        let was_primary = self.is_primary(client_id);
        self.clients.retain(|c| c.id != client_id);
        if was_primary {
            self.clients.first().map(|c| c.size)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winsize_smoke() {
        let ws = Winsize::new(24, 80);
        assert_eq!(ws.rows, 24);
        assert_eq!(ws.cols, 80);
        assert_eq!(ws.xpixel, 0);
        assert_eq!(ws.ypixel, 0);
    }

    // The very first client to attach becomes the primary and owns the pty size.
    #[test]
    fn first_client_wins() {
        let mut a = WinsizeAuthority::new();
        a.attach("a");
        a.attach("b");
        assert_eq!(a.primary(), Some("a"));
        assert!(a.is_primary("a"));
        assert!(!a.is_primary("b"));

        // The first client's size is applied.
        assert_eq!(
            a.set_size("a", 40, 120),
            SizeEffect::Apply(Winsize::new(40, 120))
        );
    }

    // A resize from a non-primary client is recorded but must not reshape the pty.
    #[test]
    fn non_primary_ignored() {
        let mut a = WinsizeAuthority::new();
        a.attach("a"); // primary
        a.attach("b");

        // b resizes: recorded, but NOT applied.
        assert_eq!(a.set_size("b", 50, 200), SizeEffect::Recorded);
        assert_eq!(a.size_of("b"), Some(Winsize::new(50, 200)));
        // The pty still belongs to a at a's size.
        assert_eq!(a.primary(), Some("a"));

        // a's own resize is applied.
        assert_eq!(
            a.set_size("a", 30, 100),
            SizeEffect::Apply(Winsize::new(30, 100))
        );
    }

    // Detaching a non-primary must not promote or touch the pty size.
    #[test]
    fn detach_non_primary_no_promotion() {
        let mut a = WinsizeAuthority::new();
        a.attach("a");
        a.attach("b");

        assert_eq!(a.on_detach("b"), None);
        assert_eq!(a.primary(), Some("a"));
        assert_eq!(a.len(), 1);
    }

    // When the primary detaches, the next-oldest is promoted and its last-known
    // size is returned so the pty can be resized immediately.
    #[test]
    fn handoff_on_disconnect() {
        let mut a = WinsizeAuthority::new();
        a.attach("old"); // primary
        a.attach("new");

        // new reports its size; recorded but not applied while old is primary.
        assert_eq!(a.set_size("new", 60, 240), SizeEffect::Recorded);

        // old detaches -> new is promoted and its size is applied immediately.
        let promoted = a.on_detach("old");
        assert_eq!(promoted, Some(Winsize::new(60, 240)));
        assert_eq!(a.primary(), Some("new"));

        // After promotion, new's subsequent resizes ARE applied.
        assert_eq!(a.set_size("new", 10, 40), SizeEffect::Apply(Winsize::new(10, 40)));
        assert_eq!(a.len(), 1);
    }

    // Detaching the last client yields no size to apply.
    #[test]
    fn detach_last_client_no_promotion() {
        let mut a = WinsizeAuthority::new();
        a.attach("only");
        assert_eq!(a.on_detach("only"), None);
        assert!(a.is_empty());
        assert_eq!(a.primary(), None);
        assert_eq!(a.is_primary("only"), false);
    }

    // Re-attaching an existing id keeps its attach position (does not reorder).
    #[test]
    fn reattach_keeps_position() {
        let mut a = WinsizeAuthority::new();
        a.attach("first");
        a.attach("second");
        a.attach("first"); // duplicate -> ignored
        assert_eq!(a.primary(), Some("first"));
        assert_eq!(a.len(), 2);
    }

    // Sizes are clamped to a minimum of 1x1, matching the session's resize path.
    #[test]
    fn set_size_clamps_to_one() {
        let mut a = WinsizeAuthority::new();
        a.attach("a");
        // 0x0 is meaningless; the primary applies a clamped 1x1.
        assert_eq!(a.set_size("a", 0, 0), SizeEffect::Apply(Winsize::new(1, 1)));
    }
}

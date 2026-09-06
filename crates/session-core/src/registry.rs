//! Session registry: create/find/kill/list. Fleshed out by the registry card.
//!
//! Owns the map of live sessions keyed by id, the daemon's view of what exists,
//! and `kill`/`kill-all` teardown semantics.

/// Placeholder registry of numbered/named sessions. The registry card owns the
/// real map (create/find/kill/list) alongside config.
pub struct Registry;

impl Registry {
    pub fn new() -> Self {
        Self
    }
}

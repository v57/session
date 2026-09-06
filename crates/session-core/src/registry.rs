//! Session registry: the daemon's map of live sessions, plus the
//! create/find/kill/list lifecycle.
//!
//! The registry owns the `HashMap<Uuid, Session>` that is the daemon's view of
//! what exists. `kill`/`kill_all` terminate a session's child and drop it from
//! the map so `list` no longer reports it (a session dropped here closes its
//! command channel, which the pump treats as teardown and reaps the pty).

use crate::config::Config;
use crate::session::{AttachHandle, ClientId, Session};
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

/// Lifecycle status of a session, reported by [`Registry::list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Child is running.
    Running,
    /// Child has exited (seen in [`Session::is_exited`]) but not yet reaped.
    Exited,
}

/// A one-session summary returned by [`Registry::list`].
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: Uuid,
    /// Display title: the child command, or the login shell.
    pub title: String,
    /// Creation time as unix epoch seconds.
    pub created: u64,
    /// Number of currently-attached clients.
    pub attached_count: usize,
    pub status: SessionStatus,
}

/// The daemon's map of live sessions, keyed by id.
pub struct Registry {
    sessions: HashMap<Uuid, Session>,
    /// Scrollback lines applied to every session this registry spawns.
    scrollback_cap: usize,
}

impl Registry {
    /// New registry whose sessions retain `scrollback_cap` lines.
    pub fn new(scrollback_cap: usize) -> Self {
        Self {
            sessions: HashMap::new(),
            scrollback_cap,
        }
    }

    /// New registry applying the config's defaults.
    pub fn from_config(cfg: &Config) -> Self {
        Self::new(cfg.scrollback_cap)
    }

    /// Spawn a new session, insert it, and return its id.
    ///
    /// `cmd == None` runs the user's login shell; `Some(argv)` runs `argv[0]`
    /// with the remaining entries as arguments.
    pub fn create_session(&mut self, cmd: Option<Vec<OsString>>) -> anyhow::Result<Uuid> {
        let session = Session::spawn(cmd, self.scrollback_cap)?;
        let id = session.id();
        self.sessions.insert(id, session);
        Ok(id)
    }

    /// Borrow a live session by id.
    pub fn find(&self, id: &Uuid) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Mutable borrow of a live session (needed for attach/set_size/detach).
    pub fn find_mut(&mut self, id: &Uuid) -> Option<&mut Session> {
        self.sessions.get_mut(id)
    }

    /// Attach a client to a session, returning its replay + live stream.
    pub fn attach(&mut self, id: &Uuid, client_id: ClientId) -> anyhow::Result<AttachHandle> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        session.attach(client_id)
    }

    /// Kill a session (terminate its child) and remove it from the registry.
    pub fn kill(&mut self, id: &Uuid) -> anyhow::Result<()> {
        {
            let session = self
                .sessions
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            session.kill();
        }
        // Dropping the session closes its command channel, which the pump treats
        // as teardown and reaps the pty.
        self.sessions.remove(id);
        Ok(())
    }

    /// Kill every session and clear the registry.
    pub fn kill_all(&mut self) {
        for session in self.sessions.values_mut() {
            session.kill();
        }
        self.sessions.clear();
    }

    /// Remove a session without killing its child (for callers that already
    /// reaped it). Returns the removed session, if present.
    pub fn remove(&mut self, id: &Uuid) -> Option<Session> {
        self.sessions.remove(id)
    }

    /// Summaries of every session, in creation order (ties broken by id).
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self
            .sessions
            .values()
            .map(|s| SessionInfo {
                id: s.id(),
                title: s.title().to_string(),
                created: s
                    .create_time()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                attached_count: s.attached(),
                status: if s.is_exited() {
                    SessionStatus::Exited
                } else {
                    SessionStatus::Running
                },
            })
            .collect();
        infos.sort_by_key(|i| (i.created, i.id));
        infos
    }

    /// Number of sessions currently in the map.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True if the registry holds no sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// True if a session with this id exists.
    pub fn contains(&self, id: &Uuid) -> bool {
        self.sessions.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn shell_argv(snippet: &str) -> Vec<OsString> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        vec![
            OsString::from(shell),
            OsString::from("-c"),
            OsString::from(snippet),
        ]
    }

    // GATE: create -> list -> kill round-trip on a real shell child.
    #[tokio::test]
    async fn create_list_kill_roundtrip() {
        let mut reg = Registry::new(2000);
        let id = reg.create_session(None).expect("create shell session");

        assert!(reg.contains(&id));
        assert!(reg.find(&id).is_some());
        assert_eq!(reg.len(), 1);

        let list = reg.list();
        assert_eq!(list.len(), 1);
        let info = &list[0];
        assert_eq!(info.id, id);
        assert_eq!(info.attached_count, 0);
        assert_eq!(info.status, SessionStatus::Running);
        assert!(!info.title.is_empty(), "title should reflect the shell");

        reg.kill(&id).expect("kill session");
        assert!(reg.find(&id).is_none());
        assert!(!reg.contains(&id));
        assert!(reg.is_empty());
        assert!(reg.list().is_empty());
    }

    // A custom command session can be created, attached to, and torn down.
    #[tokio::test]
    async fn custom_cmd_create_attach_kill_all() {
        let mut reg = Registry::new(2000);
        let id = reg.create_session(Some(shell_argv("cat"))).expect("create cat");

        let handle = reg.attach(&id, "client-a".into()).expect("attach");
        let _redraw = handle.redraw;

        // find_mut exposes the underlying session for mutable control paths.
        {
            let s = reg.find_mut(&id).expect("find_mut session");
            assert_eq!(s.attached(), 1);
        }
        assert_eq!(reg.list()[0].attached_count, 1);

        reg.kill_all();
        assert!(reg.is_empty());
    }

    // Killing an unknown id is an error, not a panic.
    #[tokio::test]
    async fn kill_unknown_id_is_error() {
        let mut reg = Registry::new(2000);
        assert!(reg.kill(&Uuid::new_v4()).is_err());
    }

    // from_config inherits the scrollback cap.
    #[test]
    fn from_config_uses_scrollback_cap() {
        let mut cfg = crate::config::Config::default();
        cfg.scrollback_cap = 5000;
        let reg = Registry::from_config(&cfg);
        assert_eq!(reg.scrollback_cap, 5000);
    }
}

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
    pub id: String,
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
    sessions: HashMap<String, Session>,
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
    pub fn create_session(&mut self, name: &str, cmd: Option<Vec<OsString>>) -> anyhow::Result<()> {
        let session = Session::spawn(name, cmd, self.scrollback_cap)?;
        self.sessions.insert(name.to_string(), session);
        Ok(())
    }

    /// Borrow a live session by id.
    pub fn find(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Mutable borrow of a live session (needed for attach/set_size/detach).
    pub fn find_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(id)
    }

    /// Attach a client to a session, returning its replay + live stream.
    pub fn attach(&mut self, id: &str, client_id: ClientId) -> anyhow::Result<AttachHandle> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        session.attach(client_id)
    }

    /// Kill a session (terminate its child) and remove it from the registry.
    pub fn kill(&mut self, id: &str) -> anyhow::Result<()> {
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
    pub fn remove(&mut self, id: &str) -> Option<Session> {
        self.sessions.remove(id)
    }

    /// Remove every session whose child process has exited (e.g. the shell
    /// `exit`ed, or a one-shot command finished). Returns how many were reaped.
    ///
    /// Dropping a session closes its command channel; the pump already finished
    /// on child exit, so nothing is left running. Call this periodically (the
    /// daemon's background reaper) and before operations that must only observe
    /// live sessions (`list`, `attach`, create-or-attach) so a finished session
    /// is never reported or attached to as a zombie.
    pub fn reap_exited(&mut self) -> usize {
        let zombies: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.is_exited())
            .map(|(id, _)| id.clone())
            .collect();
        let reaped = zombies.len();
        for id in &zombies {
            self.sessions.remove(id);
        }
        reaped
    }

    /// Summaries of every session, in creation order (ties broken by id).
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self
            .sessions
            .values()
            .map(|s| SessionInfo {
                id: s.id().to_string(),
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
        infos.sort_by_key(|i| (i.created, i.id.clone()));
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
    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

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
        let name = "s1";
        reg.create_session(name, None).expect("create shell session");

        assert!(reg.contains(name));
        assert!(reg.find(name).is_some());
        assert_eq!(reg.len(), 1);

        let list = reg.list();
        assert_eq!(list.len(), 1);
        let info = &list[0];
        assert_eq!(info.id, name);
        assert_eq!(info.attached_count, 0);
        assert_eq!(info.status, SessionStatus::Running);
        assert!(!info.title.is_empty(), "title should reflect the shell");

        reg.kill(name).expect("kill session");
        assert!(reg.find(name).is_none());
        assert!(!reg.contains(name));
        assert!(reg.is_empty());
        assert!(reg.list().is_empty());
    }

    // A custom command session can be created, attached to, and torn down.
    #[tokio::test]
    async fn custom_cmd_create_attach_kill_all() {
        let mut reg = Registry::new(2000);
        reg.create_session("cat-session", Some(shell_argv("cat"))).expect("create cat");

        let handle = reg.attach("cat-session", "client-a".into()).expect("attach");
        let _redraw = handle.redraw;

        // find_mut exposes the underlying session for mutable control paths.
        {
            let s = reg.find_mut("cat-session").expect("find_mut session");
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
        assert!(reg.kill("nope").is_err());
    }

    // A session whose child process exits is reaped: `reap_exited` drops it so
    // `list` no longer reports it and create-or-attach would make a fresh one.
    #[tokio::test]
    async fn reap_exited_removes_finished_session() {
        let mut reg = Registry::new(2000);
        reg.create_session("done", Some(shell_argv("exit 0")))
            .expect("create a command that exits immediately");
        assert_eq!(reg.len(), 1);

        // The pump observes the child's exit asynchronously. Wait for it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if reg.find("done").map(|s| s.is_exited()).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            reg.find("done").map(|s| s.is_exited()).unwrap_or(false),
            "the child should be observed as exited"
        );

        // Reaping drops the finished session from the map and the listing.
        assert_eq!(reg.reap_exited(), 1, "one exited session should be reaped");
        assert!(reg.find("done").is_none());
        assert!(!reg.contains("done"));
        assert!(reg.list().is_empty(), "a reaped session must not be listed");
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

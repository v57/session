//! Session registry: the daemon's map of live sessions, plus the
//! create/find/rename/kill/list lifecycle.
//!
//! The registry owns the `HashMap<String, Session>` that is the daemon's view
//! of what exists — sessions are keyed by their caller-supplied NAME (the
//! create-or-attach identity), not a uuid. `kill`/`kill_all` terminate a
//! session's child and drop it from the map so `list` no longer reports it (a
//! session dropped here closes its command channel, which the pump treats as
//! teardown and reaps the pty).

use crate::config::Config;
use crate::session::{AttachHandle, ClientId, Session};
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
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
    /// Whether the session is password protected.
    pub protected: bool,
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

    /// Rename a live session (`from` -> `to`), moving the registry key and
    /// updating the session's stored id so `list`/attach report the new name.
    ///
    /// Refused when `from` is unknown, `to` is already live, or the session
    /// has attached clients (every attached connection binds to the name it
    /// attached with — renaming under an attached client would strand that
    /// client's input routing, so it must detach first). Renaming a session
    /// to itself is a no-op.
    pub fn rename(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
        if from == to {
            return Ok(());
        }
        if !self.sessions.contains_key(from) {
            return Err(anyhow::anyhow!("no such session: {from}"));
        }
        if self.sessions.contains_key(to) {
            return Err(anyhow::anyhow!("a session named '{to}' already exists"));
        }
        if self.sessions.get(from).is_some_and(|s| s.attached() > 0) {
            return Err(anyhow::anyhow!(
                "session '{from}' has attached clients; detach before renaming"
            ));
        }
        let mut session = self
            .sessions
            .remove(from)
            .expect("existence checked above");
        session.set_id(to);
        self.sessions.insert(to.to_string(), session);
        Ok(())
    }

    /// Require `password` for this session from now on.
    pub fn protect(&mut self, id: &str, password: &str) -> anyhow::Result<()> {
        if password.is_empty() {
            return Err(anyhow::anyhow!("the password must not be empty"));
        }
        self.sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?
            .protect(password);
        Ok(())
    }

    /// Whether the session exists and is password protected.
    pub fn is_protected(&self, id: &str) -> bool {
        self.sessions.get(id).is_some_and(|s| s.protection().is_some())
    }

    /// Check `password` for a request on session `id`. Passes for unprotected
    /// and unknown sessions (the request itself then reports "no such
    /// session"); otherwise fails with a [`Denied`] the daemon can downcast.
    pub fn authorize(&self, id: &str, password: Option<&str>) -> anyhow::Result<()> {
        match self.sessions.get(id).and_then(|s| s.protection()) {
            Some(protection) => Ok(protection.check(password)?),
            None => Ok(()),
        }
    }

    /// The lock that password attempts on session `id` take turns on, if it is
    /// protected.
    pub fn password_turn(&self, id: &str) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.sessions.get(id)?.protection().map(|p| p.turn())
    }

    /// Kill every session that is not password protected; protected ones need
    /// `kill <name>` with their password. Returns how many were left running.
    pub fn kill_all(&mut self) -> usize {
        self.sessions.retain(|_, session| {
            if session.protection().is_some() {
                return true;
            }
            session.kill();
            false
        });
        self.sessions.len()
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
                protected: s.protection().is_some(),
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
    use std::time::{Duration, Instant};

    use crate::testing::shell as shell_argv;

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
        reg.create_session("cat-session", Some(crate::testing::echo())).expect("create cat");

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

    // A protected session refuses requests without its password, reports
    // `protected` in the listing, and survives kill-all.
    #[tokio::test]
    async fn protected_session_needs_its_password() {
        use crate::password::Denied;
        let mut reg = Registry::new(2000);
        reg.create_session("open", Some(crate::testing::echo())).expect("create open");
        reg.create_session("locked", Some(crate::testing::echo())).expect("create locked");
        assert!(reg.protect("locked", "").is_err(), "empty password is refused");
        reg.protect("locked", "pw").expect("protect");

        assert!(reg.is_protected("locked"));
        assert!(!reg.is_protected("open"));
        let denied = |r: anyhow::Result<()>| r.unwrap_err().downcast::<Denied>().ok();
        assert_eq!(denied(reg.authorize("locked", None)), Some(Denied::Required));
        assert_eq!(denied(reg.authorize("locked", Some("nope"))), Some(Denied::Wrong));
        assert!(reg.authorize("locked", Some("pw")).is_ok());
        assert!(reg.authorize("open", None).is_ok());
        assert!(reg.authorize("missing", None).is_ok());
        assert!(reg.password_turn("locked").is_some());
        assert!(reg.password_turn("open").is_none());

        let listed: Vec<(String, bool)> =
            reg.list().into_iter().map(|i| (i.id, i.protected)).collect();
        assert!(listed.contains(&("locked".to_string(), true)));
        assert!(listed.contains(&("open".to_string(), false)));

        // Rename keeps the protection with the session.
        reg.rename("locked", "renamed").expect("rename");
        assert!(reg.is_protected("renamed"));

        assert_eq!(reg.kill_all(), 1, "the protected session is left running");
        assert!(reg.contains("renamed"));
        reg.kill("renamed").unwrap();
    }

    // Killing an unknown id is an error, not a panic.
    #[tokio::test]
    async fn kill_unknown_id_is_error() {
        let mut reg = Registry::new(2000);
        assert!(reg.kill("nope").is_err());
    }

    // Rename moves the registry key AND the session's stored id: the old name
    // no longer resolves and `list` reports the new one.
    #[tokio::test]
    async fn rename_moves_session_identity() {
        let mut reg = Registry::new(2000);
        reg.create_session("old-name", None).expect("create session");

        reg.rename("old-name", "new-name").expect("rename");
        assert!(!reg.contains("old-name"));
        assert!(reg.contains("new-name"));
        assert_eq!(reg.find("new-name").expect("renamed session").id(), "new-name");
        assert_eq!(reg.list()[0].id, "new-name");

        // Renaming to the same name is a no-op success.
        assert!(reg.rename("new-name", "new-name").is_ok());
        reg.kill("new-name").expect("kill renamed session");
    }

    // Rename refuses an unknown source, a taken target, and an ATTACHED
    // session (its client connections bind to the old name).
    #[tokio::test]
    async fn rename_rejects_unknown_taken_and_attached() {
        let mut reg = Registry::new(2000);
        assert!(reg.rename("nope", "x").is_err(), "unknown source");

        reg.create_session("a", None).expect("create a");
        reg.create_session("b", None).expect("create b");
        assert!(reg.rename("a", "b").is_err(), "taken target");

        // An attached client must detach through the session (dropping the
        // AttachHandle only drops its broadcast receiver).
        let client: crate::session::ClientId = "client-1".into();
        let _handle = reg.attach("b", client.clone()).expect("attach b");
        assert!(reg.rename("b", "c").is_err(), "attached session refuses rename");
        reg.find_mut("b")
            .expect("session b")
            .detach(&client);
        assert!(reg.rename("b", "c").is_ok(), "detached session renames");
        assert!(reg.contains("c"));

        reg.kill("a").expect("kill a");
        reg.kill("c").expect("kill c");
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

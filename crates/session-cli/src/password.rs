//! Session passwords on the client side: where they come from.
//!
//! A password is taken from `SESSION_PASSWORD` when it is set (scripts, or an
//! app that keeps the password in its keychain), otherwise asked for on the
//! terminal with echo off. Shell runs `session <name>` in a terminal, so the
//! prompt simply shows up there.
//!
//! The variable is visible to other processes of the same user that can read
//! this process's environment; the prompt is the safer choice. The daemon
//! launcher strips it from the daemon's environment so sessions never inherit it.

use anyhow::{bail, Result};
use std::io::{IsTerminal, Write};

/// Environment variable holding the session password.
pub const ENV: &str = "SESSION_PASSWORD";
/// Prompts for an existing session's password before giving up.
const MAX_PROMPTS: usize = 3;

/// Hands out passwords for one command run: `SESSION_PASSWORD` once, then
/// prompts (a wrong password from the variable is not retried).
pub struct Asker {
    env: Option<String>,
    env_used: bool,
    prompts: usize,
}

impl Asker {
    pub fn from_env() -> Self {
        Self::new(std::env::var(ENV).ok())
    }

    /// An asker whose first answer is `password` (`None`: prompt).
    pub fn new(password: Option<String>) -> Self {
        Self {
            env: password.filter(|p| !p.is_empty()),
            env_used: false,
            prompts: 0,
        }
    }

    /// The password of the protected `session`, after the daemon asked for one;
    /// `wrong` says the last one sent was rejected.
    pub fn existing(&mut self, session: &str, wrong: bool) -> Result<String> {
        if let Some(password) = &self.env {
            if self.env_used {
                bail!("wrong password for session '{session}' (from {ENV})");
            }
            self.env_used = true;
            return Ok(password.clone());
        }
        if self.prompts >= MAX_PROMPTS {
            bail!("wrong password for session '{session}'");
        }
        if wrong {
            eprintln!("session: wrong password");
        }
        self.prompts += 1;
        prompt(&format!("Password for session '{session}': "))
    }

    /// A password to protect the new session `session` with.
    pub fn new_password(&mut self, session: &str) -> Result<String> {
        if let Some(password) = &self.env {
            self.env_used = true;
            return Ok(password.clone());
        }
        let password = prompt(&format!("New password for session '{session}': "))?;
        if password.is_empty() {
            bail!("the password must not be empty");
        }
        if prompt("Repeat the password: ")? != password {
            bail!("the passwords do not match");
        }
        Ok(password)
    }
}

/// Ask on the terminal with echo off (raw mode, so it works the same on Unix
/// and Windows consoles).
fn prompt(text: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("the session is password protected: run this in a terminal or set {ENV}");
    }
    eprint!("{text}");
    let _ = std::io::stderr().flush();
    crossterm::terminal::enable_raw_mode()?;
    let result = read_hidden_line(&mut std::io::stdin());
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!();
    result
}

/// Read one line from a raw-mode terminal, a byte at a time. Reading through
/// `std::io::stdin()` (not crossterm's event reader) matters: anything typed
/// after Enter stays in stdin's buffer, where the proxy reads it next and
/// sends it to the session, instead of being swallowed.
fn read_hidden_line(input: &mut impl std::io::Read) -> Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if input.read(&mut byte)? == 0 {
            bail!("cancelled");
        }
        match byte[0] {
            b'\r' | b'\n' => return Ok(String::from_utf8_lossy(&line).into_owned()),
            0x03 | 0x04 => bail!("cancelled"), // Ctrl-C, Ctrl-D
            0x15 => line.clear(),              // Ctrl-U
            0x08 | 0x7f => {
                // Backspace: drop one whole UTF-8 character.
                while let Some(b) = line.pop() {
                    if b & 0xc0 != 0x80 {
                        break;
                    }
                }
            }
            b => line.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker(env: Option<&str>) -> Asker {
        Asker::new(env.map(str::to_string))
    }

    // The variable answers the first challenge only: a wrong one is an error,
    // not a loop (tests have no terminal, so a prompt would fail anyway).
    #[test]
    fn env_password_is_used_once() {
        let mut a = asker(Some("pw"));
        assert_eq!(a.existing("s", false).unwrap(), "pw");
        let err = a.existing("s", true).unwrap_err().to_string();
        assert!(err.contains("wrong password") && err.contains(ENV), "{err}");
    }

    #[test]
    fn hidden_line_editing_and_typeahead() {
        let mut input: &[u8] = "p\u{e9}x\x7f\x7fw\rnext".as_bytes();
        assert_eq!(read_hidden_line(&mut input).unwrap(), "pw");
        assert_eq!(input, b"next", "bytes after Enter are left unread");
        let mut input: &[u8] = b"junk\x15pw\n";
        assert_eq!(read_hidden_line(&mut input).unwrap(), "pw");
        let mut input: &[u8] = b"pw\x03";
        assert!(read_hidden_line(&mut input).is_err());
    }

    // After protecting a new session with the variable, a rejection means the
    // name already exists with another password.
    #[test]
    fn env_new_password_counts_as_used() {
        let mut a = asker(Some("pw"));
        assert_eq!(a.new_password("s").unwrap(), "pw");
        assert!(a.existing("s", true).is_err());
    }
}

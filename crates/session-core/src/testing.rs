//! Platform commands for tests that run a real child process.

use std::ffi::OsString;

/// argv that runs `snippet` in the platform shell: `$SHELL -c` (default
/// `/bin/sh`) on Unix, `cmd /c` on Windows.
pub fn shell(snippet: &str) -> Vec<OsString> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), snippet.into()]
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        vec![shell.into(), "-c".into(), snippet.into()]
    }
}

/// A child that echoes what it is sent: `cat`, or on Windows an interactive
/// `cmd`, whose console echoes what is typed.
pub const ECHO_CMD: &str = if cfg!(windows) { "cmd /q /k" } else { "cat" };
/// A child that runs for about 30 seconds.
pub const IDLE_CMD: &str = if cfg!(windows) { "ping -n 31 127.0.0.1" } else { "sleep 30" };
/// A child that prints `RERUN` and exits.
pub const RERUN_CMD: &str = if cfg!(windows) { "cmd /c echo RERUN" } else { "echo RERUN" };

/// [`ECHO_CMD`] as argv.
pub fn echo() -> Vec<OsString> {
    ECHO_CMD.split_whitespace().map(OsString::from).collect()
}

/// What a terminal answers to ConPTY's opening cursor position query. ConPTY
/// (Windows) holds back a child's output until it gets one; tests that drive a
/// pty directly, without a session to answer, send it up front.
pub const CURSOR_REPLY: &[u8] = if cfg!(windows) { b"\x1b[1;1R" } else { b"" };

//! Cross-platform helper to read a process's current working directory.
//!
//! The daemon needs a session's LIVE cwd (the shell follows `cd`), but the
//! spawn-time cwd captured in [`crate::pty::Pty`] is frozen at launch. So the
//! daemon queries the OS for the child pid's cwd on demand via `process_cwd`.
//!
//! No dependency on macOS/Linux: Linux reads `/proc/<pid>/cwd`, macOS shells
//! out to the system `lsof -d cwd -p <pid> -Fn`. Windows uses the `sysinfo`
//! crate (a Windows-only target dependency), whose `Process::cwd()` has been
//! supported since 0.20.2.

use std::path::PathBuf;

/// Best-effort read of `pid`'s current working directory, or `None` if it
/// cannot be determined (process gone, permission, unsupported platform).
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        macos_lsof_cwd(pid)
    }
    #[cfg(target_os = "windows")]
    {
        windows_sysinfo_cwd(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = pid;
        None
    }
}

/// Windows: `sysinfo`'s `Process::cwd()`. `refresh_processes()` then a lookup
/// by pid; returns `None` for pids we cannot read (e.g. a terminated process).
#[cfg(target_os = "windows")]
fn windows_sysinfo_cwd(pid: u32) -> Option<PathBuf> {
    use sysinfo::{Pid, System};

    let mut sys = System::new();
    sys.refresh_processes();
    sys.process(Pid::from_u32(pid))
        .and_then(|p| p.cwd())
        .map(|p| p.to_path_buf())
}

/// macOS: `lsof -a -d cwd -p <pid> -Fn` and parse the `n<path>` line.
#[cfg(target_os = "macos")]
fn macos_lsof_cwd(pid: u32) -> Option<PathBuf> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(path) = line.strip_prefix('n') {
            let path = path.trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_of_self_is_some() {
        let pid = std::process::id();
        let dir = process_cwd(pid).expect("should read our own cwd");
        assert!(dir.is_absolute(), "cwd should be absolute: {dir:?}");
    }
}

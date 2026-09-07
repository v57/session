//! session-cli: the `session` command-line frontend.
//!
//! Scope: managed/attach-only. It supports list, kill, kill-all, detach, create,
//! stdio attach, and `session <id> [cmd...]` to create/attach a session. It is a
//! transparent raw-mode byte proxy — it does NOT render (the Shell app renders).
//! `--daemon` runs the daemon loop (linked from session-core).
//!
//! # Dispatch (this card)
//!
//! This file parses the command line into a [`Command`] and dispatches:
//!
//! * `session list`            -> JSON listing of live sessions.
//! * `session kill <uuid>`     -> kill one session (by id).
//! * `session kill-all`        -> kill every session.
//! * `session detach`          -> send a detach on the management channel.
//! * `session attach <uuid>`   -> attach to an existing session.
//! * `session <name>`          -> attach, or create-if-absent, then attach.
//! * `session <name> <cmd...>` -> start that named session running `cmd`, then
//!                                attach; if the name is already live, `cmd` is
//!                                ignored (resume).
//! * `session --daemon`        -> run the daemon loop (via session-core).
//!
//! The management subcommands (list/kill/kill-all/detach) route to the
//! connect/client layer ([`client`], [`daemon_launcher`]); the attach subcommands
//! route to the raw-mode byte proxy ([`proxy`]). Exit codes: `0` success,
//! `1` operational error, `2` usage error.

use client::Client;
use daemon_launcher::ensure_daemon_running;
use session_core::config::Config;
use session_core::protocol::{ClientToDaemon, DaemonToClient};
use std::process::ExitCode;

mod client;
mod daemon_launcher;
mod proxy;

/// Exit code for a usage error (bad arguments, unknown subcommand shape).
const EXIT_USAGE: u8 = 2;
/// Exit code for an operational failure (daemon unreachable, I/O error, etc.).
const EXIT_ERROR: u8 = 1;

/// A parsed-and-validated command line. Pure — no I/O — so the argument/dispatch
/// logic is unit-testable without touching the daemon or a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// `session --daemon`
    Daemon,
    /// `session list`
    List,
    /// `session kill <uuid>`
    Kill { id: String },
    /// `session kill-all`
    KillAll,
    /// `session detach`
    Detach,
    /// `session attach <uuid>` (attach to an existing session)
    Attach { id: String },
    /// `session <uuid> [cmd...]`
    AttachOrCreate { id: String, cmd: Option<String> },
    /// Help request (-h / --help / help).
    Help,
    /// No recognizable command (usage).
    Usage,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "session".to_string());
    let rest: Vec<String> = args.into_iter().skip(1).collect();

    // The CLI performs one operation and exits; a multi-thread runtime also
    // backs the daemon case (it serves many clients concurrently).
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("session: failed to start async runtime: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };

    match parse_command(&rest) {
        Command::Help => {
            print_usage(&prog);
            ExitCode::SUCCESS
        }
        Command::Usage => {
            print_usage(&prog);
            ExitCode::from(EXIT_USAGE)
        }
        // The daemon and the management subcommands are async and run inside `rt`.
        Command::Daemon => rt.block_on(run_daemon()),
        Command::List => rt.block_on(cmd_list()),
        Command::Kill { id } => rt.block_on(cmd_kill(id)),
        Command::KillAll => rt.block_on(cmd_kill_all()),
        Command::Detach => rt.block_on(cmd_detach()),
        // The attach subcommands route to the raw-mode proxy. `proxy::attach` is a
        // self-contained, sync entry point that builds its OWN runtime, so it must
        // NOT be called from inside `rt.block_on` (that would nest a runtime).
        Command::Attach { id } => run_attach_sync(&Config::load(), &id, None),
        Command::AttachOrCreate { id, cmd } => {
            run_attach_sync(&Config::load(), &id, cmd.as_deref())
        }
    }
}

/// Run the daemon loop until a fatal error, or until it detects an
/// already-running daemon (the singleton guard returns `Ok(())` immediately).
async fn run_daemon() -> ExitCode {
    let cfg = Config::load();
    match session_core::daemon::run(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("session: daemon error: {e}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Parse the argument tail (everything after the program name) into a [`Command`].
fn parse_command(rest: &[String]) -> Command {
    let Some(cmd) = rest.first() else {
        return Command::Usage;
    };
    match cmd.as_str() {
        "--daemon" => Command::Daemon,
        "-h" | "--help" | "help" => Command::Help,
        "list" => Command::List,
        "kill" => match rest.get(1) {
            Some(id) => Command::Kill { id: id.clone() },
            None => Command::Usage,
        },
        "kill-all" => Command::KillAll,
        "detach" => Command::Detach,
        // `create` is intentionally NOT a command. Sessions are started with
        // `session <name> [cmd...]`. Reserve it so a stray `session create`
        // is a usage error instead of silently creating a session named "create".
        "create" => Command::Usage,
        "attach" => match rest.get(1) {
            Some(id) => Command::Attach { id: id.clone() },
            None => Command::Usage,
        },
        // An unrecognized leading flag is a usage error, NOT a session id.
        _ if cmd.starts_with('-') => Command::Usage,
        // `session <uuid> [cmd...]` -> attach-or-create, optionally with a command.
        _ => Command::AttachOrCreate {
            id: cmd.clone(),
            cmd: join_cmd(&rest[1..]),
        },
    }
}

/// Join an argument tail into a single whitespace-separated command string.
/// `None` when the tail is empty (i.e. run the default shell).
fn join_cmd(args: &[String]) -> Option<String> {
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

fn print_usage(prog: &str) {
    eprintln!("usage: {prog} [--daemon] <command> [args]");
    eprintln!("commands:");
    eprintln!("  list                       list live sessions as JSON");
    eprintln!("  kill <uuid>                kill a session by id");
    eprintln!("  kill-all                   kill every session");
    eprintln!("  detach                     detach the current client (if any)");
    eprintln!("  attach <uuid>              attach to an existing session");
    eprintln!("  <name> [cmd...]            attach-or-create; cmd runs if the name is new");
    eprintln!("  --daemon                   run the session daemon loop");
}

/// Connect to (or auto-start) the daemon and return a handshaked client.
async fn connect_client() -> anyhow::Result<Client> {
    let cfg = Config::load();
    ensure_daemon_running(&cfg).await
}

/// `session list` — JSON listing of live sessions.
async fn cmd_list() -> ExitCode {
    let mut client = match connect_client().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("session: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    if let Err(e) = client.send(ClientToDaemon::List).await {
        eprintln!("session: failed to request listing: {e}");
        return ExitCode::from(EXIT_ERROR);
    }
    loop {
        match client.recv().await {
            Ok(Some(DaemonToClient::Listing { sessions })) => {
                match serde_json::to_string_pretty(&sessions) {
                    Ok(json) => {
                        println!("{json}");
                        return ExitCode::SUCCESS;
                    }
                    Err(e) => {
                        eprintln!("session: failed to encode listing: {e}");
                        return ExitCode::from(EXIT_ERROR);
                    }
                }
            }
            Ok(Some(DaemonToClient::Error { message })) => {
                eprintln!("session: {message}");
                return ExitCode::from(EXIT_ERROR);
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                eprintln!("session: daemon closed the connection");
                return ExitCode::from(EXIT_ERROR);
            }
            Err(e) => {
                eprintln!("session: {e}");
                return ExitCode::from(EXIT_ERROR);
            }
        }
    }
}

/// `session kill <uuid>` — kill one session by id.
async fn cmd_kill(id: String) -> ExitCode {
    let mut client = match connect_client().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("session: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    if let Err(e) = client.send(ClientToDaemon::Kill { session_id: id }).await {
        eprintln!("session: failed to send kill: {e}");
        return ExitCode::from(EXIT_ERROR);
    }
    loop {
        match client.recv().await {
            Ok(Some(DaemonToClient::Killed { session_id })) => {
                println!("session: killed {session_id}");
                return ExitCode::SUCCESS;
            }
            Ok(Some(DaemonToClient::Error { message })) => {
                eprintln!("session: {message}");
                return ExitCode::from(EXIT_ERROR);
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                eprintln!("session: daemon closed the connection");
                return ExitCode::from(EXIT_ERROR);
            }
            Err(e) => {
                eprintln!("session: {e}");
                return ExitCode::from(EXIT_ERROR);
            }
        }
    }
}

/// `session kill-all` — kill every session. The protocol acks via a purely
/// informational `Error{"killed all sessions"}` frame, so that is success.
async fn cmd_kill_all() -> ExitCode {
    let mut client = match connect_client().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("session: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    if let Err(e) = client.send(ClientToDaemon::KillAll).await {
        eprintln!("session: failed to send kill-all: {e}");
        return ExitCode::from(EXIT_ERROR);
    }
    loop {
        match client.recv().await {
            Ok(Some(DaemonToClient::Error { message })) => {
                println!("session: {message}");
                return ExitCode::SUCCESS;
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                println!("session: killed all sessions");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("session: {e}");
                return ExitCode::from(EXIT_ERROR);
            }
        }
    }
}

/// `session detach` — send a detach on the management channel (a detach with no
/// attachment is a daemon-side no-op).
async fn cmd_detach() -> ExitCode {
    let mut client = match connect_client().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("session: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    if let Err(e) = client.send(ClientToDaemon::Detach).await {
        eprintln!("session: failed to send detach: {e}");
        return ExitCode::from(EXIT_ERROR);
    }
    println!("session: detached");
    ExitCode::SUCCESS
}

/// Route an attach-or-create subcommand to the raw-mode byte proxy.
///
/// This is a *sync* entry point because [`proxy::attach`] builds its OWN tokio
/// runtime internally; calling it from inside the outer runtime's `block_on`
/// would nest runtimes and panic. We run the daemon auto-start in a short-lived
/// runtime that is dropped BEFORE `proxy::attach` is called, so the daemon is up
/// and the proxy sees a listener. The daemon and session always survive the
/// client detaching.
fn run_attach_sync(cfg: &Config, session: &str, cmd: Option<&str>) -> ExitCode {
    // Auto-start the daemon (spawns a detached `session --daemon`) so the proxy's
    // `Conn::connect` finds a listener. The temporary runtime is dropped before
    // `proxy::attach` creates its own.
    {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("session: failed to start async runtime: {e}");
                return ExitCode::from(EXIT_ERROR);
            }
        };
        if let Err(e) = rt.block_on(ensure_daemon_running(cfg)) {
            eprintln!("session: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    }

    match proxy::attach(cfg, session, cmd, proxy::ProxyOptions::default()) {
        Ok(_exit) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("session: {e}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn no_args_is_usage() {
        assert_eq!(parse_command(&[]), Command::Usage);
    }

    #[test]
    fn help_flags_are_help() {
        assert_eq!(parse_command(&[s("-h")]), Command::Help);
        assert_eq!(parse_command(&[s("--help")]), Command::Help);
        assert_eq!(parse_command(&[s("help")]), Command::Help);
    }

    #[test]
    fn unknown_leading_flag_is_usage_not_a_session_id() {
        assert_eq!(parse_command(&[s("--foo")]), Command::Usage);
        assert_eq!(parse_command(&[s("-x")]), Command::Usage);
    }

    #[test]
    fn daemon_flag() {
        assert_eq!(parse_command(&[s("--daemon")]), Command::Daemon);
    }

    #[test]
    fn list() {
        assert_eq!(parse_command(&[s("list")]), Command::List);
    }

    #[test]
    fn kill_requires_an_id() {
        assert_eq!(
            parse_command(&[s("kill"), s("abc")]),
            Command::Kill { id: s("abc") }
        );
        assert_eq!(parse_command(&[s("kill")]), Command::Usage);
    }

    #[test]
    fn create_is_not_a_command() {
        assert_eq!(parse_command(&[s("create")]), Command::Usage);
        assert_eq!(parse_command(&[s("create"), s("myname")]), Command::Usage);
    }

    #[test]
    fn kill_all_and_detach() {
        assert_eq!(parse_command(&[s("kill-all")]), Command::KillAll);
        assert_eq!(parse_command(&[s("detach")]), Command::Detach);
    }

    #[test]
    fn attach_requires_an_id() {
        assert_eq!(
            parse_command(&[s("attach"), s("id1")]),
            Command::Attach { id: s("id1") }
        );
        assert_eq!(parse_command(&[s("attach")]), Command::Usage);
    }


    #[test]
    fn uuid_with_cmd_is_attach_or_create_with_cmd() {
        assert_eq!(
            parse_command(&[s("s1"), s("ls"), s("-la")]),
            Command::AttachOrCreate {
                id: s("s1"),
                cmd: Some(s("ls -la"))
            }
        );
    }

    #[test]
    fn bare_uuid_is_attach_or_create() {
        assert_eq!(
            parse_command(&[s("some-uuid")]),
            Command::AttachOrCreate {
                id: s("some-uuid"),
                cmd: None
            }
        );
    }

    #[test]
    fn unrecognized_first_token_is_treated_as_session_id() {
        assert_eq!(
            parse_command(&[s("nonsense")]),
            Command::AttachOrCreate {
                id: s("nonsense"),
                cmd: None
            }
        );
    }

    #[test]
    fn join_cmd_none_for_empty() {
        assert_eq!(join_cmd(&[]), None);
        assert_eq!(join_cmd(&[s("echo"), s("hi")]), Some(s("echo hi")));
    }
}

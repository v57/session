//! session-cli: the `session` command-line frontend.
//!
//! Scope: managed/attach-only. It supports list, kill, kill-all, detach, create,
//! stdio attach, and `session <id> [cmd...]` to create/attach a session. It is a
//! transparent raw-mode byte proxy — it does NOT render (the Shell app renders).
//! `--daemon` runs the daemon loop (linked from session-core).
//!
//! The CLI is fleshed out in Phase 2 (client.rs, daemon_launcher.rs, proxy.rs);
//! this file is the subcommand-dispatch skeleton owned by the bootstrap card.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("session");
    let rest = &args[1..];

    // `session --daemon`: run the daemon loop.
    if rest.first().map(String::as_str) == Some("--daemon") {
        return daemon();
    }

    let Some(cmd) = rest.first().map(String::as_str) else {
        eprintln!("usage: {prog} [--daemon] <command> [args]");
        eprintln!("commands: list | kill | kill-all | detach | create | attach | <id> [cmd...]");
        return ExitCode::from(2);
    };

    match cmd {
        "list" => stub("list"),
        "kill" => stub("kill"),
        "kill-all" => stub("kill-all"),
        "detach" => stub("detach"),
        "create" => stub("create"),
        "attach" => stub("attach"),
        // `session <id> [cmd...]` -> create/attach a session.
        _ => attach(cmd),
    }
}

/// Run the daemon loop (stub, linked to session-core).
fn daemon() -> ExitCode {
    eprintln!(
        "session: daemon stub (session-core v{}) — not implemented yet",
        session_core::version()
    );
    ExitCode::SUCCESS
}

/// Stub for a managed subcommand; Phase 2 replaces these with real dispatch.
fn stub(name: &str) -> ExitCode {
    eprintln!("session: '{name}' not implemented yet");
    ExitCode::SUCCESS
}

/// Stub for `session <id> [cmd...]` create/attach path.
fn attach(id: &str) -> ExitCode {
    eprintln!("session: attach to '{id}' not implemented yet");
    ExitCode::SUCCESS
}

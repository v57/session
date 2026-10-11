# session

Persistent terminal sessions without the keybindings: tmux/screen minus the prefix key.

Close the window and the session keeps running. Reattach from any terminal and the screen and
scrollback are all there. `session` intercepts no keys: to detach, close the window.

- One small binary for macOS, Linux and Windows. A per-user daemon starts automatically on first use.
- Each session runs in a pty with a full terminal emulator (screen + 2000 lines of scrollback), so a
  reattach redraws exactly what was there.
- Several terminals can attach to one session at once. They share input and output.
- `session -d <name>` prints the live working directory of the session's shell.
- Sessions can have a password, so other programs running as you can't attach to them.

## Install

macOS and Linux:

```bash
curl -fsSL https://v57.dev/install/session | bash
```

Windows (PowerShell):

```powershell
powershell -c "irm v57.dev/install/session.ps1|iex"
```

The scripts download the latest [release](https://github.com/v57/session/releases), verify its
SHA-256 checksum, and install to `~/.local/bin` (Windows: `%LOCALAPPDATA%\Programs\session\bin`). If
that folder isn't on your `PATH`, they print how to add it. Set `SESSION_VERSION=v0.1.0` to install a
specific release or `SESSION_INSTALL_DIR` to install somewhere else.

You can also download an archive from the releases page or build from source (see below).

## Use

```
session <name> [cmd...]     attach to <name>, or create it (running cmd or your shell)
session -p <name> [cmd...]  the same, but a new session gets a password
session attach <name>       attach to an existing session only
session list                list sessions as JSON
session -d <name>           print the session's current working directory
session rename <from> <to>  rename a session (no clients attached)
session kill <name>         end a session
session kill-all            end every session without a password
```

`session list` prints a JSON array, one object per session:

```json
[{ "id": "work", "title": "zsh", "attached": 1, "protected": false }]
```

## Passwords

`session -p work` asks for a new password (twice) and creates `work` with it. After that, attaching,
`kill`, `rename` and `-d` on `work` ask for the password. `list` still shows the session, marked
`"protected": true`. `kill-all` leaves protected sessions running.

The password is asked for on the terminal with echo off. For scripts, set `SESSION_PASSWORD`
instead. Other processes running as you can read that variable, so the prompt is the safer choice.

The daemon keeps only a salted hash of the password, in memory, and answers at most one wrong guess
per second per session. This keeps out other programs running as your user. It does not protect
against your administrator (root), who can read any process's memory.

## Build

```bash
cargo build --release   # target/release/session
cargo test
```

## License

MIT

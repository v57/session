//! Wire message frames between the daemon and its attached clients.
//!
//! Both directions are `serde` enums, **tag-delimited** via a `type` discriminator
//! (adjacently tagged), serialized to newline-delimited JSON (NDJSON) — one frame
//! per line. Every raw byte payload (terminal output, redraw replays, keystrokes)
//! is **base64-encoded** so a frame is always ASCII-safe and survives any
//! length-prefixed or line-delimited transport unchanged.
//!
//! Both sides declare a [`PROTOCOL_VERSION`]; the daemon validates the version
//! carried in `ClientToDaemon::Hello` and rejects a mismatch with
//! [`ProtocolError::VersionMismatch`].

use serde::{Deserialize, Serialize};

/// Protocol version, bumped on any incompatible wire change. Both the client
/// and the daemon advertise this in their `Hello` frame.
///
/// v2: added [`DaemonToClient::Exit`] so a client is told when its session's
/// child process exits and can close instead of lingering in raw mode.
///
/// v3: added session passwords (`password` on Attach, AttachCreate, Kill,
/// Rename and Cwd, `protect` on AttachCreate, [`DaemonToClient::PasswordRequired`]).
/// A v2 daemon would silently ignore them and create an unprotected session,
/// so the version is bumped instead of relying on unknown-field leniency.
pub const PROTOCOL_VERSION: u32 = 3;

/// A frame from the client (the CLI / proxy) to the daemon.
///
/// Serialized as `{"type": "<variant>", "data": {...}}` — unit variants omit the
/// `data` field. `bytes` fields are base64-encoded on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ClientToDaemon {
    /// Client announces its protocol version and (implicitly) opens a session.
    Hello { version: u32 },
    /// Client attaches to an already-existing session by id.
    Attach {
        session_id: String,
        /// The session's password, when it is protected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
    /// Client creates a new session (optionally running `cmd`), then attaches.
    AttachCreate {
        session_id: String,
        /// Optional command to run in the session; `None` = default shell.
        #[serde(skip_serializing_if = "Option::is_none")]
        cmd: Option<String>,
        /// The password: checked when the session already exists and is
        /// protected, and set as its password when `protect` creates it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        /// Create the session password protected (`password` is required).
        /// Refused when the name is already live without a password.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        protect: bool,
    },
    /// Raw input bytes (keystrokes) to feed to the session's pty.
    Input {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    /// Client reports its new terminal size (character cell rows/cols).
    Resize { rows: u16, cols: u16 },
    /// Client detaches (leaves) without killing the session.
    Detach,
    /// Client requests the list of live sessions.
    List,
    /// Client requests that one session be killed.
    Kill {
        session_id: String,
        /// The session's password, when it is protected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
    /// Client requests that all sessions be killed. Password-protected
    /// sessions are left running (kill them one by one with their password).
    KillAll,
    /// Client renames a live session (`from` -> `to`). Refused by the daemon
    /// when `from` is unknown, `to` is already live, or the session has
    /// attached clients (each client connection binds to the name it attached
    /// with; renaming under an attached client would strand its input
    /// routing). Headless — needs no TTY.
    Rename {
        from: String,
        to: String,
        /// The password of `from`, when it is protected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
    /// Client requests the working directory of a session (the Shell app uses
    /// this to follow the shell's `cd` for file-drop / file-manager mode).
    Cwd {
        session_id: String,
        /// The session's password, when it is protected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
}

/// A frame from the daemon back to a client.
///
/// Same tag-delimited envelope; `bytes` / `redraw_bytes` are base64-encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DaemonToClient {
    /// Daemon advertises its own protocol version on connect.
    Hello { daemon_version: u32 },
    /// Attachment established; `redraw_bytes` replays the full screen
    /// (grid + scrollback) so the client can paint immediately.
    Attached {
        session_id: String,
        #[serde(with = "base64_bytes")]
        redraw_bytes: Vec<u8>,
    },
    /// Incremental terminal output bytes to display.
    Output {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    /// The referenced session was killed.
    Killed { session_id: String },
    /// A session was renamed; `session_id` is its NEW name.
    Renamed { session_id: String },
    /// The referenced session's child process exited. The daemon is closing the
    /// session; an attached client should end its view instead of lingering.
    Exit { session_id: String },
    /// Snapshot of all live sessions.
    Listing { sessions: Vec<SessionInfo> },
    /// An error for the preceding request.
    Error { message: String },
    /// The requested session's current working directory.
    Cwd { session_id: String, cwd: String },
    /// The request names a password-protected session and sent no password
    /// (`wrong == false`) or a wrong one (`wrong == true`). The client may ask
    /// the user and send the same request again with `password`.
    PasswordRequired { session_id: String, wrong: bool },
}

/// Metadata about one live session, as reported by [`DaemonToClient::Listing`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Stable session id.
    pub id: String,
    /// Human-readable title (usually the command or the last line written).
    #[serde(default)]
    pub title: String,
    /// Number of currently attached clients.
    #[serde(default)]
    pub attached: usize,
    /// Whether attaching (and kill, rename, cwd) needs the session's password.
    #[serde(default)]
    pub protected: bool,
}

impl SessionInfo {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: String::new(),
            attached: 0,
            protected: false,
        }
    }
}

/// Errors produced while framing, unframing, or validating protocol messages.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The peer's `Hello` version does not match [`PROTOCOL_VERSION`].
    #[error("protocol version mismatch: daemon speaks v{expected}, peer sent v{got}")]
    VersionMismatch { expected: u32, got: u32 },
    /// A base64 payload could not be decoded.
    #[error("invalid base64 payload: {0}")]
    InvalidBase64(String),
    /// A frame failed to serialize or deserialize.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
}

/// Validate a peer's advertised version against [`PROTOCOL_VERSION`].
pub fn check_version(version: u32) -> Result<(), ProtocolError> {
    if version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            got: version,
        })
    }
}

// ---------------------------------------------------------------------------
// Frame <-> JSON helpers (newline-delimited: one frame per line)
// ---------------------------------------------------------------------------

impl ClientToDaemon {
    /// Serialize this frame to a single NDJSON line (JSON + trailing `\n`).
    pub fn to_frame(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut v =
            serde_json::to_vec(self).map_err(|e| ProtocolError::InvalidFrame(e.to_string()))?;
        v.push(b'\n');
        Ok(v)
    }

    /// Deserialize a single NDJSON line (trailing newline whitespace tolerated).
    pub fn from_frame(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let trimmed = trim_ndjson(bytes);
        serde_json::from_slice(trimmed).map_err(|e| ProtocolError::InvalidFrame(e.to_string()))
    }
}

impl DaemonToClient {
    /// Serialize this frame to a single NDJSON line (JSON + trailing `\n`).
    pub fn to_frame(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut v =
            serde_json::to_vec(self).map_err(|e| ProtocolError::InvalidFrame(e.to_string()))?;
        v.push(b'\n');
        Ok(v)
    }

    /// Deserialize a single NDJSON line (trailing newline whitespace tolerated).
    pub fn from_frame(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let trimmed = trim_ndjson(bytes);
        serde_json::from_slice(trimmed).map_err(|e| ProtocolError::InvalidFrame(e.to_string()))
    }
}

/// Drop a single trailing `\n`/`\r\n` (and stray whitespace) from an NDJSON line.
fn trim_ndjson(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[..end]
}

// ---------------------------------------------------------------------------
// base64 bytes (ASCII-safe raw payloads)
// ---------------------------------------------------------------------------

/// Serialize a `Vec<u8>` as a base64 string; deserialize back. Used via
/// `#[serde(with = "base64_bytes")]` on raw-payload fields so a frame carries
/// only ASCII characters on the wire.
mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::b64;

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&b64::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        b64::decode(&s).map_err(serde::de::Error::custom)
    }
}

/// Minimal standard base64 (RFC 4648, `+/` alphabet with `=` padding).
///
/// Implemented in-module rather than pulled in as a crate so this card needs no
/// manifest change (the bootstrap card is the sole owner of Cargo.toml).
mod b64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// Encode raw bytes to a padded base64 string.
    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    /// Decode a base64 string (padding optional, whitespace tolerated) back to bytes.
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        fn val(c: u8) -> Option<u8> {
            match c {
                b'A'..=b'Z' => Some(c - b'A'),
                b'a'..=b'z' => Some(c - b'a' + 26),
                b'0'..=b'9' => Some(c - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }

        // Ignore whitespace and stop at the first `=` (padding).
        let payload: Vec<u8> = s
            .bytes()
            .zip(0..)
            .filter(|&(b, _)| !b.is_ascii_whitespace())
            .take_while(|&(b, _)| b != b'=')
            .map(|(b, _)| b)
            .collect();

        if payload.is_empty() && !s.is_empty() {
            return Err("string contained no base64 digits".into());
        }
        if payload.len() % 4 == 1 {
            return Err(format!("invalid base64 length {}", payload.len()));
        }

        let mut out = Vec::with_capacity(payload.len() / 4 * 3);
        for chunk in payload.chunks(4) {
            let mut n: u32 = 0;
            let mut count = 0usize;
            for &c in chunk {
                let v = val(c).ok_or_else(|| format!("invalid base64 character {c:?}"))?;
                n = (n << 6) | v as u32;
                count += 1;
            }
            match count {
                2 => out.push((n >> 4) as u8),
                3 => {
                    out.push((n >> 10) as u8);
                    out.push((n >> 2) as u8);
                }
                4 => {
                    out.push((n >> 16) as u8);
                    out.push((n >> 8) as u8);
                    out.push(n as u8);
                }
                _ => unreachable!("chunk length is 1..=4"),
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests: serde round-trip of EVERY frame + a version-mismatch error path
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_frame<T>(frame: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(frame).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, &back, "round-trip failed for {json}");
    }

    #[test]
    fn roundtrip_every_client_to_daemon_frame() {
        roundtrip_frame(&ClientToDaemon::Hello {
            version: PROTOCOL_VERSION,
        });
        roundtrip_frame(&ClientToDaemon::Attach {
            session_id: "s1".into(),
            password: None,
        });
        roundtrip_frame(&ClientToDaemon::Attach {
            session_id: "s1".into(),
            password: Some("pw".into()),
        });
        roundtrip_frame(&ClientToDaemon::AttachCreate {
            session_id: "s2".into(),
            cmd: Some("bash".into()),
            password: None,
            protect: false,
        });
        roundtrip_frame(&ClientToDaemon::AttachCreate {
            session_id: "s3".into(),
            cmd: None,
            password: Some("pw".into()),
            protect: true,
        });
        roundtrip_frame(&ClientToDaemon::Input {
            bytes: vec![0x1b, b'[', b'2', b'J'],
        });
        roundtrip_frame(&ClientToDaemon::Resize { rows: 24, cols: 80 });
        roundtrip_frame(&ClientToDaemon::Detach);
        roundtrip_frame(&ClientToDaemon::List);
        roundtrip_frame(&ClientToDaemon::Kill {
            session_id: "s9".into(),
            password: Some("pw".into()),
        });
        roundtrip_frame(&ClientToDaemon::KillAll);
        roundtrip_frame(&ClientToDaemon::Rename {
            from: "old".into(),
            to: "new".into(),
            password: None,
        });
        roundtrip_frame(&ClientToDaemon::Cwd {
            session_id: "s5".into(),
            password: Some("pw".into()),
        });
    }

    #[test]
    fn roundtrip_every_daemon_to_client_frame() {
        roundtrip_frame(&DaemonToClient::Hello {
            daemon_version: PROTOCOL_VERSION,
        });
        roundtrip_frame(&DaemonToClient::Attached {
            session_id: "s1".into(),
            redraw_bytes: vec![0x1b, b'[', b'H', 0x0a],
        });
        roundtrip_frame(&DaemonToClient::Output {
            bytes: vec![b'h', b'i', b'\n'],
        });
        roundtrip_frame(&DaemonToClient::Killed {
            session_id: "s9".into(),
        });
        roundtrip_frame(&DaemonToClient::Renamed {
            session_id: "s10".into(),
        });
        roundtrip_frame(&DaemonToClient::Exit {
            session_id: "s9".into(),
        });
        roundtrip_frame(&DaemonToClient::Listing {
            sessions: vec![
                SessionInfo::new("s1"),
                SessionInfo {
                    id: "s2".into(),
                    title: "vim".into(),
                    attached: 2,
                    protected: true,
                },
            ],
        });
        roundtrip_frame(&DaemonToClient::Error {
            message: "no such session".into(),
        });
        roundtrip_frame(&DaemonToClient::Cwd {
            session_id: "s5".into(),
            cwd: "/Users/v57/projects".into(),
        });
        roundtrip_frame(&DaemonToClient::PasswordRequired {
            session_id: "s5".into(),
            wrong: true,
        });
    }

    // Password fields are optional on the wire: a frame without them (an
    // unprotected request) still decodes, and they are omitted when unset.
    #[test]
    fn password_fields_are_optional_on_the_wire() {
        let frame = ClientToDaemon::from_frame(br#"{"type":"AttachCreate","data":{"session_id":"s"}}"#)
            .unwrap();
        assert_eq!(
            frame,
            ClientToDaemon::AttachCreate {
                session_id: "s".into(),
                cmd: None,
                password: None,
                protect: false,
            }
        );
        let json = String::from_utf8(frame.to_frame().unwrap()).unwrap();
        assert!(!json.contains("password") && !json.contains("protect"), "{json}");
        let info: SessionInfo = serde_json::from_str(r#"{"id":"s"}"#).unwrap();
        assert!(!info.protected);
    }

    #[test]
    fn roundtrip_ndjson_frames() {
        let frames = vec![
            ClientToDaemon::Attach {
                session_id: "s1".into(),
                password: None,
            },
            ClientToDaemon::Resize {
                rows: 24,
                cols: 132,
            },
            ClientToDaemon::Detach,
        ];
        let mut wire = Vec::new();
        for f in &frames {
            wire.extend_from_slice(&f.to_frame().unwrap());
        }
        let mut decoded = Vec::new();
        for line in wire.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            decoded.push(ClientToDaemon::from_frame(line).unwrap());
        }
        assert_eq!(frames, decoded);
    }

    #[test]
    fn base64_roundtrip_various_lengths() {
        // 0..=3 bytes exercise each padding branch of the encoder.
        for len in 0..=64 {
            let data: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31)).collect();
            let enc = b64::encode(&data);
            assert!(enc
                .bytes()
                .all(|b| b.is_ascii() && !b.is_ascii_whitespace()));
            let dec = b64::decode(&enc).unwrap();
            assert_eq!(data, dec, "base64 round-trip failed at len {len}");
        }
    }

    #[test]
    fn base64_binary_payload_survives_json() {
        // NULs and high bytes must survive base64 + JSON untouched.
        let raw = vec![0u8, 255, 1, 2, 3, 0, 128, 254, 10, 13];
        let f = ClientToDaemon::Input { bytes: raw.clone() };
        let frame = f.to_frame().unwrap();
        let back = ClientToDaemon::from_frame(&frame).unwrap();
        assert_eq!(back, f);
        assert_eq!(back, ClientToDaemon::Input { bytes: raw });
    }

    #[test]
    fn version_matches() {
        assert!(check_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn version_mismatch_is_an_error() {
        let err = check_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::VersionMismatch { expected, got } if expected == PROTOCOL_VERSION && got == PROTOCOL_VERSION + 1
        ));
        // Mismatch also refuses to accept an older peer.
        let err = check_version(0).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: 0
            }
        ));
    }

    #[test]
    fn hello_handshake_version_check() {
        // Serde round-trips the Hello carrying the version, and the daemon's
        // version check rejects a peer that speaks an older protocol.
        let hello = ClientToDaemon::Hello {
            version: PROTOCOL_VERSION,
        };
        let back = ClientToDaemon::from_frame(&hello.to_frame().unwrap()).unwrap();
        let ClientToDaemon::Hello { version } = back else {
            panic!("expected Hello frame");
        };
        assert_eq!(version, PROTOCOL_VERSION);
        assert!(check_version(version).is_ok());

        let bad = ClientToDaemon::Hello {
            version: PROTOCOL_VERSION + 1,
        };
        let ClientToDaemon::Hello { version } =
            ClientToDaemon::from_frame(&bad.to_frame().unwrap()).unwrap()
        else {
            panic!("expected Hello frame");
        };
        assert!(matches!(
            check_version(version),
            Err(ProtocolError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn malformed_frame_and_bad_base64_are_errors() {
        assert!(ClientToDaemon::from_frame(b"{oops").is_err());
        assert!(b64::decode("!!").is_err());
    }
}

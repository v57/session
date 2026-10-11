//! Optional per-session passwords.
//!
//! The daemon socket already admits only the same user (peer credentials on
//! Unix, a per-user pipe on Windows). A password goes one step further: other
//! processes running as that user cannot attach to, kill, rename, or read the
//! working directory of a protected session without it. `list` still reports a
//! protected session (marked `protected`), so a frontend like Shell can show it
//! and attach by running `session <name>`, which asks for the password.
//!
//! The daemon keeps only a salted SHA-256 digest, in memory (sessions do not
//! outlive the daemon, so neither do their passwords). Wrong guesses are slowed
//! down by the daemon: each session's attempts take turns behind a
//! [`Protection::turn`] lock, and a wrong one holds it for
//! [`WRONG_PASSWORD_DELAY`], so guessing runs at about one try per second no
//! matter how many connections an attacker opens.

use std::sync::Arc;
use std::time::Duration;

/// How long a wrong password holds its session's turn before the daemon answers.
pub const WRONG_PASSWORD_DELAY: Duration = Duration::from_secs(1);

/// Why a request on a protected session was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    /// No password was sent.
    #[error("this session is password protected")]
    Required,
    /// The password sent does not match.
    #[error("wrong password")]
    Wrong,
}

/// A session's password and the lock its attempts take turns on.
pub struct Protection {
    salt: [u8; 16],
    digest: [u8; 32],
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl Protection {
    /// Protect with `password` under a fresh random salt.
    pub fn new(password: &str) -> Self {
        let salt = *uuid::Uuid::new_v4().as_bytes();
        Self {
            salt,
            digest: digest(&salt, password),
            turn: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Check `password` (`None` = none sent) against the stored digest.
    pub fn check(&self, password: Option<&str>) -> Result<(), Denied> {
        let password = password.ok_or(Denied::Required)?;
        let candidate = digest(&self.salt, password);
        // Constant-time compare: the time taken never depends on how many
        // leading bytes match.
        let diff = candidate
            .iter()
            .zip(self.digest.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if diff == 0 {
            Ok(())
        } else {
            Err(Denied::Wrong)
        }
    }

    /// The lock that password attempts on this session take turns on.
    pub fn turn(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.turn)
    }
}

fn digest(salt: &[u8; 16], password: &str) -> [u8; 32] {
    let mut input = Vec::with_capacity(salt.len() + password.len());
    input.extend_from_slice(salt);
    input.extend_from_slice(password.as_bytes());
    sha256(&input)
}

/// SHA-256 (FIPS 180-4). Inline like the protocol's base64, to keep the binary
/// free of a crypto crate for one hash.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pad: 0x80, zeros to 56 mod 64, then the bit length as a big-endian u64.
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());

    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }

    let mut out = [0u8; 32];
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // FIPS 180-4 / NIST test vectors, including the two-block case.
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&sha256(&[b'a'; 1000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn check_accepts_only_the_password() {
        let p = Protection::new("hunter2");
        assert_eq!(p.check(Some("hunter2")), Ok(()));
        assert_eq!(p.check(Some("hunter3")), Err(Denied::Wrong));
        assert_eq!(p.check(Some("")), Err(Denied::Wrong));
        assert_eq!(p.check(None), Err(Denied::Required));
    }

    #[test]
    fn same_password_gets_a_different_salt() {
        let a = Protection::new("pw");
        let b = Protection::new("pw");
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.digest, b.digest);
    }
}

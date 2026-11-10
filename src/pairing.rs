//! First-contact pairing with a short code that both people can compare.
//!
//! A bare "hash the handshake into six digits" scheme is not enough: someone
//! sitting between the two computers finishes one handshake first and can then
//! try keys on the second until its six digits happen to match. To close that
//! hole each side contributes a random nonce, and the dialling side has to
//! *commit* to its nonce before it sees the other one. Neither side (nor
//! anyone in the middle) can then steer the code, so a match has a one in a
//! million chance of being wrong.

use blake2::{Blake2s256, Digest};

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2s256::new();
    for part in parts {
        h.update((part.len() as u32).to_le_bytes());
        h.update(part);
    }
    h.finalize().into()
}

/// Commitment the dialling side sends before learning the other nonce.
pub fn commitment(handshake_hash: &[u8], nonce: &[u8; 16]) -> [u8; 32] {
    hash(&[b"screen-portals commit", handshake_hash, nonce])
}

/// The six-digit code both computers display.
pub fn code(handshake_hash: &[u8], dialer_nonce: &[u8; 16], listener_nonce: &[u8; 16]) -> String {
    let digest = hash(&[b"screen-portals code", handshake_hash, dialer_nonce, listener_nonce]);
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    format!("{:06}", u64::from_le_bytes(first) % 1_000_000)
}

/// "123456" -> "123 456" for display.
pub fn pretty(code: &str) -> String {
    if code.len() == 6 {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_six_digits_and_deterministic() {
        let c = code(b"transcript", &[1; 16], &[2; 16]);
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        assert_eq!(c, code(b"transcript", &[1; 16], &[2; 16]));
        assert_eq!(pretty("123456"), "123 456");
    }

    #[test]
    fn code_depends_on_every_input() {
        let base = code(b"transcript", &[1; 16], &[2; 16]);
        // Collisions are possible in principle (1 in a million each); these
        // fixed inputs are known not to collide.
        assert_ne!(base, code(b"transcripT", &[1; 16], &[2; 16]));
        assert_ne!(base, code(b"transcript", &[9; 16], &[2; 16]));
        assert_ne!(base, code(b"transcript", &[1; 16], &[9; 16]));
        assert_ne!(base, code(b"transcript", &[2; 16], &[1; 16]));
    }

    #[test]
    fn commitment_binds_nonce_and_transcript() {
        let c = commitment(b"transcript", &[7; 16]);
        assert_eq!(c, commitment(b"transcript", &[7; 16]));
        assert_ne!(c, commitment(b"transcript", &[8; 16]));
        assert_ne!(c, commitment(b"other", &[7; 16]));
    }
}

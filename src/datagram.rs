//! Encrypted UDP datagrams for pointer movement.
//!
//! Packet layout: `session (8) | counter (8) | ciphertext + tag`.
//! The counter doubles as the nonce and as the "newest wins" rule: a packet
//! whose counter is not higher than the last accepted one is dropped, which
//! also makes replayed packets useless.

use crate::proto::Dgram;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Bytes in front of the encrypted part: session and counter.
pub const HEADER: usize = 16;

fn nonce(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(bytes)
}

/// Read the session identifier without decrypting.
pub fn session_of(packet: &[u8]) -> Option<u64> {
    let head: [u8; 8] = packet.get(..8)?.try_into().ok()?;
    Some(u64::from_le_bytes(head))
}

/// Encrypts the datagrams this computer sends on one connection.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    session: u64,
    counter: AtomicU64,
}

impl Sealer {
    /// `key` and `session` are the ones this side announced in its Hello.
    pub fn new(key: &[u8; 32], session: u64) -> Sealer {
        Sealer {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            session,
            counter: AtomicU64::new(0),
        }
    }

    /// Encrypt one message. Each call uses the next counter value, so a nonce is never reused.
    pub fn seal(&self, message: &Dgram) -> Vec<u8> {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let plain = postcard::to_allocvec(message).expect("datagram always serialises");
        let mut packet = Vec::with_capacity(HEADER + plain.len() + 16);
        packet.extend_from_slice(&self.session.to_le_bytes());
        packet.extend_from_slice(&counter.to_le_bytes());
        let sealed = self
            .cipher
            .encrypt(&nonce(counter), Payload { msg: &plain, aad: &packet[..HEADER] })
            .expect("encryption cannot fail for small messages");
        packet.extend_from_slice(&sealed);
        packet
    }
}

/// Checks and decrypts the datagrams arriving on one connection.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    session: u64,
    newest: Mutex<u64>,
}

impl Opener {
    /// `key` and `session` are the ones the other side announced in its Hello.
    pub fn new(key: &[u8; 32], session: u64) -> Opener {
        Opener {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            session,
            newest: Mutex::new(0),
        }
    }

    /// Returns the message only if it is authentic and newer than everything
    /// accepted before.
    pub fn open(&self, packet: &[u8]) -> Option<Dgram> {
        if packet.len() < HEADER + 16 || session_of(packet)? != self.session {
            return None;
        }
        let counter = u64::from_le_bytes(packet[8..16].try_into().ok()?);
        let mut newest = self.newest.lock().unwrap();
        if counter <= *newest {
            return None;
        }
        let plain = self
            .cipher
            .decrypt(&nonce(counter), Payload { msg: &packet[HEADER..], aad: &packet[..HEADER] })
            .ok()?;
        let message = postcard::from_bytes(&plain).ok()?;
        *newest = counter;
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let sealer = Sealer::new(&[3; 32], 42);
        let opener = Opener::new(&[3; 32], 42);
        let packet = sealer.seal(&Dgram::MouseAbs { x: 10, y: 20 });
        assert_eq!(session_of(&packet), Some(42));
        assert_eq!(opener.open(&packet), Some(Dgram::MouseAbs { x: 10, y: 20 }));
    }

    #[test]
    fn replayed_and_stale_packets_are_dropped() {
        let sealer = Sealer::new(&[3; 32], 42);
        let opener = Opener::new(&[3; 32], 42);
        let first = sealer.seal(&Dgram::MouseAbs { x: 1, y: 1 });
        let second = sealer.seal(&Dgram::MouseAbs { x: 2, y: 2 });
        assert!(opener.open(&second).is_some());
        assert_eq!(opener.open(&first), None, "older position must lose");
        assert_eq!(opener.open(&second), None, "replay must be rejected");
    }

    #[test]
    fn tampering_and_wrong_keys_are_rejected() {
        let sealer = Sealer::new(&[3; 32], 42);
        let mut packet = sealer.seal(&Dgram::Probe);
        assert_eq!(Opener::new(&[4; 32], 42).open(&packet), None);
        assert_eq!(Opener::new(&[3; 32], 43).open(&packet), None);
        let last = packet.len() - 1;
        packet[last] ^= 1;
        assert_eq!(Opener::new(&[3; 32], 42).open(&packet), None);
        // A forged high counter must not poison the window.
        let opener = Opener::new(&[3; 32], 42);
        let mut forged = sealer.seal(&Dgram::Probe);
        forged[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(opener.open(&forged), None);
        assert!(opener.open(&sealer.seal(&Dgram::Probe)).is_some());
        assert_eq!(opener.open(&[0u8; 5]), None);
    }
}

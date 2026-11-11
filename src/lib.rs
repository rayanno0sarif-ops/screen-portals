//! Screen Portals: share one mouse, keyboard and clipboard between computers
//! on a local network.
//!
//! The crate is split so that everything outside `platform` is plain,
//! portable Rust that can be tested without touching real input devices.

pub mod audio;
pub mod config;
pub mod datagram;
pub mod discovery;
pub mod keymap;
pub mod layout;
pub mod node;
pub mod pairing;
pub mod platform;
pub mod proto;
pub mod picture;
pub mod secure;
pub mod transfer;
pub mod update;
pub mod uri;
pub mod ui;

/// Fill a buffer with operating-system randomness.
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("operating system random source unavailable");
    buf
}

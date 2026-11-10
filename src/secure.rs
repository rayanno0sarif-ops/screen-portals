//! The encrypted control stream: a Noise XX handshake over TCP followed by
//! length-prefixed encrypted frames. XX means both computers prove ownership
//! of their long-term key, so after pairing a device is recognised by its key
//! and nothing is ever sent in the clear.

use crate::proto::Msg;
use snow::{Builder, TransportState};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"screen-portals/1";
const TAG: usize = 16;
const MAX_FRAME: usize = 65535;
/// One byte of every frame says whether more of the same message follows.
const MAX_CHUNK: usize = MAX_FRAME - TAG - 1;
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn params() -> Result<snow::params::NoiseParams, String> {
    PATTERN.parse().map_err(err)
}

/// Create a new long-term identity: (private key, public key).
pub fn generate_keypair() -> Result<(Vec<u8>, [u8; 32]), String> {
    let pair = Builder::new(params()?).generate_keypair().map_err(err)?;
    let public: [u8; 32] = pair.public.as_slice().try_into().map_err(err)?;
    Ok((pair.private, public))
}

fn write_frame(tcp: &mut TcpStream, data: &[u8]) -> Result<(), String> {
    let mut frame = Vec::with_capacity(2 + data.len());
    frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
    frame.extend_from_slice(data);
    tcp.write_all(&frame).map_err(err)
}

fn read_frame(tcp: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut len = [0u8; 2];
    tcp.read_exact(&mut len).map_err(err)?;
    let mut data = vec![0u8; u16::from_be_bytes(len) as usize];
    tcp.read_exact(&mut data).map_err(err)?;
    Ok(data)
}

/// What a finished handshake gives back.
pub struct Established {
    pub stream: SecureStream,
    /// The other computer's long-term public key, proven by the handshake.
    pub remote_key: [u8; 32],
    /// Hash of the whole handshake; identical on both ends of one connection.
    pub hash: Vec<u8>,
}

/// Run the handshake on a freshly connected socket.
pub fn handshake(mut tcp: TcpStream, initiator: bool, private: &[u8]) -> Result<Established, String> {
    let builder = Builder::new(params()?).local_private_key(private).prologue(PROLOGUE);
    let mut state = if initiator { builder.build_initiator() } else { builder.build_responder() }.map_err(err)?;

    let mut out = vec![0u8; MAX_FRAME];
    let mut scratch = vec![0u8; MAX_FRAME];
    // XX is three messages; the initiator writes the first and the third.
    for step in 0..3 {
        let our_turn = (step % 2 == 0) == initiator;
        if our_turn {
            let n = state.write_message(&[], &mut out).map_err(err)?;
            write_frame(&mut tcp, &out[..n])?;
        } else {
            let frame = read_frame(&mut tcp)?;
            state.read_message(&frame, &mut scratch).map_err(err)?;
        }
    }

    let remote_key: [u8; 32] = state
        .get_remote_static()
        .ok_or("handshake finished without a remote key")?
        .try_into()
        .map_err(err)?;
    let hash = state.get_handshake_hash().to_vec();
    let noise = state.into_transport_mode().map_err(err)?;
    let reader = tcp.try_clone().map_err(err)?;
    let writer = tcp.try_clone().map_err(err)?;
    Ok(Established {
        stream: SecureStream { reader: Mutex::new(reader), writer: Mutex::new(writer), noise: Mutex::new(noise), raw: tcp },
        remote_key,
        hash,
    })
}

/// Encrypted, ordered message stream. One thread may read while others write.
pub struct SecureStream {
    reader: Mutex<TcpStream>,
    writer: Mutex<TcpStream>,
    noise: Mutex<TransportState>,
    raw: TcpStream,
}

impl SecureStream {
    /// Encrypt and send one message, splitting it over several frames if it is large.
    pub fn send(&self, message: &Msg) -> Result<(), String> {
        let bytes = postcard::to_allocvec(message).map_err(err)?;
        if bytes.len() > MAX_MESSAGE {
            return Err("message too large".into());
        }
        // Holding the writer lock for the whole message keeps encryption
        // order identical to the order on the wire.
        let mut writer = self.writer.lock().unwrap();
        let mut plain = Vec::with_capacity(MAX_CHUNK + 1);
        let mut sealed = vec![0u8; MAX_FRAME];
        let mut chunks = bytes.chunks(MAX_CHUNK).peekable();
        while let Some(chunk) = chunks.next() {
            plain.clear();
            plain.push(chunks.peek().is_some() as u8);
            plain.extend_from_slice(chunk);
            let n = self.noise.lock().unwrap().write_message(&plain, &mut sealed).map_err(err)?;
            write_frame(&mut writer, &sealed[..n])?;
        }
        Ok(())
    }

    /// Wait for the next whole message.
    pub fn recv(&self) -> Result<Msg, String> {
        let mut reader = self.reader.lock().unwrap();
        let mut plain = vec![0u8; MAX_FRAME];
        let mut message = Vec::new();
        loop {
            let frame = read_frame(&mut reader)?;
            let n = self.noise.lock().unwrap().read_message(&frame, &mut plain).map_err(err)?;
            if n == 0 {
                return Err("empty frame".into());
            }
            message.extend_from_slice(&plain[1..n]);
            if message.len() > MAX_MESSAGE {
                return Err("message too large".into());
            }
            if plain[0] == 0 {
                break;
            }
        }
        postcard::from_bytes(&message).map_err(err)
    }

    /// How long reading and writing may block before the connection counts as dead.
    pub fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) {
        let _ = self.raw.set_read_timeout(read);
        let _ = self.raw.set_write_timeout(write);
    }

    /// The address of the other end, if the connection is still open.
    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.raw.peer_addr().ok()
    }

    /// Close the connection; any thread blocked in `recv` wakes with an error.
    pub fn shutdown(&self) {
        let _ = self.raw.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn connected_pair() -> (Established, Established, [u8; 32], [u8; 32]) {
        let (a_private, a_public) = generate_keypair().unwrap();
        let (b_private, b_public) = generate_keypair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            handshake(tcp, false, &b_private).unwrap()
        });
        let a = handshake(TcpStream::connect(addr).unwrap(), true, &a_private).unwrap();
        (a, server.join().unwrap(), a_public, b_public)
    }

    #[test]
    fn handshake_authenticates_both_sides() {
        let (a, b, a_public, b_public) = connected_pair();
        assert_eq!(a.remote_key, b_public);
        assert_eq!(b.remote_key, a_public);
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn messages_round_trip_including_large_ones() {
        let (a, b, _, _) = connected_pair();
        a.stream.send(&Msg::Key { hid: 4, down: true }).unwrap();
        assert_eq!(b.stream.recv().unwrap(), Msg::Key { hid: 4, down: true });

        // Bigger than one Noise frame, so it must be split and reassembled.
        let big = "portal ".repeat(60_000);
        let expected = Msg::ClipboardText(big.clone());
        let sender = std::thread::spawn(move || {
            a.stream.send(&Msg::ClipboardText(big)).unwrap();
            a.stream.send(&Msg::Ping).unwrap();
        });
        assert_eq!(b.stream.recv().unwrap(), expected);
        assert_eq!(b.stream.recv().unwrap(), Msg::Ping);
        sender.join().unwrap();
    }

    #[test]
    fn nothing_readable_travels_in_the_clear() {
        let (private, _) = generate_keypair().unwrap();
        let (other_private, _) = generate_keypair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let secret = "correct horse battery staple";
        let client = std::thread::spawn(move || {
            let a = handshake(TcpStream::connect(addr).unwrap(), true, &private).unwrap();
            a.stream.send(&Msg::ClipboardText(secret.to_string())).unwrap();
        });
        // Do the handshake properly, then look at the raw bytes of the next frame.
        let (tcp, _) = listener.accept().unwrap();
        let mut raw = tcp.try_clone().unwrap();
        let _b = handshake(tcp, false, &other_private).unwrap();
        let frame = read_frame(&mut raw).unwrap();
        client.join().unwrap();
        assert!(frame.len() > secret.len());
        assert!(!frame.windows(secret.len()).any(|w| w == secret.as_bytes()));
    }
}

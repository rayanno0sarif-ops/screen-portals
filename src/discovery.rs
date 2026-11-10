//! Finding other Screen Portals computers on the local network.
//!
//! Each computer broadcasts a small announcement every second and a half. An
//! announcement is only a hint about *where* to connect: it carries nothing
//! secret, and whoever answers still has to pass the encrypted handshake with
//! a key this computer has already paired with.

use crate::config::Key;
use crate::node::Node;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// UDP port the "I am here" announcements are broadcast on.
pub const DISCOVERY_PORT: u16 = 47810;
const MAGIC: [u8; 4] = *b"SPB1";
const INTERVAL: Duration = Duration::from_millis(1500);

/// One announcement. Nothing in it is trusted; it only says where to knock.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Beacon {
    pub magic: [u8; 4],
    pub key: Key,
    pub name: String,
    pub tcp_port: u16,
    pub pairing: bool,
}

/// An announcement as the bytes that are broadcast.
pub fn encode(beacon: &Beacon) -> Vec<u8> {
    postcard::to_allocvec(beacon).expect("beacon always serialises")
}

/// Read an announcement, or `None` for anything else that arrives on the port.
pub fn decode(packet: &[u8]) -> Option<Beacon> {
    let beacon: Beacon = postcard::from_bytes(packet).ok()?;
    (beacon.magic == MAGIC).then_some(beacon)
}

fn open_socket(bind: IpAddr) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Lets several copies share the port, e.g. two users on one machine.
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddr::new(bind, DISCOVERY_PORT).into())?;
    let socket: UdpSocket = socket.into();
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(socket)
}

/// Start announcing this computer and listening for others.
pub fn spawn(node: Arc<Node>, bind: IpAddr) {
    thread::spawn(move || {
        let socket = match open_socket(bind) {
            Ok(socket) => socket,
            Err(e) => {
                eprintln!("[screen-portals] automatic discovery is unavailable ({e}); use --connect instead");
                return;
            }
        };
        let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DISCOVERY_PORT);
        let mut buf = [0u8; 512];
        let mut next_announce = Instant::now();
        loop {
            if Instant::now() >= next_announce {
                let beacon = Beacon {
                    magic: MAGIC,
                    key: node.public_key(),
                    name: node.name().to_string(),
                    tcp_port: node.tcp_port(),
                    pairing: node.is_pairing(),
                };
                let _ = socket.send_to(&encode(&beacon), target);
                next_announce = Instant::now() + INTERVAL;
            }
            if let Ok((n, from)) = socket.recv_from(&mut buf) {
                if let Some(beacon) = decode(&buf[..n]) {
                    let addr = SocketAddr::new(from.ip(), beacon.tcp_port);
                    node.on_beacon(beacon.key, &beacon.name, addr, beacon.pairing);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_round_trip() {
        let beacon = Beacon { magic: MAGIC, key: [9; 32], name: "desk".into(), tcp_port: 47811, pairing: true };
        assert_eq!(decode(&encode(&beacon)), Some(beacon));
    }

    #[test]
    fn junk_is_ignored() {
        assert_eq!(decode(b""), None);
        assert_eq!(decode(b"hello there, not a beacon"), None);
        let wrong = Beacon { magic: *b"NOPE", key: [9; 32], name: "desk".into(), tcp_port: 1, pairing: false };
        assert_eq!(decode(&encode(&wrong)), None);
    }
}

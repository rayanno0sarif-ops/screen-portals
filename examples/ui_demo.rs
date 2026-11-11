//! Preview the settings window without touching real input devices.
//!
//! Starts three pretend computers on 127.0.0.1 (a desk PC, a laptop and an
//! offline tablet), pairs them, and serves the desk's settings page. Open the
//! printed address in a browser. Nothing here hooks the mouse or keyboard.
//!
//!     cargo run --example ui_demo [--empty | --slow] [port-file]
//!
//! `--empty` starts with nothing paired; `--slow` adds a large file that is
//! still on its way, to watch a transfer in progress.

use screen_portals::config::{PeerInfo, Store};
use screen_portals::layout::{PortalMode, Side};
use screen_portals::node::{Node, Options, PairingOptions};
use screen_portals::platform::mock::MockPlatform;
use screen_portals::ui::{self, PairPrompt};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn options(name: &str, pairing: Option<PairingOptions>, received: PathBuf) -> Options {
    Options { name: name.into(), bind: LOCALHOST, tcp_port: 0, discovery: false, pairing, connect: Vec::new(), received_dir: Some(received) }
}

/// Pretend file contents that arrive slowly, so progress can be watched.
struct Trickle(u64);

impl std::io::Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(Duration::from_millis(12));
        let n = (buf.len() as u64).min(self.0) as usize;
        buf[..n].fill(0x5A);
        self.0 -= n as u64;
        Ok(n)
    }
}

fn main() -> Result<(), String> {
    let root = std::env::temp_dir().join(format!("screen-portals-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let empty = std::env::args().any(|a| a == "--empty");

    let prompt = PairPrompt::new();
    let desk_store = Store::open(Some(root.join("desk")))?;
    if !empty {
        // A computer that was paired earlier and is switched off right now.
        let mut config = desk_store.load_config()?;
        let mut tablet = PeerInfo::new(&[7u8; 32], "Studio tablet", Side::Above);
        tablet.screen = Some([1600, 1000]);
        tablet.offset = 480;
        tablet.mode = PortalMode::Touching;
        config.upsert(tablet);
        desk_store.save_config(&config)?;
    }
    let desk = Node::start(
        Arc::new(MockPlatform::new(2560, 1440)),
        desk_store,
        options(
            "Desk PC",
            Some(PairingOptions { side: Some(Side::Right), confirm: prompt.confirm_fn(), active: !empty }),
            root.join("desk-received"),
        ),
    )?;
    let server = ui::serve(desk.clone(), prompt.clone(), Box::new(|| std::process::exit(0)))?;

    let laptop = Node::start(
        Arc::new(MockPlatform::new(1920, 1200)),
        Store::open(Some(root.join("laptop")))?,
        options(
            "Work laptop",
            Some(PairingOptions { side: None, confirm: Box::new(|_, _| true), active: true }),
            root.join("laptop-received"),
        ),
    )?;

    if !empty {
        // Pair them; answer the desk's question the way the window would.
        let answering = prompt.clone();
        std::thread::spawn(move || loop {
            if answering.current().is_some() {
                answering.answer(true);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        });
        laptop.dial(SocketAddr::new(LOCALHOST, desk.tcp_port()))?;
        while desk.connected().is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }

        // Something for the list of transfers: a file that arrived from the
        // laptop, and with --slow a large one trickling out to it.
        let note = root.join("notes from the laptop.txt");
        std::fs::write(&note, "Sent from the pretend laptop.").map_err(|e| e.to_string())?;
        laptop.send_files(&desk.public_key(), vec![note])?;
        if std::env::args().any(|a| a == "--slow") {
            let (desk, to) = (desk.clone(), laptop.public_key());
            std::thread::spawn(move || {
                let size = 48 * 1024 * 1024;
                let _ = desk.send_stream(&to, "holiday video.mp4", size, &mut Trickle(size));
            });
        }
    } else {
        // Start with nothing paired. When "Add a computer" is pressed in the
        // window, the laptop shows up and the code has to be confirmed there.
        let (desk, laptop) = (desk.clone(), laptop.clone());
        std::thread::spawn(move || loop {
            if desk.is_pairing() && desk.peers().is_empty() {
                let _ = laptop.dial(SocketAddr::new(LOCALHOST, desk.tcp_port()));
            }
            std::thread::sleep(Duration::from_millis(500));
        });
    }

    println!("{}", server.url());
    if let Some(file) = std::env::args().skip(1).find(|a| !a.starts_with("--")) {
        std::fs::write(file, server.url()).map_err(|e| e.to_string())?;
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

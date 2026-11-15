//! A pretend computer for testing a real one.
//!
//! It pairs with whoever connects, prints everything it is asked to do
//! (one `EVENT ...` line each), and acts on commands read from its input:
//!
//!     move <dx> <dy>         move its mouse
//!     key <hid> <1|0>        press or release a key (USB HID number)
//!     button <n> <1|0>       press or release a mouse button
//!     copy <text>            copy some text
//!
//! Used by `tests/macos-desktop.sh`, where a real Mac is the other side.
//!
//!     cargo run --example pretend_peer -- <port> <settings folder>

use screen_portals::config::Store;
use screen_portals::node::{Node, Options, PairingOptions};
use screen_portals::platform::mock::{Event, MockPlatform};
use screen_portals::platform::Platform;
use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn describe(event: &Event) -> String {
    match event {
        // Pictures are long; their size is enough to compare.
        Event::ClipboardImage(png) => format!("ClipboardImage len={}", png.len()),
        other => format!("{other:?}"),
    }
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).ok_or("say which port to listen on")?;
    let dir = PathBuf::from(args.next().ok_or("say which settings folder to use")?);
    let screen = Arc::new(MockPlatform::new(1600, 1000));
    let node = Node::start(
        screen.clone(),
        Store::open(Some(dir.join("settings")))?,
        Options {
            name: "pretend-pc".into(),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            tcp_port: port,
            discovery: false,
            pairing: Some(PairingOptions { side: None, confirm: Box::new(|_, _| true), active: true }),
            connect: Vec::new(),
            received_dir: Some(dir.join("received")),
        },
    )?;
    println!("LISTENING {}", node.tcp_port());

    {
        let (node, screen) = (node.clone(), screen.clone());
        std::thread::spawn(move || {
            let mut shown = 0;
            let mut connected = false;
            loop {
                if !connected && !node.connected().is_empty() {
                    connected = true;
                    println!("CONNECTED");
                }
                let events = screen.events();
                for event in events.iter().skip(shown) {
                    println!("EVENT {}", describe(event));
                }
                shown = events.len();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
    }

    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let number = |i: usize| words.get(i).and_then(|w| w.parse::<i32>().ok()).unwrap_or(0);
        match words.first().copied() {
            Some("move") => {
                let (x, y) = screen.cursor();
                node.on_mouse_move(x + number(1), y + number(2));
            }
            Some("key") => {
                node.on_key(number(1) as u16, number(2) != 0);
            }
            Some("button") => {
                node.on_button(number(1) as u8, number(2) != 0);
            }
            Some("copy") => node.on_clipboard_text(line.trim_start()["copy".len()..].trim().to_string()),
            _ => {}
        }
        println!("DONE {line}");
    }
    node.shutdown();
    Ok(())
}

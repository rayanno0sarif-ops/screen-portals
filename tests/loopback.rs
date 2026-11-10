//! End-to-end tests: two (or three) complete engines talking over real
//! sockets on 127.0.0.1, each attached to a pretend computer that records
//! what it was told to do. No real cursor or keyboard is touched.

use screen_portals::config::Store;
use screen_portals::layout::{Placement, PortalMode, Side};
use screen_portals::node::{Node, Options, PairingOptions};
use screen_portals::platform::mock::{Event, MockPlatform};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("screen-portals-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct Computer {
    node: Arc<Node>,
    screen: Arc<MockPlatform>,
    codes: Arc<Mutex<Vec<String>>>,
}

impl Computer {
    fn addr(&self) -> SocketAddr {
        SocketAddr::new(LOCALHOST, self.node.tcp_port())
    }
}

/// `pairing`: None = normal run mode, Some((side, accept)) = pairing mode.
fn computer(name: &str, dir: &PathBuf, size: (i32, i32), pairing: Option<(Option<Side>, bool)>) -> Computer {
    let screen = Arc::new(MockPlatform::new(size.0, size.1));
    let codes = Arc::new(Mutex::new(Vec::new()));
    let seen = codes.clone();
    let node = Node::start(
        screen.clone(),
        Store::open(Some(dir.clone())).unwrap(),
        Options {
            name: name.to_string(),
            bind: LOCALHOST,
            tcp_port: 0,
            discovery: false,
            pairing: pairing.map(|(side, accept)| PairingOptions {
                side,
                active: true,
                confirm: Box::new(move |_name: &str, code: &str| {
                    seen.lock().unwrap().push(code.to_string());
                    accept
                }),
            }),
            connect: Vec::new(),
            received_dir: Some(dir.join("received")),
        },
    )
    .unwrap();
    Computer { node, screen, codes }
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_event(screen: &MockPlatform, what: &str, matcher: impl Fn(&Event) -> bool) -> Event {
    let mut found = None;
    wait_for(what, || {
        found = screen.events().into_iter().find(|e| matcher(e));
        found.is_some()
    });
    found.unwrap()
}

#[test]
fn pair_then_share_mouse_keyboard_and_clipboard() {
    let (dir_a, dir_b) = (temp_dir("a"), temp_dir("b"));
    // "desk" is 1000x500 and is told that "laptop" (2000x1000) sits to its right.
    let a = computer("desk", &dir_a, (1000, 500), Some((Some(Side::Right), true)));
    let b = computer("laptop", &dir_b, (2000, 1000), Some((None, true)));

    // --- pairing ---
    assert_eq!(a.node.dial(b.addr()).unwrap(), "laptop");
    wait_for("laptop to finish pairing", || !b.node.connected().is_empty());
    let (code_a, code_b) = (a.codes.lock().unwrap()[0].clone(), b.codes.lock().unwrap()[0].clone());
    assert_eq!(code_a, code_b, "both computers must show the same code");
    assert_eq!(code_a.len(), 6);
    assert_eq!(a.node.peers()[0].side, Side::Right);
    assert_eq!(b.node.peers()[0].side, Side::Left, "the other computer mirrors the side");
    assert_eq!(b.node.peers()[0].name, "desk");

    // --- the encrypted UDP path comes up in both directions ---
    wait_for("fast pointer path", || {
        a.node.connected().iter().all(|l| l.fast_pointer) && b.node.connected().iter().all(|l| l.fast_pointer)
    });

    // --- ordinary local movement is left alone ---
    assert!(!a.node.on_mouse_move(500, 250));
    assert!(!a.node.on_key(0x04, true));
    assert!(!a.node.on_key(0x04, false));
    assert!(!a.node.is_remote());

    // --- holding Shift, push through the right edge at mid height ---
    assert!(!a.node.on_key(0xE1, true));
    assert!(a.node.on_mouse_move(1003, 250), "crossing event must be hidden locally");
    assert!(a.node.is_remote());
    assert_eq!(a.screen.events(), vec![Event::Warp(500, 250)], "local cursor parks mid-screen");
    let Event::Abs(x, y) = wait_event(&b.screen, "cursor entering laptop", |e| matches!(e, Event::Abs(..))) else {
        unreachable!()
    };
    assert!(x < 200, "enters at the left edge, got {x}");
    assert!((y as i32 - 32767).abs() < 200, "enters at the same relative height, got {y}");
    wait_event(&b.screen, "held Shift follows the cursor", |e| *e == Event::Key(0xE1, true));

    // --- moving 100px right: travels by UDP, lands 100px further on the laptop ---
    b.screen.clear();
    assert!(a.node.on_mouse_move(600, 250));
    let expected_x = ((2 + 100) * 65535 / 1999) as u16;
    wait_event(&b.screen, "pointer movement", |e| *e == Event::Abs(expected_x, y));

    // --- typing, clicking and scrolling go to the laptop, not the desk ---
    assert!(a.node.on_key(0x04, true));
    assert!(a.node.on_button(1, true));
    assert!(a.node.on_button(1, false));
    assert!(a.node.on_wheel(0, -120));
    wait_event(&b.screen, "key", |e| *e == Event::Key(0x04, true));
    wait_event(&b.screen, "wheel", |e| *e == Event::Wheel(0, -120));
    let events = b.screen.events();
    let click = events.iter().position(|e| *e == Event::Button(1, true)).expect("click arrived");
    assert_eq!(events[click - 1], Event::Abs(expected_x, y), "a click carries its own position");
    assert!(events.contains(&Event::Button(1, false)));

    // --- releasing the Shift that was pressed on the desk reaches both ---
    assert!(!a.node.on_key(0xE1, false), "the desk must see its own Shift release");
    wait_event(&b.screen, "Shift release", |e| *e == Event::Key(0xE1, false));

    // --- clipboard goes across, and is not bounced back ---
    a.node.on_clipboard_text("hello from desk".to_string());
    wait_event(&b.screen, "clipboard", |e| *e == Event::Clipboard("hello from desk".into()));
    b.node.on_clipboard_text("hello from desk".to_string()); // the laptop noticing its own update
    b.node.on_clipboard_text("reply from laptop".to_string());
    wait_event(&a.screen, "clipboard reply", |e| *e == Event::Clipboard("reply from laptop".into()));
    assert!(!a.screen.events().contains(&Event::Clipboard("hello from desk".into())), "no echo");

    // --- pulling back left returns the cursor; the still-held A key is released over there ---
    a.screen.clear();
    assert!(a.node.on_mouse_move(500 - 800, 250));
    assert!(!a.node.is_remote());
    assert_eq!(a.screen.events(), vec![Event::Warp(997, 250)], "cursor reappears just inside the right edge");
    wait_event(&b.screen, "stuck key released", |e| *e == Event::Key(0x04, false));
    assert!(!a.node.on_mouse_move(600, 250));

    // --- a drag in progress never switches computers ---
    assert!(!a.node.on_button(1, true));
    assert!(!a.node.on_mouse_move(1005, 250));
    assert!(!a.node.is_remote());
    assert!(!a.node.on_button(1, false));

    // --- Ctrl+Alt+Home is the escape hatch ---
    assert!(a.node.on_mouse_move(1005, 100));
    assert!(a.node.is_remote());
    assert!(a.node.on_key(0xE0, true));
    assert!(a.node.on_key(0xE2, true));
    assert!(a.node.on_key(0x4A, true));
    assert!(!a.node.is_remote(), "escape hotkey returns control");

    // --- if the connection drops while the cursor is away, it comes home ---
    wait_for("still linked", || !a.node.connected().is_empty());
    assert!(a.node.on_mouse_move(1005, 100));
    assert!(a.node.is_remote());
    b.screen.clear();
    b.node.shutdown();
    wait_for("cursor to come home after disconnect", || !a.node.is_remote());
    wait_for("links to close", || a.node.connected().is_empty() && b.node.connected().is_empty());

    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
}

#[test]
fn pairing_survives_restart_and_strangers_are_refused() {
    let (dir_a, dir_b, dir_c) = (temp_dir("ra"), temp_dir("rb"), temp_dir("rc"));
    {
        let a = computer("desk", &dir_a, (1000, 500), Some((None, true)));
        let b = computer("laptop", &dir_b, (1000, 500), Some((Some(Side::Above), true)));
        a.node.dial(b.addr()).unwrap();
        wait_for("pairing", || !b.node.connected().is_empty());
        a.node.shutdown();
        b.node.shutdown();
        wait_for("old links to close", || a.node.connected().is_empty() && b.node.connected().is_empty());
    }

    // "Restart" both in normal mode from the same settings folders.
    let a = computer("desk", &dir_a, (1000, 500), None);
    let b = computer("laptop", &dir_b, (1000, 500), None);
    assert_eq!(a.node.peers()[0].side, Side::Below);
    a.node.dial(b.addr()).unwrap();
    wait_for("reconnect without any prompt", || !b.node.connected().is_empty());
    assert!(a.codes.lock().unwrap().is_empty() && b.codes.lock().unwrap().is_empty());

    // A stranger in pairing mode cannot get into computers that are not pairing.
    let stranger = computer("stranger", &dir_c, (800, 600), Some((Some(Side::Left), true)));
    assert!(stranger.node.dial(a.addr()).is_err());
    assert!(stranger.node.peers().is_empty());
    assert_eq!(a.node.connected().len(), 1);
    assert!(stranger.codes.lock().unwrap().is_empty(), "no code is ever shown for a refused device");

    for dir in [dir_a, dir_b, dir_c] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn declining_the_code_pairs_nothing() {
    let (dir_a, dir_b) = (temp_dir("da"), temp_dir("db"));
    let a = computer("desk", &dir_a, (1000, 500), Some((Some(Side::Right), true)));
    let b = computer("laptop", &dir_b, (1000, 500), Some((None, false))); // says "no"
    assert!(a.node.dial(b.addr()).is_err());
    assert!(a.node.peers().is_empty());
    assert!(b.node.peers().is_empty());
    assert!(a.node.connected().is_empty());

    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn pairing_without_a_side_starts_with_a_consistent_guess() {
    let (dir_a, dir_b) = (temp_dir("sa"), temp_dir("sb"));
    let a = computer("desk", &dir_a, (1000, 500), Some((None, true)));
    let b = computer("laptop", &dir_b, (1000, 500), Some((None, true)));
    a.node.dial(b.addr()).unwrap();
    wait_for("pairing", || !b.node.connected().is_empty());
    assert_eq!(a.node.peers()[0].side, Side::Right);
    assert_eq!(b.node.peers()[0].side, Side::Left);
    assert!(!a.node.is_pairing() && !b.node.is_pairing(), "pairing mode ends once a computer is added");

    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn arranging_on_one_computer_updates_both_and_moves_the_doorway() {
    let (dir_a, dir_b) = (temp_dir("la"), temp_dir("lb"));
    let a = computer("desk", &dir_a, (1000, 500), Some((Some(Side::Right), true)));
    let b = computer("laptop", &dir_b, (2000, 1000), Some((None, true)));
    a.node.dial(b.addr()).unwrap();
    wait_for("pairing", || !b.node.connected().is_empty());
    let key_b = a.node.peers()[0].key_bytes().unwrap();

    // Sizes are learned at connection time, so both can draw each other.
    assert_eq!(a.node.peers()[0].screen, Some([2000, 1000]));
    wait_for("laptop to learn the desk's size", || b.node.peers()[0].screen == Some([1000, 500]));

    // Move the laptop above the desk and only let the mouse through the
    // right-hand corner.
    a.node.set_placement(&key_b, Placement { side: Side::Above, offset: -400, mode: PortalMode::End }).unwrap();
    wait_for("laptop to mirror the arrangement", || {
        let p = &b.node.peers()[0];
        (p.side, p.offset, p.mode) == (Side::Below, 400, PortalMode::End)
    });
    let status = a.node.status();
    let door = status.peers[0].door.clone().expect("a doorway exists");
    assert!((door.local[0] - 0.75).abs() < 1e-9 && (door.local[1] - 1.0).abs() < 1e-9);
    assert!((door.remote[0] - 0.875).abs() < 1e-9);

    // The old right edge is now a wall.
    assert!(!a.node.on_mouse_move(1005, 250));
    assert!(!a.node.on_mouse_move(1005, 250));
    // The top edge away from the corner is a wall too.
    assert!(!a.node.on_mouse_move(300, -5));
    // The top edge near the right corner is the doorway.
    assert!(a.node.on_mouse_move(900, -5));
    assert!(a.node.is_remote());
    assert_eq!(status.controlling, None);
    assert_eq!(a.node.status().controlling.as_deref(), Some("laptop"));
    // It arrives at the bottom edge of the laptop, in its right-hand corner.
    let Event::Abs(x, y) = wait_event(&b.screen, "arrival", |e| matches!(e, Event::Abs(..))) else { unreachable!() };
    assert!(y > 65000, "bottom edge, got {y}");
    assert!(x > 57000, "right-hand corner, got {x}");

    // Sliding left along the laptop's bottom edge leaves the doorway: wall.
    assert!(a.node.on_mouse_move(500 - 1500, 250));
    assert!(a.node.on_mouse_move(500, 250 + 50));
    assert!(a.node.is_remote(), "outside the corner the edge does not let the cursor back");
    // Back inside the corner it does.
    assert!(a.node.on_mouse_move(500 + 1500, 250));
    a.screen.clear();
    assert!(a.node.on_mouse_move(500, 250 + 50));
    assert!(!a.node.is_remote());
    let Event::Warp(x, y) = a.screen.events()[0].clone() else { unreachable!() };
    assert_eq!(y, 2, "returns just inside the top edge");
    assert!(x >= 750, "returns inside the corner, got {x}");

    // Pausing keeps the cursor here even at the doorway.
    a.node.set_paused(true);
    assert!(!a.node.on_mouse_move(900, -5));
    a.node.set_paused(false);
    assert!(a.node.on_mouse_move(900, -5));
    a.node.come_home();

    // Forgetting drops the connection and the pairing.
    assert!(a.node.forget_key(&key_b).unwrap());
    wait_for("link to close", || a.node.connected().is_empty());
    assert!(a.node.peers().is_empty());
    assert!(!a.node.on_mouse_move(900, -5));

    b.node.shutdown();
    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn http(port: u16, request: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect((LOCALHOST, port)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

fn post(port: u16, token: &str, path: &str, json: &str) -> (u16, String) {
    http(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Portal-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
            json.len()
        ),
    )
}

#[test]
fn settings_window_can_pair_and_arrange_but_only_with_its_token() {
    use screen_portals::ui::{self, PairPrompt};
    let (dir_a, dir_b) = (temp_dir("ua"), temp_dir("ub"));
    let prompt = PairPrompt::new();
    let screen_a = Arc::new(MockPlatform::new(1000, 500));
    let a = Node::start(
        screen_a,
        Store::open(Some(dir_a.clone())).unwrap(),
        Options {
            name: "desk".into(),
            bind: LOCALHOST,
            tcp_port: 0,
            discovery: false,
            pairing: Some(PairingOptions { side: None, confirm: prompt.confirm_fn(), active: false }),
            connect: Vec::new(),
            received_dir: Some(dir_a.join("received")),
        },
    )
    .unwrap();
    let b = computer("laptop", &dir_b, (2000, 1000), Some((None, true)));
    let server = ui::serve(a.clone(), prompt.clone(), Box::new(|| {})).unwrap();
    let (port, token) = (server.port, server.token.clone());
    assert!(server.url().ends_with(&format!("#{token}")));

    // The page itself is served; the data is not, without the token.
    let (status, page) = http(port, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"));
    assert_eq!(status, 200);
    assert!(page.contains("<title>Screen Portals</title>"));
    assert!(!page.contains(&token), "the token never appears in the page");
    assert_eq!(http(port, &format!("GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")).0, 403);
    assert_eq!(http(port, &format!("GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Portal-Token: wrong\r\n\r\n")).0, 403);
    // A web page that tricked a browser into reaching us under another name is refused.
    assert_eq!(http(port, &format!("GET /api/state HTTP/1.1\r\nHost: evil.example:{port}\r\nX-Portal-Token: {token}\r\n\r\n")).0, 403);
    assert_eq!(http(port, &format!("GET / HTTP/1.1\r\nHost: evil.example\r\n\r\n")).0, 403);

    let get_state = || {
        let (status, body) = http(port, &format!("GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Portal-Token: {token}\r\n\r\n"));
        assert_eq!(status, 200);
        serde_json::from_str::<serde_json::Value>(&body).unwrap()
    };
    let state = get_state();
    assert_eq!(state["name"], "desk");
    assert_eq!(state["pairing"], false);
    assert_eq!(state["can_pair"], true);
    assert_eq!(state["prompt"], serde_json::Value::Null);

    // Not in pairing mode yet: a new computer is refused.
    assert!(b.node.dial(SocketAddr::new(LOCALHOST, a.tcp_port())).is_err());

    // "Add a computer", then the code appears in the window and is accepted there.
    assert_eq!(post(port, &token, "/api/pairing", r#"{"on":true}"#).0, 200);
    b.node.set_pairing(true);
    let a_addr = SocketAddr::new(LOCALHOST, a.tcp_port());
    let dialer = { let b = b.node.clone(); std::thread::spawn(move || b.dial(a_addr)) };
    let mut code = String::new();
    wait_for("the code to show in the window", || {
        code = get_state()["prompt"]["code"].as_str().unwrap_or("").to_string();
        !code.is_empty()
    });
    assert_eq!(get_state()["prompt"]["name"], "laptop");
    assert_eq!(post(port, &token, "/api/pairing/answer", r#"{"on":true}"#).0, 200);
    dialer.join().unwrap().unwrap();
    assert_eq!(code, b.codes.lock().unwrap()[0]);
    wait_for("the new computer to appear", || get_state()["peers"][0]["connected"] == true);
    let state = get_state();
    assert_eq!(state["peers"][0]["name"], "laptop");
    assert_eq!(state["peers"][0]["screen"], serde_json::json!([2000, 1000]));
    assert_eq!(state["pairing"], false);
    let key = state["peers"][0]["key"].as_str().unwrap().to_string();

    // Arrange it from the window.
    let (status, body) = post(port, &token, "/api/layout", &format!(r#"{{"key":"{key}","side":"below","offset":-9999,"mode":"touching"}}"#));
    assert_eq!(status, 200, "{body}");
    let state = get_state();
    assert_eq!(state["peers"][0]["side"], "below");
    assert_eq!(state["peers"][0]["offset"], 80 - 2000, "kept in contact with this screen");
    assert_eq!(state["peers"][0]["mode"], "touching");
    wait_for("the laptop to follow", || b.node.peers()[0].side == Side::Above);

    // Bad input is rejected without changing anything.
    assert_eq!(post(port, &token, "/api/layout", r#"{"key":"zz","side":"below","offset":0,"mode":"edge"}"#).0, 400);
    assert_eq!(post(port, &token, "/api/layout", &format!(r#"{{"key":"{key}","side":"sideways","offset":0,"mode":"edge"}}"#)).0, 400);
    assert_eq!(post(port, &token, "/api/nonsense", "{}").0, 400);

    // Choosing where sound plays, from the window.
    assert_eq!(post(port, &token, "/api/speakers", &format!(r#"{{"key":"{key}"}}"#)).0, 200);
    assert_eq!(get_state()["speakers"], key.as_str());
    assert_eq!(post(port, &token, "/api/speakers", r#"{"key":"zz"}"#).0, 400);
    assert_eq!(post(port, &token, "/api/speakers", r#"{"key":null}"#).0, 200);
    assert_eq!(get_state()["speakers"], serde_json::Value::Null);
    assert_eq!(get_state()["sound"], "own");

    assert_eq!(post(port, &token, "/api/pause", r#"{"on":true}"#).0, 200);
    assert!(a.is_paused());
    assert_eq!(post(port, &token, "/api/forget", &format!(r#"{{"key":"{key}"}}"#)).0, 200);
    assert!(a.peers().is_empty());

    a.shutdown();
    b.node.shutdown();
    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn pair(tag: &str) -> (Computer, Computer, PathBuf, PathBuf) {
    let (dir_a, dir_b) = (temp_dir(&format!("{tag}a")), temp_dir(&format!("{tag}b")));
    let a = computer("desk", &dir_a, (1000, 500), Some((Some(Side::Right), true)));
    let b = computer("laptop", &dir_b, (2000, 1000), Some((None, true)));
    a.node.dial(b.addr()).unwrap();
    wait_for("pairing", || !b.node.connected().is_empty());
    (a, b, dir_a, dir_b)
}

/// A folder with something nested, an empty file, and a file several times
/// larger than the sender is allowed to run ahead.
fn sample_files(tag: &str) -> (PathBuf, Vec<u8>) {
    let src = temp_dir(tag);
    std::fs::create_dir_all(src.join("album/inner")).unwrap();
    std::fs::write(src.join("album/inner/deep.txt"), "deep").unwrap();
    std::fs::write(src.join("album/empty.txt"), "").unwrap();
    let big: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    (src, big)
}

#[test]
fn copied_files_and_pictures_follow_the_cursor() {
    use screen_portals::node::TransferState;
    use screen_portals::proto::TransferKind;
    let (a, b, dir_a, dir_b) = pair("carry");
    let (src, big) = sample_files("carrysrc");

    // Copying alone sends nothing: files wait for the cursor.
    a.node.on_clipboard_files(vec![src.join("album"), src.join("big.bin")]);
    std::thread::sleep(Duration::from_millis(150));
    assert!(a.node.transfers().is_empty() && b.node.transfers().is_empty());

    // Crossing to the laptop takes them along, ready to paste there.
    assert!(a.node.on_mouse_move(1003, 250));
    let Event::ClipboardFiles(paths) = wait_event(&b.screen, "files on the laptop's clipboard", |e| matches!(e, Event::ClipboardFiles(_)))
    else {
        unreachable!()
    };
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0].file_name().unwrap(), "album");
    assert_eq!(paths[1].file_name().unwrap(), "big.bin");
    assert_eq!(std::fs::read(&paths[1]).unwrap(), big, "contents arrive byte for byte");
    assert_eq!(std::fs::read_to_string(paths[0].join("inner/deep.txt")).unwrap(), "deep");
    assert_eq!(std::fs::read(paths[0].join("empty.txt")).unwrap(), b"");
    assert!(!paths[0].starts_with(&dir_b), "copied files wait in a private temporary folder");

    wait_for("the desk to hear it all arrived", || a.node.transfers()[0].state == TransferState::Done);
    let sent = a.node.transfers().remove(0);
    assert_eq!((sent.incoming, sent.kind, sent.files, sent.bytes), (false, TransferKind::Clipboard, 3, 700_004));
    assert_eq!((sent.label.as_str(), sent.peer.as_str(), sent.done), ("album", "laptop", 700_004));
    let got = b.node.transfers().remove(0);
    assert_eq!((got.incoming, got.state, got.peer.as_str(), got.files), (true, TransferState::Done, "desk", 3));

    // Going back and forth again does not send the same copy twice.
    a.node.come_home();
    assert!(a.node.on_mouse_move(1003, 250));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(a.node.transfers().len(), 1);

    // A picture copied on the laptop comes back with the cursor.
    let png = screen_portals::picture::png_from_bgra(2, 2, &[0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 9, 9, 9, 0]).unwrap();
    b.screen.copy_image(Some(png.clone()));
    b.node.on_clipboard_image();
    a.node.come_home();
    wait_event(&a.screen, "the picture on the desk's clipboard", |e| *e == Event::ClipboardImage(png.clone()));
    wait_for("the laptop to hear it arrived", || b.node.transfers()[0].state == TransferState::Done);
    assert_eq!(b.node.transfers()[0].kind, TransferKind::Image);

    // New files replace the earlier ones on the laptop; the old copies are removed.
    std::fs::write(src.join("note.txt"), "second copy").unwrap();
    a.node.on_clipboard_files(vec![src.join("note.txt")]);
    b.screen.clear();
    assert!(a.node.on_mouse_move(1003, 250));
    let Event::ClipboardFiles(newer) = wait_event(&b.screen, "the newer files", |e| matches!(e, Event::ClipboardFiles(_))) else {
        unreachable!()
    };
    assert_eq!(std::fs::read_to_string(&newer[0]).unwrap(), "second copy");
    wait_for("the earlier copies to be cleaned up", || !paths[1].exists());
    a.node.come_home();

    // Too much to carry automatically: it stays behind, and says why.
    a.node.set_carry_limit(1000);
    a.node.on_clipboard_files(vec![src.join("big.bin")]);
    b.screen.clear();
    assert!(a.node.on_mouse_move(1003, 250));
    wait_for("the large copy to be skipped", || a.node.transfers()[0].state == TransferState::Skipped);
    assert!(a.node.transfers()[0].note.as_deref().unwrap().starts_with("Larger than"));
    a.node.come_home();

    // The receiving computer applies its own limit as well.
    a.node.set_carry_limit(u64::MAX);
    b.node.set_carry_limit(1000);
    a.node.on_clipboard_files(vec![src.join("big.bin")]);
    assert!(a.node.on_mouse_move(1003, 250));
    wait_for("the laptop to refuse", || a.node.transfers()[0].state == TransferState::Failed);
    assert_eq!(b.node.transfers()[0].state, TransferState::Skipped);
    assert!(!b.screen.events().iter().any(|e| matches!(e, Event::ClipboardFiles(_))), "nothing refused reaches the clipboard");

    // Typing still works while and after all of this.
    assert!(a.node.on_key(0x04, true));
    wait_event(&b.screen, "key", |e| *e == Event::Key(0x04, true));

    a.node.shutdown();
    b.node.shutdown();
    for dir in [dir_a, dir_b, src] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn http_bytes(port: u16, head: &str, body: &[u8]) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect((LOCALHOST, port)).unwrap();
    stream.write_all(head.as_bytes()).unwrap();
    // A refusal may arrive (and close the connection) before the body is out.
    let _ = stream.write_all(body);
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

#[test]
fn files_sent_from_the_window_are_saved_and_never_overwrite() {
    use screen_portals::node::TransferState;
    use screen_portals::ui::{self, PairPrompt};
    let (a, b, dir_a, dir_b) = pair("win");
    let (src, big) = sample_files("winsrc");
    let server = ui::serve(a.node.clone(), PairPrompt::new(), Box::new(|| {})).unwrap();
    let (port, token) = (server.port, server.token.clone());
    let key_b = a.node.peers()[0].key.clone();
    let received = dir_b.join("received");
    let send = |token: &str, peer: &str, name: &str, body: &[u8]| {
        let head = format!(
            "POST /api/send HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Portal-Token: {token}\r\nX-Portal-Peer: {peer}\r\nX-Portal-File: {name}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        http_bytes(port, &head, body)
    };

    // The call returns once the other computer has the whole file in place.
    let (status, state) = send(&token, &key_b, "report%20final.pdf", &big);
    assert_eq!(status, 200, "{state}");
    let saved = received.join("report final.pdf");
    assert_eq!(std::fs::read(&saved).unwrap(), big);
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["transfers"][0]["state"], "done");
    assert_eq!(state["transfers"][0]["kind"], "sent");
    assert_eq!(state["transfers"][0]["label"], "report final.pdf");
    assert_eq!(state["transfers"][0]["bytes"], 700_000);
    assert_eq!(state["transfers"][0]["incoming"], false);

    // The same name again is kept beside the first, not on top of it.
    assert_eq!(send(&token, &key_b, "report%20final.pdf", b"second").0, 200);
    assert_eq!(std::fs::read(received.join("report final (2).pdf")).unwrap(), b"second");
    assert_eq!(std::fs::read(&saved).unwrap(), big);

    // Empty files and names that try to climb out of the folder.
    assert_eq!(send(&token, &key_b, "empty.txt", b"").0, 200);
    assert_eq!(std::fs::read(received.join("empty.txt")).unwrap(), b"");
    assert_eq!(send(&token, &key_b, "..%2F..%2Fevil.exe", b"x").0, 200);
    assert_eq!(std::fs::read(received.join("received file")).unwrap(), b"x");
    assert!(!dir_b.join("evil.exe").exists() && !dir_b.parent().unwrap().join("evil.exe").exists());

    // Not without the token, and not to a computer that is not there.
    assert_eq!(send("wrong", &key_b, "a.txt", b"x").0, 403);
    assert_eq!(send(&token, &"00".repeat(32), "a.txt", b"x").0, 400);
    assert_eq!(send(&token, "zz", "a.txt", b"x").0, 400);
    assert!(!received.join("a.txt").exists());

    // The laptop knows what arrived and can show it; nothing half-done is left.
    let arrived = b.node.transfers();
    assert_eq!(arrived.len(), 4);
    assert!(arrived.iter().all(|t| t.incoming && t.state == TransferState::Done && t.can_reveal && t.peer == "desk"));
    let first = arrived.last().unwrap();
    assert!(b.node.reveal_transfer(first.id));
    assert_eq!(b.screen.events().last(), Some(&Event::Reveal(saved.clone())));
    assert!(!b.node.reveal_transfer(9999));
    assert!(!a.node.reveal_transfer(a.node.transfers()[0].id), "a sent file has nothing to show here");
    let leftovers: Vec<_> = std::fs::read_dir(&received)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".incoming-"))
        .collect();
    assert!(leftovers.is_empty());

    // Whole folders can be sent by the engine as well.
    a.node.send_files(&a.node.peers()[0].key_bytes().unwrap(), vec![src.join("album")]).unwrap();
    wait_for("the folder to arrive", || b.node.transfers()[0].state == TransferState::Done && b.node.transfers().len() == 5);
    assert_eq!(std::fs::read_to_string(received.join("album/inner/deep.txt")).unwrap(), "deep");
    assert!(received.join("album/empty.txt").exists());

    // The window's other transfer calls.
    let id = a.node.transfers()[0].id;
    assert_eq!(post(port, &token, "/api/transfer/cancel", &format!(r#"{{"id":{id}}}"#)).0, 200);
    assert_eq!(post(port, &token, "/api/transfer/reveal", &format!(r#"{{"id":{id}}}"#)).0, 400);
    assert_eq!(a.node.transfers()[0].state, TransferState::Done, "a finished transfer cannot be cancelled");

    // A connection that drops mid-file leaves nothing behind on the laptop.
    let half = {
        use std::io::Write;
        let mut stream = std::net::TcpStream::connect((LOCALHOST, port)).unwrap();
        let head = format!(
            "POST /api/send HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Portal-Token: {token}\r\nX-Portal-Peer: {key_b}\r\nX-Portal-File: half.bin\r\nContent-Length: 1000000\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&big[..400_000]).unwrap();
        stream
    };
    wait_for("the laptop to start receiving", || b.node.transfers()[0].label == "half.bin" && b.node.transfers()[0].done > 0);
    drop(half);
    wait_for("the cut-off file to be given up", || b.node.transfers()[0].state != TransferState::Active);
    assert_eq!(a.node.transfers()[0].state, TransferState::Failed);
    assert!(!received.join("half.bin").exists());
    wait_for("its temporary folder to be removed", || {
        !std::fs::read_dir(&received).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with(".incoming-"))
    });

    a.node.shutdown();
    b.node.shutdown();
    for dir in [dir_a, dir_b, src] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn sounds(screen: &MockPlatform) -> Vec<(u32, u32, Vec<i16>)> {
    screen
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Audio { seq, rate, stereo } => Some((seq, rate, stereo)),
            _ => None,
        })
        .collect()
}

fn captures(screen: &MockPlatform) -> Vec<bool> {
    screen.events().into_iter().filter_map(|e| if let Event::Capture(on) = e { Some(on) } else { None }).collect()
}

#[test]
fn sound_comes_out_of_the_chosen_computer_only() {
    use screen_portals::node::SoundState;
    let (a, b, dir_a, dir_b) = pair("snd");
    wait_for("fast path", || {
        a.node.connected().iter().all(|l| l.fast_pointer) && b.node.connected().iter().all(|l| l.fast_pointer)
    });
    let key_b = a.node.peers()[0].key_bytes().unwrap();
    let key_a = b.node.peers()[0].key_bytes().unwrap();
    let block: Vec<i16> = (0..600).flat_map(|i| [i as i16 + 1, -(i as i16) - 1]).collect();

    // Until a computer is chosen, everyone keeps their own sound.
    assert_eq!(a.node.status().sound, SoundState::Own);
    a.node.on_audio_captured(44_100, &block);
    std::thread::sleep(Duration::from_millis(100));
    assert!(sounds(&b.screen).is_empty() && captures(&a.screen).is_empty());
    assert!(a.node.set_speakers(Some([9u8; 32])).is_err(), "only this or a paired computer can be chosen");

    // Choose the laptop, on the desk. The desk goes quiet and starts
    // capturing; the laptop hears about the choice and changes nothing.
    a.node.set_speakers(Some(key_b)).unwrap();
    assert_eq!(captures(&a.screen), vec![true]);
    assert_eq!(a.node.status().sound, SoundState::Sending);
    wait_for("the laptop to agree", || b.node.speakers() == Some(key_b));
    assert_eq!(b.node.status().sound, SoundState::Own);
    assert!(captures(&b.screen).is_empty());

    // The desk's sound arrives on the laptop, in order and unchanged.
    a.node.on_audio_captured(44_100, &block);
    wait_for("the sound to arrive", || sounds(&b.screen).len() == 3);
    let arrived = sounds(&b.screen);
    assert_eq!(arrived.iter().map(|s| s.0).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(arrived.iter().all(|s| s.1 == 44_100));
    assert_eq!(arrived.into_iter().flat_map(|s| s.2).collect::<Vec<_>>(), block);

    // Silence is not sent, and the laptop's own sound goes nowhere.
    a.node.on_audio_captured(44_100, &vec![0i16; 88_200]);
    b.node.on_audio_captured(48_000, &block);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(sounds(&b.screen).len(), 3);
    assert!(sounds(&a.screen).is_empty());

    // Swap from the other computer: the desk gets its sound back and plays
    // the laptop's as well.
    b.node.set_speakers(Some(key_a)).unwrap();
    assert_eq!(captures(&b.screen), vec![true]);
    wait_for("the desk to stop sending", || captures(&a.screen) == vec![true, false]);
    assert_eq!(a.node.status().sound, SoundState::Own);
    assert_eq!(a.node.status().speakers, Some(a.node.status().id));
    b.node.on_audio_captured(48_000, &block);
    wait_for("the laptop's sound on the desk", || sounds(&a.screen).len() == 3);
    a.node.on_audio_captured(44_100, &block);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(sounds(&b.screen).len(), 3, "the computer that plays never sends");

    // Back to the laptop, then the laptop goes away: the desk plays its own
    // sound again rather than staying silent.
    a.node.set_speakers(Some(key_b)).unwrap();
    wait_for("the laptop to stop sending", || captures(&b.screen) == vec![true, false]);
    assert_eq!(captures(&a.screen), vec![true, false, true]);
    b.node.shutdown();
    wait_for("the desk to get its sound back", || captures(&a.screen) == vec![true, false, true, false]);
    assert_eq!(a.node.status().sound, SoundState::Waiting);
    a.node.shutdown();
    wait_for("links to close", || a.node.connected().is_empty());

    // The choice is remembered: after a restart it takes effect as soon as
    // the two are connected again, without anyone choosing anything.
    let a = computer("desk", &dir_a, (1000, 500), None);
    let b = computer("laptop", &dir_b, (2000, 1000), None);
    assert_eq!(a.node.speakers(), Some(key_b));
    assert!(captures(&a.screen).is_empty(), "nothing is silenced while the laptop is unreachable");
    a.node.dial(b.addr()).unwrap();
    wait_for("the desk to send its sound again", || captures(&a.screen) == vec![true]);

    // "Each computer" switches it all off, and quitting never leaves a
    // computer silenced.
    b.node.set_speakers(None).unwrap();
    wait_for("the desk to stop", || captures(&a.screen) == vec![true, false]);
    a.node.set_speakers(Some(key_b)).unwrap();
    assert_eq!(captures(&a.screen), vec![true, false, true]);
    a.node.shutdown();
    assert_eq!(captures(&a.screen).last(), Some(&false));

    b.node.shutdown();
    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn drag_tokens(screen: &MockPlatform) -> Vec<u64> {
    screen.events().into_iter().filter_map(|e| if let Event::DragCatch(token) = e { Some(token) } else { None }).collect()
}

#[test]
fn a_drag_crosses_with_the_cursor_and_comes_back() {
    use screen_portals::node::TransferState;
    use screen_portals::platform::{Caught, DragPayload};
    let (a, b, dir_a, dir_b) = pair("drag");
    let (src, big) = sample_files("dragsrc");

    // Holding the button at the doorway makes the desk look for a drag,
    // once, and the cursor waits for the answer.
    assert!(!a.node.on_button(1, true));
    assert!(!a.node.on_mouse_move(1003, 250));
    assert!(!a.node.on_mouse_move(1004, 250));
    assert_eq!(drag_tokens(&a.screen).len(), 1);
    assert!(!a.node.is_remote());

    // Nothing was being dragged (say, text was being selected): stay put.
    a.node.on_drag_caught(drag_tokens(&a.screen)[0], Caught::Nothing);
    assert!(!a.node.on_mouse_move(1005, 250));
    assert!(!a.node.is_remote() && drag_tokens(&a.screen).len() == 1);
    assert!(!a.node.on_button(1, false));

    // A new press, this time dragging some text: it crosses with the cursor
    // and the laptop starts the same drag where the cursor arrives.
    assert!(!a.node.on_button(1, true));
    assert!(!a.node.on_mouse_move(1003, 250));
    let token = *drag_tokens(&a.screen).last().unwrap();
    a.node.on_drag_caught(token, Caught::Data(DragPayload::Text("carried".into())));
    assert!(a.node.is_remote());
    assert!(a.screen.events().contains(&Event::Warp(500, 250)));
    wait_event(&b.screen, "the drag to start on the laptop", |e| *e == Event::DragBegin(DragPayload::Text("carried".into())));
    assert!(!b.screen.events().contains(&Event::Button(1, true)), "the laptop's platform presses the button itself");
    // Letting go on the desk's mouse drops it on the laptop.
    assert!(a.node.on_button(1, false));
    wait_event(&b.screen, "the release", |e| *e == Event::Button(1, false));
    a.node.come_home();

    // An answer that arrives after the button was released is not lost,
    // and does not move the cursor.
    assert!(!a.node.on_button(1, true));
    assert!(!a.node.on_mouse_move(1003, 250));
    let token = *drag_tokens(&a.screen).last().unwrap();
    assert!(!a.node.on_button(1, false));
    a.node.on_drag_caught(token, Caught::Data(DragPayload::Text("too late".into())));
    assert!(!a.node.is_remote());

    // Dragged files travel first; the drag starts on the laptop once they
    // are really there.
    b.screen.clear();
    assert!(!a.node.on_button(1, true));
    assert!(!a.node.on_mouse_move(1003, 250));
    let token = *drag_tokens(&a.screen).last().unwrap();
    a.node.on_drag_caught(token, Caught::Data(DragPayload::Files(vec![src.join("big.bin"), src.join("album")])));
    assert!(a.node.is_remote());
    let Event::DragBegin(DragPayload::Files(paths)) = wait_event(&b.screen, "the files to arrive", |e| matches!(e, Event::DragBegin(_))) else {
        unreachable!()
    };
    assert_eq!(std::fs::read(&paths[0]).unwrap(), big);
    assert_eq!(std::fs::read_to_string(paths[1].join("inner/deep.txt")).unwrap(), "deep");
    assert!(a.node.on_button(1, false));
    wait_event(&b.screen, "the drop", |e| *e == Event::Button(1, false));
    wait_for("the desk to hear they arrived", || a.node.transfers()[0].state == TransferState::Done);
    a.node.come_home();

    // The other direction. The desk's cursor is on the laptop, presses the
    // button there and comes home through the doorway still holding it.
    assert!(a.node.on_mouse_move(1003, 250));
    assert!(a.node.on_button(1, true));
    wait_event(&b.screen, "the press", |e| *e == Event::Button(1, true));
    b.screen.clear();
    a.screen.clear();
    assert!(a.node.on_mouse_move(500 - 3000, 250));
    assert!(!a.node.is_remote());
    // The laptop is asked to catch whatever is being dragged there, rather
    // than being told to drop it at the edge.
    wait_for("the laptop to look for a drag", || !drag_tokens(&b.screen).is_empty());
    assert!(!b.screen.events().contains(&Event::Button(1, false)));
    b.node.on_drag_caught(drag_tokens(&b.screen)[0], Caught::Data(DragPayload::Url("https://example.com/page".into())));
    wait_event(&a.screen, "the drag to continue on the desk", |e| *e == Event::DragBegin(DragPayload::Url("https://example.com/page".into())));
    // Home again and still holding: the edge is not crossed by accident.
    assert!(!a.node.on_button(1, false), "the desk's own release ends the drag there");

    // The same return with nothing being dragged just lets the button go.
    assert!(a.node.on_mouse_move(1003, 250));
    assert!(a.node.on_button(1, true));
    wait_event(&b.screen, "the press", |e| *e == Event::Button(1, true));
    b.screen.clear();
    assert!(a.node.on_mouse_move(500 - 3000, 250));
    wait_for("the laptop to look again", || !drag_tokens(&b.screen).is_empty());
    b.node.on_drag_caught(drag_tokens(&b.screen)[0], Caught::Nothing);
    assert!(b.screen.events().contains(&Event::Button(1, false)));
    assert!(!a.node.on_button(1, false));

    // A browser window pushed against the doorway: its page opens on the
    // laptop, and the cursor stays on the desk.
    assert!(!a.node.on_button(1, true));
    assert!(!a.node.on_mouse_move(1003, 250));
    let token = *drag_tokens(&a.screen).last().unwrap();
    a.node.on_drag_caught(token, Caught::Page("https://example.org/article".into()));
    wait_event(&b.screen, "the page to open", |e| *e == Event::OpenUrl("https://example.org/article".into()));
    assert!(!a.node.is_remote());
    assert!(!a.node.on_button(1, false));

    // If the connection drops mid-drag, the drag is abandoned, not dropped.
    assert!(!a.node.on_button(1, true));
    assert!(!a.node.on_mouse_move(1003, 250));
    let token = *drag_tokens(&a.screen).last().unwrap();
    a.node.on_drag_caught(token, Caught::Data(DragPayload::Text("held".into())));
    wait_event(&b.screen, "the drag", |e| *e == Event::DragBegin(DragPayload::Text("held".into())));
    b.screen.clear();
    a.node.shutdown();
    wait_event(&b.screen, "the drag to be abandoned", |e| *e == Event::DragCancel);
    let events = b.screen.events();
    let cancel = events.iter().position(|e| *e == Event::DragCancel).unwrap();
    let release = events.iter().position(|e| *e == Event::Button(1, false)).expect("button released");
    assert!(cancel < release, "cancelled before the button is let go");

    b.node.shutdown();
    for dir in [dir_a, dir_b, src] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

//! The command-line program: pair, run, list and forget computers, and
//! check that this one is ready (`doctor`). The windowed application in
//! `app.rs` does the same job with a tray icon and a settings window.

use screen_portals::config::{default_name, fingerprint, Store};
use screen_portals::layout::Side;
use screen_portals::node::DEFAULT_TCP_PORT;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
Screen Portals - one mouse and keyboard across your computers

USAGE
  screen-portals pair --side <left|right|above|below>   pair with a new computer, then keep running
  screen-portals run                                    connect to paired computers
  screen-portals peers                                  list paired computers
  screen-portals forget <name>                          remove a pairing
  screen-portals doctor                                 check this computer is ready
  screen-portals doctor pointer                         also move the cursor briefly to test playback
  screen-portals doctor sound                           also test sound (silences this computer for a second)
  screen-portals doctor drag                            also test carrying a drag (clicks where the cursor is)

OPTIONS
  --side <where>        where the OTHER computer sits relative to this one (pair only;
                        needed on one of the two computers, the other mirrors it)
  --name <name>         name shown to other computers (default: this computer's name)
  --connect <host:port> connect to this address instead of relying on discovery (repeatable)
  --port <number>       TCP port to listen on (default 47811)
  --no-discovery        do not announce or search on the local network
  --config-dir <dir>    where the identity key and pairings are stored

WHILE RUNNING
  Push the mouse through the screen edge that faces the other computer.
  Ctrl+Alt+Home brings the cursor back to this computer at any time.
";

/// What the command line asked for.
struct Args {
    command: String,
    positional: Vec<String>,
    side: Option<Side>,
    name: Option<String>,
    connect: Vec<String>,
    port: u16,
    discovery: bool,
    config_dir: Option<PathBuf>,
}

/// Read the command line; the error is a sentence for the user.
fn parse_args() -> Result<Args, String> {
    let mut raw = std::env::args().skip(1);
    let command = raw.next().unwrap_or_else(|| "help".to_string());
    let mut args = Args {
        command,
        positional: Vec::new(),
        side: None,
        name: None,
        connect: Vec::new(),
        port: DEFAULT_TCP_PORT,
        discovery: true,
        config_dir: None,
    };
    while let Some(arg) = raw.next() {
        let mut value = |flag: &str| raw.next().ok_or_else(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "--side" => {
                let text = value("--side")?;
                args.side = Some(Side::parse(&text).ok_or_else(|| format!("unknown side \"{text}\"; use left, right, above or below"))?);
            }
            "--name" => args.name = Some(value("--name")?),
            "--connect" => args.connect.push(value("--connect")?),
            "--port" => {
                let text = value("--port")?;
                args.port = text.parse().map_err(|_| format!("\"{text}\" is not a port number"))?;
            }
            "--config-dir" => args.config_dir = Some(PathBuf::from(value("--config-dir")?)),
            "--no-discovery" => args.discovery = false,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ => args.positional.push(arg),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match args.command.as_str() {
        "run" | "pair" => run(args),
        "peers" => peers(args),
        "forget" => forget(args),
        "doctor" => doctor(args),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command \"{other}\"\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `peers`: list who this computer is paired with.
fn peers(args: Args) -> Result<(), String> {
    let store = Store::open(args.config_dir)?;
    let identity = store.identity()?;
    let config = store.load_config()?;
    println!("This computer: {} (key {})", args.name.unwrap_or_else(default_name), fingerprint(&identity.public));
    if config.peers.is_empty() {
        println!("No paired computers yet. Run `screen-portals pair --side <where>` on both.");
    }
    for peer in &config.peers {
        let key = peer.key_bytes().map(|k| fingerprint(&k)).unwrap_or_else(|| "invalid".into());
        let addr = peer.addr.as_deref().map(|a| format!(", fixed address {a}")).unwrap_or_default();
        println!("  {:<24} {} this computer (key {key}{addr})", peer.name, peer.side.phrase());
    }
    Ok(())
}

/// `forget <name>`: remove a pairing so that computer can no longer connect.
fn forget(args: Args) -> Result<(), String> {
    let name = args.positional.first().ok_or("say which computer to forget: screen-portals forget <name>")?;
    let store = Store::open(args.config_dir)?;
    let mut config = store.load_config()?;
    let before = config.peers.len();
    config.peers.retain(|p| !p.name.eq_ignore_ascii_case(name));
    if config.peers.len() == before {
        return Err(format!("no paired computer is called \"{name}\""));
    }
    store.save_config(&config)?;
    println!("Forgot \"{name}\". It can no longer connect to this computer.");
    Ok(())
}

/// `doctor`: check that this computer can share input, and optionally try the pointer, sound and dragging for real.
#[cfg(windows)]
fn doctor(args: Args) -> Result<(), String> {
    use screen_portals::platform::windows;
    windows::init();
    let store = Store::open(args.config_dir)?;
    let identity = store.identity()?;
    let config = store.load_config()?;
    let d = windows::diagnose();
    let mark = |ok: bool| if ok { "ok" } else { "FAILED" };
    println!("Settings folder   {}", store.dir().display());
    println!("Identity key      {}", fingerprint(&identity.public));
    println!("Paired computers  {}", config.peers.len());
    println!("Desktop           {}x{} at ({}, {})", d.desktop.w, d.desktop.h, d.desktop.x, d.desktop.y);
    println!("Main monitor      {}x{}", d.main_monitor.0, d.main_monitor.1);
    println!("Cursor            ({}, {})", d.cursor.0, d.cursor.1);
    println!("Mouse hook        {}", mark(d.mouse_hook));
    println!("Keyboard hook     {}", mark(d.keyboard_hook));
    println!("Clipboard access  {}", mark(d.clipboard_readable));
    let tcp = std::net::TcpListener::bind(("0.0.0.0", args.port)).is_ok();
    println!("TCP port {:<8} {}", args.port, if tcp { "free" } else { "in use (another free port will be chosen)" });
    if args.positional.iter().any(|p| p == "pointer") {
        let worst = windows::pointer_self_test();
        println!("Pointer playback  {} (largest error {worst} px)", mark(worst <= 2));
        if worst > 2 {
            return Err("the cursor did not land where it was sent; an elevated or secure window may be in front".into());
        }
    }
    if args.positional.iter().any(|p| p == "sound") {
        match screen_portals::platform::win_audio::self_test() {
            Ok(detail) => println!("Sound             ok ({detail})"),
            Err(problem) => {
                println!("Sound             FAILED");
                return Err(problem);
            }
        }
    }
    if args.positional.iter().any(|p| p == "drag") {
        // Reading only: nothing is opened, clicked or changed for this one.
        let (browsers, page) = screen_portals::platform::win_page::survey();
        match (browsers, page) {
            (0, _) => println!("Browser pages     no browser window is open to try"),
            (n, Some(_)) => println!("Browser pages     ok (read the page of one of {n} browser windows)"),
            (n, None) => println!("Browser pages     could not read a page from {n} browser windows"),
        }
        match screen_portals::platform::win_drag::self_test() {
            Ok(detail) => println!("Dragging          ok ({detail})"),
            Err(problem) => {
                println!("Dragging          FAILED");
                return Err(problem);
            }
        }
    }
    if d.mouse_hook && d.keyboard_hook {
        Ok(())
    } else {
        Err("Windows did not allow input hooks; Screen Portals cannot share input on this session".into())
    }
}

#[cfg(not(windows))]
fn doctor(_args: Args) -> Result<(), String> {
    Err("this build only supports Windows so far".into())
}

/// `run` and `pair`: start the engine and keep going until Ctrl+C.
#[cfg(windows)]
fn run(args: Args) -> Result<(), String> {
    use screen_portals::node::{Node, Options, PairingOptions};
    use screen_portals::pairing;
    use screen_portals::platform::windows::{self, WinPlatform};
    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;

    windows::init();
    let pairing_mode = args.command == "pair";
    if args.side.is_some() && !pairing_mode {
        return Err("--side only applies to `pair`".into());
    }
    let store = Store::open(args.config_dir)?;
    let pairing = pairing_mode.then(|| PairingOptions {
        side: args.side,
        active: true,
        confirm: Box::new(|name: &str, code: &str| {
            println!();
            println!("  Pairing with \"{name}\"");
            println!("  Code: {}", pairing::pretty(code));
            print!("  Does \"{name}\" show the same code? [y/N] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }),
    });

    let node = Node::start(
        Arc::new(WinPlatform),
        store,
        Options {
            name: args.name.unwrap_or_else(default_name),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tcp_port: args.port,
            discovery: args.discovery,
            pairing,
            connect: args.connect,
            received_dir: windows::downloads_dir().map(|d| d.join("Screen Portals")),
        },
    )?;

    println!("Screen Portals is running as \"{}\" (key {}).", node.name(), fingerprint(&node.public_key()));
    let peers = node.peers();
    if pairing_mode {
        println!("Pairing mode: run `screen-portals pair` on the other computer now.");
    } else if peers.is_empty() {
        println!("No paired computers yet. Stop this and run `screen-portals pair --side <where>` on both computers.");
    }
    for peer in &peers {
        println!("  waiting for \"{}\" ({} this computer)", peer.name, peer.side.phrase());
    }
    println!("Copied text is shared at once; copied files and pictures follow the cursor.");
    println!("Ctrl+Alt+Home brings the cursor back here. Press Ctrl+C in this window to quit.");
    windows::run(node)
}

#[cfg(not(windows))]
fn run(_args: Args) -> Result<(), String> {
    Err("this build only supports Windows so far; the network protocol is ready for other platforms".into())
}

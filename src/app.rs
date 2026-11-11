//! The windowed application: lives in the notification area, shares input in
//! the background and opens the settings window on request.
#![windows_subsystem = "windows"]
#![allow(non_snake_case)]

#[cfg(windows)]
fn run() -> Result<(), String> {
    use screen_portals::config::{default_name, Store};
    use screen_portals::node::{Node, Options, PairingOptions, DEFAULT_TCP_PORT};
    use screen_portals::platform::windows::{self, TrayOptions, WinPlatform};
    use screen_portals::ui::{self, PairPrompt};
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use screen_portals::update;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// How often to ask whether a newer build has been published.
    const CHECK_EVERY: Duration = Duration::from_secs(10 * 60);

    /// Runs for the life of the program: look for a newer build now and
    /// then, and when one exists and nothing would be interrupted, swap it
    /// in and restart.
    fn keep_updated(node: Arc<Node>, exe: PathBuf, config_dir: Option<PathBuf>, build: u64, source: String) {
        let mut next = Instant::now() + Duration::from_secs(30);
        let mut ready: Option<Vec<u8>> = None;
        loop {
            std::thread::sleep(Duration::from_secs(15));
            if ready.is_none() && (node.take_update_hint() || Instant::now() >= next) {
                next = Instant::now() + CHECK_EVERY;
                match update::fetch_newer(build, &source, &windows::fetch_https) {
                    Ok(Some((_, program))) => ready = Some(program),
                    Ok(None) => {}
                    // Offline, or nothing published yet: try again later.
                    Err(_) => {}
                }
            }
            // Wait for a moment when a restart interrupts nothing.
            let Some(program) = ready.as_ref().filter(|_| node.is_quiet()) else { continue };
            if update::swap_in(&exe, program).is_err() {
                // Most likely the folder is not writable. Do not keep trying
                // every few seconds; the next regular check will.
                ready = None;
                continue;
            }
            if restart(&exe, config_dir.as_deref()) {
                windows::request_quit();
                return;
            }
            ready = None;
        }
    }

    /// Start the freshly installed program; it waits for this one to close.
    fn restart(exe: &Path, config_dir: Option<&Path>) -> bool {
        let mut command = std::process::Command::new(exe);
        command.args(["--hidden", "--after-update"]);
        if let Some(dir) = config_dir {
            command.arg("--config-dir").arg(dir);
        }
        command.spawn().is_ok()
    }

    let mut config_dir: Option<PathBuf> = None;
    let mut hidden = false;
    let (mut after_update, mut updates) = (false, true);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config-dir" => config_dir = Some(PathBuf::from(args.next().ok_or("--config-dir needs a folder")?)),
            // For starting with Windows: stay in the tray until asked.
            "--hidden" => hidden = true,
            // Started by the previous version, which is still closing.
            "--after-update" => after_update = true,
            "--no-update" => updates = false,
            other => return Err(format!("Unknown option {other}")),
        }
    }

    windows::init();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    update::clean_up(&exe);
    if after_update {
        windows::wait_for_previous_copy(Duration::from_secs(15));
    }
    let store = Store::open(config_dir.clone())?;
    // Where a second launch finds the window of the copy already running.
    let window_file = store.dir().join("window.txt");
    if windows::already_running() {
        if let Ok(url) = std::fs::read_to_string(&window_file) {
            windows::open_app_window(url.trim());
        }
        return Ok(());
    }

    let prompt = PairPrompt::new();
    let node = Node::start(
        Arc::new(WinPlatform),
        store,
        Options {
            name: default_name(),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tcp_port: DEFAULT_TCP_PORT,
            discovery: true,
            pairing: Some(PairingOptions { side: None, confirm: prompt.confirm_fn(), active: false }),
            connect: Vec::new(),
            received_dir: windows::downloads_dir().map(|d| d.join("Screen Portals")),
        },
    )?;
    let server = ui::serve(node.clone(), prompt.clone(), Box::new(windows::request_quit))?;
    let url = server.url();
    let _ = std::fs::write(&window_file, &url);
    if !hidden {
        windows::open_app_window(&url);
    }

    if updates {
        if let (Some(build), Some(source)) = (update::this_build(), update::release_url()) {
            let node = node.clone();
            std::thread::spawn(move || keep_updated(node, exe, config_dir, build, source));
        }
    }

    let result = windows::run_with_tray(
        node,
        TrayOptions {
            open_window: Box::new(move || windows::open_app_window(&url)),
            needs_attention: Box::new(move || prompt.current().is_some()),
        },
    );
    let _ = std::fs::remove_file(&window_file);
    result
}

#[cfg(windows)]
fn main() {
    if let Err(e) = run() {
        screen_portals::platform::windows::error_box(&e);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The windowed application only supports Windows so far.");
}

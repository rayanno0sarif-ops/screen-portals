//! Linux implementation, for desktops that run on an X server (X11).
//!
//! How it differs from Windows: X has no way to hide single input events
//! from other programs. So while the cursor is at home this program only
//! *watches* (XInput2 "raw" events), and while it is on another computer it
//! *grabs* the mouse and keyboard, which keeps every other program from
//! seeing them. Input from another computer is played back with the XTEST
//! extension.
//!
//! Wayland sessions do not allow any of this from an ordinary program, so
//! they are refused with an explanation (see `session_problem`).

use super::x11_clip::{self, Atoms, Clip};
use super::{Caught, DragPayload, Platform};
use crate::keymap;
use crate::layout::Rect;
use crate::node::{Node, TransferState};
use crate::proto::{TransferKind, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, BUTTON_X1, BUTTON_X2};
use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _, XIEventMask};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor, EventMask, GrabMode,
    GrabStatus, Rectangle, Window, WindowClass, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

/// How far the hidden cursor may wander from its resting place, in pixels,
/// before it is put back. Far from every screen edge, so movement is never
/// cut short.
const WANDER: i32 = 80;

static X: OnceLock<Arc<X>> = OnceLock::new();
static NODE: OnceLock<Arc<Node>> = OnceLock::new();
static QUIT: AtomicBool = AtomicBool::new(false);
static TRAY: OnceLock<TrayOptions> = OnceLock::new();

/// The connection to the X server and what was set up on it.
pub(super) struct X {
    pub conn: RustConnection,
    pub root: Window,
    /// A window of ours that is never shown: it owns the clipboard and
    /// receives messages meant to wake the event loop.
    pub window: Window,
    pub atoms: Atoms,
    pub clip: Mutex<Clip>,
    screen: Mutex<Rect>,
    park: Mutex<(i32, i32)>,
    /// True while the mouse and keyboard are grabbed for another computer.
    held: AtomicBool,
    /// Input devices that only ever carry played-back input (XTEST).
    playback_devices: Vec<u16>,
    /// An invisible cursor, shown while the real one is on another computer.
    blank: Cursor,
    /// Wheel movement not yet large enough for one notch.
    wheel: Mutex<(i32, i32)>,
    /// Keys currently pressed on behalf of another computer.
    keys_down: Mutex<HashSet<u8>>,
}

/// Why this desktop session cannot be used, if it cannot.
pub fn session_problem() -> Option<String> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if set("SCREEN_PORTALS_FORCE_X11") {
        return None;
    }
    let wayland = std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind.eq_ignore_ascii_case("wayland")) || set("WAYLAND_DISPLAY");
    if wayland {
        return Some(
            "this desktop session uses Wayland, which does not let a program share the mouse and keyboard this way. \
             Sign out and pick an \"Xorg\" or \"X11\" session on the sign-in screen (often behind a gear icon), then start Screen Portals again"
                .into(),
        );
    }
    if !set("DISPLAY") {
        return Some("no desktop session was found (DISPLAY is not set); start Screen Portals from inside your desktop".into());
    }
    None
}

fn x_error(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

fn connect() -> Result<X, String> {
    if let Some(problem) = session_problem() {
        return Err(problem);
    }
    let (conn, screen_index) = x11rb::connect(None).map_err(|e| x_error("cannot reach the display", e))?;
    let screen = conn.setup().roots.get(screen_index).ok_or("the display has no screen")?.clone();
    let root = screen.root;

    // The three extensions everything rests on.
    let missing = |name: &str| format!("the display lacks the {name} extension, which Screen Portals needs");
    conn.xinput_xi_query_version(2, 2).ok().and_then(|c| c.reply().ok()).ok_or_else(|| missing("XInput 2"))?;
    conn.xtest_get_version(2, 2).ok().and_then(|c| c.reply().ok()).ok_or_else(|| missing("XTEST"))?;
    conn.xfixes_query_version(5, 0).ok().and_then(|c| c.reply().ok()).ok_or_else(|| missing("XFIXES"))?;

    let failed = |e: x11rb::errors::ReplyOrIdError| x_error("the display refused a request", e);
    let window = conn.generate_id().map_err(failed)?;
    conn.create_window(
        0,
        window,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| x_error("cannot create a window", e))?;

    // A cursor with nothing in it: one pixel, fully masked out.
    let pixmap = conn.generate_id().map_err(failed)?;
    let gc = conn.generate_id().map_err(failed)?;
    let blank = conn.generate_id().map_err(failed)?;
    let io = |e: x11rb::errors::ConnectionError| x_error("lost the display", e);
    conn.create_pixmap(1, pixmap, root, 1, 1).map_err(io)?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0)).map_err(io)?;
    conn.poly_fill_rectangle(pixmap, gc, &[Rectangle { x: 0, y: 0, width: 1, height: 1 }]).map_err(io)?;
    conn.create_cursor(blank, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0).map_err(io)?;
    conn.free_gc(gc).map_err(io)?;
    conn.free_pixmap(pixmap).map_err(io)?;

    // Watch all mice and keyboards, and hear about screen size changes.
    let watch = XIEventMask::RAW_MOTION
        | XIEventMask::RAW_BUTTON_PRESS
        | XIEventMask::RAW_BUTTON_RELEASE
        | XIEventMask::RAW_KEY_PRESS
        | XIEventMask::RAW_KEY_RELEASE;
    let all_master_devices = 1u16;
    conn.xinput_xi_select_events(root, &[xinput::EventMask { deviceid: all_master_devices, mask: vec![watch] }])
        .map_err(io)?
        .check()
        .map_err(|e| x_error("the display will not report mouse and keyboard activity", e))?;
    conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY)).map_err(io)?;

    let all_devices = 0u16;
    let playback_devices = conn
        .xinput_xi_query_device(all_devices)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|reply| {
            reply
                .infos
                .iter()
                .filter(|info| String::from_utf8_lossy(&info.name).contains("XTEST"))
                .map(|info| info.deviceid)
                .collect::<Vec<u16>>()
        })
        .unwrap_or_default();

    let atoms = Atoms::new(&conn).map_err(io)?.reply().map_err(|e| x_error("the display refused a request", e))?;
    let x = X {
        conn,
        root,
        window,
        atoms,
        clip: Mutex::new(Clip::default()),
        screen: Mutex::new(Rect { x: 0, y: 0, w: screen.width_in_pixels as i32, h: screen.height_in_pixels as i32 }),
        park: Mutex::new((screen.width_in_pixels as i32 / 2, screen.height_in_pixels as i32 / 2)),
        held: AtomicBool::new(false),
        playback_devices,
        blank,
        wheel: Mutex::new((0, 0)),
        keys_down: Mutex::new(HashSet::new()),
    };
    x11_clip::watch(&x)?;
    x.measure();
    x.conn.flush().map_err(io)?;
    Ok(x)
}

impl X {
    /// Read the desktop size and pick the cursor's resting place: the middle
    /// of the main monitor, which is always a real, visible pixel.
    fn measure(&self) {
        if let Some(geometry) = self.conn.get_geometry(self.root).ok().and_then(|c| c.reply().ok()) {
            *self.screen.lock().unwrap() = Rect { x: 0, y: 0, w: geometry.width as i32, h: geometry.height as i32 };
        }
        let screen = *self.screen.lock().unwrap();
        let mut park = (screen.w / 2, screen.h / 2);
        if let Some(reply) = self.conn.randr_get_monitors(self.root, true).ok().and_then(|c| c.reply().ok()) {
            let main = reply.monitors.iter().find(|m| m.primary).or(reply.monitors.first());
            if let Some(m) = main.filter(|m| m.width > 0 && m.height > 0) {
                park = (m.x as i32 + m.width as i32 / 2, m.y as i32 + m.height as i32 / 2);
            }
        }
        *self.park.lock().unwrap() = park;
    }

    fn pointer(&self) -> Option<(i32, i32)> {
        let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((reply.root_x as i32, reply.root_y as i32))
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let _ = self.conn.xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0);
        let _ = self.conn.flush();
    }

    fn click(&self, button: u8) {
        self.fake(BUTTON_PRESS_EVENT, button, 0, 0);
        self.fake(BUTTON_RELEASE_EVENT, button, 0, 0);
    }

    /// Make the event loop look around even though nothing happened on the
    /// display. Safe from any thread.
    pub(super) fn wake(&self) {
        let message = ClientMessageEvent::new(32, self.window, self.atoms.SCREEN_PORTALS_WAKE, [0u32; 5]);
        let _ = self.conn.send_event(false, self.window, EventMask::NO_EVENT, message);
        let _ = self.conn.flush();
    }

    /// Take the mouse and keyboard away from every other program. False if
    /// something else (an open menu, a screen locker) has them already.
    fn grab(&self) -> bool {
        let watch = EventMask::POINTER_MOTION | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE;
        for _ in 0..5 {
            let pointer = self
                .conn
                .grab_pointer(false, self.root, watch, GrabMode::ASYNC, GrabMode::ASYNC, NONE, self.blank, CURRENT_TIME)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.status == GrabStatus::SUCCESS);
            let keyboard = pointer
                && self
                    .conn
                    .grab_keyboard(false, self.root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|r| r.status == GrabStatus::SUCCESS);
            if keyboard {
                return true;
            }
            self.ungrab();
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn ungrab(&self) {
        let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let _ = self.conn.flush();
    }
}

/// The real thing: this computer's mouse, keyboard, clipboard, sound and screen.
pub struct LinuxPlatform {
    x: Arc<X>,
}

/// Connect to the desktop session. Fails with a sentence for the user when
/// the session cannot be used (Wayland, or no display at all).
pub fn platform() -> Result<LinuxPlatform, String> {
    if let Some(x) = X.get() {
        return Ok(LinuxPlatform { x: x.clone() });
    }
    let x = Arc::new(connect()?);
    Ok(LinuxPlatform { x: X.get_or_init(|| x).clone() })
}

/// The running engine, for the parts of the platform that feed it.
pub(super) fn node() -> Option<&'static Arc<Node>> {
    NODE.get()
}

/// The engine's button numbers as X numbers them (2 is the middle button).
fn to_x_button(button: u8) -> Option<u8> {
    match button {
        BUTTON_LEFT => Some(1),
        BUTTON_MIDDLE => Some(2),
        BUTTON_RIGHT => Some(3),
        BUTTON_X1 => Some(8),
        BUTTON_X2 => Some(9),
        _ => None,
    }
}

fn from_x_button(button: u32) -> Option<u8> {
    match button {
        1 => Some(BUTTON_LEFT),
        2 => Some(BUTTON_MIDDLE),
        3 => Some(BUTTON_RIGHT),
        8 => Some(BUTTON_X1),
        9 => Some(BUTTON_X2),
        _ => None,
    }
}

/// X reports the wheel as clicks of buttons 4 to 7; one click is one notch
/// (120 units, as on Windows). Returns (sideways, up).
fn wheel_of(button: u32) -> Option<(i16, i16)> {
    match button {
        4 => Some((0, 120)),
        5 => Some((0, -120)),
        6 => Some((-120, 0)),
        7 => Some((120, 0)),
        _ => None,
    }
}

impl Platform for LinuxPlatform {
    fn screen(&self) -> Rect {
        *self.x.screen.lock().unwrap()
    }

    fn park_point(&self) -> (i32, i32) {
        *self.x.park.lock().unwrap()
    }

    fn cursor(&self) -> (i32, i32) {
        // While the cursor is on another computer the engine measures
        // movement from the resting place; the event loop reports movement
        // relative to it (see `Pump::held_motion`).
        if self.x.held.load(Ordering::Relaxed) {
            return self.park_point();
        }
        self.x.pointer().unwrap_or_else(|| self.park_point())
    }

    fn warp(&self, x: i32, y: i32) {
        let _ = self.x.conn.warp_pointer(NONE, self.x.root, 0, 0, 0, 0, x as i16, y as i16);
        let _ = self.x.conn.flush();
    }

    fn inject_abs(&self, x: u16, y: u16) {
        let screen = self.screen();
        let place = |n: u16, len: i32| ((n as f64 / 65535.0) * (len - 1).max(0) as f64).round() as i32;
        self.x.fake(MOTION_NOTIFY_EVENT, 0, (screen.x + place(x, screen.w)) as i16, (screen.y + place(y, screen.h)) as i16);
    }

    fn inject_button(&self, button: u8, down: bool) {
        if let Some(button) = to_x_button(button) {
            self.x.fake(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, button, 0, 0);
        }
    }

    fn inject_wheel(&self, dx: i16, dy: i16) {
        // Wheels with fine steps send fractions of a notch; add them up.
        let (mut across, mut up) = {
            let mut rest = self.x.wheel.lock().unwrap();
            rest.0 += dx as i32;
            rest.1 += dy as i32;
            let whole = (rest.0 / 120, rest.1 / 120);
            rest.0 -= whole.0 * 120;
            rest.1 -= whole.1 * 120;
            whole
        };
        while up != 0 {
            self.x.click(if up > 0 { 4 } else { 5 });
            up -= up.signum();
        }
        while across != 0 {
            self.x.click(if across > 0 { 7 } else { 6 });
            across -= across.signum();
        }
    }

    fn inject_key(&self, hid: u16, down: bool) {
        let Some(code) = keymap::to_linux(hid).and_then(|code| u8::try_from(code + 8).ok()) else { return };
        // A held key repeats by itself here, so repeats sent by the other
        // computer are not needed (the X server would ignore them anyway).
        let changed = {
            let mut keys = self.x.keys_down.lock().unwrap();
            if down { keys.insert(code) } else { keys.remove(&code) }
        };
        if changed {
            self.x.fake(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, code, 0, 0);
        }
    }

    fn set_clipboard_text(&self, text: &str) {
        x11_clip::offer(&self.x, x11_clip::Offer::Text(text.to_string()));
    }

    fn set_clipboard_files(&self, paths: &[PathBuf]) {
        x11_clip::offer(&self.x, x11_clip::Offer::Files(paths.to_vec()));
    }

    fn set_clipboard_image(&self, png: &[u8]) {
        x11_clip::offer(&self.x, x11_clip::Offer::Image(png.to_vec()));
    }

    fn clipboard_image(&self) -> Option<Vec<u8>> {
        x11_clip::read_image(&self.x)
    }

    fn reveal(&self, path: &Path) {
        let path = path.to_path_buf();
        std::thread::spawn(move || reveal(&path));
    }

    fn capture_audio(&self, on: bool) {
        super::linux_audio::capture(on);
    }

    fn play_audio(&self, source: u64, seq: u32, rate: u32, stereo: &[i16]) {
        super::linux_audio::play(source, seq, rate, stereo);
    }

    fn drag_catch(&self, token: u64) {
        // Taking over another program's drag is not built for Linux yet,
        // so the honest answer is always "nothing". It must come from
        // another thread: the engine is in the middle of something here.
        std::thread::spawn(move || {
            if let Some(node) = NODE.get() {
                node.on_drag_caught(token, Caught::Nothing);
            }
        });
    }

    fn drag_begin(&self, payload: DragPayload) {
        // A drag cannot be continued here yet. Keep what arrived ready to
        // paste instead, so it is not lost.
        match payload {
            DragPayload::Text(text) | DragPayload::Url(text) => self.set_clipboard_text(&text),
            DragPayload::Files(paths) => self.set_clipboard_files(&paths),
        }
    }

    fn open_url(&self, url: &str) {
        // Only ever called with an address that passed `transfer::safe_url`.
        quiet(Command::new("xdg-open").arg(url));
    }
}

/// Start a helper program without waiting for it or showing its output.
fn quiet(command: &mut Command) -> bool {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
}

/// Show a file in the file manager, selected if the file manager can do
/// that, otherwise just open its folder.
fn reveal(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let uri = crate::uri::file_uri(path.as_os_str().as_bytes());
    let selected = Command::new("dbus-send")
        .args(["--session", "--print-reply", "--dest=org.freedesktop.FileManager1", "--type=method_call"])
        .args(["/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems"])
        .arg(format!("array:string:{uri}"))
        .arg("string:")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !selected {
        let folder = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        quiet(Command::new("xdg-open").arg(folder));
    }
}

extern "C" fn on_signal(_signal: libc::c_int) {
    // Only this much is safe inside a signal handler; the event loop
    // notices within a moment and shuts down properly.
    QUIT.store(true, Ordering::Relaxed);
}

/// Call once at the very start of the program.
pub fn init() {
    super::linux_audio::recover();
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // Ctrl+C, an ordinary request to end, and the terminal closing.
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(signal, handler);
        }
    }
}

/// What the event loop remembers between events.
#[derive(Default)]
struct Pump {
    /// Where the hidden cursor was last seen while grabbed.
    last: (i32, i32),
    /// Set after asking for the hidden cursor to be put back: the number of
    /// that request. Events the display sent before carrying it out still
    /// continue from the old position.
    warp: Option<u64>,
    /// A key release that may turn out to be half of an automatic repeat.
    release: Option<(u8, u32)>,
    /// The mouse moved since the cursor was last looked at.
    moved: bool,
}

impl Pump {
    fn injected(x: &X, source: u16) -> bool {
        x.playback_devices.contains(&source) && std::env::var_os("SCREEN_PORTALS_ACCEPT_XTEST").is_none()
    }

    fn flush_release(&mut self, node: &Arc<Node>) {
        if let Some((code, _)) = self.release.take() {
            key(node, code as u32, false);
        }
    }

    /// The grabbed (hidden) cursor moved.
    fn held_motion(&mut self, x: &X, node: &Arc<Node>, at: (i32, i32), sequence: u64) {
        if self.warp.is_some_and(|warp| sequence >= warp) {
            // The first report after the cursor was put back: movement is
            // counted from here on.
            self.warp = None;
            self.last = at;
            return;
        }
        let (dx, dy) = (at.0 - self.last.0, at.1 - self.last.1);
        self.last = at;
        let park = *x.park.lock().unwrap();
        if dx != 0 || dy != 0 {
            node.on_mouse_move(park.0 + dx, park.1 + dy);
        }
        let strayed = (at.0 - park.0).abs() > WANDER || (at.1 - park.1).abs() > WANDER;
        if strayed && self.warp.is_none() && node.is_remote() {
            if let Ok(cookie) = x.conn.warp_pointer(NONE, x.root, 0, 0, 0, 0, park.0 as i16, park.1 as i16) {
                self.warp = Some(cookie.sequence_number());
            }
            let _ = x.conn.flush();
        }
    }

    fn handle(&mut self, x: &X, node: &Arc<Node>, event: Event, sequence: u64) {
        let held = x.held.load(Ordering::Relaxed);
        // Anything but the matching press ends the wait for a repeat.
        if !matches!(event, Event::KeyPress(_)) {
            self.flush_release(node);
        }
        match event {
            // ----- watching, while the cursor is at home -----
            Event::XinputRawMotion(e) if !held && !Self::injected(x, e.sourceid) => self.moved = true,
            Event::XinputRawButtonPress(e) if !held && !Self::injected(x, e.sourceid) => {
                if let Some(button) = from_x_button(e.detail) {
                    node.on_button(button, true);
                }
            }
            Event::XinputRawButtonRelease(e) if !held && !Self::injected(x, e.sourceid) => {
                if let Some(button) = from_x_button(e.detail) {
                    node.on_button(button, false);
                }
            }
            Event::XinputRawKeyPress(e) if !held && !Self::injected(x, e.sourceid) => key(node, e.detail, true),
            Event::XinputRawKeyRelease(e) if !held && !Self::injected(x, e.sourceid) => key(node, e.detail, false),

            // ----- grabbed, while the cursor is on another computer -----
            Event::MotionNotify(e) if held => self.held_motion(x, node, (e.root_x as i32, e.root_y as i32), sequence),
            Event::ButtonPress(e) if held => match wheel_of(e.detail as u32) {
                Some((dx, dy)) => {
                    node.on_wheel(dx, dy);
                }
                None => {
                    if let Some(button) = from_x_button(e.detail as u32) {
                        node.on_button(button, true);
                    }
                }
            },
            Event::ButtonRelease(e) if held => {
                if let Some(button) = from_x_button(e.detail as u32) {
                    node.on_button(button, false);
                }
            }
            Event::KeyPress(e) if held => {
                // A held key arrives as release and press with the same
                // time stamp, over and over. Pass only the presses on.
                if self.release != Some((e.detail, e.time)) {
                    self.flush_release(node);
                }
                self.release = None;
                key(node, e.detail as u32, true);
            }
            Event::KeyRelease(e) if held => self.release = Some((e.detail, e.time)),

            Event::ConfigureNotify(e) if e.window == x.root => x.measure(),
            Event::RandrScreenChangeNotify(_) => x.measure(),
            other => x11_clip::on_event(x, node, &other),
        }
    }

    /// Grab or let go so that the display matches what the engine thinks.
    fn follow(&mut self, x: &X, node: &Arc<Node>) {
        let held = x.held.load(Ordering::Relaxed);
        let wanted = node.is_remote();
        if wanted && !held {
            if x.grab() {
                self.last = x.pointer().unwrap_or(*x.park.lock().unwrap());
                self.warp = None;
                self.release = None;
                x.held.store(true, Ordering::Relaxed);
            } else {
                // Something else holds the mouse or keyboard (an open
                // menu, a lock screen): the cursor cannot leave right now.
                node.come_home();
            }
        } else if !wanted && held {
            self.flush_release(node);
            x.held.store(false, Ordering::Relaxed);
            x.ungrab();
        }
    }
}

fn key(node: &Arc<Node>, x_code: u32, down: bool) {
    let hid = x_code.checked_sub(8).and_then(|code| u16::try_from(code).ok()).and_then(keymap::from_linux);
    if let Some(hid) = hid {
        node.on_key(hid, down);
    }
}

/// Runs beside the event loop: wakes it when the engine changed its mind
/// without any input (a computer disconnected, sharing was paused), and
/// does the once-a-second chores of the windowed application.
fn ticker(x: Arc<X>, node: Arc<Node>) {
    let mut chores = Instant::now();
    let mut attention = false;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        if QUIT.load(Ordering::Relaxed) {
            x.wake();
            return;
        }
        if node.is_remote() != x.held.load(Ordering::Relaxed) {
            x.wake();
        }
        if chores.elapsed() < Duration::from_secs(1) {
            continue;
        }
        chores = Instant::now();
        if let Some(tray) = TRAY.get() {
            // A pairing code is waiting: bring the window up so it is seen.
            let now = (tray.needs_attention)();
            if now && !attention {
                (tray.open_window)();
            }
            attention = now;
            announce(&node);
        }
    }
}

/// Install the watchers and process events until the program exits. Must
/// be called on the thread that will stay for the lifetime of the program.
pub fn run(node: Arc<Node>) -> Result<(), String> {
    let x = X.get().ok_or("the display is not connected")?.clone();
    if NODE.set(node.clone()).is_err() {
        return Err("already running".into());
    }
    {
        let (x, node) = (x.clone(), node.clone());
        std::thread::spawn(move || ticker(x, node));
    }
    let mut pump = Pump::default();
    let lost = |e: x11rb::errors::ConnectionError| format!("lost the connection to the display: {e}");
    let result = loop {
        // Wait for one event, then take everything else that is ready, so
        // a burst of mouse movement costs one look at the cursor.
        let first = match x.conn.wait_for_event_with_sequence() {
            Ok(first) => first,
            Err(e) => break Err(lost(e)),
        };
        let mut next = Some(first);
        while let Some((event, sequence)) = next {
            pump.handle(&x, &node, event, sequence);
            next = match x.conn.poll_for_event_with_sequence() {
                Ok(next) => next,
                Err(_) => None,
            };
        }
        pump.flush_release(&node);
        if std::mem::take(&mut pump.moved) && !x.held.load(Ordering::Relaxed) {
            if let Some((px, py)) = x.pointer() {
                node.on_mouse_move(px, py);
            }
        }
        pump.follow(&x, &node);
        if QUIT.load(Ordering::Relaxed) {
            break Ok(());
        }
        let _ = x.conn.flush();
    };
    node.shutdown();
    x.held.store(false, Ordering::Relaxed);
    x.ungrab();
    result
}

// ---------- the windowed application ----------

/// What the windowed application needs to be told. (Named after the
/// Windows notification-area icon; Linux has no icon yet, only the window.)
pub struct TrayOptions {
    /// Show the settings window.
    pub open_window: Box<dyn Fn() + Send + Sync>,
    /// True while something is waiting for the user (a pairing code).
    pub needs_attention: Box<dyn Fn() -> bool + Send + Sync>,
}

/// Run as the windowed application.
pub fn run_with_tray(node: Arc<Node>, tray: TrayOptions) -> Result<(), String> {
    if TRAY.set(tray).is_err() {
        return Err("already running".into());
    }
    run(node)
}

/// The newest received file the user has been told about.
static ANNOUNCED: AtomicU32 = AtomicU32::new(0);

/// Files that were sent here land in a folder without any window opening,
/// so say so once with a desktop notification.
fn announce(node: &Arc<Node>) {
    let arrived = node.transfers().into_iter().find(|t| t.incoming && t.kind == TransferKind::Sent && t.state == TransferState::Done);
    let Some(arrived) = arrived else { return };
    if arrived.id <= ANNOUNCED.load(Ordering::Relaxed) {
        return;
    }
    ANNOUNCED.store(arrived.id, Ordering::Relaxed);
    let what = if arrived.files > 1 { format!("{} and {} more", arrived.label, arrived.files - 1) } else { arrived.label.clone() };
    let text = format!("{what}\nSaved in {}", node.received_dir().display());
    quiet(Command::new("notify-send").args(["--app-name=Screen Portals", "--"]).arg(format!("Received from {}", arrived.peer)).arg(text));
}

/// Ask the running application to exit cleanly. Safe to call from any thread.
pub fn request_quit() {
    QUIT.store(true, Ordering::Relaxed);
    if let Some(x) = X.get() {
        x.wake();
    }
}

/// Show the settings page in its own window if a browser that can do that
/// is installed (no tabs or address bar), otherwise in the usual browser.
pub fn open_app_window(url: &str) {
    for browser in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "brave-browser"] {
        if quiet(Command::new(browser).arg(format!("--app={url}")).arg("--window-size=1120,760")) {
            return;
        }
    }
    quiet(Command::new("xdg-open").arg(url));
}

/// The user's Downloads folder, wherever their desktop keeps it.
pub fn downloads_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let asked = Command::new("xdg-user-dir").arg("DOWNLOAD").stdin(Stdio::null()).stderr(Stdio::null()).output().ok();
    let named = asked
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string()))
        // With no Downloads folder configured the tool answers "home".
        .filter(|dir| dir.is_absolute() && Some(dir) != home.as_ref());
    named.or_else(|| home.map(|home| home.join("Downloads")))
}

fn lock_file() -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|dir| dir.is_dir()).unwrap_or_else(std::env::temp_dir);
    let name = format!("screen-portals-{}.lock", unsafe { libc::getuid() });
    std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(name)).ok()
}

fn try_lock(file: &std::fs::File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// True if another copy of the application already owns this session.
pub fn already_running() -> bool {
    // The lock is kept for the life of the process on purpose.
    static LOCK: OnceLock<Option<std::fs::File>> = OnceLock::new();
    match LOCK.get_or_init(lock_file) {
        Some(file) => !try_lock(file),
        None => false,
    }
}

/// After an update the new copy starts while the old one is still closing.
/// Wait for the old one to go; returns false if it never does.
pub fn wait_for_previous_copy(patience: Duration) -> bool {
    let Some(file) = lock_file() else { return true };
    let deadline = Instant::now() + patience;
    loop {
        if try_lock(&file) {
            // Let go again, so that only `already_running` decides who
            // owns the session (closing the file releases the lock).
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Download one address from github.com over HTTPS, refusing anything
/// larger than `limit` bytes. Uses the system's own curl, named by its full
/// path so nothing else on the search path can stand in for it.
pub fn fetch_https(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://github.com/") {
        return Err("updates only come from github.com".into());
    }
    let curl = ["/usr/bin/curl", "/bin/curl"].into_iter().find(|path| Path::new(path).is_file()).ok_or("curl is not installed")?;
    let output = Command::new(curl)
        .args(["--silent", "--show-error", "--fail", "--location", "--proto", "=https", "--proto-redir", "=https"])
        .args(["--max-time", "300", "--max-filesize", &limit.to_string(), "--output", "-", url])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start the download: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.stdout.len() as u64 > limit {
        return Err("the download was larger than announced".into());
    }
    Ok(output.stdout)
}

/// Tell the user why the program stopped: on the terminal, and as a
/// desktop notification for when it was started from a menu.
pub fn error_box(text: &str) {
    eprintln!("Screen Portals: {text}");
    quiet(Command::new("notify-send").args(["--app-name=Screen Portals", "--", "Screen Portals could not start"]).arg(text));
}

// ---------- checks for `doctor` ----------

/// What `doctor` reports about this session.
pub struct Diagnosis {
    pub desktop: Rect,
    pub park: (i32, i32),
    pub cursor: (i32, i32),
}

/// Look at the session without changing anything. An error means input
/// cannot be shared here, and says why.
pub fn diagnose() -> Result<Diagnosis, String> {
    let platform = platform()?;
    Ok(Diagnosis { desktop: platform.screen(), park: platform.park_point(), cursor: platform.cursor() })
}

/// Briefly move the real cursor to a few spots the way a remote computer
/// would, measure where it lands, then put it back. Returns the largest
/// error in pixels.
pub fn pointer_self_test() -> Result<i32, String> {
    let platform = platform()?;
    let screen = platform.screen();
    let original = platform.cursor();
    let mut worst = 0;
    for (fx, fy) in [(0.25, 0.25), (0.75, 0.5), (0.5, 0.9), (0.0, 0.0), (1.0, 1.0)] {
        let want = (screen.x + ((screen.w - 1) as f64 * fx).round() as i32, screen.y + ((screen.h - 1) as f64 * fy).round() as i32);
        let spot = crate::layout::Remote {
            side: crate::layout::Side::Right,
            w: screen.w,
            h: screen.h,
            x: want.0 - screen.x,
            y: want.1 - screen.y,
            door: (0.0, 1.0),
        };
        let (nx, ny) = spot.normalized();
        platform.inject_abs(nx, ny);
        std::thread::sleep(Duration::from_millis(30));
        let got = platform.cursor();
        worst = worst.max((got.0 - want.0).abs()).max((got.1 - want.1).abs());
    }
    platform.warp(original.0, original.1);
    Ok(worst)
}

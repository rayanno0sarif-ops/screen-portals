//! macOS implementation.
//!
//! - **Watching** this Mac's mouse and keyboard: an "event tap", which sees
//!   every event before programs do and can hold back the ones that now
//!   belong to another computer.
//! - **Playing back** input from another computer: events posted the way
//!   the mouse and keyboard themselves post them, marked so the tap can
//!   tell them apart from real ones.
//! - While the cursor is on another computer, the Mac's own cursor is
//!   detached from the mouse and stays put; movement is read from each
//!   event's distance moved instead.
//! - The menu bar gets a small icon, like the notification area on Windows.
//!
//! macOS only allows this to programs the user has switched on under
//! System Settings › Privacy & Security › Accessibility.

use super::{mac_clip, Caught, DragPayload, Platform};
use crate::keymap;
use crate::layout::Rect;
use crate::node::{Node, TransferState};
use crate::proto::{TransferKind, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, BUTTON_X1, BUTTON_X2};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSEvent, NSEventModifierFlags, NSEventType, NSImage,
    NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSString};
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use super::unix::{already_running, fetch_https, wait_for_previous_copy};

/// The parts of the system this file talks to directly.
#[allow(non_upper_case_globals)]
mod cg {
    use std::ffi::c_void;

    pub type Ref = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CGSize {
        pub width: f64,
        pub height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CGRect {
        pub origin: CGPoint,
        pub size: CGSize,
    }

    pub type TapCallback = unsafe extern "C" fn(proxy: Ref, kind: u32, event: Ref, info: *mut c_void) -> Ref;
    pub type TimerCallback = extern "C" fn(timer: Ref, info: *mut c_void);

    // Event kinds.
    pub const LEFT_DOWN: u32 = 1;
    pub const LEFT_UP: u32 = 2;
    pub const RIGHT_DOWN: u32 = 3;
    pub const RIGHT_UP: u32 = 4;
    pub const MOVED: u32 = 5;
    pub const LEFT_DRAGGED: u32 = 6;
    pub const RIGHT_DRAGGED: u32 = 7;
    pub const KEY_DOWN: u32 = 10;
    pub const KEY_UP: u32 = 11;
    pub const FLAGS_CHANGED: u32 = 12;
    pub const SCROLL: u32 = 22;
    pub const OTHER_DOWN: u32 = 25;
    pub const OTHER_UP: u32 = 26;
    pub const OTHER_DRAGGED: u32 = 27;
    pub const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    pub const TAP_DISABLED_BY_USER: u32 = 0xFFFF_FFFF;

    // Fields of an event.
    pub const CLICK_STATE: u32 = 1;
    pub const BUTTON_NUMBER: u32 = 3;
    pub const DELTA_X: u32 = 4;
    pub const DELTA_Y: u32 = 5;
    pub const AUTOREPEAT: u32 = 8;
    pub const KEYCODE: u32 = 9;
    pub const USER_DATA: u32 = 42;
    pub const SCROLL_FIXED_1: u32 = 93;
    pub const SCROLL_FIXED_2: u32 = 94;

    // Modifier flags, as a whole and per key.
    pub const FLAG_SHIFT: u64 = 0x0002_0000;
    pub const FLAG_CONTROL: u64 = 0x0004_0000;
    pub const FLAG_OPTION: u64 = 0x0008_0000;
    pub const FLAG_COMMAND: u64 = 0x0010_0000;

    pub const SESSION_TAP: u32 = 1;
    pub const HID_TAP: u32 = 0;
    pub const HEAD_INSERT: u32 = 0;
    pub const DEFAULT_TAP: u32 = 0;
    pub const HID_SYSTEM_STATE: i32 = 1;
    pub const SCROLL_UNIT_LINE: u32 = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: TapCallback, info: *mut c_void) -> Ref;
        pub fn CGEventTapEnable(tap: Ref, enable: bool);
        pub fn CGEventGetLocation(event: Ref) -> CGPoint;
        pub fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
        pub fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
        pub fn CGEventGetDoubleValueField(event: Ref, field: u32) -> f64;
        pub fn CGEventGetFlags(event: Ref) -> u64;
        pub fn CGEventSetFlags(event: Ref, flags: u64);
        pub fn CGEventSetType(event: Ref, kind: u32);
        pub fn CGEventCreate(source: Ref) -> Ref;
        pub fn CGEventCreateMouseEvent(source: Ref, kind: u32, at: CGPoint, button: u32) -> Ref;
        pub fn CGEventCreateKeyboardEvent(source: Ref, key: u16, down: bool) -> Ref;
        pub fn CGEventCreateScrollWheelEvent2(source: Ref, units: u32, count: u32, wheel1: i32, wheel2: i32, wheel3: i32) -> Ref;
        pub fn CGEventPost(tap: u32, event: Ref);
        pub fn CGEventSourceCreate(state: i32) -> Ref;
        pub fn CGEventSourceSetLocalEventsSuppressionInterval(source: Ref, seconds: f64);
        pub fn CGWarpMouseCursorPosition(at: CGPoint) -> i32;
        pub fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
        pub fn CGMainDisplayID() -> u32;
        pub fn CGDisplayBounds(display: u32) -> CGRect;
        pub fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        pub fn CGRequestPostEventAccess() -> bool;
        pub fn CGRequestListenEventAccess() -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub fn AXIsProcessTrusted() -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFRunLoopCommonModes: Ref;
        pub fn CFRelease(object: Ref);
        pub fn CFMachPortCreateRunLoopSource(allocator: Ref, port: Ref, order: isize) -> Ref;
        pub fn CFRunLoopGetMain() -> Ref;
        pub fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
        pub fn CFRunLoopTimerCreate(
            allocator: Ref,
            fire: f64,
            interval: f64,
            flags: usize,
            order: isize,
            callback: TimerCallback,
            context: *mut c_void,
        ) -> Ref;
        pub fn CFRunLoopAddTimer(run_loop: Ref, timer: Ref, mode: Ref);
        pub fn CFAbsoluteTimeGetCurrent() -> f64;
    }
}

/// Written into every event this program posts, so its own events are
/// never mistaken for the user's ("SPRT").
const MARK: i64 = 0x5350_5254;
/// The longest gap between presses that still counts as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

static NODE: OnceLock<Arc<Node>> = OnceLock::new();
static TRAY: OnceLock<TrayOptions> = OnceLock::new();
static QUIT: AtomicBool = AtomicBool::new(false);
/// True while the Mac's cursor is detached because it is on another computer.
static HELD: AtomicBool = AtomicBool::new(false);
static TAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static SOURCE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static PLAYBACK: Mutex<Playback> = Mutex::new(Playback::new());

/// What has been pressed on behalf of another computer.
struct Playback {
    buttons: Vec<u8>,
    keys: Vec<u16>,
    /// Wheel movement not yet large enough for one notch.
    wheel: (i32, i32),
    /// The last press, to count double and triple clicks.
    last_press: Option<(u8, Instant, (i32, i32), i64)>,
}

impl Playback {
    const fn new() -> Playback {
        Playback { buttons: Vec::new(), keys: Vec::new(), wheel: (0, 0), last_press: None }
    }

    /// The modifier flags programs expect on every event while those keys are held.
    fn flags(&self) -> u64 {
        self.keys.iter().map(|code| modifier_flag(*code)).fold(0, |all, flag| all | flag)
    }
}

/// The flag a modifier key sets: the general one and the one for its side.
fn modifier_flag(code: u16) -> u64 {
    match code {
        0x3B => cg::FLAG_CONTROL | 0x0001,
        0x3E => cg::FLAG_CONTROL | 0x2000,
        0x38 => cg::FLAG_SHIFT | 0x0002,
        0x3C => cg::FLAG_SHIFT | 0x0004,
        0x37 => cg::FLAG_COMMAND | 0x0008,
        0x36 => cg::FLAG_COMMAND | 0x0010,
        0x3A => cg::FLAG_OPTION | 0x0020,
        0x3D => cg::FLAG_OPTION | 0x0040,
        _ => 0,
    }
}

fn is_modifier(code: u16) -> bool {
    modifier_flag(code) != 0
}

fn point(x: i32, y: i32) -> cg::CGPoint {
    cg::CGPoint { x: x as f64, y: y as f64 }
}

fn source() -> cg::Ref {
    SOURCE.load(Ordering::Relaxed)
}

/// Mark an event as ours, post it, and let it go.
unsafe fn post(event: cg::Ref) {
    if event.is_null() {
        return;
    }
    cg::CGEventSetIntegerValueField(event, cg::USER_DATA, MARK);
    cg::CGEventPost(cg::HID_TAP, event);
    cg::CFRelease(event);
}

fn displays() -> Vec<cg::CGRect> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    unsafe {
        if cg::CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) != 0 || count == 0 {
            return vec![cg::CGDisplayBounds(cg::CGMainDisplayID())];
        }
        ids[..count as usize].iter().map(|id| cg::CGDisplayBounds(*id)).collect()
    }
}

/// Where the cursor really is right now.
fn pointer() -> (i32, i32) {
    unsafe {
        let event = cg::CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return (0, 0);
        }
        let at = cg::CGEventGetLocation(event);
        cg::CFRelease(event);
        (at.x.floor() as i32, at.y.floor() as i32)
    }
}

/// The real thing: this Mac's mouse, keyboard, clipboard, sound and screen.
pub struct MacPlatform;

/// Why this Mac cannot share its mouse and keyboard yet, if it cannot.
fn permission_problem() -> Option<String> {
    if unsafe { cg::AXIsProcessTrusted() } {
        return None;
    }
    // Shows the system's own request, with a button to System Settings.
    unsafe {
        cg::CGRequestPostEventAccess();
        cg::CGRequestListenEventAccess();
    }
    Some(
        "macOS has not yet allowed Screen Portals to use the mouse and keyboard. Open System Settings, then \
         Privacy & Security, then Accessibility, switch on Screen Portals (or the Terminal you started it from), \
         and start it again. These builds are not signed by a registered Apple developer, so macOS asks again \
         after each update"
            .into(),
    )
}

/// Get ready to use this Mac. Fails with a sentence for the user when
/// macOS has not allowed it to watch and move the mouse.
pub fn platform() -> Result<MacPlatform, String> {
    if let Some(problem) = permission_problem() {
        return Err(problem);
    }
    if source().is_null() {
        unsafe {
            let made = cg::CGEventSourceCreate(cg::HID_SYSTEM_STATE);
            if !made.is_null() {
                // Without this, real movement is ignored for a quarter of a
                // second after each posted event or warp.
                cg::CGEventSourceSetLocalEventsSuppressionInterval(made, 0.0);
            }
            SOURCE.store(made, Ordering::Relaxed);
        }
    }
    Ok(MacPlatform)
}

impl Platform for MacPlatform {
    fn screen(&self) -> Rect {
        let all = displays();
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for d in &all {
            x0 = x0.min(d.origin.x);
            y0 = y0.min(d.origin.y);
            x1 = x1.max(d.origin.x + d.size.width);
            y1 = y1.max(d.origin.y + d.size.height);
        }
        if all.is_empty() || x1 <= x0 {
            return Rect { x: 0, y: 0, w: 1, h: 1 };
        }
        Rect { x: x0 as i32, y: y0 as i32, w: (x1 - x0) as i32, h: (y1 - y0) as i32 }
    }

    fn park_point(&self) -> (i32, i32) {
        let main = unsafe { cg::CGDisplayBounds(cg::CGMainDisplayID()) };
        ((main.origin.x + main.size.width / 2.0) as i32, (main.origin.y + main.size.height / 2.0) as i32)
    }

    fn cursor(&self) -> (i32, i32) {
        // While the cursor is on another computer the engine measures
        // movement from the resting place; the tap reports movement
        // relative to it.
        if HELD.load(Ordering::Relaxed) {
            return self.park_point();
        }
        pointer()
    }

    fn warp(&self, x: i32, y: i32) {
        unsafe {
            cg::CGWarpMouseCursorPosition(point(x, y));
        }
    }

    fn inject_abs(&self, x: u16, y: u16) {
        let screen = self.screen();
        let place = |n: u16, len: i32| ((n as f64 / 65535.0) * (len - 1).max(0) as f64).round() as i32;
        let at = point(screen.x + place(x, screen.w), screen.y + place(y, screen.h));
        let playback = PLAYBACK.lock().unwrap();
        // With a button held, movement is a drag, or programs miss it.
        let (kind, button) = if playback.buttons.contains(&BUTTON_LEFT) {
            (cg::LEFT_DRAGGED, 0)
        } else if playback.buttons.contains(&BUTTON_RIGHT) {
            (cg::RIGHT_DRAGGED, 1)
        } else if let Some(other) = playback.buttons.first() {
            (cg::OTHER_DRAGGED, other_number(*other))
        } else {
            (cg::MOVED, 0)
        };
        unsafe {
            let event = cg::CGEventCreateMouseEvent(source(), kind, at, button);
            if !event.is_null() {
                cg::CGEventSetFlags(event, playback.flags());
            }
            post(event);
        }
    }

    fn inject_button(&self, button: u8, down: bool) {
        let (kind, number) = match (button, down) {
            (BUTTON_LEFT, true) => (cg::LEFT_DOWN, 0),
            (BUTTON_LEFT, false) => (cg::LEFT_UP, 0),
            (BUTTON_RIGHT, true) => (cg::RIGHT_DOWN, 1),
            (BUTTON_RIGHT, false) => (cg::RIGHT_UP, 1),
            (BUTTON_MIDDLE | BUTTON_X1 | BUTTON_X2, true) => (cg::OTHER_DOWN, other_number(button)),
            (BUTTON_MIDDLE | BUTTON_X1 | BUTTON_X2, false) => (cg::OTHER_UP, other_number(button)),
            _ => return,
        };
        let at = pointer();
        let mut playback = PLAYBACK.lock().unwrap();
        // macOS only sees a double click if the events say so.
        let clicks = if down {
            let count = match playback.last_press {
                Some((b, when, (px, py), count))
                    if b == button && when.elapsed() < DOUBLE_CLICK && (px - at.0).abs() <= 4 && (py - at.1).abs() <= 4 =>
                {
                    count % 3 + 1
                }
                _ => 1,
            };
            playback.last_press = Some((button, Instant::now(), at, count));
            if !playback.buttons.contains(&button) {
                playback.buttons.push(button);
            }
            count
        } else {
            playback.buttons.retain(|b| *b != button);
            playback.last_press.map_or(1, |(_, _, _, count)| count)
        };
        unsafe {
            let event = cg::CGEventCreateMouseEvent(source(), kind, point(at.0, at.1), number);
            if !event.is_null() {
                cg::CGEventSetIntegerValueField(event, cg::CLICK_STATE, clicks);
                cg::CGEventSetIntegerValueField(event, cg::BUTTON_NUMBER, number as i64);
                cg::CGEventSetFlags(event, playback.flags());
            }
            post(event);
        }
    }

    fn inject_wheel(&self, dx: i16, dy: i16) {
        // One notch is 120; finer wheels send fractions of one.
        let (across, up) = {
            let mut playback = PLAYBACK.lock().unwrap();
            playback.wheel.0 += dx as i32;
            playback.wheel.1 += dy as i32;
            let whole = (playback.wheel.0 / 120, playback.wheel.1 / 120);
            playback.wheel.0 -= whole.0 * 120;
            playback.wheel.1 -= whole.1 * 120;
            whole
        };
        if across == 0 && up == 0 {
            return;
        }
        unsafe {
            // The second wheel counts the other way round to Windows'.
            post(cg::CGEventCreateScrollWheelEvent2(source(), cg::SCROLL_UNIT_LINE, 2, up, -across, 0));
        }
    }

    fn inject_key(&self, hid: u16, down: bool) {
        let Some(code) = keymap::to_mac(hid) else { return };
        let mut playback = PLAYBACK.lock().unwrap();
        let repeat = down && playback.keys.contains(&code);
        if down {
            if !repeat {
                playback.keys.push(code);
            }
        } else {
            playback.keys.retain(|k| *k != code);
        }
        unsafe {
            let event = cg::CGEventCreateKeyboardEvent(source(), code, down);
            if event.is_null() {
                return;
            }
            if is_modifier(code) {
                cg::CGEventSetType(event, cg::FLAGS_CHANGED);
            }
            // Programs read Shift, Command and the rest from every key event.
            cg::CGEventSetFlags(event, playback.flags());
            if repeat {
                cg::CGEventSetIntegerValueField(event, cg::AUTOREPEAT, 1);
            }
            post(event);
        }
    }

    fn set_clipboard_text(&self, text: &str) {
        mac_clip::offer(mac_clip::Offer::Text(text.to_string()));
    }

    fn set_clipboard_files(&self, paths: &[PathBuf]) {
        mac_clip::offer(mac_clip::Offer::Files(paths.to_vec()));
    }

    fn set_clipboard_image(&self, png: &[u8]) {
        mac_clip::offer(mac_clip::Offer::Image(png.to_vec()));
    }

    fn clipboard_image(&self) -> Option<Vec<u8>> {
        mac_clip::read_image()
    }

    fn reveal(&self, path: &Path) {
        quiet(Command::new("open").arg("-R").arg(path));
    }

    fn capture_audio(&self, _on: bool) {}

    fn can_send_audio(&self) -> bool {
        // A Mac offers no simple way to record what it plays, so its own
        // sound stays here. It can still play everyone else's.
        false
    }

    fn play_audio(&self, source: u64, seq: u32, rate: u32, stereo: &[i16]) {
        super::mac_audio::play(source, seq, rate, stereo);
    }

    fn drag_catch(&self, token: u64) {
        // Taking over another program's drag is not built for the Mac yet,
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
        quiet(Command::new("open").arg(url));
    }
}

/// The number macOS gives the middle and side buttons.
fn other_number(button: u8) -> u32 {
    match button {
        BUTTON_X1 => 3,
        BUTTON_X2 => 4,
        _ => 2,
    }
}

fn from_other_number(number: i64) -> Option<u8> {
    match number {
        2 => Some(BUTTON_MIDDLE),
        3 => Some(BUTTON_X1),
        4 => Some(BUTTON_X2),
        _ => None,
    }
}

/// Start a helper program without waiting for it or showing its output.
fn quiet(command: &mut Command) -> bool {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
}

// ---------- watching this Mac's own mouse and keyboard ----------

/// Scroll movement not yet large enough to pass on (trackpads send tiny steps).
static SCROLL_REST: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));

/// One event from the tap. Returns true if it must be held back from this
/// Mac because it now belongs to another computer.
unsafe fn handle(node: &Arc<Node>, kind: u32, event: cg::Ref) -> bool {
    let remote = HELD.load(Ordering::Relaxed);
    match kind {
        cg::MOVED | cg::LEFT_DRAGGED | cg::RIGHT_DRAGGED | cg::OTHER_DRAGGED => {
            if remote {
                let dx = cg::CGEventGetIntegerValueField(event, cg::DELTA_X) as i32;
                let dy = cg::CGEventGetIntegerValueField(event, cg::DELTA_Y) as i32;
                let park = MacPlatform.park_point();
                if dx != 0 || dy != 0 {
                    node.on_mouse_move(park.0 + dx, park.1 + dy);
                }
                true
            } else {
                let at = cg::CGEventGetLocation(event);
                node.on_mouse_move(at.x.floor() as i32, at.y.floor() as i32)
            }
        }
        cg::LEFT_DOWN | cg::LEFT_UP => node.on_button(BUTTON_LEFT, kind == cg::LEFT_DOWN),
        cg::RIGHT_DOWN | cg::RIGHT_UP => node.on_button(BUTTON_RIGHT, kind == cg::RIGHT_DOWN),
        cg::OTHER_DOWN | cg::OTHER_UP => match from_other_number(cg::CGEventGetIntegerValueField(event, cg::BUTTON_NUMBER)) {
            Some(button) => node.on_button(button, kind == cg::OTHER_DOWN),
            None => remote,
        },
        cg::SCROLL => {
            let (up, across) = {
                let mut rest = SCROLL_REST.lock().unwrap();
                rest.1 += cg::CGEventGetDoubleValueField(event, cg::SCROLL_FIXED_1) * 120.0;
                rest.0 -= cg::CGEventGetDoubleValueField(event, cg::SCROLL_FIXED_2) * 120.0;
                let whole = (rest.1.trunc(), rest.0.trunc());
                rest.1 -= whole.0;
                rest.0 -= whole.1;
                whole
            };
            if up != 0.0 || across != 0.0 {
                node.on_wheel(across.clamp(-32000.0, 32000.0) as i16, up.clamp(-32000.0, 32000.0) as i16);
            }
            remote
        }
        cg::KEY_DOWN | cg::KEY_UP => {
            let code = cg::CGEventGetIntegerValueField(event, cg::KEYCODE) as u16;
            match keymap::from_mac(code) {
                Some(hid) => node.on_key(hid, kind == cg::KEY_DOWN),
                None => remote,
            }
        }
        cg::FLAGS_CHANGED => {
            let code = cg::CGEventGetIntegerValueField(event, cg::KEYCODE) as u16;
            if code == 0x39 {
                // Caps Lock reports only that its light changed: a whole press.
                node.on_key(0x39, true);
                node.on_key(0x39, false)
            } else if let (true, Some(hid)) = (is_modifier(code), keymap::from_mac(code)) {
                let side = modifier_flag(code) & 0xFFFF;
                node.on_key(hid, cg::CGEventGetFlags(event) & side != 0)
            } else {
                remote
            }
        }
        _ => false,
    }
}

/// Detach the cursor while it is on another computer, and attach it again
/// when it is back, so that the two always agree.
fn follow(node: &Arc<Node>) {
    let wanted = node.is_remote();
    if wanted != HELD.load(Ordering::Relaxed) {
        HELD.store(wanted, Ordering::Relaxed);
        unsafe {
            cg::CGAssociateMouseAndMouseCursorPosition(if wanted { 0 } else { 1 });
        }
    }
}

unsafe extern "C" fn tap_callback(_proxy: cg::Ref, kind: u32, event: cg::Ref, _info: *mut c_void) -> cg::Ref {
    // macOS switches a tap off if it ever answers too slowly; switch it on again.
    if kind == cg::TAP_DISABLED_BY_TIMEOUT || kind == cg::TAP_DISABLED_BY_USER {
        let tap = TAP.load(Ordering::Relaxed);
        if !tap.is_null() {
            cg::CGEventTapEnable(tap, true);
        }
        return event;
    }
    // Events this program posted itself are never captured, which is what
    // stops two computers from echoing input back and forth.
    if event.is_null() || cg::CGEventGetIntegerValueField(event, cg::USER_DATA) == MARK {
        return event;
    }
    let Some(node) = NODE.get() else { return event };
    let swallow = handle(node, kind, event);
    follow(node);
    if swallow {
        std::ptr::null_mut()
    } else {
        event
    }
}

// ---------- the main thread: run loop, menu bar, chores ----------

/// What the windowed application needs to be told.
pub struct TrayOptions {
    /// Show the settings window.
    pub open_window: Box<dyn Fn() + Send + Sync>,
    /// True while something is waiting for the user (a pairing code).
    pub needs_attention: Box<dyn Fn() -> bool + Send + Sync>,
}

thread_local! {
    /// The menu-bar icon and its pause item, kept alive on the main thread.
    static MENU_BAR: RefCell<Option<(Retained<NSStatusItem>, Retained<NSMenuItem>, Retained<Delegate>)>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this class
    // does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ScreenPortalsDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        // The app was opened again (from Finder or the Dock) while running.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            if let Some(tray) = TRAY.get() {
                (tray.open_window)();
            }
            true
        }
    }

    impl Delegate {
        #[unsafe(method(openWindow:))]
        fn open_window(&self, _sender: Option<&AnyObject>) {
            if let Some(tray) = TRAY.get() {
                (tray.open_window)();
            }
        }

        #[unsafe(method(togglePause:))]
        fn toggle_pause(&self, _sender: Option<&AnyObject>) {
            if let Some(node) = NODE.get() {
                node.set_paused(!node.is_paused());
            }
            refresh_menu_bar();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            request_quit();
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Delegate> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn menu_item(mtm: MainThreadMarker, title: &str, action: objc2::runtime::Sel, target: &Delegate) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(""))
    };
    let target: &AnyObject = target;
    unsafe { item.setTarget(Some(target)) };
    item
}

fn add_menu_bar(mtm: MainThreadMarker, delegate: Retained<Delegate>) {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        let description = NSString::from_str("Screen Portals");
        match NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("rectangle.on.rectangle"), Some(&description)) {
            Some(image) => {
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
            None => button.setTitle(&NSString::from_str("Portals")),
        }
    }
    let menu = NSMenu::new(mtm);
    menu.addItem(&menu_item(mtm, "Open Screen Portals", sel!(openWindow:), &delegate));
    let pause = menu_item(mtm, "Pause sharing", sel!(togglePause:), &delegate);
    menu.addItem(&pause);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Quit Screen Portals", sel!(quit:), &delegate));
    item.setMenu(Some(&menu));
    MENU_BAR.with(|bar| *bar.borrow_mut() = Some((item, pause, delegate)));
}

fn summary(node: &Node) -> String {
    let status = node.status();
    let up: Vec<&str> = status.peers.iter().filter(|p| p.connected).map(|p| p.name.as_str()).collect();
    if status.paused {
        "Screen Portals: sharing is paused".into()
    } else if up.len() == 1 {
        format!("Screen Portals: connected to {}", up[0])
    } else if up.len() > 1 {
        format!("Screen Portals: connected to {} computers", up.len())
    } else if status.peers.is_empty() {
        "Screen Portals: no computers paired".into()
    } else {
        "Screen Portals: waiting for your computers".into()
    }
}

fn refresh_menu_bar() {
    let Some(node) = NODE.get() else { return };
    let Some(mtm) = MainThreadMarker::new() else { return };
    MENU_BAR.with(|bar| {
        if let Some((item, pause, _)) = bar.borrow().as_ref() {
            pause.setTitle(&NSString::from_str(if node.is_paused() { "Resume sharing" } else { "Pause sharing" }));
            if let Some(button) = item.button(mtm) {
                button.setToolTip(Some(&NSString::from_str(&summary(node))));
            }
        }
    });
}

/// The newest received file the user has been told about.
static ANNOUNCED: AtomicU32 = AtomicU32::new(0);

/// Show a notification. Text travels as arguments, never inside the script.
fn notify(title: &str, text: &str) {
    quiet(Command::new("osascript").args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run"]).arg(title).arg(text));
}

/// Files that were sent here land in a folder without any window opening,
/// so say so once with a notification.
fn announce(node: &Node) {
    let arrived = node.transfers().into_iter().find(|t| t.incoming && t.kind == TransferKind::Sent && t.state == TransferState::Done);
    let Some(arrived) = arrived else { return };
    if arrived.id <= ANNOUNCED.load(Ordering::Relaxed) {
        return;
    }
    ANNOUNCED.store(arrived.id, Ordering::Relaxed);
    let what = if arrived.files > 1 { format!("{} and {} more", arrived.label, arrived.files - 1) } else { arrived.label.clone() };
    notify(&format!("Received from {}", arrived.peer), &format!("{what}, saved in {}", node.received_dir().display()));
}

/// Ten times a second on the main thread: quit when asked, keep the cursor
/// attached or detached as it should be, look after the clipboard, and
/// once a second the menu-bar icon.
extern "C" fn tick(_timer: cg::Ref, _info: *mut c_void) {
    thread_local! {
        static LAST: RefCell<(Instant, bool, bool)> = RefCell::new((Instant::now(), false, false));
    }
    let Some(node) = NODE.get() else { return };
    objc2::rc::autoreleasepool(|_| {
        if QUIT.load(Ordering::Relaxed) {
            stop_app();
            return;
        }
        follow(node);
        mac_clip::tick(node);
        LAST.with(|last| {
            let mut last = last.borrow_mut();
            if last.0.elapsed() < Duration::from_secs(1) {
                return;
            }
            last.0 = Instant::now();
            if let Some(tray) = TRAY.get() {
                // A pairing code is waiting: bring the window up so it is seen.
                let attention = (tray.needs_attention)();
                if attention && !last.1 {
                    (tray.open_window)();
                }
                last.1 = attention;
                announce(node);
                refresh_menu_bar();
            }
        });
    });
}

/// Leave `NSApplication::run`, which otherwise never returns.
fn stop_app() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    app.stop(None);
    // Stopping takes effect after the next event, so send one.
    let wake = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0,
        0,
        0,
    );
    if let Some(wake) = wake {
        app.postEvent_atStart(&wake, true);
    }
}

extern "C" fn on_signal(_signal: libc::c_int) {
    // Only this much is safe inside a signal handler; the main thread
    // notices within a moment and shuts down properly.
    QUIT.store(true, Ordering::Relaxed);
}

/// Call once at the very start of the program.
pub fn init() {
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // Ctrl+C, an ordinary request to end, and the terminal closing.
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(signal, handler);
        }
    }
}

/// Watch the mouse and keyboard and process events until the program
/// exits. Must be called on the program's main thread.
pub fn run(node: Arc<Node>) -> Result<(), String> {
    run_inner(node, false)
}

/// Run as the windowed application, with an icon in the menu bar.
pub fn run_with_tray(node: Arc<Node>, tray: TrayOptions) -> Result<(), String> {
    if TRAY.set(tray).is_err() {
        return Err("already running".into());
    }
    run_inner(node, true)
}

fn run_inner(node: Arc<Node>, with_menu_bar: bool) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("Screen Portals must run on the program's main thread")?;
    if NODE.set(node.clone()).is_err() {
        return Err("already running".into());
    }
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon: the windowed application lives in the menu bar, the
    // command-line program nowhere.
    app.setActivationPolicy(if with_menu_bar { NSApplicationActivationPolicy::Accessory } else { NSApplicationActivationPolicy::Prohibited });

    let kinds = [
        cg::LEFT_DOWN,
        cg::LEFT_UP,
        cg::RIGHT_DOWN,
        cg::RIGHT_UP,
        cg::MOVED,
        cg::LEFT_DRAGGED,
        cg::RIGHT_DRAGGED,
        cg::KEY_DOWN,
        cg::KEY_UP,
        cg::FLAGS_CHANGED,
        cg::SCROLL,
        cg::OTHER_DOWN,
        cg::OTHER_UP,
        cg::OTHER_DRAGGED,
    ];
    let mask = kinds.iter().fold(0u64, |mask, kind| mask | 1 << kind);
    unsafe {
        let tap = cg::CGEventTapCreate(cg::SESSION_TAP, cg::HEAD_INSERT, cg::DEFAULT_TAP, mask, tap_callback, std::ptr::null_mut());
        if tap.is_null() {
            return Err(permission_problem().unwrap_or_else(|| "macOS refused to let Screen Portals watch the mouse and keyboard".into()));
        }
        TAP.store(tap, Ordering::Relaxed);
        let main = cg::CFRunLoopGetMain();
        let source = cg::CFMachPortCreateRunLoopSource(std::ptr::null_mut(), tap, 0);
        cg::CFRunLoopAddSource(main, source, cg::kCFRunLoopCommonModes);
        cg::CGEventTapEnable(tap, true);
        let timer = cg::CFRunLoopTimerCreate(std::ptr::null_mut(), cg::CFAbsoluteTimeGetCurrent() + 0.1, 0.1, 0, 0, tick, std::ptr::null_mut());
        cg::CFRunLoopAddTimer(main, timer, cg::kCFRunLoopCommonModes);
    }

    // Whatever was copied before now is left alone.
    mac_clip::start();
    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    if with_menu_bar {
        add_menu_bar(mtm, delegate.clone());
        refresh_menu_bar();
    }
    app.run();

    node.shutdown();
    unsafe {
        cg::CGAssociateMouseAndMouseCursorPosition(1);
    }
    HELD.store(false, Ordering::Relaxed);
    MENU_BAR.with(|bar| {
        if let Some((item, _, _)) = bar.borrow_mut().take() {
            NSStatusBar::systemStatusBar().removeStatusItem(&item);
        }
    });
    drop(delegate);
    Ok(())
}

/// Ask the running application to exit cleanly. Safe to call from any thread.
pub fn request_quit() {
    QUIT.store(true, Ordering::Relaxed);
}

/// Show the settings page in its own window if a browser that can do that
/// is installed (no tabs or address bar), otherwise in the usual browser.
pub fn open_app_window(url: &str) {
    for browser in ["Google Chrome", "Microsoft Edge", "Brave Browser", "Chromium"] {
        let opened = Command::new("open")
            .args(["-na", browser, "--args"])
            .arg(format!("--app={url}"))
            .arg("--window-size=1120,760")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if opened {
            return;
        }
    }
    quiet(Command::new("open").arg(url));
}

/// The user's Downloads folder.
pub fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
}

/// Tell the user why the program stopped: on the terminal, and in a
/// window for when it was started from Finder.
pub fn error_box(text: &str) {
    eprintln!("Screen Portals: {text}");
    let _ = Command::new("osascript")
        .args(["-e", "on run argv", "-e", "display alert \"Screen Portals could not start\" message (item 1 of argv) as critical", "-e", "end run"])
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// True if this copy runs from the read-only place macOS uses for apps
/// opened straight from a download, where it cannot update itself.
pub fn running_from_download() -> bool {
    std::env::current_exe().is_ok_and(|exe| exe.to_string_lossy().contains("/AppTranslocation/"))
}

// ---------- checks for `doctor` ----------

/// What `doctor` reports about this Mac.
pub struct Diagnosis {
    pub desktop: Rect,
    pub displays: usize,
    pub park: (i32, i32),
    pub cursor: (i32, i32),
}

/// Look at this Mac without changing anything. An error means input
/// cannot be shared yet, and says why.
pub fn diagnose() -> Result<Diagnosis, String> {
    let platform = platform()?;
    Ok(Diagnosis { desktop: platform.screen(), displays: displays().len(), park: platform.park_point(), cursor: platform.cursor() })
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
        std::thread::sleep(Duration::from_millis(50));
        let got = platform.cursor();
        worst = worst.max((got.0 - want.0).abs()).max((got.1 - want.1).abs());
    }
    platform.warp(original.0, original.1);
    Ok(worst)
}

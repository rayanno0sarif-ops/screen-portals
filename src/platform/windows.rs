//! Windows implementation: low-level mouse and keyboard hooks to watch local
//! input, `SendInput` to play back input from another computer, and a hidden
//! window to follow clipboard changes.

use super::Platform;
use crate::keymap;
use crate::layout::Rect;
use crate::node::Node;
use crate::proto::{BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, BUTTON_X1, BUTTON_X2};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::node::TransferState;
use crate::picture;
use crate::proto::TransferKind;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::DataExchange::{GetClipboardOwner, RegisterClipboardFormatW};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    DragQueryFileW, FOLDERID_Downloads, SHGetKnownFolderPath, ShellExecuteW, Shell_NotifyIconW, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, DestroyMenu, MessageBoxW, PostQuitMessage, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, TrackPopupMenu, HICON,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    PostMessageW, RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, WNDCLASSW,
};

// Win32 constants, spelled out so the meaning is visible here.
const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const WM_CLIPBOARDUPDATE: u32 = 0x031D;
const WM_ENDSESSION: u32 = 0x0016;
const WM_APP_SET_CLIPBOARD: u32 = 0x8001;
const LLMHF_INJECTED: u32 = 0x01;
const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_UP: u32 = 0x80;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;
const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008;
const CF_BITMAP: u32 = 2;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const VK_PAUSE: u16 = 0x13;
/// Parent handle that makes a window message-only (never shown).
const HWND_MESSAGE: HWND = -3isize as HWND;

static NODE: OnceLock<Arc<Node>> = OnceLock::new();
static WINDOW: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PENDING_CLIPBOARD: Mutex<Option<ClipWrite>> = Mutex::new(None);

/// Something to put on the clipboard, waiting for the window thread.
enum ClipWrite {
    Text(String),
    Files(Vec<PathBuf>),
    /// The PNG as it arrived, and the same picture as a Windows bitmap.
    Image(Vec<u8>, Vec<u8>),
}

/// What another program just put on the clipboard.
enum ClipRead {
    Files(Vec<PathBuf>),
    Text(String),
    Image,
    Other,
}

/// The clipboard belongs to the window thread, so hand the content over.
fn post_clipboard(write: ClipWrite) {
    *PENDING_CLIPBOARD.lock().unwrap() = Some(write);
    let window = WINDOW.load(Ordering::Relaxed);
    if !window.is_null() {
        unsafe {
            PostMessageW(window, WM_APP_SET_CLIPBOARD, 0, 0);
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Call once at program start, before anything asks for screen coordinates,
/// so that positions are real pixels on high-DPI displays.
/// Ctrl+C or closing the console window: tidy up, then let Windows end the
/// program as usual.
unsafe extern "system" fn console_closing(_kind: u32) -> i32 {
    if let Some(node) = NODE.get() {
        node.shutdown();
    }
    0
}

/// Call once at the very start of the program, before any window or hook exists.
pub fn init() {
    super::win_audio::recover();
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_closing), 1);
    }
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// The real thing: this computer's mouse, keyboard, clipboard, sound and screen.
pub struct WinPlatform;

/// The platform to hand to the engine. (It cannot fail on Windows; on
/// Linux the same call can, when the desktop session is unusable.)
pub fn platform() -> Result<WinPlatform, String> {
    Ok(WinPlatform)
}

fn send_mouse(dx: i32, dy: i32, data: i32, flags: u32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

fn send_key(vk: u16, scan: u16, flags: u32) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

impl Platform for WinPlatform {
    fn screen(&self) -> Rect {
        unsafe {
            Rect {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        }
    }

    fn park_point(&self) -> (i32, i32) {
        // Middle of the main monitor: always a real, visible pixel with room
        // to measure movement in every direction.
        unsafe { (GetSystemMetrics(SM_CXSCREEN) / 2, GetSystemMetrics(SM_CYSCREEN) / 2) }
    }

    fn cursor(&self) -> (i32, i32) {
        let mut point = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut point);
        }
        (point.x, point.y)
    }

    fn warp(&self, x: i32, y: i32) {
        unsafe {
            SetCursorPos(x, y);
        }
    }

    fn inject_abs(&self, x: u16, y: u16) {
        send_mouse(x as i32, y as i32, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK);
    }

    fn inject_button(&self, button: u8, down: bool) {
        // A drag that was about to start here is called off instead if its
        // button is released first; see `win_drag::release_before_press`.
        if button == BUTTON_LEFT && !down && super::win_drag::release_before_press() {
            return;
        }
        let pick = |d: u32, u: u32| if down { d } else { u };
        match button {
            BUTTON_LEFT => send_mouse(0, 0, 0, pick(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)),
            BUTTON_RIGHT => send_mouse(0, 0, 0, pick(MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)),
            BUTTON_MIDDLE => send_mouse(0, 0, 0, pick(MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)),
            BUTTON_X1 => send_mouse(0, 0, 1, pick(MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP)),
            BUTTON_X2 => send_mouse(0, 0, 2, pick(MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP)),
            _ => {}
        }
    }

    fn inject_wheel(&self, dx: i16, dy: i16) {
        if dy != 0 {
            send_mouse(0, 0, dy as i32, MOUSEEVENTF_WHEEL);
        }
        if dx != 0 {
            send_mouse(0, 0, dx as i32, MOUSEEVENTF_HWHEEL);
        }
    }

    fn inject_key(&self, hid: u16, down: bool) {
        let up = if down { 0 } else { KEYEVENTF_KEYUP };
        if hid == keymap::HID_PAUSE {
            send_key(VK_PAUSE, 0, up);
        } else if let Some((scan, extended)) = keymap::to_windows(hid) {
            let ext = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
            send_key(0, scan, KEYEVENTF_SCANCODE | ext | up);
        }
    }

    fn set_clipboard_text(&self, text: &str) {
        post_clipboard(ClipWrite::Text(text.to_string()));
    }

    fn set_clipboard_files(&self, paths: &[PathBuf]) {
        post_clipboard(ClipWrite::Files(paths.to_vec()));
    }

    fn set_clipboard_image(&self, png: &[u8]) {
        match picture::dib_from_png(png) {
            Ok(dib) => post_clipboard(ClipWrite::Image(png.to_vec(), dib)),
            Err(e) => eprintln!("[screen-portals] a copied picture could not be used: {e}"),
        }
    }

    fn clipboard_image(&self) -> Option<Vec<u8>> {
        read_clipboard_image()
    }

    fn reveal(&self, path: &Path) {
        let _ = std::process::Command::new("explorer.exe").arg("/select,").arg(path).spawn();
    }

    fn capture_audio(&self, on: bool) {
        super::win_audio::capture(on);
    }

    fn play_audio(&self, source: u64, seq: u32, rate: u32, stereo: &[i16]) {
        super::win_audio::play(source, seq, rate, stereo);
    }

    fn drag_catch(&self, token: u64) {
        super::win_drag::catch(token);
    }

    fn drag_begin(&self, payload: super::DragPayload) {
        super::win_drag::begin(payload);
    }

    fn drag_cancel(&self) {
        super::win_drag::cancel();
    }

    fn open_url(&self, url: &str) {
        // Only ever called with an address that passed `transfer::safe_url`.
        unsafe {
            ShellExecuteW(null_mut(), wide("open").as_ptr(), wide(url).as_ptr(), null(), null(), 1);
        }
    }
}

/// The page of a browser window the user is moving right now, if any.
pub(super) fn page_being_moved() -> Option<String> {
    super::win_page::page_being_moved()
}

/// The running engine, for the parts of the platform that feed it.
pub(super) fn node() -> Option<&'static Arc<Node>> {
    NODE.get()
}

/// The user's Downloads folder, wherever they have moved it.
pub fn downloads_dir() -> Option<PathBuf> {
    unsafe {
        let mut raw: *mut u16 = null_mut();
        let found = SHGetKnownFolderPath(&FOLDERID_Downloads, 0, null_mut(), &mut raw) >= 0 && !raw.is_null();
        let path = found.then(|| {
            let mut len = 0;
            while *raw.add(len) != 0 {
                len += 1;
            }
            PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, len)))
        });
        CoTaskMemFree(raw as *const c_void);
        path
    }
}

/// Copy bytes into memory the clipboard can take ownership of.
unsafe fn put_format(format: u32, bytes: &[u8]) {
    if format == 0 {
        return;
    }
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
    if handle.is_null() {
        return;
    }
    let target = GlobalLock(handle) as *mut u8;
    if target.is_null() {
        GlobalFree(handle);
        return;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
    GlobalUnlock(handle);
    // On success the system owns the memory from here on.
    if SetClipboardData(format, handle).is_null() {
        GlobalFree(handle);
    }
}

unsafe fn global_bytes(handle: HANDLE) -> Option<Vec<u8>> {
    let data = GlobalLock(handle) as *const u8;
    if data.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(data, GlobalSize(handle)).to_vec();
    GlobalUnlock(handle);
    Some(bytes)
}

fn png_format() -> u32 {
    unsafe { RegisterClipboardFormatW(wide("PNG").as_ptr()) }
}

fn read_clipboard(window: HWND) -> ClipRead {
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) != 0 {
            return match read_clipboard_files(window) {
                Some(paths) if !paths.is_empty() => ClipRead::Files(paths),
                _ => ClipRead::Other,
            };
        }
        // Text wins over a picture: spreadsheets and documents offer both,
        // and the text is what people expect to paste.
        if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
            return match read_clipboard_text(window) {
                Some(text) if !text.is_empty() => ClipRead::Text(text),
                _ => ClipRead::Other,
            };
        }
        let png = png_format();
        if (png != 0 && IsClipboardFormatAvailable(png) != 0)
            || IsClipboardFormatAvailable(CF_DIB) != 0
            || IsClipboardFormatAvailable(CF_BITMAP) != 0
        {
            return ClipRead::Image;
        }
        ClipRead::Other
    }
}

fn read_clipboard_files(window: HWND) -> Option<Vec<PathBuf>> {
    unsafe {
        if !open_clipboard(window) {
            return None;
        }
        let handle = GetClipboardData(CF_HDROP);
        let paths = if handle.is_null() { Vec::new() } else { dropped_paths(handle) };
        CloseClipboard();
        Some(paths)
    }
}

/// The paths in a list of copied files.
pub(super) unsafe fn dropped_paths(handle: HANDLE) -> Vec<PathBuf> {
    let count = DragQueryFileW(handle, u32::MAX, null_mut(), 0);
    (0..count)
        .map(|index| {
            let len = DragQueryFileW(handle, index, null_mut(), 0) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(handle, index, buf.as_mut_ptr(), buf.len() as u32) as usize;
            PathBuf::from(OsString::from_wide(&buf[..got.min(len)]))
        })
        .collect()
}

/// A list of copied files in the layout Windows expects: a DROPFILES
/// header, then each path ending in a zero, then one more zero.
pub(super) fn drop_files(paths: &[PathBuf]) -> Vec<u8> {
    let mut list: Vec<u16> = Vec::new();
    for path in paths {
        list.extend(path.as_os_str().encode_wide());
        list.push(0);
    }
    list.push(0);
    let mut bytes = Vec::with_capacity(20 + list.len() * 2);
    bytes.extend_from_slice(&20u32.to_le_bytes()); // where the list starts
    bytes.extend_from_slice(&[0u8; 12]); // drop point, unused
    bytes.extend_from_slice(&1u32.to_le_bytes()); // wide characters
    bytes.extend_from_slice(&utf16_bytes(&list));
    bytes
}

/// The copied picture as a PNG. Safe to call from any thread.
fn read_clipboard_image() -> Option<Vec<u8>> {
    unsafe {
        let png = png_format();
        if !open_clipboard(null_mut()) {
            return None;
        }
        // Programs that offer a PNG themselves keep transparency that way.
        let mut ready = None;
        if png != 0 && IsClipboardFormatAvailable(png) != 0 {
            let handle = GetClipboardData(png);
            if !handle.is_null() {
                ready = global_bytes(handle).filter(|bytes| picture::looks_like_png(bytes));
            }
        }
        let mut raw = None;
        if ready.is_none() {
            let bitmap = GetClipboardData(CF_BITMAP);
            if !bitmap.is_null() {
                raw = bitmap_pixels(bitmap);
            }
        }
        CloseClipboard();
        // Compressing happens after the clipboard is free for others again.
        ready.or_else(|| raw.and_then(|(w, h, pixels)| picture::png_from_bgra(w, h, &pixels).ok()))
    }
}

/// Top-down BGRA pixels of a bitmap, whatever format it is stored in.
unsafe fn bitmap_pixels(bitmap: HANDLE) -> Option<(u32, u32, Vec<u8>)> {
    let mut info: BITMAP = std::mem::zeroed();
    if GetObjectW(bitmap, std::mem::size_of::<BITMAP>() as i32, &mut info as *mut BITMAP as *mut c_void) == 0 {
        return None;
    }
    let (w, h) = (info.bmWidth, info.bmHeight);
    if w <= 0 || h <= 0 || w as u64 * h as u64 > picture::MAX_PIXELS {
        return None;
    }
    let mut header: BITMAPINFO = std::mem::zeroed();
    header.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    header.bmiHeader.biWidth = w;
    header.bmiHeader.biHeight = -h; // negative: first row is the top one
    header.bmiHeader.biPlanes = 1;
    header.bmiHeader.biBitCount = 32;
    let mut pixels = vec![0u8; w as usize * h as usize * 4];
    let dc = GetDC(null_mut());
    let lines = GetDIBits(dc, bitmap, 0, h as u32, pixels.as_mut_ptr() as *mut c_void, &mut header, 0);
    ReleaseDC(null_mut(), dc);
    (lines == h).then_some((w as u32, h as u32, pixels))
}

fn open_clipboard(window: HWND) -> bool {
    // Another program may hold the clipboard for a moment; retry briefly.
    for _ in 0..10 {
        if unsafe { OpenClipboard(window) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn read_clipboard_text(window: HWND) -> Option<String> {
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 || !open_clipboard(window) {
            return None;
        }
        let mut text = None;
        let handle = GetClipboardData(CF_UNICODETEXT);
        if !handle.is_null() {
            let data = GlobalLock(handle) as *const u16;
            if !data.is_null() {
                let capacity = GlobalSize(handle) / 2;
                let mut len = 0;
                while len < capacity && *data.add(len) != 0 {
                    len += 1;
                }
                text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(data, len)));
                GlobalUnlock(handle);
            }
        }
        CloseClipboard();
        text
    }
}

fn utf16_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

fn write_clipboard(window: HWND, write: &ClipWrite) {
    unsafe {
        if !open_clipboard(window) {
            return;
        }
        EmptyClipboard();
        match write {
            ClipWrite::Text(text) => put_format(CF_UNICODETEXT, &utf16_bytes(&wide(text))),
            ClipWrite::Files(paths) => {
                put_format(CF_HDROP, &drop_files(paths));
                // Pasting copies the files rather than moving them.
                put_format(RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr()), &1u32.to_le_bytes());
            }
            ClipWrite::Image(png, dib) => {
                put_format(CF_DIB, dib);
                put_format(png_format(), png);
            }
        }
        CloseClipboard();
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_CLIPBOARDUPDATE => {
            // What this program put there itself is not news.
            if GetClipboardOwner() == window {
                return 0;
            }
            if let Some(node) = NODE.get() {
                match read_clipboard(window) {
                    ClipRead::Files(paths) => node.on_clipboard_files(paths),
                    ClipRead::Text(text) => node.on_clipboard_text(text),
                    ClipRead::Image => node.on_clipboard_image(),
                    ClipRead::Other => node.on_clipboard_cleared(),
                }
            }
            0
        }
        WM_APP_SET_CLIPBOARD => {
            let pending = PENDING_CLIPBOARD.lock().unwrap().take();
            if let Some(write) = pending {
                write_clipboard(window, &write);
            }
            0
        }
        WM_APP_TRAY => {
            match lparam as u32 & 0xFFFF {
                // Left click on the icon, or a click on its notification.
                WM_LBUTTONUP | NIN_BALLOONUSERCLICK => {
                    if let Some(tray) = TRAY.get() {
                        (tray.open_window)();
                    }
                }
                WM_RBUTTONUP => tray_menu(window),
                _ => {}
            }
            0
        }
        WM_TIMER => {
            tray_refresh(window, false);
            0
        }
        WM_APP_QUIT => {
            quit(window);
            0
        }
        // Windows is shutting down or the user is signing out: let go of
        // everything (including a silenced speaker) while there is time.
        WM_ENDSESSION => {
            if wparam != 0 {
                if let Some(node) = NODE.get() {
                    node.shutdown();
                }
            }
            0
        }
        // Explorer restarted: the notification area is new, so add the icon again.
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            tray_refresh(window, true);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

// ---------- notification-area icon ----------

/// What the tray icon needs from the application.
pub struct TrayOptions {
    /// Show the settings window.
    pub open_window: Box<dyn Fn() + Send + Sync>,
    /// True while something is waiting for the user (a pairing code).
    pub needs_attention: Box<dyn Fn() -> bool + Send + Sync>,
}

static TRAY: OnceLock<TrayOptions> = OnceLock::new();
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static ICON_ON: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static ICON_OFF: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// (tooltip, icon lit, attention) last shown, to avoid needless updates.
static TRAY_SHOWN: Mutex<Option<(String, bool, bool)>> = Mutex::new(None);

const WM_APP_TRAY: u32 = 0x8002;
const WM_APP_QUIT: u32 = 0x8003;
const WM_TIMER: u32 = 0x0113;
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIF_MESSAGE: u32 = 1;
const NIF_ICON: u32 = 2;
const NIF_TIP: u32 = 4;
const NIF_INFO: u32 = 0x10;
const NIIF_INFO: u32 = 1;
const NIN_BALLOONUSERCLICK: u32 = 0x0405;
/// The newest received file the user has been told about.
static ANNOUNCED: AtomicU32 = AtomicU32::new(0);
const MF_STRING: u32 = 0x0000;
const MF_SEPARATOR: u32 = 0x0800;
const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_NONOTIFY: u32 = 0x0080;
const TPM_RETURNCMD: u32 = 0x0100;
const SM_CXSMICON: i32 = 49;
const MENU_OPEN: usize = 1;
const MENU_PAUSE: usize = 2;
const MENU_QUIT: usize = 3;

/// Is the point inside a rounded rectangle? All in 0..1 icon coordinates.
fn in_round_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    x >= x0 && x <= x1 && y >= y0 && y <= y1 && (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// Draw the tray picture: two screens side by side, with a lit doorway
/// between them while connected. Returns premultiplied BGRA pixels.
pub fn icon_pixels(size: usize, lit: bool) -> Vec<u8> {
    const SAMPLES: usize = 4;
    let grey = [0x9A, 0x96, 0x8E];
    let teal = [0x2F, 0xC9, 0xA4];
    let dim = [0x6B, 0x67, 0x60];
    let shapes: [(f32, f32, f32, f32, f32, [u8; 3], bool); 3] = [
        (0.03, 0.14, 0.43, 0.86, 0.10, grey, true),
        (0.57, 0.14, 0.97, 0.86, 0.10, if lit { teal } else { dim }, true),
        (0.465, 0.30, 0.535, 0.70, 0.035, teal, lit),
    ];
    let mut pixels = vec![0u8; size * size * 4];
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut hits) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (px as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    for (x0, y0, x1, y1, radius, colour, shown) in shapes {
                        if shown && in_round_rect(x, y, x0, y0, x1, y1, radius) {
                            r += colour[0] as u32;
                            g += colour[1] as u32;
                            b += colour[2] as u32;
                            hits += 1;
                            break;
                        }
                    }
                }
            }
            let total = (SAMPLES * SAMPLES) as u32;
            let at = (py * size + px) * 4;
            pixels[at] = (b / total) as u8;
            pixels[at + 1] = (g / total) as u8;
            pixels[at + 2] = (r / total) as u8;
            pixels[at + 3] = (hits * 255 / total) as u8;
        }
    }
    pixels
}

unsafe fn tray_icon(lit: bool) -> HICON {
    let slot = if lit { &ICON_ON } else { &ICON_OFF };
    let existing = slot.load(Ordering::Relaxed);
    if !existing.is_null() {
        return existing;
    }
    let size = GetSystemMetrics(SM_CXSMICON).clamp(16, 64) as usize;
    let colour = icon_pixels(size, lit);
    // One bit per pixel, rows padded to 16 bits; set where fully transparent.
    let stride = (size + 15) / 16 * 2;
    let mut mask = vec![0u8; stride * size];
    for y in 0..size {
        for x in 0..size {
            if colour[(y * size + x) * 4 + 3] == 0 {
                mask[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    let icon = CreateIcon(GetModuleHandleW(null()), size as i32, size as i32, 1, 32, mask.as_ptr(), colour.as_ptr());
    slot.store(icon, Ordering::Relaxed);
    icon
}

fn tray_summary() -> (String, bool) {
    let Some(node) = NODE.get() else { return ("Screen Portals".into(), false) };
    let status = node.status();
    let up: Vec<&str> = status.peers.iter().filter(|p| p.connected).map(|p| p.name.as_str()).collect();
    let text = if status.paused {
        "Screen Portals: sharing is paused".to_string()
    } else if up.len() == 1 {
        format!("Screen Portals: connected to {}", up[0])
    } else if up.len() > 1 {
        format!("Screen Portals: connected to {} computers", up.len())
    } else if status.peers.is_empty() {
        "Screen Portals: no computers paired".to_string()
    } else {
        "Screen Portals: waiting for your computers".to_string()
    };
    (text, !up.is_empty() && !status.paused)
}

unsafe fn tray_refresh(window: HWND, add: bool) {
    let Some(tray) = TRAY.get() else { return };
    let (tip, lit) = tray_summary();
    let attention = (tray.needs_attention)();
    let mut shown = TRAY_SHOWN.lock().unwrap();
    let was_attention = shown.as_ref().map_or(false, |s| s.2);
    let current = (tip.clone(), lit, attention);
    if add || shown.as_ref() != Some(&current) {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = window;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = tray_icon(lit);
        for (slot, unit) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
            *slot = unit;
        }
        Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &data);
        *shown = Some(current);
    }
    drop(shown);
    // A pairing code is waiting: bring the window up so it is seen.
    if attention && !was_attention {
        (tray.open_window)();
    }
    tray_announce(window);
}

/// Files that were sent here land in a folder without any window opening,
/// so say so once with a notification.
unsafe fn tray_announce(window: HWND) {
    let Some(node) = NODE.get() else { return };
    let arrived = node
        .transfers()
        .into_iter()
        .find(|t| t.incoming && t.kind == TransferKind::Sent && t.state == TransferState::Done);
    let Some(arrived) = arrived else { return };
    if arrived.id <= ANNOUNCED.load(Ordering::Relaxed) {
        return;
    }
    ANNOUNCED.store(arrived.id, Ordering::Relaxed);
    let title = format!("Received from {}", arrived.peer);
    let what = if arrived.files > 1 { format!("{} and {} more", arrived.label, arrived.files - 1) } else { arrived.label.clone() };
    let text = format!("{what}\nSaved in {}", node.received_dir().display());
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = 1;
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_INFO;
    for (slot, unit) in data.szInfoTitle.iter_mut().zip(title.encode_utf16().take(63)) {
        *slot = unit;
    }
    for (slot, unit) in data.szInfo.iter_mut().zip(text.encode_utf16().take(255)) {
        *slot = unit;
    }
    Shell_NotifyIconW(NIM_MODIFY, &data);
}

unsafe fn tray_menu(window: HWND) {
    let (Some(node), Some(tray)) = (NODE.get(), TRAY.get()) else { return };
    let paused = node.is_paused();
    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, MENU_OPEN, wide("Open Screen Portals").as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_PAUSE, wide(if paused { "Resume sharing" } else { "Pause sharing" }).as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, MENU_QUIT, wide("Quit Screen Portals").as_ptr());
    let mut at = POINT { x: 0, y: 0 };
    GetCursorPos(&mut at);
    // Without this the menu would not close when clicking elsewhere.
    SetForegroundWindow(window);
    let choice = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, at.x, at.y, 0, window, null());
    DestroyMenu(menu);
    match choice as usize {
        MENU_OPEN => (tray.open_window)(),
        MENU_PAUSE => {
            node.set_paused(!paused);
            tray_refresh(window, false);
        }
        MENU_QUIT => quit(window),
        _ => {}
    }
}

unsafe fn quit(window: HWND) {
    if let Some(node) = NODE.get() {
        node.shutdown();
    }
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = 1;
    Shell_NotifyIconW(NIM_DELETE, &data);
    PostQuitMessage(0);
}

/// Ask the running application to exit cleanly. Safe to call from any thread.
pub fn request_quit() {
    let window = WINDOW.load(Ordering::Relaxed);
    if !window.is_null() {
        unsafe {
            PostMessageW(window, WM_APP_QUIT, 0, 0);
        }
    }
}

/// Show the settings page in its own window (Edge's app mode: no tabs or
/// address bar). Falls back to the default browser.
pub fn open_app_window(url: &str) {
    for base in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        let Some(root) = std::env::var_os(base) else { continue };
        let exe = std::path::Path::new(&root).join("Microsoft").join("Edge").join("Application").join("msedge.exe");
        if exe.is_file() {
            let started = std::process::Command::new(exe).arg(format!("--app={url}")).arg("--window-size=1120,760").spawn();
            if started.is_ok() {
                return;
            }
        }
    }
    unsafe {
        ShellExecuteW(null_mut(), wide("open").as_ptr(), wide(url).as_ptr(), null(), null(), 1);
    }
}

/// True if another copy of the application already owns this session.
pub fn already_running() -> bool {
    unsafe {
        // The handle is kept for the life of the process on purpose.
        CreateMutexW(null(), 0, wide("Local\\ScreenPortals.SingleInstance").as_ptr());
        GetLastError() == 183 // ERROR_ALREADY_EXISTS
    }
}

/// After an update the new copy starts while the old one is still closing.
/// Wait for the old one to go; returns false if it never does.
pub fn wait_for_previous_copy(patience: Duration) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    let deadline = std::time::Instant::now() + patience;
    loop {
        unsafe {
            let handle = CreateMutexW(null(), 0, wide("Local\\ScreenPortals.SingleInstance").as_ptr());
            let taken = GetLastError() == 183;
            // Let go again either way, so that only `already_running`
            // decides who owns the name.
            if !handle.is_null() {
                CloseHandle(handle);
            }
            if !taken {
                return true;
            }
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Download one address from github.com over HTTPS, refusing anything
/// larger than `limit` bytes. Uses the copy of curl that ships with
/// Windows, named by its full path so nothing else can stand in for it.
pub fn fetch_https(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    use std::os::windows::process::CommandExt;
    if !url.starts_with("https://github.com/") {
        return Err("updates only come from github.com".into());
    }
    let root = std::env::var_os("SystemRoot").ok_or("cannot find Windows")?;
    let curl = Path::new(&root).join("System32").join("curl.exe");
    let output = std::process::Command::new(curl)
        .args(["--silent", "--show-error", "--fail", "--location", "--proto", "=https", "--proto-redir", "=https"])
        .args(["--max-time", "300", "--max-filesize", &limit.to_string(), "--output", "-", url])
        .creation_flags(0x0800_0000) // no console window flashing up
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

/// For a windowed program with no console: tell the user why it stopped.
pub fn error_box(text: &str) {
    unsafe {
        MessageBoxW(null_mut(), wide(text).as_ptr(), wide("Screen Portals").as_ptr(), 0x10);
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        // Events we (or anything else) injected are never captured, which is
        // what stops two computers from echoing input back and forth.
        if info.flags & LLMHF_INJECTED == 0 {
            if let Some(node) = NODE.get() {
                let high = ((info.mouseData >> 16) & 0xFFFF) as u16;
                let x_button = if high == 2 { BUTTON_X2 } else { BUTTON_X1 };
                let swallow = match wparam as u32 {
                    WM_MOUSEMOVE => node.on_mouse_move(info.pt.x, info.pt.y),
                    WM_LBUTTONDOWN => node.on_button(BUTTON_LEFT, true),
                    WM_LBUTTONUP => node.on_button(BUTTON_LEFT, false),
                    WM_RBUTTONDOWN => node.on_button(BUTTON_RIGHT, true),
                    WM_RBUTTONUP => node.on_button(BUTTON_RIGHT, false),
                    WM_MBUTTONDOWN => node.on_button(BUTTON_MIDDLE, true),
                    WM_MBUTTONUP => node.on_button(BUTTON_MIDDLE, false),
                    WM_XBUTTONDOWN => node.on_button(x_button, true),
                    WM_XBUTTONUP => node.on_button(x_button, false),
                    WM_MOUSEWHEEL => node.on_wheel(0, high as i16),
                    WM_MOUSEHWHEEL => node.on_wheel(high as i16, 0),
                    _ => false,
                };
                if swallow {
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        if info.flags & LLKHF_INJECTED == 0 {
            if let Some(node) = NODE.get() {
                let down = info.flags & LLKHF_UP == 0;
                let extended = info.flags & LLKHF_EXTENDED != 0;
                let swallow = match keymap::from_windows(info.vkCode, info.scanCode, extended) {
                    Some(hid) => node.on_key(hid, down),
                    // A key we cannot translate still must not leak into this
                    // computer while the cursor is on the other one.
                    None => node.is_remote(),
                };
                if swallow {
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(null_mut(), code, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::icon_pixels;

    fn pixel(pixels: &[u8], size: usize, x: usize, y: usize) -> [u8; 4] {
        let at = (y * size + x) * 4;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    }

    #[test]
    fn tray_picture_is_two_screens_with_a_doorway_when_lit() {
        for size in [16, 20, 24, 32] {
            let lit = icon_pixels(size, true);
            let dark = icon_pixels(size, false);
            assert_eq!(lit.len(), size * size * 4);
            // Corners and the top row are empty.
            assert_eq!(pixel(&lit, size, 0, 0)[3], 0);
            assert_eq!(pixel(&lit, size, size - 1, size - 1)[3], 0);
            // Middle of the left screen is solid grey in both.
            let (lx, rx, mid) = (size / 4, size * 3 / 4, size / 2);
            assert_eq!(pixel(&lit, size, lx, mid), [0x8E, 0x96, 0x9A, 255]);
            assert_eq!(pixel(&dark, size, lx, mid), [0x8E, 0x96, 0x9A, 255]);
            // The right screen is teal only when connected (pixels are B, G, R, A).
            assert_eq!(pixel(&lit, size, rx, mid), [0xA4, 0xC9, 0x2F, 255]);
            assert_eq!(pixel(&dark, size, rx, mid), [0x60, 0x67, 0x6B, 255]);
            // Colour never exceeds alpha (premultiplied), so edges cannot glow.
            for p in lit.chunks(4).chain(dark.chunks(4)) {
                assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
            }
        }
    }

    /// Checks the two layouts we build by hand against Windows itself,
    /// without touching the clipboard.
    #[test]
    fn windows_understands_our_file_lists_and_bitmaps() {
        use super::*;
        use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBitmap, DeleteObject};
        unsafe {
            // A list of copied files, read back by the system's own parser.
            let paths = vec![
                PathBuf::from(r"C:\Users\someone\one file.txt"),
                PathBuf::from("D:\\caf\u{e9} \u{65e5}\u{672c}\\data.bin"),
                PathBuf::from(r"\\server\share\folder"),
            ];
            let bytes = drop_files(&paths);
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            assert!(!handle.is_null());
            let target = GlobalLock(handle) as *mut u8;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
            GlobalUnlock(handle);
            assert_eq!(dropped_paths(handle), paths);
            GlobalFree(handle);

            // A bitmap in whatever format: we always get top-down BGRA back.
            let bgra: [u8; 24] = [
                0, 0, 255, 0, /**/ 0, 255, 0, 0, /**/ 255, 0, 0, 0, //
                10, 20, 30, 0, /**/ 128, 128, 128, 0, /**/ 250, 240, 230, 0,
            ];
            let plain = CreateBitmap(3, 2, 1, 32, bgra.as_ptr() as *const c_void);
            assert!(!plain.is_null());
            let (w, h, pixels) = bitmap_pixels(plain).expect("pixels of a plain bitmap");
            DeleteObject(plain);
            assert_eq!((w, h), (3, 2));
            let colours = |p: &[u8]| p.chunks(4).map(|px| [px[0], px[1], px[2]]).collect::<Vec<_>>();
            assert_eq!(colours(&pixels), colours(&bgra));

            // The bitmap we place on the clipboard for a received picture,
            // turned into a real bitmap by Windows the way a pasting program
            // would, must show the same picture.
            let png = picture::png_from_bgra(3, 2, &bgra).unwrap();
            let dib = picture::dib_from_png(&png).unwrap();
            let dc = GetDC(null_mut());
            let made = CreateDIBitmap(
                dc,
                dib.as_ptr() as *const BITMAPINFOHEADER,
                4, // CBM_INIT: fill it from the pixels that follow
                dib.as_ptr().add(40) as *const c_void,
                dib.as_ptr() as *const BITMAPINFO,
                0,
            );
            ReleaseDC(null_mut(), dc);
            assert!(!made.is_null(), "Windows accepted our bitmap");
            let (w, h, pixels) = bitmap_pixels(made).expect("pixels of our bitmap");
            DeleteObject(made);
            assert_eq!((w, h), (3, 2));
            assert_eq!(colours(&pixels), colours(&bgra));
        }
    }

    /// Puts back whatever text was copied before the test ran.
    struct Restore(super::HWND, Option<String>);

    impl Drop for Restore {
        fn drop(&mut self) {
            use super::*;
            match self.1.take() {
                Some(text) => write_clipboard(self.0, &ClipWrite::Text(text)),
                None => unsafe {
                    if open_clipboard(self.0) {
                        EmptyClipboard();
                        CloseClipboard();
                    }
                },
            }
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
            }
        }
    }

    /// Uses the real Windows clipboard, so it only runs when asked for:
    ///
    ///     cargo test --lib -- --ignored real_clipboard
    ///
    /// Copied text is put back afterwards. If files or a picture are on the
    /// clipboard the test leaves them alone and checks nothing.
    #[test]
    #[ignore]
    fn real_clipboard_holds_files_and_pictures() {
        use super::*;
        unsafe {
            if [CF_HDROP, CF_DIB, CF_BITMAP].iter().any(|format| IsClipboardFormatAvailable(*format) != 0) {
                eprintln!("skipped: the clipboard holds files or a picture");
                return;
            }
            let window =
                CreateWindowExW(0, wide("STATIC").as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), GetModuleHandleW(null()), null());
            assert!(!window.is_null());
            let _restore = Restore(window, read_clipboard_text(window));

            // Files, including a name with a space and one outside ASCII.
            let dir = std::env::temp_dir().join(format!("screen-portals-clipboard-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let paths = vec![dir.join("one file.txt"), dir.join("caf\u{e9} \u{65e5}\u{672c}.bin")];
            for path in &paths {
                std::fs::write(path, "x").unwrap();
            }
            write_clipboard(window, &ClipWrite::Files(paths.clone()));
            assert_eq!(GetClipboardOwner(), window, "our own writes can be told apart");
            match read_clipboard(window) {
                ClipRead::Files(read) => assert_eq!(read, paths),
                _ => panic!("expected files on the clipboard"),
            }

            // A picture offered the way this program offers it: PNG and bitmap.
            let bgra = [
                0, 0, 255, 0, /**/ 0, 255, 0, 0, /**/ 255, 0, 0, 0, //
                10, 20, 30, 0, /**/ 128, 128, 128, 0, /**/ 250, 240, 230, 0,
            ];
            let png = picture::png_from_bgra(3, 2, &bgra).unwrap();
            let dib = picture::dib_from_png(&png).unwrap();
            write_clipboard(window, &ClipWrite::Image(png.clone(), dib.clone()));
            assert!(matches!(read_clipboard(window), ClipRead::Image));
            assert_eq!(read_clipboard_image(), Some(png.clone()));

            // A picture offered the way most programs do: only as a bitmap.
            // Windows converts it and our reading must give the same pixels.
            assert!(open_clipboard(window));
            EmptyClipboard();
            put_format(CF_DIB, &dib);
            CloseClipboard();
            assert!(matches!(read_clipboard(window), ClipRead::Image));
            let read = picture::decode_png(&read_clipboard_image().expect("a picture")).unwrap();
            let want = picture::decode_png(&png).unwrap();
            assert_eq!((read.width, read.height), (3, 2));
            assert_eq!(read.pixels, want.pixels);

            // Text still works through the shared writing path.
            write_clipboard(window, &ClipWrite::Text("portal".into()));
            assert!(matches!(read_clipboard(window), ClipRead::Text(text) if text == "portal"));

            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Results of a self-test that changes nothing on the machine.
pub struct Diagnosis {
    pub desktop: Rect,
    pub main_monitor: (i32, i32),
    pub cursor: (i32, i32),
    pub mouse_hook: bool,
    pub keyboard_hook: bool,
    pub clipboard_readable: bool,
}

/// Check that this program is allowed to do what it needs. Hooks are
/// installed and removed again immediately.
pub fn diagnose() -> Diagnosis {
    let platform = WinPlatform;
    unsafe {
        let module = GetModuleHandleW(null());
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
        let result = Diagnosis {
            desktop: platform.screen(),
            main_monitor: (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)),
            cursor: platform.cursor(),
            mouse_hook: !mouse.is_null(),
            keyboard_hook: !keyboard.is_null(),
            clipboard_readable: {
                let opened = open_clipboard(null_mut());
                if opened {
                    CloseClipboard();
                }
                opened
            },
        };
        if !mouse.is_null() {
            UnhookWindowsHookEx(mouse);
        }
        if !keyboard.is_null() {
            UnhookWindowsHookEx(keyboard);
        }
        result
    }
}

/// Briefly move the real cursor to a few spots the way a remote computer
/// would, measure where it lands, then put it back. Returns the largest
/// error in pixels.
pub fn pointer_self_test() -> i32 {
    let platform = WinPlatform;
    let screen = platform.screen();
    let original = platform.cursor();
    let mut worst = 0;
    for (fx, fy) in [(0.25, 0.25), (0.75, 0.5), (0.5, 0.9), (0.0, 0.0), (1.0, 1.0)] {
        let want = (
            screen.x + ((screen.w - 1) as f64 * fx).round() as i32,
            screen.y + ((screen.h - 1) as f64 * fy).round() as i32,
        );
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
    worst
}

/// Run with an icon in the notification area (the windowed application).
pub fn run_with_tray(node: Arc<Node>, tray: TrayOptions) -> Result<(), String> {
    if TRAY.set(tray).is_err() {
        return Err("already running".into());
    }
    run(node)
}

/// Install the hooks and process events until the program exits. Must be
/// called on the thread that will stay for the lifetime of the program.
pub fn run(node: Arc<Node>) -> Result<(), String> {
    if NODE.set(node).is_err() {
        return Err("already running".into());
    }
    let with_tray = TRAY.get().is_some();
    unsafe {
        let module = GetModuleHandleW(null());
        let class_name = wide("ScreenPortalsMessageWindow");
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            return Err("could not register the window class".into());
        }
        // The tray needs an ordinary (never shown) window, because
        // message-only windows miss the "taskbar was recreated" broadcast.
        let (ex_style, style, parent) = if with_tray { (0x80, 0x8000_0000, null_mut()) } else { (0, 0, HWND_MESSAGE) };
        let window = CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            class_name.as_ptr(),
            style,
            0,
            0,
            0,
            0,
            parent,
            null_mut(),
            module,
            null(),
        );
        if window.is_null() {
            return Err("could not create the message window".into());
        }
        WINDOW.store(window, Ordering::Relaxed);
        // Ready before the hooks can ask for it.
        super::win_drag::start();
        if AddClipboardFormatListener(window) == 0 {
            eprintln!("[screen-portals] clipboard sharing is unavailable on this session");
        }
        if with_tray {
            TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
            tray_refresh(window, true);
            SetTimer(window, 1, 1000, None);
        }

        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
        if mouse.is_null() {
            return Err("Windows refused the mouse hook".into());
        }
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
        if keyboard.is_null() {
            UnhookWindowsHookEx(mouse);
            return Err("Windows refused the keyboard hook".into());
        }

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        UnhookWindowsHookEx(keyboard);
        UnhookWindowsHookEx(mouse);
    }
    Ok(())
}

//! Windows drag and drop: taking over whatever is being dragged when the
//! cursor reaches the edge, and starting an equivalent drag on arrival.
//!
//! A drag belongs to the program it started in, so it cannot literally
//! leave the computer. Instead:
//!
//! * **Catching.** A tiny, nearly invisible window is placed under the
//!   cursor and registered as a place things can be dropped. If a drag is in
//!   progress Windows offers it to that window; we accept it as a copy and
//!   let the button go, which hands us the dragged files, link or text.
//! * **Beginning.** On the other computer the same tiny window is placed
//!   under the cursor, the button is pressed on it, and from that press we
//!   start a real drag carrying what was caught. The user, still holding
//!   the button, simply sees the drag continue.
//!
//! All of this runs on its own thread with its own message loop, because
//! drag and drop is tied to a thread and must never hold up the input hooks.

use super::windows as host;
use super::{Caught, DragPayload};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{implement, Ref, BOOL, HRESULT};
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_NOTIMPL, HGLOBAL,
    HWND as OleHwnd, OLE_E_ADVISENOTSUPPORTED, POINTL, S_OK,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, FORMATETC, STGMEDIUM, STGMEDIUM_0,
};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc;
use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEINPUT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, PostMessageW, RegisterClassW,
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, TranslateMessage, MSG, WNDCLASSW,
};

const CF_UNICODETEXT: u16 = 13;
const CF_HDROP: u16 = 15;
const TYMED_HGLOBAL: u32 = 1;
const MK_LBUTTON: u32 = 1;
const WM_TIMER: u32 = 0x0113;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_MOUSEACTIVATE: u32 = 0x0021;
const MA_NOACTIVATE: isize = 3;
const WM_APP_COMMAND: u32 = 0x8001;
const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
/// Side of the square helper window, in pixels.
const PAD: i32 = 96;
/// How long a drag has to show itself once the helper window is in place.
const CATCH_WAIT: Duration = Duration::from_millis(250);
/// How long the pressed button has to reach the helper window.
const BEGIN_WAIT: Duration = Duration::from_millis(400);
/// Dragged text larger than this is left behind.
const MAX_TEXT: usize = 1024 * 1024;

enum Cmd {
    Catch(u64),
    Begin(DragPayload),
    Cancel,
}

/// What the helper window is in the middle of.
enum Doing {
    Idle,
    /// Waiting for a drag to be offered. `seen` is set once one has been.
    Catching { token: u64, until: Instant, seen: bool },
    /// Waiting for the pressed button to arrive so a drag can start.
    Beginning { payload: DragPayload, until: Instant },
    /// A browser window is held against the edge. Its page is only sent if
    /// it is still held there, unmoved, when the time is up: snapping a
    /// window to the edge of the screen must not count.
    Lingering { token: u64, until: Instant, at: (i32, i32) },
}

/// Where a requested drag has got to before its button press exists.
#[derive(PartialEq)]
enum Press {
    Nothing,
    /// Asked for, but the press has not been made yet.
    Queued,
    /// The button was released before the press was made: skip the press.
    Abandoned,
}

static WINDOW: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static QUEUE: Mutex<VecDeque<Cmd>> = Mutex::new(VecDeque::new());
static DOING: Mutex<Doing> = Mutex::new(Doing::Idle);
static CANCEL: AtomicBool = AtomicBool::new(false);
static PRESS: Mutex<Press> = Mutex::new(Press::Nothing);
/// How long a browser window must be held against the edge.
const LINGER: Duration = Duration::from_millis(1200);
/// A drag we started has just begun and the helper window is still in its way.
static IN_THE_WAY: AtomicBool = AtomicBool::new(false);
static STARTED: OnceLock<()> = OnceLock::new();
/// During the self-test, results go here instead of to the engine.
static TAP: Mutex<Option<mpsc::Sender<(u64, Caught)>>> = Mutex::new(None);

/// Step-by-step notes on standard error when SCREEN_PORTALS_TRACE is set.
fn trace(what: &str) {
    if std::env::var_os("SCREEN_PORTALS_TRACE").is_some() {
        eprintln!("[drag] {what}");
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn utf16_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

fn format_id(name: &str) -> u16 {
    unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) as u16 }
}

/// Start the drag thread. Done once when the program starts, so that the
/// first drag does not have to wait for it inside the mouse hook.
pub fn start() {
    STARTED.get_or_init(|| {
        let (ready, wait) = mpsc::channel();
        std::thread::spawn(move || run(ready));
        let _ = wait.recv_timeout(Duration::from_secs(3));
    });
}

fn post(cmd: Cmd) {
    start();
    QUEUE.lock().unwrap().push_back(cmd);
    let window = WINDOW.load(Ordering::Relaxed);
    if !window.is_null() {
        unsafe {
            PostMessageW(window, WM_APP_COMMAND, 0, 0);
        }
    }
}

/// See whether something is being dragged at the cursor and, if so, take
/// it. The answer arrives through `Node::on_drag_caught` with this token.
pub fn catch(token: u64) {
    post(Cmd::Catch(token));
}

/// Start dragging `payload` from where the cursor is, as if the left button
/// had just been pressed on it. The drag ends when the button is released.
pub fn begin(payload: DragPayload) {
    CANCEL.store(false, Ordering::Relaxed);
    *PRESS.lock().unwrap() = Press::Queued;
    post(Cmd::Begin(payload));
}

/// The left button is about to be released on behalf of another computer.
/// If a drag was asked for but its press has not been made yet, the press
/// is called off and this returns true: the release must then be skipped
/// too, or it would arrive before the press and leave the button stuck down.
pub fn release_before_press() -> bool {
    let mut press = PRESS.lock().unwrap();
    if *press == Press::Queued {
        *press = Press::Abandoned;
        true
    } else {
        false
    }
}

/// Abandon a drag started with `begin` without dropping it anywhere.
pub fn cancel() {
    CANCEL.store(true, Ordering::Relaxed);
    post(Cmd::Cancel);
}

fn deliver(token: u64, caught: Caught) {
    if let Some(tap) = TAP.lock().unwrap().as_ref() {
        let _ = tap.send((token, caught));
        return;
    }
    if let Some(node) = host::node() {
        node.on_drag_caught(token, caught);
    }
}

fn mouse(flags: u32, dx: i32, dy: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Put the helper window under the cursor (or take it away again).
unsafe fn place(window: HWND, show: bool) {
    if !show {
        ShowWindow(window, 0);
        return;
    }
    let mut at = POINT { x: 0, y: 0 };
    GetCursorPos(&mut at);
    // HWND_TOPMOST, without taking the keyboard focus from anything.
    SetWindowPos(window, -1isize as HWND, at.x - PAD / 2, at.y - PAD / 2, PAD, PAD, 0x0010 | 0x0040);
}

// ---------- what a drag carries ----------

unsafe fn global_from(bytes: &[u8]) -> Option<HGLOBAL> {
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
    if handle.is_null() {
        return None;
    }
    let target = GlobalLock(handle) as *mut u8;
    if target.is_null() {
        GlobalFree(handle);
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
    GlobalUnlock(handle);
    Some(HGLOBAL(handle))
}

fn format_of(id: u16) -> FORMATETC {
    FORMATETC { cfFormat: id, ptd: null_mut(), dwAspect: 1, lindex: -1, tymed: TYMED_HGLOBAL }
}

/// The data offered by a drag we start: a fixed list of formats, each a
/// block of bytes.
#[implement(IDataObject)]
struct Carried {
    formats: Vec<(u16, Vec<u8>)>,
}

impl Carried {
    fn new(payload: &DragPayload) -> Carried {
        let text = |s: &str| utf16_bytes(&wide(s));
        let formats = match payload {
            DragPayload::Files(paths) => vec![
                (CF_HDROP, host::drop_files(paths)),
                // Dropping copies; the staged originals are ours to tidy up.
                (format_id("Preferred DropEffect"), 1u32.to_le_bytes().to_vec()),
            ],
            DragPayload::Text(s) => vec![(CF_UNICODETEXT, text(s))],
            DragPayload::Url(url) => vec![
                (format_id("UniformResourceLocatorW"), text(url)),
                (format_id("UniformResourceLocator"), url.bytes().chain(std::iter::once(0)).collect()),
                (CF_UNICODETEXT, text(url)),
            ],
        };
        Carried { formats }
    }

    fn find(&self, format: *const FORMATETC) -> Option<&Vec<u8>> {
        let wanted = unsafe { format.as_ref()? };
        if wanted.tymed & TYMED_HGLOBAL == 0 {
            return None;
        }
        self.formats.iter().find(|(id, _)| *id == wanted.cfFormat).map(|(_, bytes)| bytes)
    }
}

impl IDataObject_Impl for Carried_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let bytes = self.find(format).ok_or(windows::core::Error::from(DV_E_FORMATETC))?;
        let global = unsafe { global_from(bytes) }.ok_or(windows::core::Error::from(DV_E_FORMATETC))?;
        Ok(STGMEDIUM { tymed: TYMED_HGLOBAL, u: STGMEDIUM_0 { hGlobal: global }, pUnkForRelease: std::mem::ManuallyDrop::new(None) })
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        if self.find(format).is_some() {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    // Programs try to attach drag pictures and the like; we carry none.
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != 1 {
            return Err(E_NOTIMPL.into());
        }
        let list: Vec<FORMATETC> = self.formats.iter().map(|(id, _)| format_of(*id)).collect();
        unsafe { SHCreateStdEnumFmtEtc(&list) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Decides when a drag we started ends: on release, or when cancelled.
#[implement(IDropSource)]
struct Holding;

impl IDropSource_Impl for Holding_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        // The helper window has done its job; get it out of the way (once)
        // so the real windows underneath can be dropped on. It may come
        // back later to catch this same drag at the edge.
        if IN_THE_WAY.swap(false, Ordering::Relaxed) {
            unsafe {
                ShowWindow(WINDOW.load(Ordering::Relaxed), 0);
            }
        }
        if escape.as_bool() || CANCEL.load(Ordering::Relaxed) {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// One format of a foreign drag, as bytes.
unsafe fn read_format(data: &IDataObject, id: u16) -> Option<Vec<u8>> {
    let mut medium = data.GetData(&format_of(id)).ok()?;
    let handle = medium.u.hGlobal.0;
    let mut bytes = None;
    let locked = GlobalLock(handle) as *const u8;
    if !locked.is_null() {
        bytes = Some(std::slice::from_raw_parts(locked, GlobalSize(handle)).to_vec());
        GlobalUnlock(handle);
    }
    ReleaseStgMedium(&mut medium);
    bytes
}

fn text_from_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).take_while(|u| *u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// What a drag from another program carries, in the forms we can move:
/// files first, then a link, then plain text.
unsafe fn read_payload(data: &IDataObject) -> Option<DragPayload> {
    if let Ok(mut medium) = data.GetData(&format_of(CF_HDROP)) {
        let paths: Vec<PathBuf> = host::dropped_paths(medium.u.hGlobal.0);
        ReleaseStgMedium(&mut medium);
        if !paths.is_empty() {
            return Some(DragPayload::Files(paths));
        }
    }
    if let Some(bytes) = read_format(data, format_id("UniformResourceLocatorW")) {
        if let Some(url) = crate::transfer::safe_url(&text_from_utf16(&bytes)) {
            return Some(DragPayload::Url(url));
        }
    }
    let bytes = read_format(data, CF_UNICODETEXT)?;
    if bytes.len() > MAX_TEXT * 2 {
        return None;
    }
    let text = text_from_utf16(&bytes);
    match crate::transfer::safe_url(text.trim()) {
        Some(url) => Some(DragPayload::Url(url)),
        None if text.is_empty() => None,
        None => Some(DragPayload::Text(text)),
    }
}

/// The helper window as a place to drop things.
#[implement(IDropTarget)]
struct Catcher;

impl Catcher_Impl {
    /// A copy if the dragging program allows one, otherwise a link. Never a
    /// move: that would delete the originals from under the user.
    fn effect(allowed: *mut DROPEFFECT) -> DROPEFFECT {
        let offered = unsafe { allowed.as_ref().copied().unwrap_or(DROPEFFECT_NONE) };
        if offered.0 & DROPEFFECT_COPY.0 != 0 {
            DROPEFFECT_COPY
        } else if offered.0 & DROPEFFECT_LINK.0 != 0 {
            DROPEFFECT_LINK
        } else {
            DROPEFFECT_NONE
        }
    }
}

impl IDropTarget_Impl for Catcher_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        trace("a drag entered the helper window");
        let mut doing = DOING.lock().unwrap();
        let wanted = matches!(*doing, Doing::Catching { seen: false, .. });
        let usable = wanted && data.as_ref().map_or(false, |d| unsafe { read_payload(d) }.is_some());
        let chosen = if usable { Self::effect(effect) } else { DROPEFFECT_NONE };
        if let Some(effect) = unsafe { effect.as_mut() } {
            *effect = chosen;
        }
        if chosen != DROPEFFECT_NONE {
            if let Doing::Catching { seen, .. } = &mut *doing {
                *seen = true;
            }
            // Letting the button go over our window is what hands it to us.
            mouse(MOUSEEVENTF_LEFTUP, 0, 0);
        }
        Ok(())
    }

    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let catching = matches!(*DOING.lock().unwrap(), Doing::Catching { seen: true, .. });
        let chosen = if catching { Self::effect(effect) } else { DROPEFFECT_NONE };
        if let Some(effect) = unsafe { effect.as_mut() } {
            *effect = chosen;
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        trace("the drag was dropped on the helper window");
        let chosen = Self::effect(effect);
        if let Some(effect) = unsafe { effect.as_mut() } {
            *effect = chosen;
        }
        let was = std::mem::replace(&mut *DOING.lock().unwrap(), Doing::Idle);
        let token = match was {
            Doing::Catching { token, .. } => token,
            other => {
                *DOING.lock().unwrap() = other;
                return Ok(());
            }
        };
        let window = WINDOW.load(Ordering::Relaxed);
        unsafe {
            place(window, false);
        }
        let payload = data.as_ref().and_then(|d| unsafe { read_payload(d) });
        deliver(token, payload.map_or(Caught::Nothing, Caught::Data));
        Ok(())
    }
}

// ---------- the helper window and its thread ----------

unsafe fn step(window: HWND) {
    let next = QUEUE.lock().unwrap().pop_front();
    let Some(cmd) = next else { return };
    match cmd {
        Cmd::Catch(token) => {
            let busy = !matches!(*DOING.lock().unwrap(), Doing::Idle);
            if busy {
                deliver(token, Caught::Nothing);
                return;
            }
            trace("looking for a drag");
            *DOING.lock().unwrap() = Doing::Catching { token, until: Instant::now() + CATCH_WAIT, seen: false };
            place(window, true);
            // A drag only looks at what is under the cursor when the mouse
            // moves, so give it the smallest possible nudge.
            mouse(MOUSEEVENTF_MOVE, 0, 0);
            SetTimer(window, 1, 30, None);
        }
        Cmd::Begin(payload) => {
            // Held until the press has been made, so a release can never
            // slip in between the decision and the press.
            let mut press = PRESS.lock().unwrap();
            let wanted = std::mem::replace(&mut *press, Press::Nothing) == Press::Queued;
            if !wanted || !matches!(*DOING.lock().unwrap(), Doing::Idle) {
                return;
            }
            trace("pressing the button on the helper window");
            *DOING.lock().unwrap() = Doing::Beginning { payload, until: Instant::now() + BEGIN_WAIT };
            place(window, true);
            mouse(MOUSEEVENTF_LEFTDOWN, 0, 0);
            drop(press);
            SetTimer(window, 1, 30, None);
        }
        // The flag is what stops a drag; nothing else to do here.
        Cmd::Cancel => {}
    }
}

/// Give up on whatever did not happen in time.
unsafe fn tick(window: HWND) {
    let mut doing = DOING.lock().unwrap();
    let expired = match &*doing {
        Doing::Catching { until, seen: false, .. } | Doing::Beginning { until, .. } | Doing::Lingering { until, .. } => {
            Instant::now() > *until
        }
        // A drag that was offered gets a little longer to be handed over.
        Doing::Catching { until, seen: true, .. } => Instant::now() > *until + Duration::from_millis(600),
        _ => false,
    };
    if !expired {
        if matches!(*doing, Doing::Idle) {
            KillTimer(window, 1);
        }
        return;
    }
    let was = std::mem::replace(&mut *doing, Doing::Idle);
    drop(doing);
    trace("gave up waiting");
    KillTimer(window, 1);
    place(window, false);
    match was {
        // Nothing was being dragged in the usual sense. It may still be a
        // browser window held by its title bar: give it time to show that
        // it is being held here on purpose.
        Doing::Catching { token, .. } => {
            if super::win_page::browser_being_moved() {
                let mut at = POINT { x: 0, y: 0 };
                GetCursorPos(&mut at);
                *DOING.lock().unwrap() = Doing::Lingering { token, until: Instant::now() + LINGER, at: (at.x, at.y) };
                SetTimer(window, 1, 100, None);
            } else {
                deliver(token, Caught::Nothing);
            }
        }
        Doing::Lingering { token, at, .. } => {
            let mut now = POINT { x: 0, y: 0 };
            GetCursorPos(&mut now);
            let held_still = (now.x - at.0).abs() <= 12 && (now.y - at.1).abs() <= 12;
            let page = if held_still { host::page_being_moved() } else { None };
            deliver(token, page.map_or(Caught::Nothing, Caught::Page));
        }
        // The press never reached us; balance it so no button stays down.
        Doing::Beginning { .. } => mouse(MOUSEEVENTF_LEFTUP, 0, 0),
        _ => {}
    }
}

unsafe fn start_drag(window: HWND) {
    let was = std::mem::replace(&mut *DOING.lock().unwrap(), Doing::Idle);
    let payload = match was {
        Doing::Beginning { payload, .. } => payload,
        other => {
            *DOING.lock().unwrap() = other;
            return;
        }
    };
    trace("the press arrived; starting a drag");
    KillTimer(window, 1);
    IN_THE_WAY.store(true, Ordering::Relaxed);
    let data: IDataObject = Carried::new(&payload).into();
    let source: IDropSource = Holding.into();
    let mut effect = DROPEFFECT_NONE;
    // Runs until the button is released or the drag is cancelled, keeping
    // this thread's messages flowing meanwhile.
    let _ = DoDragDrop(&data, &source, DROPEFFECT_COPY | DROPEFFECT_LINK, &mut effect);
    trace(&format!("our drag ended with effect {}", effect.0));
    IN_THE_WAY.store(false, Ordering::Relaxed);
    if matches!(*DOING.lock().unwrap(), Doing::Idle) {
        place(window, false);
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_APP_COMMAND => {
            step(window);
            0
        }
        WM_TIMER => {
            tick(window);
            0
        }
        WM_LBUTTONDOWN => {
            start_drag(window);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

fn run(ready: mpsc::Sender<()>) {
    unsafe {
        let _ = OleInitialize(None);
        let module = GetModuleHandleW(null());
        let class_name = wide("ScreenPortalsDragWindow");
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
        RegisterClassW(&class);
        // Always on top, never focused, absent from the taskbar, and so
        // faint it cannot be seen: it only has to be under the cursor.
        let window = CreateWindowExW(
            0x0008 | 0x0080 | 0x0800_0000 | 0x0008_0000,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0x8000_0000,
            0,
            0,
            PAD,
            PAD,
            null_mut(),
            null_mut(),
            module,
            null(),
        );
        if window.is_null() {
            let _ = ready.send(());
            return;
        }
        SetLayeredWindowAttributes(window, 0, 1, 0x2);
        let target: IDropTarget = Catcher.into();
        let _ = RegisterDragDrop(OleHwnd(window), &target);
        WINDOW.store(window, Ordering::Relaxed);
        let _ = ready.send(());
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Check the whole path on this computer: start a drag the way an arriving
/// one is started, then catch it the way a leaving one is caught, and see
/// that the same thing comes out. Presses and releases the left mouse
/// button where the cursor is, on our own invisible window.
pub fn self_test() -> Result<String, String> {
    // Games that lock the mouse, and programs running as administrator,
    // stop simulated input from reaching anything. Nothing can be tested
    // (or carried) while one of them is in front, so say so and leave the
    // user's game alone.
    unsafe {
        let mut before = POINT { x: 0, y: 0 };
        GetCursorPos(&mut before);
        let nudge = if before.x > 0 { -1 } else { 1 };
        mouse(MOUSEEVENTF_MOVE, nudge, 0);
        std::thread::sleep(Duration::from_millis(30));
        let mut after = POINT { x: 0, y: 0 };
        GetCursorPos(&mut after);
        mouse(MOUSEEVENTF_MOVE, -nudge, 0);
        if after.x == before.x {
            return Err("the program in front is holding the mouse (a game, or something running as administrator); close or leave it and run this again".into());
        }
    }
    let (tap, results) = mpsc::channel();
    *TAP.lock().unwrap() = Some(tap);
    let mut passed = Vec::new();
    let dir = std::env::temp_dir().join(format!("screen-portals-drag-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("dragged file.txt");
    let _ = std::fs::write(&file, "x");
    let cases = [
        ("text", DragPayload::Text("carried across".into())),
        ("link", DragPayload::Url("https://example.com/page?x=1".into())),
        ("file", DragPayload::Files(vec![file.clone()])),
    ];
    let mut failure = None;
    for (index, (name, payload)) in cases.into_iter().enumerate() {
        begin(payload.clone());
        std::thread::sleep(Duration::from_millis(250));
        catch(index as u64);
        match results.recv_timeout(Duration::from_secs(3)) {
            Ok((_, Caught::Data(got))) if got == payload => passed.push(name),
            Ok((_, other)) => failure = Some(format!("a dragged {name} came out as {other:?}")),
            Err(_) => failure = Some(format!("a dragged {name} was never caught")),
        }
        // Whatever happened, leave no drag running and no button down.
        cancel();
        std::thread::sleep(Duration::from_millis(150));
        if failure.is_some() {
            mouse(MOUSEEVENTF_LEFTUP, 0, 0);
            break;
        }
    }
    *TAP.lock().unwrap() = None;
    let _ = std::fs::remove_dir_all(dir);
    match failure {
        Some(problem) => Err(problem),
        None => Ok(format!("{} carried", passed.join(", "))),
    }
}

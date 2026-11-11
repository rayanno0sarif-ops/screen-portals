//! A pretend computer that records what it was asked to do. Used by the
//! tests so the whole engine can run end to end without moving a real cursor.

use super::{DragPayload, Platform};
use crate::layout::Rect;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One thing the engine asked the pretend computer to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Warp(i32, i32),
    Abs(u16, u16),
    Button(u8, bool),
    Wheel(i16, i16),
    Key(u16, bool),
    Clipboard(String),
    ClipboardFiles(Vec<PathBuf>),
    ClipboardImage(Vec<u8>),
    Reveal(PathBuf),
    /// Capturing this computer's sound was switched on or off.
    Capture(bool),
    /// Sound from another computer was played here.
    Audio { seq: u32, rate: u32, stereo: Vec<i16> },
    /// The platform was asked to look for a drag at the cursor.
    DragCatch(u64),
    DragBegin(DragPayload),
    DragCancel,
    OpenUrl(String),
}

/// A pretend computer with a screen of a given size.
pub struct MockPlatform {
    screen: Rect,
    cursor: Mutex<(i32, i32)>,
    events: Mutex<Vec<Event>>,
    image: Mutex<Option<Vec<u8>>>,
}

impl MockPlatform {
    /// A pretend computer whose desktop is `w` by `h` pixels.
    pub fn new(w: i32, h: i32) -> MockPlatform {
        MockPlatform {
            screen: Rect { x: 0, y: 0, w, h },
            cursor: Mutex::new((w / 2, h / 2)),
            events: Mutex::new(Vec::new()),
            image: Mutex::new(None),
        }
    }

    /// Pretend the user copied this picture (PNG bytes) here.
    pub fn copy_image(&self, png: Option<Vec<u8>>) {
        *self.image.lock().unwrap() = png;
    }

    /// Everything asked of it so far, in order.
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    /// Forget what has been recorded.
    pub fn clear(&self) {
        self.events.lock().unwrap().clear();
    }

    fn record(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

impl Platform for MockPlatform {
    fn screen(&self) -> Rect {
        self.screen
    }

    fn park_point(&self) -> (i32, i32) {
        (self.screen.w / 2, self.screen.h / 2)
    }

    fn cursor(&self) -> (i32, i32) {
        *self.cursor.lock().unwrap()
    }

    fn warp(&self, x: i32, y: i32) {
        *self.cursor.lock().unwrap() = (x, y);
        self.record(Event::Warp(x, y));
    }

    fn inject_abs(&self, x: u16, y: u16) {
        self.record(Event::Abs(x, y));
    }

    fn inject_button(&self, button: u8, down: bool) {
        self.record(Event::Button(button, down));
    }

    fn inject_wheel(&self, dx: i16, dy: i16) {
        self.record(Event::Wheel(dx, dy));
    }

    fn inject_key(&self, hid: u16, down: bool) {
        self.record(Event::Key(hid, down));
    }

    fn set_clipboard_text(&self, text: &str) {
        self.record(Event::Clipboard(text.to_string()));
    }

    fn set_clipboard_files(&self, paths: &[PathBuf]) {
        self.record(Event::ClipboardFiles(paths.to_vec()));
    }

    fn set_clipboard_image(&self, png: &[u8]) {
        self.record(Event::ClipboardImage(png.to_vec()));
    }

    fn clipboard_image(&self) -> Option<Vec<u8>> {
        self.image.lock().unwrap().clone()
    }

    fn reveal(&self, path: &Path) {
        self.record(Event::Reveal(path.to_path_buf()));
    }

    fn capture_audio(&self, on: bool) {
        self.record(Event::Capture(on));
    }

    fn play_audio(&self, _source: u64, seq: u32, rate: u32, stereo: &[i16]) {
        self.record(Event::Audio { seq, rate, stereo: stereo.to_vec() });
    }

    fn drag_catch(&self, token: u64) {
        self.record(Event::DragCatch(token));
    }

    fn drag_begin(&self, payload: DragPayload) {
        self.record(Event::DragBegin(payload));
    }

    fn drag_cancel(&self) {
        self.record(Event::DragCancel);
    }

    fn open_url(&self, url: &str) {
        self.record(Event::OpenUrl(url.to_string()));
    }
}

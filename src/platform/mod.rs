//! The boundary between the portable engine and a real operating system.
//!
//! The engine calls [`Platform`] to move the cursor, press keys and set the
//! clipboard. The platform calls back into `Node::on_*` when the person at
//! this computer moves the mouse or types.

use crate::layout::Rect;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod linux_audio;
pub mod mock;
#[cfg(windows)]
pub mod win_audio;
#[cfg(windows)]
pub mod win_drag;
#[cfg(windows)]
pub mod win_page;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod x11_clip;

/// Something being dragged with the mouse, in a form that can travel.
#[derive(Debug, Clone, PartialEq)]
pub enum DragPayload {
    Files(Vec<PathBuf>),
    Text(String),
    /// A web address (always http or https).
    Url(String),
}

/// What was found when the platform looked for a drag at the cursor.
#[derive(Debug, Clone, PartialEq)]
pub enum Caught {
    /// The button is held for some other reason (selecting, resizing...).
    Nothing,
    /// A drag was in progress and has been taken over.
    Data(DragPayload),
    /// A browser window is being moved by its title bar; this is its page.
    Page(String),
}

/// Everything the engine needs from an operating system. Calls may come from any thread and must not block for long.
pub trait Platform: Send + Sync + 'static {
    /// Bounds of the whole desktop (all monitors).
    fn screen(&self) -> Rect;
    /// Where the local cursor rests while it is controlling another computer.
    fn park_point(&self) -> (i32, i32);
    fn cursor(&self) -> (i32, i32);
    /// Move the local cursor.
    fn warp(&self, x: i32, y: i32);
    /// Move the cursor on behalf of a remote computer; 0..=65535 on each axis.
    fn inject_abs(&self, x: u16, y: u16);
    fn inject_button(&self, button: u8, down: bool);
    fn inject_wheel(&self, dx: i16, dy: i16);
    fn inject_key(&self, hid: u16, down: bool);
    fn set_clipboard_text(&self, text: &str);
    /// Put these files and folders on the clipboard, ready to paste.
    fn set_clipboard_files(&self, _paths: &[PathBuf]) {}
    /// Put this picture (a PNG) on the clipboard. May take a moment.
    fn set_clipboard_image(&self, _png: &[u8]) {}
    /// The picture currently on the clipboard, as a PNG. May take a moment.
    fn clipboard_image(&self) -> Option<Vec<u8>> {
        None
    }
    /// Show this file or folder to the user in the file manager.
    fn reveal(&self, _path: &Path) {}
    /// Start (or stop) capturing everything this computer plays and silence
    /// its own speakers meanwhile. Captured sound is handed to
    /// `Node::on_audio_captured`. Stopping must have finished, and the
    /// speakers must be back, by the time this returns.
    fn capture_audio(&self, _on: bool) {}
    /// Play sound that arrived from another computer: 16-bit stereo at
    /// `rate`. `source` tells senders apart and `seq` orders their packets.
    fn play_audio(&self, _source: u64, _seq: u32, _rate: u32, _stereo: &[i16]) {}
    /// Look for a drag in progress at the cursor and take it over. The
    /// answer always arrives, through `Node::on_drag_caught` with `token`,
    /// and by then the left button has been let go on this computer.
    fn drag_catch(&self, _token: u64) {}
    /// Start dragging this from where the cursor is, as if the left button
    /// had just gone down on it. Releasing the button drops it.
    fn drag_begin(&self, _payload: DragPayload) {}
    /// Abandon a drag started with `drag_begin` without dropping it.
    fn drag_cancel(&self) {}
    /// Open a web page in the user's browser.
    fn open_url(&self, _url: &str) {}
}

// A platform reports clipboard changes through `Node::on_clipboard_text`,
// `on_clipboard_files`, `on_clipboard_image` and `on_clipboard_cleared`. It
// should only report changes made by other programs, not its own writes.

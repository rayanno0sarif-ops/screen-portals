//! The clipboard on macOS ("the general pasteboard").
//!
//! macOS does not announce clipboard changes. Instead every copy bumps a
//! counter, so the main thread looks at it ten times a second (see
//! `mac::tick`). All clipboard work happens on the main thread; other
//! threads leave requests here and wait for the answer.

use crate::node::Node;
use crate::{picture, uri};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardItem, NSPasteboardType, NSPasteboardTypeFileURL,
    NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString, NSURL};
use std::cell::Cell;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// Something that arrived from another computer, ready to paste here.
pub enum Offer {
    Text(String),
    Files(Vec<PathBuf>),
    /// A picture, as a PNG.
    Image(Vec<u8>),
}

static PENDING: Mutex<Option<Offer>> = Mutex::new(None);
static WAITING: Mutex<Vec<mpsc::Sender<Option<Vec<u8>>>>> = Mutex::new(Vec::new());

thread_local! {
    /// The clipboard's counter when it was last looked at (main thread).
    static SEEN: Cell<isize> = const { Cell::new(0) };
}

/// Put this on the clipboard (within a tenth of a second). Safe from any thread.
pub fn offer(offer: Offer) {
    *PENDING.lock().unwrap() = Some(offer);
}

/// The picture currently on the clipboard, as a PNG.
pub fn read_image() -> Option<Vec<u8>> {
    if objc2::MainThreadMarker::new().is_some() {
        return image_now();
    }
    let (answer, wait) = mpsc::channel();
    WAITING.lock().unwrap().push(answer);
    wait.recv_timeout(Duration::from_secs(3)).ok().flatten()
}

fn has(types: &NSArray<NSPasteboardType>, wanted: &NSPasteboardType) -> bool {
    types.iter().any(|t| t.isEqualToString(wanted))
}

fn image_now() -> Option<Vec<u8>> {
    let board = NSPasteboard::generalPasteboard();
    let types = board.types()?;
    unsafe {
        if has(&types, NSPasteboardTypePNG) {
            let png = board.dataForType(NSPasteboardTypePNG)?.to_vec();
            if picture::looks_like_png(&png) {
                return Some(png);
            }
        }
        // Most Mac programs copy pictures as TIFF; turn that into a PNG.
        if has(&types, NSPasteboardTypeTIFF) {
            let tiff = board.dataForType(NSPasteboardTypeTIFF)?;
            let image = NSBitmapImageRep::imageRepWithData(&tiff)?;
            let png = image.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
            return Some(png.to_vec());
        }
    }
    None
}

/// The local paths of files copied in Finder (or elsewhere).
fn files_now(board: &NSPasteboard) -> Vec<PathBuf> {
    let Some(items) = board.pasteboardItems() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let text = unsafe { item.stringForType(NSPasteboardTypeFileURL) }?;
            // Finder hands out "file:///.file/id=..." references; asking for
            // the path turns them into ordinary ones.
            let url = NSURL::URLWithString(&text)?;
            let path = url.filePathURL()?.path()?;
            Some(PathBuf::from(path.to_string()))
        })
        .collect()
}

fn write(board: &NSPasteboard, offer: &Offer) {
    board.clearContents();
    unsafe {
        match offer {
            Offer::Text(text) => {
                board.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
            }
            Offer::Files(paths) => {
                // One entry per file, each a file:// address, which is what
                // Finder pastes.
                let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
                    .iter()
                    .map(|path| {
                        let item = NSPasteboardItem::new();
                        item.setString_forType(&NSString::from_str(&uri::file_uri(path.as_os_str().as_bytes())), NSPasteboardTypeFileURL);
                        ProtocolObject::from_retained(item)
                    })
                    .collect();
                board.writeObjects(&NSArray::from_retained_slice(&items));
            }
            Offer::Image(png) => {
                let data = NSData::with_bytes(png);
                board.setData_forType(Some(&data), NSPasteboardTypePNG);
                // Many Mac programs only paste TIFF.
                if let Some(tiff) = NSBitmapImageRep::imageRepWithData(&data).and_then(|image| image.TIFFRepresentation()) {
                    board.setData_forType(Some(&tiff), NSPasteboardTypeTIFF);
                }
            }
        }
    }
}

/// What to tell the engine about a change made by another program.
enum News {
    Files(Vec<PathBuf>),
    Text(String),
    Image,
    Cleared,
}

fn news(board: &NSPasteboard) -> News {
    let Some(types) = board.types() else { return News::Cleared };
    unsafe {
        if has(&types, NSPasteboardTypeFileURL) {
            let paths = files_now(board);
            if !paths.is_empty() {
                return News::Files(paths);
            }
        }
        // Text wins over a picture: documents and spreadsheets offer both,
        // and the text is what people expect to paste.
        if has(&types, NSPasteboardTypeString) {
            if let Some(text) = board.stringForType(NSPasteboardTypeString) {
                let text = text.to_string();
                if !text.is_empty() {
                    return News::Text(text);
                }
            }
        }
        if has(&types, NSPasteboardTypePNG) || has(&types, NSPasteboardTypeTIFF) {
            return News::Image;
        }
    }
    News::Cleared
}

/// Remember what is on the clipboard now, so that only later copies count
/// as news. Called on the main thread before watching starts.
pub fn start() {
    let count = NSPasteboard::generalPasteboard().changeCount();
    SEEN.with(|seen| seen.set(count));
}

/// Called on the main thread ten times a second.
pub fn tick(node: &Arc<Node>) {
    let board = NSPasteboard::generalPasteboard();
    if let Some(offer) = PENDING.lock().unwrap().take() {
        write(&board, &offer);
        // What this program put there itself is not news.
        SEEN.with(|seen| seen.set(board.changeCount()));
    }
    let waiting: Vec<_> = WAITING.lock().unwrap().drain(..).collect();
    if !waiting.is_empty() {
        let image = image_now();
        for answer in waiting {
            let _ = answer.send(image.clone());
        }
    }
    let count = board.changeCount();
    if SEEN.with(|seen| seen.replace(count)) == count {
        return;
    }
    match news(&board) {
        News::Files(paths) => node.on_clipboard_files(paths),
        News::Text(text) => node.on_clipboard_text(text),
        News::Image => node.on_clipboard_image(),
        News::Cleared => node.on_clipboard_cleared(),
    }
}

//! The clipboard on X11.
//!
//! X has no clipboard that holds data. A program that copies something
//! announces that it *owns* the clipboard, and hands the data over only
//! when another program asks to paste, in whichever format that program
//! picks. So this file does two jobs:
//!
//! - **Reading**: notice when another program becomes the owner, ask it
//!   which formats it has, then ask for the best one.
//! - **Offering**: become the owner for what arrived from another computer,
//!   and answer programs that paste.
//!
//! Large data travels in pieces (the "INCR" convention); both directions
//! handle that. Everything here runs on the event loop's thread except
//! `offer` and `read_image`, which may be called from anywhere.

use super::linux::X;
use crate::node::Node;
use crate::{picture, uri};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, PropMode, Property, SelectionNotifyEvent,
    SelectionRequestEvent, Window, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

x11rb::atom_manager! {
    /// The names this program uses when talking to the display.
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        INCR,
        UTF8_STRING,
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        TEXT_PLAIN: b"text/plain",
        URI_LIST: b"text/uri-list",
        GNOME_FILES: b"x-special/gnome-copied-files",
        IMAGE_PNG: b"image/png",
        SCREEN_PORTALS_DATA,
        SCREEN_PORTALS_WAKE,
    }
}

/// Data larger than this is handed over in pieces.
const PIECE: usize = 256 * 1024;
/// Nothing bigger than this is read from another program.
const MAX_READ: usize = 256 * 1024 * 1024;
/// A program that does not answer within this long is given up on.
const PATIENCE: Duration = Duration::from_secs(3);

/// Something of ours for other programs to paste.
pub enum Offer {
    Text(String),
    Files(Vec<PathBuf>),
    /// A picture, as a PNG.
    Image(Vec<u8>),
}

/// What is being asked of the program that owns the clipboard.
#[derive(Clone, Copy, PartialEq)]
enum Want {
    Formats,
    Text,
    Files,
    Image,
}

struct Fetch {
    want: Want,
    /// Pieces collected so far, once the owner has said it sends in pieces.
    pieces: Option<Vec<u8>>,
    since: Instant,
}

/// Data being handed to a pasting program piece by piece.
struct Sending {
    to: Window,
    property: Atom,
    format: Atom,
    data: Vec<u8>,
    sent: usize,
    since: Instant,
}

/// Everything remembered about the clipboard.
#[derive(Default)]
pub struct Clip {
    offer: Option<Offer>,
    fetch: Option<Fetch>,
    /// Threads waiting in `read_image`.
    waiting: Vec<mpsc::Sender<Option<Vec<u8>>>>,
    sending: Vec<Sending>,
}

/// What to tell the engine once the clipboard lock has been let go.
enum News {
    Text(String),
    Files(Vec<PathBuf>),
    Image,
    Cleared,
}

/// Ask to be told whenever the clipboard changes hands.
pub(super) fn watch(x: &X) -> Result<(), String> {
    x.conn
        .xfixes_select_selection_input(x.window, x.atoms.CLIPBOARD, SelectionEventMask::SET_SELECTION_OWNER)
        .map_err(|e| format!("cannot watch the clipboard: {e}"))?;
    Ok(())
}

/// Become the clipboard's owner for something that arrived from another
/// computer. Safe from any thread.
pub(super) fn offer(x: &X, offer: Offer) {
    let mut clip = x.clip.lock().unwrap();
    clip.offer = Some(offer);
    let _ = x.conn.set_selection_owner(x.window, x.atoms.CLIPBOARD, CURRENT_TIME);
    let _ = x.conn.flush();
}

/// The picture another program has copied, as a PNG. Waits for that
/// program to hand it over. Safe from any thread except the event loop's.
pub(super) fn read_image(x: &X) -> Option<Vec<u8>> {
    let (answer, wait) = mpsc::channel();
    {
        let mut clip = x.clip.lock().unwrap();
        if let Some(Offer::Image(png)) = &clip.offer {
            return Some(png.clone());
        }
        clip.waiting.push(answer);
    }
    x.wake();
    wait.recv_timeout(PATIENCE + Duration::from_secs(1)).ok().flatten()
}

fn ask(x: &X, clip: &mut Clip, want: Want, format: Atom) {
    clip.fetch = Some(Fetch { want, pieces: None, since: Instant::now() });
    let _ = x.conn.delete_property(x.window, x.atoms.SCREEN_PORTALS_DATA);
    let _ = x.conn.convert_selection(x.window, x.atoms.CLIPBOARD, format, x.atoms.SCREEN_PORTALS_DATA, CURRENT_TIME);
    let _ = x.conn.flush();
}

/// Start fetching the picture if a thread is waiting for it and nothing
/// else is under way.
fn serve_waiting(x: &X, clip: &mut Clip) {
    if clip.fetch.as_ref().is_some_and(|fetch| fetch.since.elapsed() > PATIENCE) {
        clip.fetch = None;
    }
    if clip.fetch.is_none() && !clip.waiting.is_empty() {
        ask(x, clip, Want::Image, x.atoms.IMAGE_PNG);
    }
}

fn answer_waiting(clip: &mut Clip, png: Option<Vec<u8>>) {
    for waiting in clip.waiting.drain(..) {
        let _ = waiting.send(png.clone());
    }
}

/// The owner handed over everything that was asked for.
fn arrived(x: &X, clip: &mut Clip, want: Want, data: Vec<u8>) -> Option<News> {
    let a = &x.atoms;
    match want {
        Want::Formats => {
            let formats: Vec<Atom> = data.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect();
            let has = |atom: Atom| formats.contains(&atom);
            let string: Atom = AtomEnum::STRING.into();
            if has(a.URI_LIST) {
                ask(x, clip, Want::Files, a.URI_LIST);
                None
            // Text wins over a picture: spreadsheets and documents offer
            // both, and the text is what people expect to paste.
            } else if let Some(text) = [a.UTF8_STRING, a.TEXT_PLAIN_UTF8, a.TEXT_PLAIN, string].into_iter().find(|f| has(*f)) {
                ask(x, clip, Want::Text, text);
                None
            } else if has(a.IMAGE_PNG) {
                // The picture itself is only fetched when it is needed.
                Some(News::Image)
            } else {
                Some(News::Cleared)
            }
        }
        Want::Text => {
            let text = String::from_utf8_lossy(&data).trim_end_matches('\0').to_string();
            Some(if text.is_empty() { News::Cleared } else { News::Text(text) })
        }
        Want::Files => {
            let paths: Vec<PathBuf> = uri::paths_from_list(&String::from_utf8_lossy(&data))
                .into_iter()
                .map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes)))
                .collect();
            Some(if paths.is_empty() { News::Cleared } else { News::Files(paths) })
        }
        Want::Image => {
            answer_waiting(clip, Some(data).filter(|png| picture::looks_like_png(png)));
            None
        }
    }
}

/// The fetch under way ended without data.
fn failed(clip: &mut Clip, want: Want) -> Option<News> {
    match want {
        Want::Image => {
            answer_waiting(clip, None);
            None
        }
        _ => Some(News::Cleared),
    }
}

/// Read (and remove) what the owner put on our window.
fn take_property(x: &X) -> Option<(Atom, Vec<u8>)> {
    let reply = x.conn.get_property(true, x.window, x.atoms.SCREEN_PORTALS_DATA, AtomEnum::ANY, 0, u32::MAX).ok()?.reply().ok()?;
    Some((reply.type_, reply.value))
}

/// What a pasting program gets for one format, if we have it.
fn render(x: &X, offer: &Offer, format: Atom) -> Option<Vec<u8>> {
    let a = &x.atoms;
    let string: Atom = AtomEnum::STRING.into();
    let is_text = [a.UTF8_STRING, a.TEXT_PLAIN_UTF8, a.TEXT_PLAIN, string].contains(&format);
    match offer {
        Offer::Text(text) if is_text => Some(text.as_bytes().to_vec()),
        Offer::Files(paths) => {
            let uris: Vec<String> = paths.iter().map(|path| uri::file_uri(path.as_os_str().as_bytes())).collect();
            if format == a.URI_LIST {
                Some(uris.iter().map(|uri| format!("{uri}\r\n")).collect::<String>().into_bytes())
            } else if format == a.GNOME_FILES {
                // What GNOME's and most other file managers look for.
                Some(format!("copy\n{}", uris.join("\n")).into_bytes())
            } else if is_text {
                let lines: Vec<String> = paths.iter().map(|path| path.display().to_string()).collect();
                Some(lines.join("\n").into_bytes())
            } else {
                None
            }
        }
        Offer::Image(png) if format == a.IMAGE_PNG => Some(png.clone()),
        _ => None,
    }
}

fn formats(x: &X, offer: &Offer) -> Vec<Atom> {
    let a = &x.atoms;
    let text = [a.UTF8_STRING, a.TEXT_PLAIN_UTF8, a.TEXT_PLAIN, AtomEnum::STRING.into()];
    let mut list = vec![a.TARGETS];
    match offer {
        Offer::Text(_) => list.extend(text),
        Offer::Files(_) => {
            list.extend([a.URI_LIST, a.GNOME_FILES]);
            list.extend(text);
        }
        Offer::Image(_) => list.push(a.IMAGE_PNG),
    }
    list
}

/// Another program wants to paste what we offer.
fn requested(x: &X, clip: &mut Clip, request: &SelectionRequestEvent) {
    // Very old programs name no property; the convention is to use the format's name.
    let property = if request.property == NONE { request.target } else { request.property };
    let mut done = false;
    if let Some(offer) = clip.offer.as_ref().filter(|_| request.selection == x.atoms.CLIPBOARD) {
        if request.target == x.atoms.TARGETS {
            let atom: Atom = AtomEnum::ATOM.into();
            done = x.conn.change_property32(PropMode::REPLACE, request.requestor, property, atom, &formats(x, offer)).is_ok();
        } else if let Some(data) = render(x, offer, request.target) {
            let piece = PIECE.min(x.conn.maximum_request_bytes().saturating_sub(1024)).max(1024);
            if data.len() <= piece {
                done = x.conn.change_property8(PropMode::REPLACE, request.requestor, property, request.target, &data).is_ok();
            } else {
                // Too big for one go: announce the size, then send a piece
                // each time the other program has taken the last one.
                let watch = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
                let _ = x.conn.change_window_attributes(request.requestor, &watch);
                let size = [data.len().min(u32::MAX as usize) as u32];
                done = x.conn.change_property32(PropMode::REPLACE, request.requestor, property, x.atoms.INCR, &size).is_ok();
                clip.sending.retain(|s| s.since.elapsed() < Duration::from_secs(120));
                clip.sending.push(Sending { to: request.requestor, property, format: request.target, data, sent: 0, since: Instant::now() });
            }
        }
    }
    let reply = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property: if done { property } else { NONE },
    };
    let _ = x.conn.send_event(false, request.requestor, EventMask::NO_EVENT, reply);
    let _ = x.conn.flush();
}

/// A pasting program took a piece; give it the next one. An empty piece
/// says that was all.
fn next_piece(x: &X, clip: &mut Clip, to: Window, property: Atom) {
    let Some(index) = clip.sending.iter().position(|s| s.to == to && s.property == property) else { return };
    let piece = PIECE.min(x.conn.maximum_request_bytes().saturating_sub(1024)).max(1024);
    let sending = &mut clip.sending[index];
    let end = (sending.sent + piece).min(sending.data.len());
    let _ = x.conn.change_property8(PropMode::REPLACE, to, property, sending.format, &sending.data[sending.sent..end]);
    let _ = x.conn.flush();
    if sending.sent == end {
        clip.sending.remove(index);
    } else {
        sending.sent = end;
    }
}

/// Every event the mouse and keyboard code does not want comes here.
pub(super) fn on_event(x: &X, node: &Arc<Node>, event: &Event) {
    let news = {
        let mut clip = x.clip.lock().unwrap();
        let clip = &mut *clip;
        let news = match event {
            // The clipboard changed hands.
            Event::XfixesSelectionNotify(e) if e.selection == x.atoms.CLIPBOARD => {
                if e.owner == x.window {
                    None // what this program put there itself is not news
                } else {
                    // Whatever was being fetched belongs to the old owner.
                    clip.fetch = None;
                    answer_waiting(clip, None);
                    if e.owner == NONE {
                        Some(News::Cleared)
                    } else {
                        ask(x, clip, Want::Formats, x.atoms.TARGETS);
                        None
                    }
                }
            }
            // The owner answered a request of ours.
            Event::SelectionNotify(e) if e.requestor == x.window => match clip.fetch.take() {
                None => None,
                Some(fetch) if e.property == NONE => failed(clip, fetch.want),
                Some(mut fetch) => match take_property(x) {
                    None => failed(clip, fetch.want),
                    Some((kind, _)) if kind == x.atoms.INCR => {
                        // It will arrive in pieces; removing the note
                        // above was the signal to start.
                        fetch.pieces = Some(Vec::new());
                        clip.fetch = Some(fetch);
                        None
                    }
                    Some((_, data)) => arrived(x, clip, fetch.want, data),
                },
            },
            // A piece arrived, or a pasting program took one of ours.
            Event::PropertyNotify(e) => {
                if e.window == x.window && e.atom == x.atoms.SCREEN_PORTALS_DATA && e.state == Property::NEW_VALUE {
                    match clip.fetch.take() {
                        Some(mut fetch) if fetch.pieces.is_some() => match take_property(x) {
                            None => failed(clip, fetch.want),
                            Some((_, piece)) if piece.is_empty() => arrived(x, clip, fetch.want, fetch.pieces.take().unwrap_or_default()),
                            Some((_, piece)) => {
                                let so_far = fetch.pieces.get_or_insert_with(Vec::new);
                                so_far.extend_from_slice(&piece);
                                if so_far.len() > MAX_READ {
                                    failed(clip, fetch.want)
                                } else {
                                    clip.fetch = Some(fetch);
                                    None
                                }
                            }
                        },
                        other => {
                            clip.fetch = other;
                            None
                        }
                    }
                } else {
                    if e.state == Property::DELETE {
                        next_piece(x, clip, e.window, e.atom);
                    }
                    None
                }
            }
            Event::SelectionRequest(e) => {
                requested(x, clip, e);
                None
            }
            // Another program copied something: ours is no longer wanted.
            Event::SelectionClear(e) if e.selection == x.atoms.CLIPBOARD => {
                clip.offer = None;
                None
            }
            _ => None,
        };
        // `read_image` on another thread may be waiting for the picture
        // (it wakes this loop with a message of its own).
        serve_waiting(x, clip);
        news
    };
    // The engine is told after the lock is gone, in case it turns round
    // and asks the clipboard for something.
    match news {
        Some(News::Text(text)) => node.on_clipboard_text(text),
        Some(News::Files(paths)) => node.on_clipboard_files(paths),
        Some(News::Image) => node.on_clipboard_image(),
        Some(News::Cleared) => node.on_clipboard_cleared(),
        None => {}
    }
}

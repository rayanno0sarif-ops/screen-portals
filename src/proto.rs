//! Wire messages. Everything here is platform neutral: positions are
//! normalised to 0..=65535 across the receiving desktop and keys are USB HID
//! usage codes, so a future macOS build can speak the same protocol.

use crate::layout::{PortalMode, Side};
use serde::{Deserialize, Serialize};

/// Both computers must speak the same version; it changes whenever a message is added.
pub const PROTOCOL_VERSION: u16 = 5;

/// What a transfer carries, which decides where it lands on arrival.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    /// Files that were copied; they arrive ready to paste.
    Clipboard,
    /// A copied picture, as PNG; it arrives ready to paste.
    Image,
    /// Files sent on purpose; they arrive in the received-files folder.
    Sent,
    /// Files being dragged with the mouse; on arrival the drag carries on.
    Drag,
}

/// Raw bytes as one block rather than one element at a time.
mod blob {
    use serde::{Deserializer, Serializer};

    /// Write the bytes as one block.
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    /// Read a block of bytes written by `serialize`.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct Bytes;
        impl<'de> serde::de::Visitor<'de> for Bytes {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
        }
        deserializer.deserialize_byte_buf(Bytes)
    }
}

/// Messages carried on the reliable, encrypted TCP stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Msg {
    /// First message in each direction after the Noise handshake.
    Hello {
        version: u16,
        name: String,
        screen_w: u32,
        screen_h: u32,
        /// UDP port this side listens on for pointer datagrams.
        udp_port: u16,
        /// Key this side will use to encrypt the datagrams it sends.
        udp_key: [u8; 32],
        /// Identifier this side stamps on the datagrams it sends.
        udp_session: u64,
        /// True when this side does not know the other yet and is willing to pair.
        pairing: bool,
    },

    // --- pairing (only between devices that do not know each other) ---
    PairCommit([u8; 32]),
    PairNonce([u8; 16]),
    PairReveal([u8; 16]),
    /// Where the sender believes the receiver sits, if the user said so.
    PairSide(Option<Side>),
    PairConfirm(bool),

    // --- session ---
    Ping,
    /// "Your datagrams reach me", so the sender may move pointer traffic to UDP.
    UdpOk,
    /// The sender's cursor has crossed onto the receiver's desktop.
    Enter { x: u16, y: u16 },
    /// The sender has taken its cursor back.
    Leave,
    /// Pointer position, used on TCP only until UDP is confirmed.
    MouseAbs { x: u16, y: u16 },
    /// Carries the position so a click can never land on a stale location.
    Button { button: u8, down: bool, x: u16, y: u16 },
    Wheel { dx: i16, dy: i16 },
    Key { hid: u16, down: bool },
    ClipboardText(String),
    /// "I have arranged you like this relative to me." The receiver adopts
    /// the mirror image so both computers agree on the doorway.
    Layout { side: Side, offset: i32, mode: PortalMode },

    // --- files and pictures ---
    // The sender picks `id`. Everything for one transfer arrives in order:
    // Begin, then for each file an Entry followed by its Data, then End.
    /// `entries` counts files and folders, `files` only files, `bytes` the
    /// total of all file sizes. A picture has no entries, only data.
    TransferBegin { id: u64, kind: TransferKind, label: String, files: u32, entries: u32, bytes: u64 },
    /// The next file or folder. `path` is relative, with `/` between parts.
    TransferEntry { id: u64, path: String, size: u64, dir: bool },
    TransferData {
        id: u64,
        #[serde(with = "blob")]
        data: Vec<u8>,
    },
    /// From the sender: that was everything. From the receiver: it is all
    /// in place.
    TransferEnd { id: u64 },
    /// From the receiver: this many bytes have arrived so far. The sender
    /// never runs far ahead of it, so typing is not stuck behind a file.
    TransferAck { id: u64, bytes: u64 },
    /// Either side gives up on the transfer.
    TransferCancel { id: u64 },

    // --- sound ---
    /// "Sound should come out of this computer" (by its public key), or of
    /// each computer's own speakers for `None`. `rev` counts the times the
    /// user has changed it, so the most recent choice wins everywhere.
    Speakers { key: Option<[u8; 32]>, rev: u64 },

    // --- dragging ---
    // The sender's cursor has just arrived holding something. Dragged files
    // come as a transfer of kind `Drag` instead.
    DragText(String),
    DragUrl(String),
    /// "I am taking my cursor back with the button still held: if something
    /// is being dragged on your screen, catch it and send it to me."
    DragCatch,
    /// Open this web page (a browser window was pushed at the edge).
    Open { url: String },
}

/// Messages carried in encrypted UDP datagrams. Only the newest one matters.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Dgram {
    Probe,
    MouseAbs { x: u16, y: u16 },
    /// A few milliseconds of sound: 16-bit stereo, little-endian, at `rate`
    /// frames per second. Lost packets are simply not played.
    Audio {
        seq: u32,
        rate: u32,
        #[serde(with = "blob")]
        pcm: Vec<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_data_round_trips_without_growing() {
        let small = Msg::TransferData { id: 7, data: vec![0, 1, 2, 255] };
        let bytes = postcard::to_allocvec(&small).unwrap();
        assert_eq!(postcard::from_bytes::<Msg>(&bytes).unwrap(), small);

        let piece = Msg::TransferData { id: u64::MAX, data: (0..32 * 1024).map(|i| (i % 256) as u8).collect() };
        let bytes = postcard::to_allocvec(&piece).unwrap();
        assert!(bytes.len() < 32 * 1024 + 16, "one piece must fit in a single encrypted frame");
        assert_eq!(postcard::from_bytes::<Msg>(&bytes).unwrap(), piece);

        let begin = Msg::TransferBegin { id: 1, kind: TransferKind::Sent, label: "a.txt".into(), files: 1, entries: 1, bytes: 9 };
        assert_eq!(postcard::from_bytes::<Msg>(&postcard::to_allocvec(&begin).unwrap()).unwrap(), begin);
    }
}

/// Mouse buttons as they are numbered on the wire.
pub const BUTTON_LEFT: u8 = 1;
pub const BUTTON_RIGHT: u8 = 2;
pub const BUTTON_MIDDLE: u8 = 3;
pub const BUTTON_X1: u8 = 4;
pub const BUTTON_X2: u8 = 5;

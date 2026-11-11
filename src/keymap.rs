//! Translation between Windows keyboard scan codes and USB HID usage codes.
//! HID usages are what travels on the wire, so each platform only needs a
//! table to and from its own native codes.

/// (scan code, extended flag, HID usage) for PC "set 1" scan codes.
const TABLE: &[(u8, bool, u16)] = &[
    (0x01, false, 0x29), // Escape
    (0x02, false, 0x1E), (0x03, false, 0x1F), (0x04, false, 0x20), (0x05, false, 0x21), // 1 2 3 4
    (0x06, false, 0x22), (0x07, false, 0x23), (0x08, false, 0x24), (0x09, false, 0x25), // 5 6 7 8
    (0x0A, false, 0x26), (0x0B, false, 0x27), // 9 0
    (0x0C, false, 0x2D), (0x0D, false, 0x2E), // - =
    (0x0E, false, 0x2A), (0x0F, false, 0x2B), // Backspace Tab
    (0x10, false, 0x14), (0x11, false, 0x1A), (0x12, false, 0x08), (0x13, false, 0x15), // Q W E R
    (0x14, false, 0x17), (0x15, false, 0x1C), (0x16, false, 0x18), (0x17, false, 0x0C), // T Y U I
    (0x18, false, 0x12), (0x19, false, 0x13), // O P
    (0x1A, false, 0x2F), (0x1B, false, 0x30), // [ ]
    (0x1C, false, 0x28), // Enter
    (0x1D, false, 0xE0), // Left Ctrl
    (0x1E, false, 0x04), (0x1F, false, 0x16), (0x20, false, 0x07), (0x21, false, 0x09), // A S D F
    (0x22, false, 0x0A), (0x23, false, 0x0B), (0x24, false, 0x0D), (0x25, false, 0x0E), // G H J K
    (0x26, false, 0x0F), // L
    (0x27, false, 0x33), (0x28, false, 0x34), (0x29, false, 0x35), // ; ' `
    (0x2A, false, 0xE1), // Left Shift
    (0x2B, false, 0x31), // backslash
    (0x2C, false, 0x1D), (0x2D, false, 0x1B), (0x2E, false, 0x06), (0x2F, false, 0x19), // Z X C V
    (0x30, false, 0x05), (0x31, false, 0x11), (0x32, false, 0x10), // B N M
    (0x33, false, 0x36), (0x34, false, 0x37), (0x35, false, 0x38), // , . /
    (0x36, false, 0xE5), // Right Shift
    (0x37, false, 0x55), // Keypad *
    (0x38, false, 0xE2), // Left Alt
    (0x39, false, 0x2C), // Space
    (0x3A, false, 0x39), // Caps Lock
    (0x3B, false, 0x3A), (0x3C, false, 0x3B), (0x3D, false, 0x3C), (0x3E, false, 0x3D), // F1-F4
    (0x3F, false, 0x3E), (0x40, false, 0x3F), (0x41, false, 0x40), (0x42, false, 0x41), // F5-F8
    (0x43, false, 0x42), (0x44, false, 0x43), // F9 F10
    (0x45, false, 0x53), // Num Lock
    (0x46, false, 0x47), // Scroll Lock
    (0x47, false, 0x5F), (0x48, false, 0x60), (0x49, false, 0x61), // Keypad 7 8 9
    (0x4A, false, 0x56), // Keypad -
    (0x4B, false, 0x5C), (0x4C, false, 0x5D), (0x4D, false, 0x5E), // Keypad 4 5 6
    (0x4E, false, 0x57), // Keypad +
    (0x4F, false, 0x59), (0x50, false, 0x5A), (0x51, false, 0x5B), // Keypad 1 2 3
    (0x52, false, 0x62), (0x53, false, 0x63), // Keypad 0 .
    (0x56, false, 0x64), // extra key next to left Shift on ISO keyboards
    (0x57, false, 0x44), (0x58, false, 0x45), // F11 F12
    (0x1C, true, 0x58), // Keypad Enter
    (0x1D, true, 0xE4), // Right Ctrl
    (0x35, true, 0x54), // Keypad /
    (0x37, true, 0x46), // Print Screen
    (0x38, true, 0xE6), // Right Alt
    (0x47, true, 0x4A), (0x48, true, 0x52), (0x49, true, 0x4B), // Home Up PageUp
    (0x4B, true, 0x50), (0x4D, true, 0x4F), // Left Right
    (0x4F, true, 0x4D), (0x50, true, 0x51), (0x51, true, 0x4E), // End Down PageDown
    (0x52, true, 0x49), (0x53, true, 0x4C), // Insert Delete
    (0x5B, true, 0xE3), (0x5C, true, 0xE7), // Left Win, Right Win
    (0x5D, true, 0x65), // Menu
];

/// The Pause key, which Windows reports in an unusual way.
pub const HID_PAUSE: u16 = 0x48;
/// The Home key, part of the Ctrl+Alt+Home escape.
pub const HID_HOME: u16 = 0x4A;

const VK_PAUSE: u32 = 0x13;
const VK_SNAPSHOT: u32 = 0x2C;
const VK_NUMLOCK: u32 = 0x90;
const VK_RSHIFT: u32 = 0xA1;

/// Modifier keys: Ctrl, Shift, Alt and Win/Cmd on either side.
pub fn is_modifier(hid: u16) -> bool {
    (0xE0..=0xE7).contains(&hid)
}

/// Either Ctrl key.
pub fn is_ctrl(hid: u16) -> bool {
    hid == 0xE0 || hid == 0xE4
}

/// Either Alt key.
pub fn is_alt(hid: u16) -> bool {
    hid == 0xE2 || hid == 0xE6
}

/// Convert what a Windows low-level keyboard hook reports into a HID usage.
/// A handful of keys are reported with misleading scan codes by the hook, so
/// those are recognised by virtual-key code first.
pub fn from_windows(vk: u32, scan: u32, extended: bool) -> Option<u16> {
    match vk {
        VK_PAUSE => return Some(HID_PAUSE),
        VK_NUMLOCK => return Some(0x53),
        VK_RSHIFT => return Some(0xE5),
        VK_SNAPSHOT => return Some(0x46),
        _ => {}
    }
    let scan = u8::try_from(scan).ok()?;
    TABLE.iter().find(|(s, e, _)| *s == scan && *e == extended).map(|(_, _, hid)| *hid)
}

/// Convert a HID usage into the (scan code, extended) pair Windows expects
/// when injecting a key. Pause has no simple scan code and is handled by the
/// caller.
pub fn to_windows(hid: u16) -> Option<(u16, bool)> {
    TABLE.iter().find(|(_, _, h)| *h == hid).map(|(scan, ext, _)| (*scan as u16, *ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_has_no_duplicates() {
        let mut scans = HashSet::new();
        let mut hids = HashSet::new();
        for (scan, ext, hid) in TABLE {
            assert!(scans.insert((*scan, *ext)), "duplicate scan code {scan:#x}");
            assert!(hids.insert(*hid), "duplicate HID usage {hid:#x}");
        }
    }

    #[test]
    fn every_entry_round_trips() {
        for (scan, ext, hid) in TABLE {
            assert_eq!(from_windows(0, *scan as u32, *ext), Some(*hid));
            assert_eq!(to_windows(*hid), Some((*scan as u16, *ext)));
        }
    }

    #[test]
    fn well_known_keys() {
        assert_eq!(from_windows(0x41, 0x1E, false), Some(0x04)); // A
        assert_eq!(from_windows(0x0D, 0x1C, false), Some(0x28)); // Enter
        assert_eq!(from_windows(0x0D, 0x1C, true), Some(0x58)); // Keypad Enter
        assert_eq!(from_windows(0x25, 0x4B, true), Some(0x50)); // Left arrow
        assert_eq!(from_windows(0x64, 0x4B, false), Some(0x5C)); // Keypad 4
    }

    #[test]
    fn hook_quirks_are_handled() {
        // The hook reports Pause with Num Lock's scan code, and Num Lock and
        // right Shift with a spurious "extended" flag.
        assert_eq!(from_windows(VK_PAUSE, 0x45, false), Some(HID_PAUSE));
        assert_eq!(from_windows(VK_NUMLOCK, 0x45, true), Some(0x53));
        assert_eq!(from_windows(VK_RSHIFT, 0x36, true), Some(0xE5));
        assert_eq!(from_windows(0, 0x2A, true), None, "fake shift must be ignored");
        assert_eq!(from_windows(0xFF, 0x7F, false), None);
    }

    #[test]
    fn modifier_helpers() {
        assert!(is_modifier(0xE0) && is_modifier(0xE7) && !is_modifier(0x04));
        assert!(is_ctrl(0xE4) && !is_ctrl(0xE1));
        assert!(is_alt(0xE2) && is_alt(0xE6) && !is_alt(0xE0));
    }
}

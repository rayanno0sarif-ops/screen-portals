//! Translation between each system's own key codes (Windows scan codes,
//! Linux key codes) and USB HID usage codes. HID usages are what travels on
//! the wire, so each platform only needs a table to and from its own codes.

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

/// (Linux key code, HID usage). The Linux numbers are the `KEY_*` values of
/// the kernel's `input-event-codes.h`; an X server reports them plus 8.
const LINUX: &[(u16, u16)] = &[
    (1, 0x29), // Escape
    (2, 0x1E), (3, 0x1F), (4, 0x20), (5, 0x21), (6, 0x22), // 1 2 3 4 5
    (7, 0x23), (8, 0x24), (9, 0x25), (10, 0x26), (11, 0x27), // 6 7 8 9 0
    (12, 0x2D), (13, 0x2E), // - =
    (14, 0x2A), (15, 0x2B), // Backspace Tab
    (16, 0x14), (17, 0x1A), (18, 0x08), (19, 0x15), (20, 0x17), // Q W E R T
    (21, 0x1C), (22, 0x18), (23, 0x0C), (24, 0x12), (25, 0x13), // Y U I O P
    (26, 0x2F), (27, 0x30), // [ ]
    (28, 0x28), // Enter
    (29, 0xE0), // Left Ctrl
    (30, 0x04), (31, 0x16), (32, 0x07), (33, 0x09), (34, 0x0A), // A S D F G
    (35, 0x0B), (36, 0x0D), (37, 0x0E), (38, 0x0F), // H J K L
    (39, 0x33), (40, 0x34), (41, 0x35), // ; ' `
    (42, 0xE1), // Left Shift
    (43, 0x31), // backslash
    (44, 0x1D), (45, 0x1B), (46, 0x06), (47, 0x19), (48, 0x05), // Z X C V B
    (49, 0x11), (50, 0x10), // N M
    (51, 0x36), (52, 0x37), (53, 0x38), // , . /
    (54, 0xE5), // Right Shift
    (55, 0x55), // Keypad *
    (56, 0xE2), // Left Alt
    (57, 0x2C), // Space
    (58, 0x39), // Caps Lock
    (59, 0x3A), (60, 0x3B), (61, 0x3C), (62, 0x3D), (63, 0x3E), // F1-F5
    (64, 0x3F), (65, 0x40), (66, 0x41), (67, 0x42), (68, 0x43), // F6-F10
    (69, 0x53), // Num Lock
    (70, 0x47), // Scroll Lock
    (71, 0x5F), (72, 0x60), (73, 0x61), // Keypad 7 8 9
    (74, 0x56), // Keypad -
    (75, 0x5C), (76, 0x5D), (77, 0x5E), // Keypad 4 5 6
    (78, 0x57), // Keypad +
    (79, 0x59), (80, 0x5A), (81, 0x5B), // Keypad 1 2 3
    (82, 0x62), (83, 0x63), // Keypad 0 .
    (86, 0x64), // extra key next to left Shift on ISO keyboards
    (87, 0x44), (88, 0x45), // F11 F12
    (96, 0x58), // Keypad Enter
    (97, 0xE4), // Right Ctrl
    (98, 0x54), // Keypad /
    (99, 0x46), // Print Screen
    (100, 0xE6), // Right Alt
    (102, 0x4A), (103, 0x52), (104, 0x4B), // Home Up PageUp
    (105, 0x50), (106, 0x4F), // Left Right
    (107, 0x4D), (108, 0x51), (109, 0x4E), // End Down PageDown
    (110, 0x49), (111, 0x4C), // Insert Delete
    (119, 0x48), // Pause
    (125, 0xE3), (126, 0xE7), // Left and right Super ("Windows") keys
    (127, 0x65), // Menu
];

/// Convert a Linux key code into a HID usage.
pub fn from_linux(code: u16) -> Option<u16> {
    LINUX.iter().find(|(c, _)| *c == code).map(|(_, hid)| *hid)
}

/// Convert a HID usage into a Linux key code.
pub fn to_linux(hid: u16) -> Option<u16> {
    LINUX.iter().find(|(_, h)| *h == hid).map(|(code, _)| *code)
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

    #[test]
    fn linux_table_has_no_duplicates_and_round_trips() {
        let mut codes = HashSet::new();
        let mut hids = HashSet::new();
        for (code, hid) in LINUX {
            assert!(codes.insert(*code), "duplicate Linux code {code}");
            assert!(hids.insert(*hid), "duplicate HID usage {hid:#x}");
            assert_eq!(from_linux(*code), Some(*hid));
            assert_eq!(to_linux(*hid), Some(*code));
        }
        assert_eq!(from_linux(0), None);
        assert_eq!(from_linux(240), None);
    }

    #[test]
    fn both_systems_know_the_same_keys() {
        // Every key Windows can send must be playable on Linux, and the
        // other way round (Pause is special on Windows only in how it is
        // injected, so it is absent from the Windows table).
        for (_, _, hid) in TABLE {
            assert!(to_linux(*hid).is_some(), "Linux lacks HID {hid:#x}");
        }
        for (_, hid) in LINUX {
            assert!(*hid == HID_PAUSE || to_windows(*hid).is_some(), "Windows lacks HID {hid:#x}");
        }
    }

    #[test]
    fn well_known_linux_keys() {
        // Values from the kernel's input-event-codes.h.
        assert_eq!(to_linux(0x04), Some(30)); // KEY_A
        assert_eq!(to_linux(0x1D), Some(44)); // KEY_Z
        assert_eq!(to_linux(0x28), Some(28)); // KEY_ENTER
        assert_eq!(to_linux(0x2C), Some(57)); // KEY_SPACE
        assert_eq!(to_linux(0x50), Some(105)); // KEY_LEFT
        assert_eq!(to_linux(0x4C), Some(111)); // KEY_DELETE
        assert_eq!(to_linux(HID_HOME), Some(102)); // KEY_HOME
        assert_eq!(to_linux(0xE3), Some(125)); // KEY_LEFTMETA
        assert_eq!(to_linux(0xE6), Some(100)); // KEY_RIGHTALT
        assert_eq!(to_linux(0x45), Some(88)); // KEY_F12
    }
}

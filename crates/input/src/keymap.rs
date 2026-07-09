//! USB HID usage code (page 0x07) → macOS virtual keycode.
//!
//! Clients send HID usages (layout-independent, spec 05/07); macOS
//! `CGEventCreateKeyboardEvent` wants ANSI virtual keycodes. Covers the
//! common desktop/gaming set; unmapped usages are dropped (logged upstream).

/// Map a HID usage to a macOS virtual keycode, or `None` if unmapped.
#[cfg(target_os = "macos")]
#[must_use]
pub fn hid_to_macos(usage: u16) -> Option<u16> {
    let code: u16 = match usage {
        // Letters a-z (HID 0x04-0x1D).
        0x04 => 0,  // a
        0x05 => 11, // b
        0x06 => 8,  // c
        0x07 => 2,  // d
        0x08 => 14, // e
        0x09 => 3,  // f
        0x0A => 5,  // g
        0x0B => 4,  // h
        0x0C => 34, // i
        0x0D => 38, // j
        0x0E => 40, // k
        0x0F => 37, // l
        0x10 => 46, // m
        0x11 => 45, // n
        0x12 => 31, // o
        0x13 => 35, // p
        0x14 => 12, // q
        0x15 => 15, // r
        0x16 => 1,  // s
        0x17 => 17, // t
        0x18 => 32, // u
        0x19 => 9,  // v
        0x1A => 13, // w
        0x1B => 7,  // x
        0x1C => 16, // y
        0x1D => 6,  // z
        // Digits 1-0 (HID 0x1E-0x27).
        0x1E => 18,
        0x1F => 19,
        0x20 => 20,
        0x21 => 21,
        0x22 => 23,
        0x23 => 22,
        0x24 => 26,
        0x25 => 28,
        0x26 => 25,
        0x27 => 29,
        // Control / whitespace.
        0x28 => 36, // Return
        0x29 => 53, // Escape
        0x2A => 51, // Delete (Backspace)
        0x2B => 48, // Tab
        0x2C => 49, // Space
        0x2D => 27, // -
        0x2E => 24, // =
        0x2F => 33, // [
        0x30 => 30, // ]
        0x31 => 42, // backslash
        0x33 => 41, // ;
        0x34 => 39, // '
        0x35 => 50, // `
        0x36 => 43, // ,
        0x37 => 47, // .
        0x38 => 44, // /
        0x39 => 57, // CapsLock
        // Function keys F1-F12.
        0x3A => 122,
        0x3B => 120,
        0x3C => 99,
        0x3D => 118,
        0x3E => 96,
        0x3F => 97,
        0x40 => 98,
        0x41 => 100,
        0x42 => 101,
        0x43 => 109,
        0x44 => 103,
        0x45 => 111,
        // Arrows (HID 0x4F-0x52).
        0x4F => 124, // Right
        0x50 => 123, // Left
        0x51 => 125, // Down
        0x52 => 126, // Up
        // Modifiers (HID 0xE0-0xE7).
        0xE0 => 59, // Left Control
        0xE1 => 56, // Left Shift
        0xE2 => 58, // Left Option
        0xE3 => 55, // Left Command
        0xE4 => 62, // Right Control
        0xE5 => 60, // Right Shift
        0xE6 => 61, // Right Option
        0xE7 => 54, // Right Command
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn common_keys_map() {
        use super::hid_to_macos;
        assert_eq!(hid_to_macos(0x1A), Some(13)); // w
        assert_eq!(hid_to_macos(0x04), Some(0)); // a
        assert_eq!(hid_to_macos(0x2C), Some(49)); // space
        assert_eq!(hid_to_macos(0x52), Some(126)); // up
        assert_eq!(hid_to_macos(0xFFFF), None);
    }
}

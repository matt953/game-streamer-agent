//! USB HID usage code (page 0x07) → per-OS keycode.
//!
//! Clients send HID usages (layout-independent, spec 05/07); macOS
//! `CGEventCreateKeyboardEvent` wants ANSI virtual keycodes and Windows
//! `SendInput` wants PS/2 set-1 scancodes. Covers the common desktop/gaming
//! set; unmapped usages are dropped (logged upstream).

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

/// Map a HID usage to a PS/2 set-1 scancode, or `None` if unmapped. The flag
/// marks an *extended* key — one whose real scancode carries an `E0` prefix,
/// which `SendInput` wants expressed as `KEYEVENTF_EXTENDEDKEY` rather than
/// in the code itself.
///
/// Scancodes, not virtual keys: games read the keyboard through raw input,
/// which reports scancodes and never sees a `wVk`-only injection.
#[cfg(windows)]
#[must_use]
pub fn hid_to_scancode(usage: u16) -> Option<(u16, bool)> {
    let code: u16 = match usage {
        // Letters a-z (HID 0x04-0x1D).
        0x04 => 0x1E, // a
        0x05 => 0x30, // b
        0x06 => 0x2E, // c
        0x07 => 0x20, // d
        0x08 => 0x12, // e
        0x09 => 0x21, // f
        0x0A => 0x22, // g
        0x0B => 0x23, // h
        0x0C => 0x17, // i
        0x0D => 0x24, // j
        0x0E => 0x25, // k
        0x0F => 0x26, // l
        0x10 => 0x32, // m
        0x11 => 0x31, // n
        0x12 => 0x18, // o
        0x13 => 0x19, // p
        0x14 => 0x10, // q
        0x15 => 0x13, // r
        0x16 => 0x1F, // s
        0x17 => 0x14, // t
        0x18 => 0x16, // u
        0x19 => 0x2F, // v
        0x1A => 0x11, // w
        0x1B => 0x2D, // x
        0x1C => 0x15, // y
        0x1D => 0x2C, // z
        // Digits 1-0 (HID 0x1E-0x27).
        0x1E => 0x02,
        0x1F => 0x03,
        0x20 => 0x04,
        0x21 => 0x05,
        0x22 => 0x06,
        0x23 => 0x07,
        0x24 => 0x08,
        0x25 => 0x09,
        0x26 => 0x0A,
        0x27 => 0x0B,
        // Control / whitespace / punctuation.
        0x28 => 0x1C, // Return
        0x29 => 0x01, // Escape
        0x2A => 0x0E, // Backspace
        0x2B => 0x0F, // Tab
        0x2C => 0x39, // Space
        0x2D => 0x0C, // -
        0x2E => 0x0D, // =
        0x2F => 0x1A, // [
        0x30 => 0x1B, // ]
        0x31 => 0x2B, // backslash
        0x33 => 0x27, // ;
        0x34 => 0x28, // '
        0x35 => 0x29, // `
        0x36 => 0x33, // ,
        0x37 => 0x34, // .
        0x38 => 0x35, // /
        0x39 => 0x3A, // CapsLock
        // Function keys F1-F12 (F11/F12 sit apart from the F1-F10 run).
        0x3A => 0x3B,
        0x3B => 0x3C,
        0x3C => 0x3D,
        0x3D => 0x3E,
        0x3E => 0x3F,
        0x3F => 0x40,
        0x40 => 0x41,
        0x41 => 0x42,
        0x42 => 0x43,
        0x43 => 0x44,
        0x44 => 0x57,
        0x45 => 0x58,
        // Navigation cluster (all extended) + ScrollLock.
        0x47 => 0x46,             // ScrollLock
        0x49 => return ext(0x52), // Insert
        0x4A => return ext(0x47), // Home
        0x4B => return ext(0x49), // PageUp
        0x4C => return ext(0x53), // Delete
        0x4D => return ext(0x4F), // End
        0x4E => return ext(0x51), // PageDown
        0x4F => return ext(0x4D), // Right
        0x50 => return ext(0x4B), // Left
        0x51 => return ext(0x50), // Down
        0x52 => return ext(0x48), // Up
        // Keypad. Divide and Enter are extended; the rest share codes with
        // the navigation keys and are disambiguated by the missing E0.
        0x53 => 0x45,             // NumLock
        0x54 => return ext(0x35), // Keypad /
        0x55 => 0x37,             // Keypad *
        0x56 => 0x4A,             // Keypad -
        0x57 => 0x4E,             // Keypad +
        0x58 => return ext(0x1C), // Keypad Enter
        0x59 => 0x4F,             // Keypad 1
        0x5A => 0x50,             // Keypad 2
        0x5B => 0x51,             // Keypad 3
        0x5C => 0x4B,             // Keypad 4
        0x5D => 0x4C,             // Keypad 5
        0x5E => 0x4D,             // Keypad 6
        0x5F => 0x47,             // Keypad 7
        0x60 => 0x48,             // Keypad 8
        0x61 => 0x49,             // Keypad 9
        0x62 => 0x52,             // Keypad 0
        0x63 => 0x53,             // Keypad .
        0x65 => return ext(0x5D), // Application (menu)
        // Modifiers (HID 0xE0-0xE7).
        0xE0 => 0x1D,             // Left Control
        0xE1 => 0x2A,             // Left Shift
        0xE2 => 0x38,             // Left Alt
        0xE3 => return ext(0x5B), // Left Windows
        0xE4 => return ext(0x1D), // Right Control
        0xE5 => 0x36,             // Right Shift
        0xE6 => return ext(0x38), // Right Alt
        0xE7 => return ext(0x5C), // Right Windows
        _ => return None,
    };
    Some((code, false))
}

#[cfg(windows)]
fn ext(code: u16) -> Option<(u16, bool)> {
    Some((code, true))
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

    #[cfg(windows)]
    #[test]
    fn common_keys_map_to_scancodes() {
        use super::hid_to_scancode;
        assert_eq!(hid_to_scancode(0x1A), Some((0x11, false))); // w
        assert_eq!(hid_to_scancode(0x04), Some((0x1E, false))); // a
        assert_eq!(hid_to_scancode(0x2C), Some((0x39, false))); // space
        assert_eq!(hid_to_scancode(0xFFFF), None);
    }

    /// Arrows, right-hand modifiers and keypad enter must carry the E0 flag,
    /// or games read them as their numpad twins.
    #[cfg(windows)]
    #[test]
    fn extended_keys_are_flagged() {
        use super::hid_to_scancode;
        assert_eq!(hid_to_scancode(0x52), Some((0x48, true))); // Up
        assert_eq!(hid_to_scancode(0x4C), Some((0x53, true))); // Delete
        assert_eq!(hid_to_scancode(0xE4), Some((0x1D, true))); // Right Control
        assert_eq!(hid_to_scancode(0x58), Some((0x1C, true))); // Keypad Enter
        // ...and their non-extended twins must not.
        assert_eq!(hid_to_scancode(0x60), Some((0x48, false))); // Keypad 8
        assert_eq!(hid_to_scancode(0xE0), Some((0x1D, false))); // Left Control
        assert_eq!(hid_to_scancode(0x28), Some((0x1C, false))); // Return
    }
}

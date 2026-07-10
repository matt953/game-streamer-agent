//! `SendInput` injection (spec 07). Needs no permission grant, but User
//! Interface Privilege Isolation makes every injection into an *elevated*
//! foreground window fail silently — run the agent elevated to control
//! elevated apps. `gsa doctor` surfaces this.
//!
//! Keys go in as scancodes rather than virtual keys because games read the
//! keyboard through raw input, which only ever sees a scancode.
//!
//! Gamepads have no `SendInput` equivalent and go through a kernel driver
//! instead — see [`vigem`], reached only through [`VirtualGamepad`].

use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use ::windows::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

use gsa_protocol::input::{GamepadInput, InputEvent, MouseButton, MouseMove};

use crate::VirtualGamepad;
use crate::keymap::hid_to_scancode;

mod vigem;

/// One wheel notch, as `mouseData` counts them.
const WHEEL_DELTA: f32 = 120.0;

/// `MOUSEEVENTF_ABSOLUTE` coordinates are normalized over this range.
const ABSOLUTE_MAX: f32 = 65535.0;

#[derive(Debug)]
pub struct WinInjector {
    /// Connected on the first gamepad event, so hosts that never see one
    /// never touch the driver.
    gamepad: Option<Box<dyn VirtualGamepad>>,
    /// Set once the driver has proven absent, so the warning stays a warning
    /// rather than one line per event at 250 Hz.
    gamepad_unavailable: bool,
}

impl WinInjector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            gamepad: None,
            gamepad_unavailable: false,
        }
    }

    fn gamepad(&mut self, input: &GamepadInput) {
        if self.gamepad.is_none() {
            if self.gamepad_unavailable {
                return;
            }
            match vigem::VigemGamepad::connect() {
                Ok(pad) => self.gamepad = Some(Box::new(pad)),
                Err(e) => {
                    self.gamepad_unavailable = true;
                    tracing::warn!(
                        error = ?e,
                        "no controller support: install ViGEmBus \
                         (https://github.com/nefarius/ViGEmBus/releases)"
                    );
                    return;
                }
            }
        }
        if let Some(pad) = &mut self.gamepad {
            pad.set_state(input);
        }
    }

    /// Post one synthesized event. `SendInput` rejects the whole batch when
    /// a higher-integrity process owns the foreground window (UIPI), which is
    /// the overwhelmingly common failure here — hence the hint.
    fn send(&self, input: INPUT) {
        // SAFETY: `input` is a fully initialized INPUT whose union arm matches
        // its `r#type`, and `cbSize` describes it exactly.
        let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
        if sent == 0 {
            tracing::trace!("SendInput rejected an event (elevated foreground window?)");
        }
    }

    fn key(&self, usage: u16, down: bool) {
        let Some((scancode, extended)) = hid_to_scancode(usage) else {
            tracing::trace!(usage, "unmapped HID key dropped");
            return;
        };
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        self.send(keyboard_input(scancode, flags));
    }

    fn mouse_move(&self, m: MouseMove) {
        match m {
            // Normalized over the *virtual desktop*, which is the captured
            // monitor only while this host has a single display. Multi-monitor
            // hosts need the session to tell us which display it captures.
            MouseMove::Absolute { x, y, .. } => self.send(mouse_input(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                (x.clamp(0.0, 1.0) * ABSOLUTE_MAX) as i32,
                (y.clamp(0.0, 1.0) * ABSOLUTE_MAX) as i32,
                0,
            )),
            MouseMove::Relative { dx, dy, .. } => {
                self.send(mouse_input(MOUSEEVENTF_MOVE, dx as i32, dy as i32, 0));
            }
            // MouseMove is non_exhaustive; ignore future variants.
            _ => (),
        }
    }

    fn mouse_button(&self, button: MouseButton, down: bool) {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
            // MouseButton is non_exhaustive; ignore future variants.
            _ => return,
        };
        self.send(mouse_input(flags, 0, 0, u32::from(data)));
    }

    fn wheel(&self, dx: f32, dy: f32) {
        if dy != 0.0 {
            let notches = (dy * WHEEL_DELTA) as i32;
            self.send(mouse_input(MOUSEEVENTF_WHEEL, 0, 0, notches as u32));
        }
        if dx != 0.0 {
            let notches = (dx * WHEEL_DELTA) as i32;
            self.send(mouse_input(MOUSEEVENTF_HWHEEL, 0, 0, notches as u32));
        }
    }
}

impl crate::Injector for WinInjector {
    fn inject(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key { usage, down, .. } => self.key(*usage, *down),
            InputEvent::MouseMove(m) => self.mouse_move(*m),
            InputEvent::MouseButton { button, down, .. } => self.mouse_button(*button, *down),
            InputEvent::MouseWheel { dx, dy, .. } => self.wheel(*dx, *dy),
            InputEvent::Gamepad(input) => self.gamepad(input),
            // GamepadMotion has no XInput equivalent; touch/pen on the
            // Windows desktop are out of scope.
            _ => (),
        }
    }
}

fn keyboard_input(scancode: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

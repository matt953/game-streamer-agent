//! CGEvent-based injection (spec 07). Needs the Accessibility TCC grant;
//! without it `CGEventPost` silently no-ops (macOS gives no error), so the
//! session should surface Accessibility state via `gsa doctor`.
//!
//! Fidelity beyond bare key/click events: modifier **flags** are tracked and
//! stamped on every event (so Shift/Cmd/Ctrl combos — and macOS shortcuts —
//! work and don't desync); mouse **drag** events are emitted while a button
//! is held; and **click state** is tracked so double/triple clicks register.

use std::time::{Duration, Instant};

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton,
};

use gsa_protocol::input::{InputEvent, MouseButton, MouseMove};

use crate::keymap::hid_to_macos;

/// Max gap + movement for a click to count as a continuation (double/triple).
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_SLOP: f64 = 4.0;

pub struct CgInjector {
    source: objc2_core_foundation::CFRetained<CGEventSource>,
    pos: CGPoint,
    screen: (f64, f64),
    /// Current modifier flags, stamped on every event.
    flags: CGEventFlags,
    /// Which mouse buttons are held (for drag vs move).
    left_down: bool,
    right_down: bool,
    middle_down: bool,
    /// Last mouse-down for click-count tracking: (button.0, when, pos, state).
    last_click: Option<(u32, Instant, CGPoint, i64)>,
    /// Click state carried from a down to its matching up.
    click_state: i64,
}

impl std::fmt::Debug for CgInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CgInjector")
            .field("screen", &self.screen)
            .finish()
    }
}

// SAFETY: `Injector: Send`. CGEvent APIs are safe to call from any single
// thread; the session owns this injector and calls it serially.
unsafe impl Send for CgInjector {}

/// Whether this process holds the Accessibility TCC grant. Without it
/// `CGEventPost` silently no-ops, so `gsa doctor` reports it.
#[must_use]
pub fn accessibility_authorized() -> bool {
    // SAFETY: no arguments, returns a CoreFoundation `Boolean` (0 or 1).
    unsafe { AXIsProcessTrusted() != 0 }
}

// `AXIsProcessTrusted` lives in ApplicationServices; no objc2 binding exists,
// so declare it directly.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
}

/// Modifier flag for a HID usage, or `None` if it's not a modifier.
fn modifier_flag(usage: u16) -> Option<CGEventFlags> {
    match usage {
        0xE0 | 0xE4 => Some(CGEventFlags::MaskControl),
        0xE1 | 0xE5 => Some(CGEventFlags::MaskShift),
        0xE2 | 0xE6 => Some(CGEventFlags::MaskAlternate),
        0xE3 | 0xE7 => Some(CGEventFlags::MaskCommand),
        _ => None,
    }
}

impl CgInjector {
    #[must_use]
    pub fn new() -> Option<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)?;
        let bounds = CGDisplayBounds(CGMainDisplayID());
        let screen = (bounds.size.width.max(1.0), bounds.size.height.max(1.0));
        Some(Self {
            source,
            pos: CGPoint { x: 0.0, y: 0.0 },
            screen,
            flags: CGEventFlags(0),
            left_down: false,
            right_down: false,
            middle_down: false,
            last_click: None,
            click_state: 1,
        })
    }

    fn post(&self, event: Option<&CGEvent>) {
        if let Some(e) = event {
            CGEvent::set_flags(Some(e), self.flags);
        }
        CGEvent::post(CGEventTapLocation::HIDEventTap, event);
    }

    fn mouse_event(
        &self,
        ty: CGEventType,
        button: CGMouseButton,
    ) -> Option<objc2_core_foundation::CFRetained<CGEvent>> {
        CGEvent::new_mouse_event(Some(&self.source), ty, self.pos, button)
    }
}

impl crate::Injector for CgInjector {
    fn inject(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key { usage, down, .. } => {
                if let Some(flag) = modifier_flag(*usage) {
                    if *down {
                        self.flags.0 |= flag.0;
                    } else {
                        self.flags.0 &= !flag.0;
                    }
                }
                let Some(code) = hid_to_macos(*usage) else {
                    tracing::trace!(usage, "unmapped HID key dropped");
                    return;
                };
                if let Some(e) = CGEvent::new_keyboard_event(Some(&self.source), code, *down) {
                    self.post(Some(&e));
                }
            }
            InputEvent::MouseMove(m) => self.mouse_move(*m),
            InputEvent::MouseButton { button, down, .. } => self.mouse_button(*button, *down),
            InputEvent::MouseWheel { dx, dy, .. } => self.scroll(*dx, *dy),
            // Gamepad injection needs the Virtual HID entitlement (D10);
            // touch/pen on macOS desktop are out of M1 scope.
            _ => {}
        }
    }
}

impl CgInjector {
    /// Event type + button for a mouse move given which buttons are held
    /// (drag while held, plain move otherwise).
    fn move_kind(&self) -> (CGEventType, CGMouseButton) {
        if self.left_down {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.right_down {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.middle_down {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        }
    }

    fn mouse_move(&mut self, m: MouseMove) {
        let deltas = match m {
            MouseMove::Absolute { x, y, .. } => {
                self.pos = CGPoint {
                    x: f64::from(x).clamp(0.0, 1.0) * self.screen.0,
                    y: f64::from(y).clamp(0.0, 1.0) * self.screen.1,
                };
                None
            }
            MouseMove::Relative { dx, dy, .. } => {
                self.pos = CGPoint {
                    x: (self.pos.x + f64::from(dx)).clamp(0.0, self.screen.0),
                    y: (self.pos.y + f64::from(dy)).clamp(0.0, self.screen.1),
                };
                Some((dx, dy))
            }
            // MouseMove is non_exhaustive; ignore future variants.
            _ => return,
        };
        let (ty, button) = self.move_kind();
        if let Some(e) = self.mouse_event(ty, button) {
            if let Some((dx, dy)) = deltas {
                // Raw deltas so pointer-locked games read motion.
                CGEvent::set_integer_value_field(
                    Some(&e),
                    CGEventField::MouseEventDeltaX,
                    dx as i64,
                );
                CGEvent::set_integer_value_field(
                    Some(&e),
                    CGEventField::MouseEventDeltaY,
                    dy as i64,
                );
            }
            self.post(Some(&e));
        }
    }

    fn mouse_button(&mut self, button: MouseButton, down: bool) {
        let (ty, cg) = match (button, down) {
            (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (MouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (MouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
            // Back/Forward: no standard CGMouseButton; drop for M1.
            _ => return,
        };
        match button {
            MouseButton::Left => self.left_down = down,
            MouseButton::Right => self.right_down = down,
            MouseButton::Middle => self.middle_down = down,
            _ => {}
        }
        // Click state: computed on down, reused for the matching up so macOS
        // pairs them into a single (double/triple) click.
        let click_state = if down {
            self.click_state_for(cg)
        } else {
            self.click_state
        };
        if let Some(e) = self.mouse_event(ty, cg) {
            CGEvent::set_integer_value_field(
                Some(&e),
                CGEventField::MouseEventClickState,
                click_state,
            );
            self.post(Some(&e));
        }
    }

    /// Compute (and record) the click count for a fresh mouse-down.
    fn click_state_for(&mut self, button: CGMouseButton) -> i64 {
        let now = Instant::now();
        let state = match self.last_click {
            Some((b, t, p, s))
                if b == button.0
                    && now.duration_since(t) < MULTI_CLICK_INTERVAL
                    && (self.pos.x - p.x).abs() < MULTI_CLICK_SLOP
                    && (self.pos.y - p.y).abs() < MULTI_CLICK_SLOP =>
            {
                s + 1
            }
            _ => 1,
        };
        self.last_click = Some((button.0, now, self.pos, state));
        self.click_state = state;
        state
    }

    fn scroll(&self, dx: f32, dy: f32) {
        if let Some(e) = self.mouse_event(CGEventType::ScrollWheel, CGMouseButton::Left) {
            CGEvent::set_integer_value_field(
                Some(&e),
                CGEventField::ScrollWheelEventDeltaAxis1,
                dy as i64,
            );
            CGEvent::set_integer_value_field(
                Some(&e),
                CGEventField::ScrollWheelEventDeltaAxis2,
                dx as i64,
            );
            self.post(Some(&e));
        }
    }
}

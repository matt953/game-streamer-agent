//! CGEvent-based injection (spec 07). Needs the Accessibility TCC grant;
//! without it `CGEventPost` silently no-ops (macOS gives no error), so the
//! session should surface Accessibility state via `gsa doctor`.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton,
};

use gsa_protocol::input::{InputEvent, MouseButton, MouseMove};

use crate::keymap::hid_to_macos;

pub struct CgInjector {
    source: objc2_core_foundation::CFRetained<CGEventSource>,
    /// Tracks cursor position for button events + relative moves.
    pos: CGPoint,
    /// Main display size, for absolute [0,1] → pixel mapping.
    screen: (f64, f64),
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
        })
    }

    fn post(&self, event: Option<&CGEvent>) {
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
    fn mouse_move(&mut self, m: MouseMove) {
        match m {
            MouseMove::Absolute { x, y, .. } => {
                self.pos = CGPoint {
                    x: f64::from(x).clamp(0.0, 1.0) * self.screen.0,
                    y: f64::from(y).clamp(0.0, 1.0) * self.screen.1,
                };
                if let Some(e) = self.mouse_event(CGEventType::MouseMoved, CGMouseButton::Left) {
                    self.post(Some(&e));
                }
            }
            MouseMove::Relative { dx, dy, .. } => {
                self.pos = CGPoint {
                    x: (self.pos.x + f64::from(dx)).clamp(0.0, self.screen.0),
                    y: (self.pos.y + f64::from(dy)).clamp(0.0, self.screen.1),
                };
                if let Some(e) = self.mouse_event(CGEventType::MouseMoved, CGMouseButton::Left) {
                    // Carry raw deltas so pointer-locked games read motion.
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
                    self.post(Some(&e));
                }
            }
            // MouseMove is non_exhaustive; ignore future variants.
            _ => {}
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
        if let Some(e) = self.mouse_event(ty, cg) {
            self.post(Some(&e));
        }
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

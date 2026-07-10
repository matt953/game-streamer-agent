//! OS input injection (spec 07). The session routes `InputEvent`s here for
//! sources that inject at the OS level (desktop / virtual display); emulator
//! sources consume input in-process and never reach this crate.

use gsa_protocol::input::InputEvent;

mod keymap;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(windows)]
mod windows;

/// Injects remote input into the host OS. `Send` so the session can own it
/// across async tasks; calls are serial.
pub trait Injector: Send {
    /// Inject one event. Best-effort — logs and continues on failure so a
    /// single bad event never wedges the input stream.
    fn inject(&mut self, event: &InputEvent);
}

/// Create the platform injector, or `None` where unsupported / permission is
/// missing (the session then simply doesn't inject).
#[must_use]
pub fn platform_injector() -> Option<Box<dyn Injector>> {
    #[cfg(target_os = "macos")]
    {
        macos::CgInjector::new().map(|i| Box::new(i) as Box<dyn Injector>)
    }
    #[cfg(windows)]
    {
        Some(Box::new(windows::WinInjector::new()))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None // per-OS injection backends land at M4/M5 (spec 07)
    }
}

/// Whether OS-level injection is authorized: `Some(true/false)` where the
/// platform has a checkable grant, `None` where injection isn't implemented.
/// On macOS this is the Accessibility TCC grant — without it `CGEventPost`
/// silently no-ops, so this is the only reliable readiness signal.
///
/// Windows needs no grant for `SendInput`, so this is unconditionally true;
/// the analogous trap there — UIPI swallowing injection into elevated
/// windows — depends on which window has focus and can't be checked upfront.
#[must_use]
pub fn injection_authorized() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        Some(macos::accessibility_authorized())
    }
    #[cfg(windows)]
    {
        Some(true)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

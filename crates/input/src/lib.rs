//! OS input injection (spec 07). The session routes `InputEvent`s here for
//! sources that inject at the OS level (desktop / virtual display); emulator
//! sources consume input in-process and never reach this crate.

use gsa_protocol::input::InputEvent;

mod keymap;

#[cfg(target_os = "macos")]
mod macos;

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
    #[cfg(not(target_os = "macos"))]
    {
        None // per-OS injection backends land at M4/M5 (spec 07)
    }
}

//! Monitor enumeration and geometry, in physical pixels.

use std::sync::OnceLock;

use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::{BOOL, PCWSTR};

use gsa_core::{Error, Result};

/// A capturable monitor.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    /// Stable id derived from `name` — see [`display_id`].
    pub id: u32,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Enumerate capturable monitors. Unlike macOS this needs no permission
/// grant, so it only fails if the Win32 calls themselves fail.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    ensure_dpi_aware();
    Ok(monitors()?.into_iter().filter_map(monitor_info).collect())
}

/// Re-resolve the `HMONITOR` for a display at `start` time; the handle we
/// enumerated earlier may have been invalidated by a topology change.
pub(crate) fn find_monitor(display: &DisplayInfo) -> Result<HMONITOR> {
    monitors()?
        .into_iter()
        .find(|m| monitor_info(*m).is_some_and(|i| i.id == display.id))
        .ok_or_else(|| Error::Capture(format!("display {} vanished", display.name)))
}

/// Report monitor geometry in physical pixels, which is what WGC captures.
/// Without this a DPI-unaware process sees scaled logical sizes and the
/// frame-pool size disagrees with the texture it gets back.
fn ensure_dpi_aware() {
    static DPI: OnceLock<()> = OnceLock::new();
    DPI.get_or_init(|| {
        // SAFETY: no preconditions. Fails harmlessly if awareness was already
        // set (e.g. by an application manifest).
        let set =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if let Err(e) = set {
            tracing::debug!(error = %e, "process DPI awareness already set");
        }
    });
}

/// Stable source id for a monitor: FNV-1a over its GDI device name.
/// `HMONITOR` values churn across sessions and monitor indices shift when a
/// display is unplugged, but `\\.\DISPLAYn` is reproducible — which is what
/// a persisted source selector needs. Never 0; that id is the test pattern.
fn display_id(device: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in device.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash.max(1)
}

/// `EnumDisplayMonitors` callback: append each monitor to the caller's `Vec`.
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<HMONITOR>` handed to EnumDisplayMonitors
    // below, which outlives the (synchronous) enumeration.
    let out = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    out.push(monitor);
    TRUE
}

fn monitors() -> Result<Vec<HMONITOR>> {
    let mut out: Vec<HMONITOR> = Vec::new();
    // SAFETY: enumeration is synchronous, so the pointer to `out` stays valid
    // for every `collect_monitor` call.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut out as isize),
        )
    };
    if !ok.as_bool() {
        return Err(Error::Capture("EnumDisplayMonitors failed".into()));
    }
    Ok(out)
}

/// Geometry + refresh rate for one monitor, or `None` if Windows won't
/// describe it (it was unplugged between enumeration and this call).
fn monitor_info(monitor: HMONITOR) -> Option<DisplayInfo> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `cbSize` marks this as the EX form, so Windows writes at most
    // `size_of::<MONITORINFOEXW>()` bytes.
    let ok = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return None;
    }
    let rect = info.monitorInfo.rcMonitor;
    let name = wide_to_string(&info.szDevice);
    Some(DisplayInfo {
        id: display_id(&name),
        width: (rect.right - rect.left).unsigned_abs(),
        height: (rect.bottom - rect.top).unsigned_abs(),
        refresh_hz: refresh_hz(&info.szDevice),
        name,
    })
}

/// Current refresh rate for a GDI device, defaulting to 60 Hz — the values
/// 0 and 1 mean "the hardware default" rather than a real frequency.
fn refresh_hz(device: &[u16; 32]) -> u32 {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `device` is a NUL-terminated wide string inside MONITORINFOEXW,
    // and `dmSize` bounds what Windows writes into `mode`.
    let ok = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &raw mut mode,
        )
    };
    if !ok.as_bool() || mode.dmDisplayFrequency < 2 {
        return 60;
    }
    mode.dmDisplayFrequency
}

fn wide_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|c| *c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

#[cfg(test)]
mod tests {
    use super::display_id;

    #[test]
    fn display_ids_are_stable_distinct_and_nonzero() {
        assert_eq!(display_id(r"\\.\DISPLAY1"), display_id(r"\\.\DISPLAY1"));
        assert_ne!(display_id(r"\\.\DISPLAY1"), display_id(r"\\.\DISPLAY2"));
        assert_ne!(display_id(r"\\.\DISPLAY1"), 0);
    }
}

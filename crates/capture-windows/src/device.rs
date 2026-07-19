//! DXGI adapter enumeration and D3D11 device creation.
//!
//! Capture deliberately does not choose a GPU. A hardware encoder can only
//! consume textures that live on *its* adapter, so the caller picks the
//! adapter (by LUID) and capture honours it — see [`crate::CaptureOutput`].
//! Nothing here knows what an encoder is.

use std::sync::OnceLock;

use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter, IDXGIDevice,
    IDXGIFactory1,
};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::core::Interface;

use gsa_core::{Error, Result};

/// One hardware GPU, as DXGI sees it.
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    /// DXGI's locally-unique adapter id, flattened to an `i64` so callers
    /// need no Windows types. Stable for as long as the adapter exists.
    pub luid: i64,
    /// PCI vendor id — `0x10DE` NVIDIA, `0x1002` AMD, `0x8086` Intel.
    pub vendor_id: u32,
    pub name: String,
}

/// Hardware adapters, in DXGI's preference order. Software adapters (WARP,
/// the Basic Render Driver) are excluded: they can neither capture nor
/// hardware-encode.
pub fn list_adapters() -> Result<Vec<AdapterInfo>> {
    let factory = dxgi_factory()?;
    let mut out = Vec::new();
    let mut index = 0;
    // SAFETY: enumeration terminates with DXGI_ERROR_NOT_FOUND.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        // SAFETY: `adapter` is live; GetDesc1 only writes the descriptor.
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if DXGI_ADAPTER_FLAG(desc.Flags as i32) == DXGI_ADAPTER_FLAG_SOFTWARE {
            continue;
        }
        out.push(AdapterInfo {
            luid: flatten_luid(desc.AdapterLuid),
            vendor_id: desc.VendorId,
            name: wide_to_string(&desc.Description),
        });
    }
    Ok(out)
}

/// A device on a specific adapter, without a context.
///
/// Exposed for hardware encoders, which must probe NVENC on the GPU they will
/// actually encode from before capture commits to it.
pub fn create_device_on(luid: i64) -> Result<ID3D11Device> {
    Ok(create_device(Some(luid))?.0)
}

/// Which adapter a device was created on.
pub fn device_adapter_luid(device: &ID3D11Device) -> Result<i64> {
    let dxgi = device
        .cast::<IDXGIDevice>()
        .map_err(|e| Error::Capture(format!("device is not a DXGI device: {e}")))?;
    // SAFETY: `dxgi` is live; GetAdapter returns a live adapter.
    let adapter = unsafe { dxgi.GetAdapter() }
        .map_err(|e| Error::Capture(format!("IDXGIDevice::GetAdapter: {e}")))?;
    // SAFETY: `adapter` is live; GetDesc only writes the descriptor.
    let desc = unsafe { adapter.GetDesc() }
        .map_err(|e| Error::Capture(format!("IDXGIAdapter::GetDesc: {e}")))?;
    Ok(flatten_luid(desc.AdapterLuid))
}

/// A BGRA-capable hardware D3D11 device and its immediate context.
///
/// `luid` pins the adapter; `None` takes DXGI's default. Capture works on any
/// adapter — WGC copies across GPUs when the display is driven by another —
/// so pinning costs nothing but a cross-adapter blit in the worst case.
pub(crate) fn create_device(luid: Option<i64>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let adapter = match luid {
        Some(luid) => Some(find_adapter(luid)?),
        None => None,
    };
    let mut device = None;
    let mut context = None;
    // An explicit adapter requires DRIVER_TYPE_UNKNOWN; the driver type is
    // implied by the adapter itself.
    let driver_type = if adapter.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_HARDWARE
    };
    // SAFETY: `adapter` is either a live adapter or null (default); the
    // out-params are written only on success.
    unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| Error::Capture(format!("D3D11CreateDevice: {e}")))?;
    match (device, context) {
        (Some(device), Some(context)) => {
            raise_gpu_priority(&device);
            Ok((device, context))
        }
        _ => Err(Error::Capture(
            "D3D11CreateDevice returned no device".into(),
        )),
    }
}

/// Ask WDDM to schedule this device's work ahead of other applications'.
///
/// A game saturating the GPU otherwise queues tens of milliseconds of work
/// ahead of our per-frame copy, and the WGC pool buffer stays busy until the
/// copy executes — throttling capture to the game's queue-drain pace. Our GPU
/// footprint is a handful of copies per frame, so preempting costs the game
/// nothing measurable.
fn raise_gpu_priority(device: &ID3D11Device) {
    let Ok(dxgi) = device.cast::<IDXGIDevice>() else {
        return;
    };
    // SAFETY: `dxgi` is live; 7 is the documented maximum priority.
    match unsafe { dxgi.SetGPUThreadPriority(7) } {
        Ok(()) => tracing::info!("gpu thread priority raised to 7"),
        Err(e) => tracing::warn!(error = %e, "SetGPUThreadPriority failed"),
    }
}

/// Wrap a D3D11 device as the WinRT `IDirect3DDevice` the frame pool wants.
pub(crate) fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi = device
        .cast::<IDXGIDevice>()
        .map_err(|e| Error::Capture(format!("device is not a DXGI device: {e}")))?;
    // SAFETY: `dxgi` is a live IDXGIDevice, the documented input.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .map_err(|e| Error::Capture(format!("CreateDirect3D11DeviceFromDXGIDevice: {e}")))?;
    inspectable
        .cast::<IDirect3DDevice>()
        .map_err(|e| Error::Capture(format!("not an IDirect3DDevice: {e}")))
}

/// Put the process in the MTA for the lifetime of the process. WGC's
/// free-threaded pool calls back on thread-pool threads, so there is no
/// apartment we could scope this to; the cookie is deliberately leaked.
pub(crate) fn ensure_mta() {
    static MTA: OnceLock<()> = OnceLock::new();
    MTA.get_or_init(|| {
        // SAFETY: no preconditions; failure only means some other component
        // already established the process's apartment, which suits us.
        if let Err(e) = unsafe { CoIncrementMTAUsage() } {
            tracing::warn!(error = %e, "CoIncrementMTAUsage failed");
        }
    });
}

fn dxgi_factory() -> Result<IDXGIFactory1> {
    // SAFETY: no preconditions; returns a fresh factory or an HRESULT.
    unsafe { CreateDXGIFactory1() }.map_err(|e| Error::Capture(format!("CreateDXGIFactory1: {e}")))
}

fn find_adapter(luid: i64) -> Result<IDXGIAdapter> {
    let factory = dxgi_factory()?;
    let mut index = 0;
    // SAFETY: enumeration terminates with DXGI_ERROR_NOT_FOUND.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        // SAFETY: `adapter` is live.
        if let Ok(desc) = (unsafe { adapter.GetDesc1() })
            && flatten_luid(desc.AdapterLuid) == luid
        {
            return adapter
                .cast::<IDXGIAdapter>()
                .map_err(|e| Error::Capture(format!("adapter cast: {e}")));
        }
    }
    Err(Error::Capture(format!("no GPU with adapter luid {luid}")))
}

/// DXGI splits a LUID across a signed high word and an unsigned low word.
fn flatten_luid(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

fn wide_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|c| *c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

//! Adopt a decoder's CVPixelBuffer into wgpu without copying it.
//!
//! The decoder writes into an IOSurface; Metal can address that surface as a
//! texture; wgpu can adopt a Metal texture. Chained together, the decoded
//! picture is sampled by the presenter's shaders exactly where the decoder
//! left it — the two 8 MB-per-frame copies (surface→RAM, RAM→texture) that
//! used to sit between decode and display are gone entirely.
//!
//! Lifetime is handled by wgpu itself: each adopted texture carries a drop
//! callback owning the `CVMetalTexture` (which retains the pixel buffer), and
//! wgpu defers resource destruction until the GPU has finished with it.

use std::ptr::NonNull;

use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVMetalTexture, CVMetalTextureCache, CVPixelBuffer};
use objc2_metal::{MTLPixelFormat, MTLTextureType};

use crate::decoder_vt::VtSurface;

/// One wrapped plane, ready to bind.
pub struct AdoptedPlane {
    pub texture: wgpu::Texture,
}

/// The wrapping machinery: a CoreVideo Metal texture cache bound to the same
/// device wgpu renders with.
pub struct VtTextureCache {
    cache: CFRetained<CVMetalTextureCache>,
}

// SAFETY: the texture cache is a CF object, thread-safe to use from the render
// thread it was created for; we never share it across threads.
unsafe impl Send for VtTextureCache {}

impl VtTextureCache {
    /// Build a cache on wgpu's own Metal device. `None` when the device is
    /// not Metal (a software adapter), in which case the CPU path is used.
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Option<Self> {
        // SAFETY: read-only access to the hal device; the guard is dropped
        // before this returns.
        let raw = unsafe {
            let hal = device.as_hal::<wgpu::hal::api::Metal>()?;
            hal.raw_device().clone()
        };
        let mut cache: *mut CVMetalTextureCache = std::ptr::null_mut();
        // SAFETY: a live Metal device and a writable out-pointer.
        let rc = unsafe {
            CVMetalTextureCache::create(
                None,
                None,
                objc2::runtime::ProtocolObject::from_ref(&*raw),
                None,
                NonNull::from(&mut cache),
            )
        };
        if rc != 0 || cache.is_null() {
            tracing::warn!(rc, "no Metal texture cache; falling back to CPU copies");
            return None;
        }
        // SAFETY: create handed us +1 ownership of a non-null cache.
        let cache = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cache)) };
        Some(Self { cache })
    }

    /// Wrap one plane of the surface as a wgpu texture.
    ///
    /// `plane` is ignored by CoreVideo for non-planar buffers. The wgpu
    /// texture owns a retained `CVMetalTexture` through its drop callback, so
    /// the underlying surface outlives every use of the texture.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn adopt_plane(
        &self,
        device: &wgpu::Device,
        surface: &VtSurface,
        plane: usize,
        metal_format: MTLPixelFormat,
        wgpu_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Option<AdoptedPlane> {
        let pb: &CVPixelBuffer = surface.pixel_buffer();
        let mut out: *mut CVMetalTexture = std::ptr::null_mut();
        // SAFETY: live cache and buffer; dimensions describe the requested
        // plane; writable out-pointer.
        let rc = unsafe {
            CVMetalTextureCache::create_texture_from_image(
                None,
                &self.cache,
                pb,
                None,
                metal_format,
                width as usize,
                height as usize,
                plane,
                NonNull::from(&mut out),
            )
        };
        if rc != 0 || out.is_null() {
            tracing::warn!(rc, plane, "could not wrap a decoded plane");
            return None;
        }
        // SAFETY: +1 ownership from create, non-null checked above.
        let cv_texture = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) };
        // SAFETY: the CVMetalTexture holds a valid MTLTexture for its lifetime.
        let raw = objc2_core_video::CVMetalTextureGetTexture(&cv_texture)?;

        // The drop callback owns the CVMetalTexture (and through it the pixel
        // buffer); wgpu runs it only once the GPU is done with the texture.
        let keep_alive = KeepAlive(cv_texture);
        // SAFETY: `raw` is a live MTLTexture of exactly `wgpu_format`'s layout
        // and the given extent; the drop callback keeps its backing alive.
        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                raw,
                wgpu_format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                Some(Box::new(move || drop(keep_alive))),
            )
        };
        // SAFETY: the hal texture was created on this device's own Metal
        // device, matches the descriptor, and is initialised (decoded into).
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("vt-plane"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu_format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            )
        };
        Some(AdoptedPlane { texture })
    }
}

/// Moves the retained CoreVideo texture into wgpu's drop callback: held for
/// its retain count alone, released when wgpu is done with the texture.
struct KeepAlive(#[allow(dead_code)] CFRetained<CVMetalTexture>);
// SAFETY: CF objects are thread-safe to release from any thread, which is all
// the drop callback does.
unsafe impl Send for KeepAlive {}
// SAFETY: as above — the callback only releases the reference.
unsafe impl Sync for KeepAlive {}

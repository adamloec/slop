//! The presentable surface a window exposes to Vulkan.
//!
//! This module takes **raw window handles**, never a window type. `slop-rhi`
//! therefore has no opinion about `winit` or any other windowing library, the
//! headless path pulls in nothing, and a future editor or embedder can supply a
//! surface from whatever it already owns.
//!
//! Surface creation is also the one place `docs/DESIGN.md` §2.13 singles out as
//! never to hand-roll: `VK_KHR_win32_surface` against xlib and Wayland is a
//! platform split that `ash-window` already absorbs, including the Linux
//! X11-versus-Wayland axis that does not exist on Windows at all.

use std::ffi::CString;
use std::sync::Arc;

use ash::vk;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crate::{Instance, RhiError};

/// Instance extensions required to present to a window on this platform.
///
/// The result goes into [`InstanceConfig`](crate::InstanceConfig)'s
/// `required_extensions`, and must be supplied *before* the instance is created
/// — which is why this is a free function taking only a display handle rather
/// than a method on anything.
///
/// In headless mode, simply do not call it: the instance stays surface-free,
/// and [`Device`](crate::Device) will correspondingly not request the swapchain
/// extension.
///
/// # Errors
///
/// Fails if the platform's surface extension is unavailable.
pub fn required_surface_extensions(display: RawDisplayHandle) -> Result<Vec<CString>, RhiError> {
    let names = ash_window::enumerate_required_extensions(display)?;

    Ok(names
        .iter()
        .map(|&name| {
            // SAFETY: `ash-window` returns pointers to NUL-terminated string
            // literals with static lifetime.
            unsafe { std::ffi::CStr::from_ptr(name) }.to_owned()
        })
        .collect())
}

/// A Vulkan surface backed by a platform window.
pub struct Surface {
    // Drop order: the surface must be destroyed before the instance that owns
    // it. The `Arc` guarantees the instance cannot go first.
    handle: vk::SurfaceKHR,
    loader: ash::khr::surface::Instance,
    instance: Arc<Instance>,
}

impl Surface {
    /// Create a surface for a window.
    ///
    /// The instance must have been created with the extensions
    /// [`required_surface_extensions`] reports for this display, or creation
    /// fails.
    ///
    /// # Safety
    ///
    /// The handles must describe a window that outlives this `Surface`.
    /// Destroying the window first leaves the surface referring to something
    /// that no longer exists, which Vulkan cannot detect and which this type
    /// cannot express in the type system — the window is owned by a layer above
    /// the RHI.
    ///
    /// # Errors
    ///
    /// Fails if the required instance extensions were not enabled, or the
    /// platform rejects the handles.
    pub unsafe fn new(
        instance: &Arc<Instance>,
        display: RawDisplayHandle,
        window: RawWindowHandle,
    ) -> Result<Self, RhiError> {
        // SAFETY: the caller guarantees the handles describe a live window that
        // outlives this surface, which is this function's own safety contract.
        let handle = unsafe {
            ash_window::create_surface(instance.entry(), instance.raw(), display, window, None)
        }?;

        let loader = ash::khr::surface::Instance::new(instance.entry(), instance.raw());

        Ok(Self {
            handle,
            loader,
            instance: Arc::clone(instance),
        })
    }

    /// The underlying handle.
    pub fn handle(&self) -> vk::SurfaceKHR {
        self.handle
    }

    /// The extension loader, for surface queries.
    pub fn loader(&self) -> &ash::khr::surface::Instance {
        &self.loader
    }

    /// The instance this surface belongs to.
    pub fn instance(&self) -> &Arc<Instance> {
        &self.instance
    }

    /// Surface capabilities on a given adapter — image count bounds, extents,
    /// and supported transforms.
    ///
    /// # Errors
    ///
    /// Fails if the adapter cannot present to this surface.
    pub fn capabilities(
        &self,
        physical_device: vk::PhysicalDevice,
    ) -> Result<vk::SurfaceCapabilitiesKHR, RhiError> {
        // SAFETY: both handles belong to this surface's instance.
        let capabilities = unsafe {
            self.loader
                .get_physical_device_surface_capabilities(physical_device, self.handle)
        }?;

        Ok(capabilities)
    }

    /// Formats this adapter can present in.
    ///
    /// # Errors
    ///
    /// Fails if the adapter cannot present to this surface.
    pub fn formats(
        &self,
        physical_device: vk::PhysicalDevice,
    ) -> Result<Vec<vk::SurfaceFormatKHR>, RhiError> {
        // SAFETY: both handles belong to this surface's instance.
        let formats = unsafe {
            self.loader
                .get_physical_device_surface_formats(physical_device, self.handle)
        }?;

        Ok(formats)
    }

    /// Present modes this adapter supports for this surface.
    ///
    /// # Errors
    ///
    /// Fails if the adapter cannot present to this surface.
    pub fn present_modes(
        &self,
        physical_device: vk::PhysicalDevice,
    ) -> Result<Vec<vk::PresentModeKHR>, RhiError> {
        // SAFETY: both handles belong to this surface's instance.
        let modes = unsafe {
            self.loader
                .get_physical_device_surface_present_modes(physical_device, self.handle)
        }?;

        Ok(modes)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the surface was created from `self.instance`, which is still
        // alive because we hold an `Arc` to it, and this is the only destroy.
        unsafe { self.loader.destroy_surface(self.handle, None) };
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface").finish_non_exhaustive()
    }
}
